//! Optional user-space model/tool loop. The kernel does not import this module.
//!
//! This is a pull-driven state machine: it yields public API requests and never
//! performs I/O or blocks. Different drivers may run it in a process plugin or
//! an embedded host. Model/tool implementations are replaceable service peers.
use crate::api::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const MAX_PAYLOAD: usize = crate::protocol::MAX_FRAME_BYTES / 2 - 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRoute {
    pub service: String,
    pub operation: String,
}
impl ServiceRoute {
    pub fn new(service: impl Into<String>, operation: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            operation: operation.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub route: ServiceRoute,
}

/// Provider-neutral contract. Adapters own provider wire formats and streaming.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelReply {
    pub content: Value,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Optional usage metadata, including native cache statistics if supplied.
    #[serde(default)]
    pub usage: Option<Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInput {
    pub task: TaskId,
    pub step: u32,
    /// Exact ordered kernel items. No merging, trimming or prompt rewriting.
    pub context: Vec<ContextItem>,
    pub tools: Vec<ModelTool>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum LoopMessage {
    User {
        content: Value,
    },
    Assistant {
        content: Value,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        result: BackendResult,
    },
}

#[derive(Debug, Clone)]
pub struct LoopConfig {
    pub model: ServiceRoute,
    pub tools: Vec<ToolDefinition>,
    pub max_steps: u32,
    pub max_tool_calls_per_step: usize,
    pub max_context_items: usize,
    /// Advisory estimated units; applications may provide their own meter.
    pub meter: fn(&Value) -> Result<u64, KernelError>,
}
impl LoopConfig {
    pub fn new(model: ServiceRoute) -> Self {
        Self {
            model,
            tools: vec![],
            max_steps: 32,
            max_tool_calls_per_step: 16,
            max_context_items: 4096,
            meter: estimate_units,
        }
    }
    pub fn validate(&self) -> Result<(), KernelError> {
        valid_name(&self.model.service)?;
        valid_name(&self.model.operation)?;
        if self.max_steps == 0 || self.max_tool_calls_per_step == 0 || self.max_context_items == 0 {
            return Err(KernelError::invalid("loop limits must be positive"));
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            valid_name(&tool.name)?;
            valid_name(&tool.route.service)?;
            valid_name(&tool.route.operation)?;
            if !names.insert(&tool.name) {
                return Err(KernelError::invalid("duplicate loop tool name"));
            }
        }
        bounded(&serde_json::to_value(self.model_tools()).map_err(serialization)?)?;
        Ok(())
    }
    fn model_tools(&self) -> Vec<ModelTool> {
        self.tools
            .iter()
            .map(|t| ModelTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect()
    }
}

/// Byte-based heuristic, NOT a provider tokenizer or a guaranteed upper bound.
/// Replacing this
/// function does not alter message content or tool schemas.
pub fn estimate_units(value: &Value) -> Result<u64, KernelError> {
    let bytes = serde_json::to_vec(value).map_err(serialization)?;
    Ok((bytes.len() as u64).div_ceil(4).max(1))
}
pub fn input_item(content: Value) -> Result<ContextItem, KernelError> {
    let data = serde_json::to_value(LoopMessage::User { content }).map_err(serialization)?;
    let units = estimate_units(&data)?;
    Ok(ContextItem {
        data,
        units,
        opens: vec![],
        closes: vec![],
        pinned: false,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoopOutcome {
    Completed { content: Value, steps: u32 },
    Cancelled { steps: u32 },
    Failed { error: KernelError, steps: u32 },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopEvent {
    StepStarted {
        step: u32,
    },
    ModelOutput {
        step: u32,
        reply: ModelReply,
    },
    ToolFinished {
        step: u32,
        call: ToolCall,
        result: BackendResult,
    },
    NeedsCompaction {
        revision: u64,
        used: u64,
        threshold: u64,
        required: u64,
    },
    NeedsIntervention {
        error: KernelError,
    },
    Ended {
        outcome: LoopOutcome,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopStatus {
    Ready,
    Waiting,
    NeedsCompaction,
    NeedsIntervention,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LoopRequestId(pub u64);
#[derive(Debug)]
pub struct LoopRequest {
    pub id: LoopRequestId,
    pub request: Request,
}

#[derive(Debug, Clone)]
struct View {
    revision: u64,
    used: u64,
    threshold: u64,
    max_input: u64,
    items: Vec<ContextItem>,
}
#[derive(Debug, Deserialize)]
struct Page {
    revision: u64,
    used: u64,
    threshold: u64,
    max_input: u64,
    pressure: bool,
    total: usize,
    next: usize,
    items: Vec<ContextItem>,
}
#[derive(Debug)]
struct Reader {
    revision: Option<u64>,
    used: u64,
    threshold: u64,
    max_input: u64,
    total: Option<usize>,
    bytes: usize,
    page_size: usize,
    items: Vec<ContextItem>,
}
impl Reader {
    fn new() -> Self {
        Self {
            revision: None,
            used: 0,
            threshold: 0,
            max_input: 0,
            total: None,
            bytes: 0,
            page_size: 64,
            items: vec![],
        }
    }
}
#[derive(Debug, Clone)]
enum AfterAppend {
    ModelReply(ModelReply),
    ToolResult,
}
#[derive(Debug, Clone)]
enum ReadPurpose {
    Initial,
    Model,
    Resume,
}
#[derive(Debug, Clone)]
enum Pending {
    Read(ReadPurpose),
    Model,
    Append(AfterAppend),
    Tool(ToolCall),
    Finish,
    Fail(KernelError),
    Cancel,
    Terminal,
}
#[derive(Debug)]
enum Stage {
    Reading(ReadPurpose),
    Model,
    Append {
        items: Vec<ContextItem>,
        after: AfterAppend,
    },
    Tools {
        calls: Vec<ToolCall>,
        next: usize,
    },
    Finish(Value),
    Fail(KernelError),
    Cancel,
    Terminal,
    Paused {
        items: Option<Vec<ContextItem>>,
        after: Option<AfterAppend>,
    },
    Intervention,
    Waiting,
    Ended,
}

/// One state machine per task. No session or fixed agent topology is required.
pub struct AgentLoop {
    task: TaskId,
    config: LoopConfig,
    stage: Stage,
    inflight: Option<(LoopRequestId, Pending)>,
    next_id: u64,
    reader: Reader,
    view: Option<View>,
    steps: u32,
    seen_calls: BTreeSet<String>,
    current_calls: Vec<ToolCall>,
    next_tool: usize,
    paused_append: Option<(Vec<ContextItem>, AfterAppend)>,
    events: Vec<LoopEvent>,
    outcome: Option<LoopOutcome>,
    final_content: Option<Value>,
    terminal_error: Option<KernelError>,
    /// Bounded sampled native usage; full per-step metadata is exposed in events.
    pub usage: Vec<Value>,
    pub usage_dropped: u32,
    usage_bytes: usize,
}
impl AgentLoop {
    pub fn new(task: TaskId, config: LoopConfig) -> Result<Self, KernelError> {
        config.validate()?;
        Ok(Self {
            task,
            config,
            stage: Stage::Reading(ReadPurpose::Initial),
            inflight: None,
            next_id: 1,
            reader: Reader::new(),
            view: None,
            steps: 0,
            seen_calls: BTreeSet::new(),
            current_calls: vec![],
            next_tool: 0,
            paused_append: None,
            events: vec![],
            outcome: None,
            final_content: None,
            terminal_error: None,
            usage: vec![],
            usage_dropped: 0,
            usage_bytes: 0,
        })
    }
    pub fn task(&self) -> TaskId {
        self.task
    }
    pub fn status(&self) -> LoopStatus {
        if self.outcome.is_some() {
            LoopStatus::Ended
        } else if matches!(self.stage, Stage::Paused { .. }) {
            LoopStatus::NeedsCompaction
        } else if matches!(self.stage, Stage::Intervention) {
            LoopStatus::NeedsIntervention
        } else if self.inflight.is_some() {
            LoopStatus::Waiting
        } else {
            LoopStatus::Ready
        }
    }
    pub fn outcome(&self) -> Option<&LoopOutcome> {
        self.outcome.as_ref()
    }
    pub fn pending_events(&self) -> usize {
        self.events.len()
    }
    pub fn take_events(&mut self) -> Vec<LoopEvent> {
        std::mem::take(&mut self.events)
    }

    /// Discards the logical wait and asks the kernel to cancel the task, which
    /// also cancels task-bound model/tool calls. Late replies are ignored.
    pub fn cancel(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        self.inflight = None;
        self.stage = Stage::Cancel;
    }
    /// Call only after an external context strategy has committed a replacement.
    /// Existing tool outputs are retained for append; effects are never retried.
    pub fn resume_after_compaction(&mut self) -> Result<(), KernelError> {
        if !matches!(self.stage, Stage::Paused { .. }) {
            return Err(KernelError::new(
                "invalid_state",
                "loop is not waiting for compaction",
            ));
        }
        let Stage::Paused { items, after } = std::mem::replace(&mut self.stage, Stage::Waiting)
        else {
            unreachable!()
        };
        self.paused_append = items.zip(after);
        self.reader = Reader::new();
        self.stage = Stage::Reading(ReadPurpose::Resume);
        Ok(())
    }
    pub fn on_notice(&mut self, notice: &Notice) {
        if let Notice::TaskEnded {
            task,
            state,
            result,
        } = notice
        {
            if *task != self.task || self.outcome.is_some() {
                return;
            }
            match state {
                TaskState::Cancelled => self.end(LoopOutcome::Cancelled { steps: self.steps }),
                TaskState::Failed => {
                    let error = result
                        .as_ref()
                        .and_then(|v| serde_json::from_value(v["error"].clone()).ok())
                        .or_else(|| self.terminal_error.clone())
                        .unwrap_or_else(|| {
                            KernelError::new("task_failed", "task failed externally")
                        });
                    self.end(LoopOutcome::Failed {
                        error,
                        steps: self.steps,
                    });
                }
                TaskState::Completed if self.final_content.is_some() => {
                    self.end(LoopOutcome::Completed {
                        content: self.final_content.clone().unwrap(),
                        steps: self.steps,
                    })
                }
                TaskState::Completed => self.end(LoopOutcome::Failed {
                    error: KernelError::new("task_ended", "task completed outside this loop"),
                    steps: self.steps,
                }),
                TaskState::Running => {}
            }
        }
    }
    /// Yield at most one public API call. The caller must correlate its wire
    /// request ID with LoopRequestId and feed its eventual response back.
    pub fn next_request(&mut self) -> Option<LoopRequest> {
        if self.inflight.is_some() || self.outcome.is_some() {
            return None;
        }
        loop {
            let stage = std::mem::replace(&mut self.stage, Stage::Waiting);
            let (request, pending) = match stage {
                Stage::Reading(purpose) => {
                    let start = self.reader.items.len();
                    (
                        Request::ContextRead {
                            task: self.task,
                            start,
                            limit: self.reader.page_size,
                        },
                        Pending::Read(purpose),
                    )
                }
                Stage::Model => {
                    if self.steps >= self.config.max_steps {
                        self.fail(KernelError::new("step_limit", "model step limit reached"));
                        continue;
                    }
                    self.steps += 1;
                    self.events
                        .push(LoopEvent::StepStarted { step: self.steps });
                    let input = ModelInput {
                        task: self.task,
                        step: self.steps,
                        context: self.view.as_ref().unwrap().items.clone(),
                        tools: self.config.model_tools(),
                    };
                    let payload =
                        match serde_json::to_value(input)
                            .map_err(serialization)
                            .and_then(|v| {
                                bounded(&v)?;
                                Ok(v)
                            }) {
                            Ok(v) => v,
                            Err(e) => {
                                self.fail(e);
                                continue;
                            }
                        };
                    (
                        Request::TaskSystemCall {
                            task: self.task,
                            service: self.config.model.service.clone(),
                            operation: self.config.model.operation.clone(),
                            payload,
                        },
                        Pending::Model,
                    )
                }
                Stage::Append { items, after } => {
                    let view = self.view.as_ref().unwrap();
                    let required = match total_units(&items) {
                        Ok(u) => u,
                        Err(e) => {
                            self.fail(e);
                            continue;
                        }
                    };
                    if view
                        .used
                        .checked_add(required)
                        .is_none_or(|used| used > view.max_input)
                    {
                        self.pause(Some(items), Some(after), required);
                        return None;
                    }
                    (
                        Request::ContextAppend {
                            task: self.task,
                            expected_revision: view.revision,
                            items,
                        },
                        Pending::Append(after),
                    )
                }
                Stage::Tools { calls, next } => {
                    if next >= calls.len() {
                        self.reader = Reader::new();
                        self.stage = Stage::Reading(ReadPurpose::Model);
                        continue;
                    }
                    self.current_calls = calls;
                    self.next_tool = next;
                    let call = self.current_calls[next].clone();
                    let tool = self
                        .config
                        .tools
                        .iter()
                        .find(|t| t.name == call.name)
                        .unwrap();
                    (
                        Request::TaskSystemCall {
                            task: self.task,
                            service: tool.route.service.clone(),
                            operation: tool.route.operation.clone(),
                            payload: call.arguments.clone(),
                        },
                        Pending::Tool(call),
                    )
                }
                Stage::Finish(content) => {
                    self.final_content = Some(content.clone());
                    let mut result = json!({"content":content,"steps":self.steps,"usage":self.usage,"usage_dropped":self.usage_dropped});
                    if bounded(&result).is_err() {
                        result["usage"] = json!([]);
                        result["usage_dropped"] =
                            json!(self.usage_dropped.saturating_add(self.usage.len() as u32));
                    }
                    if let Err(error) = bounded(&result) {
                        self.fail(error);
                        continue;
                    }
                    (
                        Request::TaskFinish {
                            task: self.task,
                            result,
                        },
                        Pending::Finish,
                    )
                }
                Stage::Fail(error) => (
                    Request::TaskFail {
                        task: self.task,
                        error: error.clone(),
                    },
                    Pending::Fail(error),
                ),
                Stage::Cancel => (Request::TaskCancel { task: self.task }, Pending::Cancel),
                Stage::Terminal => (Request::TaskGet { task: self.task }, Pending::Terminal),
                other => {
                    self.stage = other;
                    return None;
                }
            };
            let id = LoopRequestId(self.next_id);
            self.next_id += 1;
            self.inflight = Some((id, pending));
            return Some(LoopRequest { id, request });
        }
    }

    /// Returns false for unrelated or late responses, without changing state.
    pub fn accept_response(&mut self, id: LoopRequestId, response: ApiResult) -> bool {
        if self
            .inflight
            .as_ref()
            .is_none_or(|(expected, _)| *expected != id)
        {
            return false;
        }
        let (_, pending) = self.inflight.take().unwrap();
        let result = response.into_result();
        match pending {
            Pending::Cancel => match result {
                Ok(_) => {
                    self.stage = Stage::Terminal;
                }
                Err(e) => {
                    self.terminal_error = Some(KernelError::new(
                        "finalization_failed",
                        format!("cancellation was not committed: {e}"),
                    ));
                    self.stage = Stage::Terminal;
                }
            },
            Pending::Fail(error) => match result {
                Ok(_) => self.end(LoopOutcome::Failed {
                    error,
                    steps: self.steps,
                }),
                Err(e) => {
                    self.terminal_error = Some(KernelError::new(
                        "finalization_failed",
                        format!("{error}; task failure could not be committed: {e}"),
                    ));
                    self.stage = Stage::Terminal;
                }
            },
            Pending::Terminal => match result {
                Err(error) => self.intervene(KernelError::new(
                    "finalization_failed",
                    format!("cannot confirm terminal task state: {error}"),
                )),
                Ok(value) => {
                    if let Err(error) = self.apply_response(Pending::Terminal, value) {
                        self.intervene(error);
                    }
                }
            },
            Pending::Finish => match result {
                Ok(_) => {
                    let content = self.final_content.take().unwrap();
                    self.end(LoopOutcome::Completed {
                        content,
                        steps: self.steps,
                    });
                }
                Err(e) => self.fail(e),
            },
            pending => match result {
                Err(e) if e.code == "cancelled" || e.code == "task_ended" => {
                    self.stage = Stage::Terminal;
                }
                Err(e)
                    if e.code == "response_too_large"
                        && matches!(pending, Pending::Read(_))
                        && self.reader.page_size > 1 =>
                {
                    self.reader.page_size = (self.reader.page_size / 2).max(1);
                    if let Pending::Read(purpose) = pending {
                        self.stage = Stage::Reading(purpose);
                    }
                }
                Err(e) => self.fail(e),
                Ok(value) => {
                    if let Err(e) = self.apply_response(pending, value) {
                        self.fail(e);
                    }
                }
            },
        }
        true
    }

    fn apply_response(&mut self, pending: Pending, value: Value) -> Result<(), KernelError> {
        match pending {
            Pending::Terminal => {
                let task: Task = serde_json::from_value(value).map_err(serialization)?;
                if task.id != self.task || task.state == TaskState::Running {
                    return Err(self.terminal_error.clone().unwrap_or_else(|| {
                        KernelError::new(
                            "finalization_failed",
                            "task is still running; intervention is required",
                        )
                    }));
                }
                self.on_notice(&Notice::TaskEnded {
                    task: task.id,
                    state: task.state,
                    result: task.result,
                });
            }
            Pending::Read(purpose) => {
                let page: Page = serde_json::from_value(value)
                    .map_err(|e| KernelError::new("invalid_context_response", e.to_string()))?;
                let start = self.reader.items.len();
                if page.next != start + page.items.len()
                    || page.next > page.total
                    || page.total > self.config.max_context_items
                    || (page.next < page.total && page.items.is_empty())
                {
                    return Err(KernelError::new(
                        "invalid_context_response",
                        "inconsistent context page",
                    ));
                }
                if let Some(revision) = self.reader.revision {
                    if revision != page.revision || self.reader.total != Some(page.total) {
                        return Err(KernelError::new(
                            "revision_conflict",
                            "context changed while paginating",
                        ));
                    }
                } else {
                    self.reader.revision = Some(page.revision);
                    self.reader.used = page.used;
                    self.reader.threshold = page.threshold;
                    self.reader.max_input = page.max_input;
                    self.reader.total = Some(page.total);
                }
                let bytes = serde_json::to_vec(&page.items)
                    .map_err(serialization)?
                    .len();
                self.reader.bytes = self.reader.bytes.saturating_add(bytes);
                if self.reader.bytes > MAX_PAYLOAD {
                    return Err(KernelError::new(
                        "loop_payload_too_large",
                        "context exceeds inline model request budget; compact or split in user space",
                    ));
                }
                self.reader.items.extend(page.items);
                if page.next < page.total {
                    self.stage = Stage::Reading(purpose);
                    return Ok(());
                }
                let items = std::mem::take(&mut self.reader.items);
                if matches!(purpose, ReadPurpose::Initial) {
                    for item in &items {
                        self.seen_calls.extend(item.opens.iter().cloned());
                    }
                    ensure_balanced(&items)?;
                }
                self.view = Some(View {
                    revision: page.revision,
                    used: page.used,
                    threshold: page.threshold,
                    max_input: page.max_input,
                    items,
                });
                if matches!(purpose, ReadPurpose::Resume) {
                    if let Some((items, after)) = self.paused_append.take() {
                        self.stage = Stage::Append { items, after };
                        return Ok(());
                    }
                }
                if page.pressure {
                    self.pause(None, None, 0);
                } else {
                    self.stage = Stage::Model;
                }
            }
            Pending::Model => {
                let reply =
                    match serde_json::from_value::<BackendResult>(value).map_err(serialization)? {
                        BackendResult::Error(error) => {
                            return Err(KernelError::new("model_error", error.to_string()));
                        }
                        BackendResult::Ok(value) => serde_json::from_value::<ModelReply>(value)
                            .map_err(|e| KernelError::new("invalid_model_reply", e.to_string()))?,
                    };
                self.validate_reply(&reply)?;
                if let Some(usage) = &reply.usage {
                    let bytes = serde_json::to_vec(usage).map_err(serialization)?.len();
                    if self.usage_bytes.saturating_add(bytes) <= 16_384 {
                        self.usage_bytes += bytes;
                        self.usage.push(usage.clone());
                    } else {
                        self.usage_dropped = self.usage_dropped.saturating_add(1);
                    }
                }
                let data = serde_json::to_value(LoopMessage::Assistant {
                    content: reply.content.clone(),
                    tool_calls: reply.tool_calls.clone(),
                })
                .map_err(serialization)?;
                let item = self.item(
                    data,
                    reply.tool_calls.iter().map(|c| c.id.clone()).collect(),
                    vec![],
                )?;
                self.stage = Stage::Append {
                    items: vec![item],
                    after: AfterAppend::ModelReply(reply),
                };
            }
            Pending::Append(after) => {
                let revision = value["revision"].as_u64().ok_or_else(|| {
                    KernelError::new("invalid_context_response", "missing revision")
                })?;
                let used = value["used"]
                    .as_u64()
                    .ok_or_else(|| KernelError::new("invalid_context_response", "missing usage"))?;
                let view = self.view.as_mut().unwrap();
                view.revision = revision;
                view.used = used;
                match after {
                    AfterAppend::ModelReply(reply) => {
                        self.events.push(LoopEvent::ModelOutput {
                            step: self.steps,
                            reply: reply.clone(),
                        });
                        if reply.tool_calls.is_empty() {
                            self.stage = Stage::Finish(reply.content);
                        } else {
                            self.seen_calls
                                .extend(reply.tool_calls.iter().map(|c| c.id.clone()));
                            self.stage = Stage::Tools {
                                calls: reply.tool_calls,
                                next: 0,
                            };
                        }
                    }
                    AfterAppend::ToolResult => {
                        self.next_tool += 1;
                        self.stage = Stage::Tools {
                            calls: self.current_calls.clone(),
                            next: self.next_tool,
                        };
                    }
                }
            }
            Pending::Tool(call) => {
                let result: BackendResult = serde_json::from_value(value).map_err(serialization)?;
                self.events.push(LoopEvent::ToolFinished {
                    step: self.steps,
                    call: call.clone(),
                    result: result.clone(),
                });
                let data = serde_json::to_value(LoopMessage::Tool {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    result,
                })
                .map_err(serialization)?;
                let item = self.item(data, vec![], vec![call.id])?;
                self.stage = Stage::Append {
                    items: vec![item],
                    after: AfterAppend::ToolResult,
                };
            }
            _ => {
                return Err(KernelError::new(
                    "invalid_state",
                    "unexpected response phase",
                ));
            }
        }
        Ok(())
    }
    fn validate_reply(&self, reply: &ModelReply) -> Result<(), KernelError> {
        bounded(&serde_json::to_value(reply).map_err(serialization)?)?;
        if reply.tool_calls.len() > self.config.max_tool_calls_per_step {
            return Err(KernelError::new(
                "tool_call_limit",
                "too many tool calls in one reply",
            ));
        }
        let mut ids = BTreeSet::new();
        for call in &reply.tool_calls {
            valid_name(&call.id)?;
            if !ids.insert(&call.id) || self.seen_calls.contains(&call.id) {
                return Err(KernelError::new(
                    "invalid_tool_call",
                    "duplicate tool call id",
                ));
            }
            if !self.config.tools.iter().any(|t| t.name == call.name) {
                return Err(KernelError::new(
                    "unknown_tool",
                    format!("tool is not registered: {}", call.name),
                ));
            }
            bounded(&call.arguments)?;
        }
        Ok(())
    }
    fn item(
        &self,
        data: Value,
        opens: Vec<String>,
        closes: Vec<String>,
    ) -> Result<ContextItem, KernelError> {
        bounded(&data)?;
        Ok(ContextItem {
            units: (self.config.meter)(&data)?,
            data,
            opens,
            closes,
            pinned: false,
        })
    }
    fn pause(
        &mut self,
        items: Option<Vec<ContextItem>>,
        after: Option<AfterAppend>,
        required: u64,
    ) {
        let view = self.view.as_ref().unwrap();
        self.events.push(LoopEvent::NeedsCompaction {
            revision: view.revision,
            used: view.used,
            threshold: view.threshold,
            required,
        });
        self.stage = Stage::Paused { items, after };
    }
    fn fail(&mut self, mut error: KernelError) {
        // TaskFail must itself fit the protocol even when a provider sends an
        // oversized diagnostic. Native tool result payloads remain untouched.
        if error.message.len() > 8192 {
            let mut end = 8192;
            while !error.message.is_char_boundary(end) {
                end -= 1;
            }
            error.message.truncate(end);
            error.message.push_str(" [diagnostic truncated]");
        }
        self.terminal_error = Some(error.clone());
        self.stage = Stage::Fail(error);
    }
    fn intervene(&mut self, error: KernelError) {
        self.inflight = None;
        self.stage = Stage::Intervention;
        self.events.push(LoopEvent::NeedsIntervention { error });
    }
    fn end(&mut self, outcome: LoopOutcome) {
        self.inflight = None;
        self.stage = Stage::Ended;
        self.paused_append = None;
        self.events.push(LoopEvent::Ended {
            outcome: outcome.clone(),
        });
        self.outcome = Some(outcome);
    }
}
fn bounded(value: &Value) -> Result<(), KernelError> {
    if serde_json::to_vec(value).map_err(serialization)?.len() > MAX_PAYLOAD {
        Err(KernelError::new(
            "loop_payload_too_large",
            "loop payload exceeds transport budget; compact or split in user space",
        ))
    } else {
        Ok(())
    }
}
fn total_units(items: &[ContextItem]) -> Result<u64, KernelError> {
    items.iter().try_fold(0_u64, |total, item| {
        total
            .checked_add(item.units)
            .ok_or_else(|| KernelError::invalid("context unit sum overflow"))
    })
}
fn valid_name(name: &str) -> Result<(), KernelError> {
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        Err(KernelError::invalid("invalid loop identifier"))
    } else {
        Ok(())
    }
}
fn serialization(error: serde_json::Error) -> KernelError {
    KernelError::new("invalid_backend_payload", error.to_string())
}
fn ensure_balanced(items: &[ContextItem]) -> Result<(), KernelError> {
    let mut pending = BTreeSet::new();
    for item in items {
        for id in &item.opens {
            pending.insert(id);
        }
        for id in &item.closes {
            pending.remove(id);
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(KernelError::new(
            "incomplete_context",
            "task contains unanswered tool calls; explicit recovery is required",
        ))
    }
}
