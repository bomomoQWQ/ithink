//! Chat/session policy belongs to this host, never to the kernel.
use super::web_settings::{Settings, SettingsUpdate};
use ithink::agent_loop::input_item;
use ithink::plugin::BootstrapInput;
use ithink::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const TASK_LIMIT: usize = 256;
const WAIT: Duration = Duration::from_secs(8);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTask {
    pub prompt: String,
    #[serde(default = "auto")]
    pub scenario: String,
}
fn auto() -> String {
    "auto".into()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewMessage {
    pub content: String,
}
#[derive(Clone)]
struct TaskMeta {
    prompt: String,
    scenario: String,
    created_at_ms: u64,
}
struct Conversation {
    id: u64,
    title: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    tasks: Vec<TaskId>,
}
struct HostState {
    tasks: BTreeMap<TaskId, TaskMeta>,
    conversations: BTreeMap<u64, Conversation>,
    next_conversation: u64,
}
impl HostState {
    fn new() -> Self {
        Self {
            tasks: BTreeMap::new(),
            conversations: BTreeMap::new(),
            next_conversation: 1,
        }
    }
}
type Reply = mpsc::SyncSender<Result<Value, KernelError>>;
enum Command {
    Create(NewTask, Reply),
    Cancel(TaskId, Reply),
    NewConversation(Reply),
    Message(u64, NewMessage, Reply),
    Configure(SettingsUpdate, Reply),
}
pub struct WebApp {
    commands: mpsc::SyncSender<Command>,
    snapshot: Arc<RwLock<Value>>,
    settings: Arc<RwLock<Settings>>,
    testing: AtomicBool,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl WebApp {
    pub fn start(
        fixture: &Path,
        data_dir: Option<&Path>,
        offline: bool,
    ) -> Result<Self, KernelError> {
        let settings = Settings::load(data_dir, offline)?;
        let mut runtime = Runtime::new();
        runtime.define_operation("model", "generate", "model.generate")?;
        runtime.define_operation("tools", "add", "tool.add")?;
        runtime.set_call_timeout(Duration::from_secs(125));
        let mut tools = PluginSpec::new("web-tools", fixture, Level::Worker);
        tools.args = vec!["web-tools".into(), "tools".into()];
        tools.capabilities = [CAP_BACKEND, "provide:tools"]
            .into_iter()
            .map(String::from)
            .collect();
        tools.provides = vec!["tools".into()];
        runtime.load_plugin(tools)?;
        load_model(&mut runtime, fixture, &settings)?;
        let mut spec = PluginSpec::new("web-loop", fixture, Level::Main);
        spec.args = vec!["web-loop".into(), "loop".into()];
        spec.capabilities = [
            CAP_TASK,
            CAP_CONTEXT,
            CAP_EVENT,
            "model.generate",
            "tool.add",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let aid = runtime.load_plugin(spec)?.aid;
        let started = Instant::now();
        let state = HostState::new();
        let snapshot = Arc::new(RwLock::new(make_snapshot(
            &runtime, &state, &settings, started,
        )));
        let settings = Arc::new(RwLock::new(settings));
        let stopping = Arc::new(AtomicBool::new(false));
        let (commands, receiver) = mpsc::sync_channel(64);
        let view = Arc::clone(&snapshot);
        let stop = Arc::clone(&stopping);
        let config = Arc::clone(&settings);
        let fixture = fixture.to_path_buf();
        let worker = thread::Builder::new()
            .name("ithink-web-runtime".into())
            .spawn(move || {
                let mut state = HostState::new();
                let mut last_snapshot = Instant::now();
                while !stop.load(Ordering::Acquire) {
                    runtime.pump(Duration::from_millis(5));
                    while runtime.take_notice().is_some() {}
                    for _ in 0..16 {
                        let command = match receiver.try_recv() {
                            Ok(c) => c,
                            Err(_) => break,
                        };
                        let settings = config.read().unwrap_or_else(|p| p.into_inner()).clone();
                        let (result, reply) = match command {
                            Command::NewConversation(reply) => {
                                (new_conversation(&mut state), reply)
                            }
                            Command::Message(id, input, reply) => (
                                send_message(
                                    &mut runtime,
                                    aid,
                                    &mut state,
                                    &settings,
                                    id,
                                    input.content,
                                    None,
                                ),
                                reply,
                            ),
                            Command::Create(input, reply) => {
                                let result = (|| {
                                    let id = new_conversation(&mut state)?["conversation"]
                                        .as_u64()
                                        .unwrap();
                                    send_message(
                                        &mut runtime,
                                        aid,
                                        &mut state,
                                        &settings,
                                        id,
                                        input.prompt,
                                        Some(input.scenario),
                                    )
                                })();
                                (result, reply)
                            }
                            Command::Cancel(id, reply) => {
                                let result = if state.tasks.contains_key(&id) {
                                    runtime
                                        .request(Request::TaskCancel { task: id }, WAIT)
                                        .map(|_| json!({"task":id}))
                                } else {
                                    Err(KernelError::missing("web task"))
                                };
                                (result, reply)
                            }
                            Command::Configure(input, reply) => {
                                let result =
                                    configure(&mut runtime, &fixture, &state, &settings, input)
                                        .map(|next| {
                                            let public = next.public();
                                            *config.write().unwrap_or_else(|p| p.into_inner()) =
                                                next;
                                            public
                                        });
                                (result, reply)
                            }
                        };
                        let settings = config.read().unwrap_or_else(|p| p.into_inner());
                        *view.write().unwrap_or_else(|p| p.into_inner()) =
                            make_snapshot(&runtime, &state, &settings, started);
                        let _ = reply.try_send(result);
                    }
                    if last_snapshot.elapsed() >= Duration::from_millis(150) {
                        let settings = config.read().unwrap_or_else(|p| p.into_inner());
                        *view.write().unwrap_or_else(|p| p.into_inner()) =
                            make_snapshot(&runtime, &state, &settings, started);
                        last_snapshot = Instant::now();
                    }
                }
            })
            .map_err(|e| KernelError::new("thread_start_failed", e.to_string()))?;
        Ok(Self {
            commands,
            snapshot,
            settings,
            testing: AtomicBool::new(false),
            stopping,
            worker: Some(worker),
        })
    }
    pub fn state(&self) -> Value {
        self.snapshot
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn settings(&self) -> Value {
        self.settings
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .public()
    }
    pub fn create(&self, input: NewTask) -> Result<Value, KernelError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::Create(input, tx), rx)
    }
    pub fn conversation(&self) -> Result<Value, KernelError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::NewConversation(tx), rx)
    }
    pub fn message(&self, id: u64, input: NewMessage) -> Result<Value, KernelError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::Message(id, input, tx), rx)
    }
    pub fn configure(&self, input: SettingsUpdate) -> Result<Value, KernelError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::Configure(input, tx), rx)
    }
    pub fn cancel(&self, id: TaskId) -> Result<Value, KernelError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::Cancel(id, tx), rx)
    }
    pub fn test(&self, input: SettingsUpdate) -> Result<Value, KernelError> {
        if self.testing.swap(true, Ordering::AcqRel) {
            return Err(KernelError::new("busy", "已有一个连接测试在运行"));
        }
        let result = (|| {
            let mut settings = self
                .settings
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .updated(input)?;
            settings.provider.timeout_secs = settings.provider.timeout_secs.min(10);
            if settings.mode == "offline" {
                return Ok(
                    json!({"ok":true,"latency_ms":0,"model":"offline","message":"离线演示不访问外部模型"}),
                );
            }
            ithink::openai::test_connection(&settings.provider)
        })();
        self.testing.store(false, Ordering::Release);
        result
    }
    fn send(
        &self,
        command: Command,
        rx: mpsc::Receiver<Result<Value, KernelError>>,
    ) -> Result<Value, KernelError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(KernelError::new("unavailable", "host is shutting down"));
        }
        self.commands
            .try_send(command)
            .map_err(|_| KernelError::new("busy", "web command queue full or disconnected"))?;
        rx.recv_timeout(WAIT).map_err(|_| {
            KernelError::new("timeout", "host did not reply; refresh before retrying")
        })?
    }
}
impl Drop for WebApp {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn load_model(rt: &mut Runtime, fixture: &Path, settings: &Settings) -> Result<(), KernelError> {
    if !settings.configured() {
        if rt.plugin("web-model").is_some() {
            rt.unload_plugin("web-model")?;
        }
        return Ok(());
    }
    let mut spec = if settings.mode == "offline" {
        let mut s = PluginSpec::new("web-model", fixture, Level::Worker);
        s.args = vec!["web-model".into(), "model".into()];
        s
    } else {
        let provider = fixture.with_file_name(if cfg!(windows) {
            "ithink-model-provider.exe"
        } else {
            "ithink-model-provider"
        });
        if !provider.is_file() {
            return Err(KernelError::new(
                "missing_provider",
                "请重新构建包含 web/openai 功能的模型插件",
            ));
        }
        let mut s = PluginSpec::new("web-model", provider, Level::Worker);
        s.args = vec!["web-model".into()];
        s.bootstrap = Some(BootstrapInput::json(&settings.provider)?);
        s
    };
    spec.capabilities = [CAP_BACKEND, "provide:model"]
        .into_iter()
        .map(String::from)
        .collect();
    spec.provides = vec!["model".into()];
    rt.load_plugin(spec)?;
    Ok(())
}
fn configure(
    rt: &mut Runtime,
    fixture: &Path,
    state: &HostState,
    current: &Settings,
    input: SettingsUpdate,
) -> Result<Settings, KernelError> {
    if state.tasks.keys().any(|id| {
        rt.kernel()
            .task(*id)
            .is_some_and(|t| t.state == TaskState::Running)
    }) {
        return Err(KernelError::new(
            "busy",
            "请先停止运行中的任务再切换模型设置",
        ));
    }
    let candidate = current.updated(input)?;
    // New process activation fails without removing the old provider. Do that
    // before committing disk settings; no background turns are active here.
    load_model(rt, fixture, &candidate)?;
    if let Err(error) = candidate.persist() {
        if load_model(rt, fixture, current).is_err() {
            let _ = rt.unload_plugin("web-model");
            return Err(KernelError::new(
                "settings_rollback",
                "设置保存失败且旧模型未能恢复，请重新保存配置",
            ));
        }
        return Err(error);
    }
    Ok(candidate)
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn new_conversation(state: &mut HostState) -> Result<Value, KernelError> {
    if state.conversations.len() >= 128 {
        return Err(KernelError::new(
            "conversation_limit",
            "最多保留128个内存会话，重启后清空",
        ));
    }
    let id = state.next_conversation;
    state.next_conversation += 1;
    let at = now();
    state.conversations.insert(
        id,
        Conversation {
            id,
            title: "新对话".into(),
            created_at_ms: at,
            updated_at_ms: at,
            tasks: vec![],
        },
    );
    Ok(json!({"conversation":id}))
}
fn send_message(
    rt: &mut Runtime,
    aid: Aid,
    state: &mut HostState,
    settings: &Settings,
    id: u64,
    prompt: String,
    scenario: Option<String>,
) -> Result<Value, KernelError> {
    if prompt.trim().is_empty() || prompt.chars().count() > 2000 {
        return Err(KernelError::invalid("请输入1–2000字的消息"));
    }
    if !settings.configured() {
        return Err(KernelError::new(
            "not_configured",
            "请先在设置中配置 API 地址、模型和密钥",
        ));
    }
    if state.tasks.len() >= TASK_LIMIT {
        return Err(KernelError::new(
            "task_limit",
            "本次运行已达到256条消息上限",
        ));
    }
    let conversation = state
        .conversations
        .get(&id)
        .ok_or_else(|| KernelError::missing("conversation"))?;
    if conversation.tasks.iter().any(|id| {
        rt.kernel()
            .task(*id)
            .is_some_and(|t| t.state == TaskState::Running)
    }) {
        return Err(KernelError::new(
            "busy",
            "当前对话仍在运行，可停止或新建对话",
        ));
    }
    let mut seed = vec![];
    // Previous task views already contain their inherited prefix; use only the
    // latest completed view rather than duplicating earlier messages.
    if let Some(previous) = conversation.tasks.iter().rev().find(|&&task| {
        rt.kernel()
            .task(task)
            .is_some_and(|t| t.state == TaskState::Completed)
    }) {
        seed = task_context(rt, *previous);
    }
    let scenario = scenario.unwrap_or_else(|| "auto".into());
    let actual = if settings.mode == "offline" {
        if ![
            "auto",
            "direct",
            "tool",
            "hold_model",
            "hold_tool",
            "tool_error",
            "model_error",
        ]
        .contains(&scenario.as_str())
        {
            return Err(KernelError::invalid("不支持的离线演示场景"));
        }
        let numbers = parse_addition(&prompt);
        if scenario == "tool" && numbers.is_none() {
            return Err(KernelError::invalid("加法场景请输入两个整数"));
        }
        let actual = if scenario == "auto" {
            if numbers.is_some() { "tool" } else { "echo" }
        } else {
            &scenario
        };
        let (a, b) = numbers.unwrap_or((2, 3));
        seed.push(input_item(
            json!({"scenario":actual,"text":prompt,"a":a,"b":b}),
        )?);
        actual.to_string()
    } else {
        seed.push(input_item(json!(prompt))?);
        "api".into()
    };
    let value = rt.request(
        Request::TaskCreate {
            owner: aid,
            parent: None,
            limits: ContextLimits::default(),
            seed,
        },
        WAIT,
    )?;
    let task: TaskId = serde_json::from_value(value["task"].clone())
        .map_err(|_| KernelError::new("host_error", "任务编号无效"))?;
    let at = now();
    state.tasks.insert(
        task,
        TaskMeta {
            prompt: prompt.clone(),
            scenario: actual,
            created_at_ms: at,
        },
    );
    let conversation = state.conversations.get_mut(&id).unwrap();
    if conversation.tasks.is_empty() {
        conversation.title = prompt.chars().take(36).collect();
    }
    conversation.updated_at_ms = at;
    conversation.tasks.push(task);
    Ok(json!({"task":task,"conversation":id}))
}
fn task_context(rt: &Runtime, id: TaskId) -> Vec<ContextItem> {
    let Some(ctx) = rt.kernel().context(id) else {
        return vec![];
    };
    let view = ctx.snapshot();
    if rt
        .kernel()
        .task(id)
        .is_some_and(|t| t.state != TaskState::Running)
    {
        ctx.history()
            .iter()
            .rev()
            .find_map(|e| match e {
                ContextEntry::ReleaseView { original, .. } => Some(original.clone()),
                _ => None,
            })
            .unwrap_or(view.items)
    } else {
        view.items
    }
}
fn make_snapshot(rt: &Runtime, state: &HostState, settings: &Settings, started: Instant) -> Value {
    let mut events: BTreeMap<TaskId, Vec<Value>> = BTreeMap::new();
    for record in rt.kernel().journal() {
        if record.kind == "event.published" {
            let data = &record.data["data"];
            if let Some(id) = data["task"].as_u64() {
                let entries = events.entry(TaskId(id)).or_default();
                if entries.len() < 256 {
                    entries.push(json!({"sequence":record.sequence,"event":data["event"]}));
                }
            }
        }
    }
    let tasks:BTreeMap<_,_>=state.tasks.iter().filter_map(|(id,meta)|{
        let task=rt.kernel().task(*id)?;let context=task_context(rt,*id);let view=rt.kernel().context(*id)?.snapshot();let events=events.remove(id).unwrap_or_default();
        let stage=if task.state!=TaskState::Running{match task.state{TaskState::Completed=>"已完成",TaskState::Cancelled=>"已停止",_=>"执行失败"}}else{events.last().and_then(|e|e["event"]["kind"].as_str()).map_or("正在读取上下文",|kind|match kind{"step_started"=>"正在请求模型","model_output"=>"准备执行工具","tool_finished"=>"写回工具结果","needs_compaction"=>"等待压缩上下文","needs_intervention"=>"需要处理",_=>"运行中"})};
        let used=context.iter().fold(0u64,|sum,item|sum.saturating_add(item.units));
        Some((*id,json!({"id":id,"task":id,"prompt":meta.prompt,"scenario":meta.scenario,"created_at_ms":meta.created_at_ms,"state":task.state,"stage":stage,"result":task.result,"events":events,"context":context,"used":used,"max_input":view.max_input})))
    }).collect();
    let mut conversations:Vec<_>=state.conversations.values().map(|c|json!({"id":c.id,"title":c.title,"created_at_ms":c.created_at_ms,"updated_at_ms":c.updated_at_ms,"active_task":c.tasks.iter().find(|id|rt.kernel().task(**id).is_some_and(|t|t.state==TaskState::Running)),"turns":c.tasks.iter().filter_map(|id|tasks.get(id)).collect::<Vec<_>>()})).collect();
    conversations.sort_by_key(|c| std::cmp::Reverse(c["updated_at_ms"].as_u64().unwrap_or(0)));
    json!({"mode":settings.mode,"provider":settings.public(),"uptime_ms":started.elapsed().as_millis() as u64,"task_limit":TASK_LIMIT,"agents":{"loop":rt.plugin("web-loop").is_some(),"model":rt.plugin("web-model").is_some(),"tools":rt.plugin("web-tools").is_some()},"tasks":tasks.values().rev().collect::<Vec<_>>(),"conversations":conversations})
}
fn parse_addition(text: &str) -> Option<(i64, i64)> {
    let chars: Vec<char> = text.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if *ch != '+' && *ch != '＋' {
            continue;
        }
        let left: String = chars[..index].iter().collect();
        let right: String = chars[index + 1..].iter().collect();
        let left = left.trim_end();
        let start = left
            .rfind(|c: char| !c.is_ascii_digit() && c != '-')
            .map_or(0, |i| i + left[i..].chars().next().unwrap().len_utf8());
        let right = right.trim_start();
        let end = right
            .find(|c: char| !c.is_ascii_digit() && c != '-')
            .unwrap_or(right.len());
        if let (Ok(a), Ok(b)) = (left[start..].parse::<i64>(), right[..end].parse::<i64>()) {
            return Some((a, b));
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_addition() {
        assert_eq!(parse_addition("帮我算17 + 25"), Some((17, 25)));
        assert_eq!(parse_addition("-7 ＋ -8"), Some((-7, -8)));
        assert_eq!(parse_addition("hi"), None);
    }
}
