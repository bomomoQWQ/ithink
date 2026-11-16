use crate::api::*;
use crate::context::Context;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

const MAX_PENDING: usize = 4096;
const MAX_SUBJECTS: usize = 16_384;
const MAX_TASKS: usize = 16_384;
const MAX_NAME: usize = 256;

#[derive(Debug)]
struct Backend {
    provider: Aid,
}
#[derive(Debug)]
struct PendingCall {
    caller: Aid,
    request_id: u64,
    provider: Aid,
    task: Option<TaskId>,
    deadline: Instant,
}

#[derive(Debug)]
pub(crate) enum Delivery {
    Response { to: Aid, id: u64, result: ApiResult },
    Notice { to: Aid, notice: Notice },
}

/// The state machine. No model, shell, transport or conversation logic.
/// Only Runtime may dispatch requests using a connection-bound identity.
#[derive(Debug)]
pub struct Kernel {
    subjects: BTreeMap<Aid, Subject>,
    tasks: BTreeMap<TaskId, Task>,
    children: BTreeMap<TaskId, Vec<TaskId>>,
    contexts: BTreeMap<TaskId, Context>,
    topics: BTreeMap<String, BTreeSet<Aid>>,
    topic_sequences: BTreeMap<String, u64>,
    policies: BTreeMap<(String, String), String>,
    backends: BTreeMap<String, Backend>,
    pending: BTreeMap<u64, PendingCall>,
    journal: Vec<JournalRecord>,
    deliveries: VecDeque<Delivery>,
    next_aid: u64,
    next_task: u64,
    next_event: u64,
    next_call: u64,
    pub(crate) call_timeout: Duration,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}
impl Kernel {
    pub fn new() -> Self {
        let root = Subject {
            id: HOST,
            parent: None,
            level: Level::User,
            capabilities: BTreeSet::new(),
            active: true,
        };
        Self {
            subjects: BTreeMap::from([(HOST, root)]),
            tasks: BTreeMap::new(),
            children: BTreeMap::new(),
            contexts: BTreeMap::new(),
            topics: BTreeMap::new(),
            topic_sequences: BTreeMap::new(),
            policies: BTreeMap::new(),
            backends: BTreeMap::new(),
            pending: BTreeMap::new(),
            journal: vec![],
            deliveries: VecDeque::new(),
            next_aid: 1,
            next_task: 1,
            next_event: 1,
            next_call: 1,
            call_timeout: Duration::from_secs(30),
        }
    }

    pub fn journal(&self) -> &[JournalRecord] {
        &self.journal
    }
    pub fn subject(&self, aid: Aid) -> Option<&Subject> {
        self.subjects.get(&aid)
    }
    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.get(&id)
    }
    pub fn context(&self, id: TaskId) -> Option<&Context> {
        self.contexts.get(&id)
    }

    /// Host-owned policy. A provider cannot lower the required capability.
    pub fn define_service(&mut self, service: &str, required: &str) -> Result<(), KernelError> {
        self.define_operation(service, "*", required)
    }
    /// A wildcard deliberately grants every operation in this service.
    /// Prefer exact operation policies when read/write privileges differ.
    pub fn define_operation(
        &mut self,
        service: &str,
        operation: &str,
        required: &str,
    ) -> Result<(), KernelError> {
        validate_name(service)?;
        validate_name(operation)?;
        validate_name(required)?;
        let key = (service.to_string(), operation.to_string());
        if self.policies.contains_key(&key) {
            return Err(KernelError::new(
                "already_exists",
                "operation policy is immutable while registered",
            ));
        }
        self.policies.insert(key, required.into());
        self.record(
            HOST,
            "service.defined",
            json!({"service":service,"operation":operation,"required":required}),
        );
        Ok(())
    }
    pub fn service_ready(&self, service: &str) -> bool {
        self.backends.contains_key(service)
    }

    pub(crate) fn record(&mut self, actor: Aid, kind: &str, data: Value) {
        self.journal.push(JournalRecord {
            sequence: self.journal.len() as u64 + 1,
            actor,
            kind: kind.into(),
            data,
        });
    }
    fn active(&self, aid: Aid) -> Result<&Subject, KernelError> {
        self.subjects
            .get(&aid)
            .filter(|s| s.active)
            .ok_or_else(KernelError::denied)
    }
    pub(crate) fn require(&self, aid: Aid, cap: &str) -> Result<(), KernelError> {
        let s = self.active(aid)?;
        if aid == HOST || s.capabilities.contains(cap) {
            Ok(())
        } else {
            Err(KernelError::denied())
        }
    }
    fn owns(&self, actor: Aid, owner: Aid) -> bool {
        actor == HOST || actor == owner
    }
    pub(crate) fn manages(&self, actor: Aid, mut owner: Aid) -> bool {
        if actor == HOST {
            return true;
        }
        for _ in 0..=4 {
            if actor == owner {
                return true;
            }
            match self.subjects.get(&owner).and_then(|s| s.parent) {
                Some(parent) => owner = parent,
                None => return false,
            }
        }
        false
    }
    fn lifecycle_access(&self, actor: Aid, id: TaskId) -> Result<(), KernelError> {
        self.require(actor, CAP_TASK)?;
        let task = self
            .tasks
            .get(&id)
            .ok_or_else(|| KernelError::missing("task"))?;
        if !self.manages(actor, task.owner) {
            return Err(KernelError::denied());
        }
        Ok(())
    }
    fn task_access(&self, actor: Aid, id: TaskId, cap: &str) -> Result<(), KernelError> {
        self.require(actor, cap)?;
        let task = self
            .tasks
            .get(&id)
            .ok_or_else(|| KernelError::missing("task"))?;
        if !self.owns(actor, task.owner) {
            return Err(KernelError::denied());
        }
        Ok(())
    }
    fn running(&self, id: TaskId) -> Result<(), KernelError> {
        match self.tasks.get(&id) {
            Some(t) if t.state == TaskState::Running => Ok(()),
            Some(_) => Err(KernelError::new("task_ended", "task is no longer running")),
            None => Err(KernelError::missing("task")),
        }
    }

    pub(crate) fn create_subject(
        &mut self,
        actor: Aid,
        parent: Aid,
        level: Level,
        caps: Capabilities,
    ) -> Result<Aid, KernelError> {
        self.require(actor, CAP_AGENT)?;
        let p = self.active(parent)?;
        if level == Level::User
            || level as u8 <= p.level as u8
            || (actor != HOST && actor != parent)
        {
            return Err(KernelError::denied());
        }
        if actor != HOST && !caps.is_subset(&p.capabilities) {
            return Err(KernelError::denied());
        }
        if self.subjects.len() >= MAX_SUBJECTS {
            return Err(KernelError::new("limit_exceeded", "subject limit"));
        }
        if caps.iter().any(|c| c.is_empty() || c.len() > MAX_NAME) {
            return Err(KernelError::invalid("invalid capability"));
        }
        let id = Aid(self.next_aid);
        self.next_aid += 1;
        let subject = Subject {
            id,
            parent: Some(parent),
            level,
            capabilities: caps,
            active: true,
        };
        self.record(actor, "agent.created", json!(&subject));
        self.subjects.insert(id, subject);
        Ok(id)
    }

    pub(crate) fn validate_provider(&self, aid: Aid, service: &str) -> Result<(), KernelError> {
        self.require(aid, CAP_BACKEND)?;
        self.require(aid, &format!("provide:{service}"))?;
        if !self.policies.keys().any(|(s, _)| s == service) {
            return Err(KernelError::missing("service policy"));
        }
        Ok(())
    }
    pub(crate) fn register_backend(&mut self, aid: Aid, service: &str) -> Result<(), KernelError> {
        self.validate_provider(aid, service)?;
        if let Some(b) = self.backends.get(service) {
            if b.provider != aid {
                return Err(KernelError::new(
                    "already_exists",
                    "service has another provider",
                ));
            }
        }
        self.backends
            .insert(service.into(), Backend { provider: aid });
        self.record(aid, "backend.registered", json!({"service":service}));
        Ok(())
    }
    pub(crate) fn can_register_services(
        &self,
        aid: Aid,
        old: Option<Aid>,
        services: &[String],
    ) -> Result<(), KernelError> {
        let mut unique = BTreeSet::new();
        for service in services {
            validate_name(service)?;
            if !unique.insert(service) {
                return Err(KernelError::invalid("duplicate provided service"));
            }
            self.validate_provider(aid, service)?;
            if let Some(b) = self.backends.get(service) {
                if Some(b.provider) != old {
                    return Err(KernelError::new(
                        "already_exists",
                        "service has another provider",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn dispatch(
        &mut self,
        actor: Aid,
        request_id: u64,
        request: Request,
    ) -> Option<ApiResult> {
        if let Err(e) = self.active(actor) {
            return Some(ApiResult::Error(e));
        }
        if let Request::SystemCall {
            service,
            operation,
            payload,
        } = request
        {
            return match self.start_call(actor, request_id, &service, operation, payload) {
                Ok(()) => None,
                Err(e) => Some(ApiResult::Error(e)),
            };
        }
        if let Request::TaskSystemCall {
            task,
            service,
            operation,
            payload,
        } = request
        {
            let result = self
                .task_access(actor, task, CAP_TASK)
                .and_then(|()| self.running(task))
                .and_then(|()| {
                    self.start_call_for_task(
                        actor,
                        request_id,
                        Some(task),
                        &service,
                        operation,
                        payload,
                    )
                });
            return match result {
                Ok(()) => None,
                Err(e) => Some(ApiResult::Error(e)),
            };
        }
        Some(self.apply(actor, request).into())
    }
    fn apply(&mut self, actor: Aid, request: Request) -> Result<Value, KernelError> {
        match request {
            Request::AgentCreate {
                parent,
                level,
                capabilities,
            } => Ok(json!({"aid":self.create_subject(actor,parent,level,capabilities)?})),
            Request::AgentGet { aid } => {
                if actor != HOST && actor != aid {
                    self.require(actor, CAP_AGENT)?;
                }
                Ok(json!(
                    self.subjects
                        .get(&aid)
                        .ok_or_else(|| KernelError::missing("agent"))?
                ))
            }
            Request::TaskCreate {
                owner,
                parent,
                limits,
                seed,
            } => {
                self.require(actor, CAP_TASK)?;
                if !self.manages(actor, owner) {
                    return Err(KernelError::denied());
                }
                self.active(owner)?;
                if let Some(p) = parent {
                    self.running(p)?;
                    let powner = self.tasks[&p].owner;
                    let creator_is_parent = self.owns(actor, powner);
                    if !creator_is_parent {
                        return Err(KernelError::denied());
                    }
                    let mut cursor = Some(p);
                    let mut depth = 0;
                    while let Some(id) = cursor {
                        depth += 1;
                        if depth >= 256 {
                            return Err(KernelError::new(
                                "limit_exceeded",
                                "task tree depth limit",
                            ));
                        }
                        cursor = self.tasks[&id].parent;
                    }
                }
                if self.tasks.len() >= MAX_TASKS {
                    return Err(KernelError::new("limit_exceeded", "task limit"));
                }
                let context = Context::new(limits, seed)?;
                let id = TaskId(self.next_task);
                self.next_task += 1;
                let task = Task {
                    id,
                    owner,
                    parent,
                    state: TaskState::Running,
                    result: None,
                };
                self.record(actor, "task.created", json!(&task));
                self.contexts.insert(id, context);
                self.tasks.insert(id, task);
                if let Some(parent) = parent {
                    self.children.entry(parent).or_default().push(id);
                }
                if owner != actor {
                    self.deliveries.push_back(Delivery::Notice {
                        to: owner,
                        notice: Notice::TaskAssigned { task: id },
                    });
                }
                self.pressure(id);
                Ok(json!({"task":id}))
            }
            Request::TaskGet { task } => {
                self.lifecycle_access(actor, task)?;
                let mut data = json!(&self.tasks[&task]);
                if !self.owns(actor, self.tasks[&task].owner) {
                    data["result"] = Value::Null;
                }
                Ok(data)
            }
            Request::TaskFinish { task, result } => {
                self.task_access(actor, task, CAP_TASK)?;
                self.running(task)?;
                bounded_value(result.clone())?;
                self.end_task(actor, task, TaskState::Completed, Some(result));
                Ok(json!({"task":task}))
            }
            Request::TaskCancel { task } => {
                self.lifecycle_access(actor, task)?;
                self.end_task(actor, task, TaskState::Cancelled, None);
                Ok(json!({"task":task}))
            }
            Request::TaskFail { task, error } => {
                self.task_access(actor, task, CAP_TASK)?;
                self.running(task)?;
                bounded_value(json!(&error))?;
                self.end_task(actor, task, TaskState::Failed, Some(json!({"error":error})));
                Ok(json!({"task":task}))
            }
            Request::ContextGet { task } => {
                self.task_access(actor, task, CAP_CONTEXT)?;
                bounded_value(json!(self.contexts[&task].snapshot()))
            }
            Request::ContextRead { task, start, limit } => {
                self.task_access(actor, task, CAP_CONTEXT)?;
                if limit == 0 || limit > 256 {
                    return Err(KernelError::invalid("page size must be 1..256"));
                }
                let snapshot = self.contexts[&task].snapshot();
                if start > snapshot.items.len() {
                    return Err(KernelError::invalid("context page start out of bounds"));
                }
                let end = start.saturating_add(limit).min(snapshot.items.len());
                bounded_value(
                    json!({"revision":snapshot.revision,"used":snapshot.used,"threshold":snapshot.threshold,"max_input":snapshot.max_input,"pressure":snapshot.pressure,"total":snapshot.items.len(),"next":end,"items":&snapshot.items[start..end]}),
                )
            }
            Request::ContextAppend {
                task,
                expected_revision,
                items,
            } => {
                self.task_access(actor, task, CAP_CONTEXT)?;
                self.running(task)?;
                let snapshot = self
                    .contexts
                    .get_mut(&task)
                    .unwrap()
                    .append(expected_revision, items)?;
                self.record(
                    actor,
                    "context.appended",
                    json!({"task":task,"revision":snapshot.revision}),
                );
                self.pressure(task);
                Ok(
                    json!({"revision":snapshot.revision,"used":snapshot.used,"threshold":snapshot.threshold,"max_input":snapshot.max_input,"pressure":snapshot.pressure}),
                )
            }
            Request::ContextReplace {
                task,
                expected_revision,
                start,
                end,
                replacement,
            } => {
                self.task_access(actor, task, CAP_CONTEXT)?;
                self.running(task)?;
                let snapshot = self.contexts.get_mut(&task).unwrap().replace(
                    expected_revision,
                    start,
                    end,
                    replacement,
                )?;
                self.record(
                    actor,
                    "context.replaced",
                    json!({"task":task,"revision":snapshot.revision,"start":start,"end":end}),
                );
                self.deliveries.push_back(Delivery::Notice {
                    to: self.tasks[&task].owner,
                    notice: Notice::ContextReplaced {
                        task,
                        revision: snapshot.revision,
                    },
                });
                self.pressure(task);
                Ok(
                    json!({"revision":snapshot.revision,"used":snapshot.used,"threshold":snapshot.threshold,"max_input":snapshot.max_input,"pressure":snapshot.pressure}),
                )
            }
            Request::EventSubscribe { topic } => {
                self.require(actor, CAP_EVENT)?;
                validate_name(&topic)?;
                if !self.topics.contains_key(&topic) && self.topics.len() >= 4096 {
                    return Err(KernelError::new("limit_exceeded", "topic limit"));
                }
                self.topics.entry(topic).or_default().insert(actor);
                Ok(json!(true))
            }
            Request::EventUnsubscribe { topic } => {
                self.require(actor, CAP_EVENT)?;
                if let Some(subs) = self.topics.get_mut(&topic) {
                    subs.remove(&actor);
                }
                Ok(json!(true))
            }
            Request::EventPublish { topic, data } => {
                self.require(actor, CAP_EVENT)?;
                validate_name(&topic)?;
                bounded_value(data.clone())?;
                if self.topic_sequences.len() >= 16_384
                    && !self.topic_sequences.contains_key(&topic)
                {
                    return Err(KernelError::new("limit_exceeded", "event stream limit"));
                }
                let sequence = self.topic_sequences.entry(topic.clone()).or_default();
                *sequence += 1;
                let event = Event {
                    id: EventId(self.next_event),
                    source: actor,
                    topic: topic.clone(),
                    sequence: *sequence,
                    data,
                };
                self.next_event += 1;
                self.record(actor, "event.published", json!(&event));
                if let Some(subs) = self.topics.get(&topic) {
                    for &to in subs {
                        self.deliveries.push_back(Delivery::Notice {
                            to,
                            notice: Notice::Event {
                                event: event.clone(),
                            },
                        });
                    }
                }
                Ok(json!(&event))
            }
            Request::BackendRegister { service } => {
                self.register_backend(actor, &service)?;
                Ok(json!(true))
            }
            Request::BackendComplete { call, result } => {
                bounded_value(json!(&result))?;
                let pending = self
                    .pending
                    .get(&call)
                    .ok_or_else(|| KernelError::missing("pending call"))?;
                if actor != pending.provider {
                    return Err(KernelError::denied());
                }
                let pending = self.pending.remove(&call).unwrap();
                self.record(
                    actor,
                    "system.completed",
                    json!({"call":call,"result":&result}),
                );
                self.deliveries.push_back(Delivery::Response {
                    to: pending.caller,
                    id: pending.request_id,
                    result: ApiResult::Ok(json!(result)),
                });
                Ok(json!(true))
            }
            Request::SystemCall { .. } | Request::TaskSystemCall { .. } => {
                unreachable!("system calls are deferred")
            }
            Request::PluginStart { .. } | Request::PluginStop { .. } => Err(KernelError::new(
                "runtime_required",
                "plugin lifecycle requires Runtime",
            )),
        }
    }

    fn pressure(&mut self, task: TaskId) {
        let snapshot = self.contexts[&task].snapshot();
        if snapshot.pressure {
            self.deliveries.push_back(Delivery::Notice {
                to: self.tasks[&task].owner,
                notice: Notice::ContextPressure {
                    task,
                    used: snapshot.used,
                    threshold: snapshot.threshold,
                    max_input: snapshot.max_input,
                },
            });
        }
    }
    fn end_task(&mut self, actor: Aid, root: TaskId, state: TaskState, result: Option<Value>) {
        let mut stack = vec![(root, false)];
        while let Some((id, visited)) = stack.pop() {
            if self
                .tasks
                .get(&id)
                .is_none_or(|t| t.state != TaskState::Running)
            {
                continue;
            }
            if !visited {
                stack.push((id, true));
                if let Some(children) = self.children.get(&id) {
                    for &child in children.iter().rev() {
                        stack.push((child, false));
                    }
                }
                continue;
            }
            let ending = if id == root {
                state.clone()
            } else {
                TaskState::Cancelled
            };
            let value = if id == root { result.clone() } else { None };
            let task = self.tasks.get_mut(&id).unwrap();
            task.state = ending.clone();
            task.result = value.clone();
            let owner = task.owner;
            let parent = task.parent;
            if let Err(error) = self.contexts.get_mut(&id).unwrap().release_view() {
                self.record(
                    HOST,
                    "context.release_failed",
                    json!({"task":id,"error":error}),
                );
            }
            let calls: Vec<_> = self
                .pending
                .iter()
                .filter(|(_, p)| p.task == Some(id))
                .map(|(&id, _)| id)
                .collect();
            for call in calls {
                self.fail_call(
                    call,
                    "cancelled",
                    "task ended; external effects are not rolled back",
                );
            }
            self.record(
                actor,
                "task.ended",
                json!({"task":id,"state":ending,"result":value}),
            );
            let notice = Notice::TaskEnded {
                task: id,
                state: ending,
                result: value,
            };
            self.deliveries.push_back(Delivery::Notice {
                to: owner,
                notice: notice.clone(),
            });
            if let Some(to) = parent
                .and_then(|p| self.tasks.get(&p).map(|t| t.owner))
                .or_else(|| self.subjects.get(&owner).and_then(|s| s.parent))
                .filter(|to| *to != owner)
            {
                self.deliveries.push_back(Delivery::Notice { to, notice });
            }
        }
    }

    fn start_call(
        &mut self,
        actor: Aid,
        id: u64,
        service: &str,
        operation: String,
        payload: Value,
    ) -> Result<(), KernelError> {
        self.start_call_for_task(actor, id, None, service, operation, payload)
    }
    fn start_call_for_task(
        &mut self,
        actor: Aid,
        id: u64,
        task: Option<TaskId>,
        service: &str,
        operation: String,
        payload: Value,
    ) -> Result<(), KernelError> {
        validate_name(&operation)?;
        bounded_value(payload.clone())?;
        let required = self
            .policies
            .get(&(service.to_string(), operation.clone()))
            .or_else(|| self.policies.get(&(service.to_string(), "*".into())))
            .ok_or_else(|| KernelError::missing("operation policy"))?
            .clone();
        self.require(actor, &required)?;
        let provider = self
            .backends
            .get(service)
            .ok_or_else(|| KernelError::missing("service backend"))?
            .provider;
        if self.pending.len() >= MAX_PENDING {
            return Err(KernelError::new("limit_exceeded", "pending call limit"));
        }
        let call = self.next_call;
        self.next_call += 1;
        self.pending.insert(
            call,
            PendingCall {
                caller: actor,
                request_id: id,
                provider,
                task,
                deadline: Instant::now() + self.call_timeout,
            },
        );
        self.record(
            actor,
            "system.started",
            json!({"call":call,"service":service,"operation":operation,"payload":payload}),
        );
        self.deliveries.push_back(Delivery::Notice {
            to: provider,
            notice: Notice::SystemCall {
                call,
                caller: actor,
                task,
                service: service.into(),
                operation,
                payload,
            },
        });
        Ok(())
    }
    pub(crate) fn cancel_request(&mut self, caller: Aid, request_id: u64) {
        let calls: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| p.caller == caller && p.request_id == request_id)
            .map(|(&id, _)| id)
            .collect();
        for call in calls {
            if let Some(p) = self.pending.remove(&call) {
                self.deliveries.push_back(Delivery::Notice {
                    to: p.provider,
                    notice: Notice::SystemCallCancelled { call },
                });
            }
            self.record(caller, "system.abandoned", json!({"call":call}));
        }
    }
    pub(crate) fn expire_calls(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            self.fail_call(
                id,
                "timeout",
                "backend call timed out; external effects may already have happened",
            );
        }
    }
    fn fail_call(&mut self, id: u64, code: &str, message: &str) {
        if let Some(p) = self.pending.remove(&id) {
            self.deliveries.push_back(Delivery::Notice {
                to: p.provider,
                notice: Notice::SystemCallCancelled { call: id },
            });
            self.record(HOST, "system.failed", json!({"call":id,"code":code}));
            self.deliveries.push_back(Delivery::Response {
                to: p.caller,
                id: p.request_id,
                result: ApiResult::Error(KernelError::new(code, message)),
            });
        }
    }
    pub(crate) fn retire(&mut self, aid: Aid, reason: &str) {
        if aid == HOST {
            return;
        }
        let descendants: Vec<_> = self
            .subjects
            .values()
            .filter(|s| s.active && s.parent == Some(aid))
            .map(|s| s.id)
            .collect();
        for child in descendants {
            self.retire(child, reason);
        }
        if let Some(s) = self.subjects.get_mut(&aid) {
            s.active = false;
        }
        let tasks: Vec<_> = self
            .tasks
            .values()
            .filter(|t| t.owner == aid && t.state == TaskState::Running)
            .map(|t| t.id)
            .collect();
        for task in tasks {
            self.end_task(HOST, task, TaskState::Failed, None);
        }
        for subs in self.topics.values_mut() {
            subs.remove(&aid);
        }
        self.backends.retain(|_, b| b.provider != aid);
        let calls: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| p.provider == aid || p.caller == aid)
            .map(|(&id, _)| id)
            .collect();
        for id in calls {
            self.fail_call(id, "plugin_unavailable", reason);
        }
        self.record(HOST, "agent.retired", json!({"aid":aid,"reason":reason}));
    }
    pub(crate) fn drain(&mut self) -> Vec<Delivery> {
        self.deliveries.drain(..).collect()
    }
}

fn bounded_value(value: Value) -> Result<Value, KernelError> {
    if serde_json::to_vec(&value)
        .map_err(|e| KernelError::invalid(e.to_string()))?
        .len()
        > crate::protocol::MAX_FRAME_BYTES / 2
    {
        return Err(KernelError::new(
            "response_too_large",
            "payload exceeds safe frame size; use smaller context pages",
        ));
    }
    Ok(value)
}
fn validate_name(name: &str) -> Result<(), KernelError> {
    if name.is_empty() || name.len() > MAX_NAME || name.chars().any(char::is_control) {
        Err(KernelError::invalid("invalid name"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn invoke(k: &mut Kernel, aid: Aid, r: Request) -> Result<Value, KernelError> {
        k.dispatch(aid, 1, r).unwrap().into_result()
    }
    fn caps(names: &[&str]) -> Capabilities {
        names.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn no_identity_or_capability_escalation() {
        let mut k = Kernel::new();
        let parent = k
            .create_subject(HOST, HOST, Level::Worker, caps(&[CAP_AGENT]))
            .unwrap();
        assert!(
            k.create_subject(parent, parent, Level::Main, caps(&[]))
                .is_err()
        );
        assert!(
            k.create_subject(parent, parent, Level::Ephemeral, caps(&[CAP_TASK]))
                .is_err()
        );
        assert!(
            k.create_subject(parent, HOST, Level::Ephemeral, caps(&[]))
                .is_err()
        );
        assert!(
            k.create_subject(HOST, HOST, Level::User, caps(&[]))
                .is_err()
        );
        assert!(
            k.create_subject(parent, parent, Level::Ephemeral, caps(&[]))
                .is_ok()
        );
    }
    #[test]
    fn event_order_and_direct_subscription() {
        let mut k = Kernel::new();
        let aid = k
            .create_subject(HOST, HOST, Level::Worker, caps(&[CAP_EVENT]))
            .unwrap();
        invoke(
            &mut k,
            aid,
            Request::EventSubscribe {
                topic: "topic".into(),
            },
        )
        .unwrap();
        for seq in 1..=2 {
            let v = invoke(
                &mut k,
                HOST,
                Request::EventPublish {
                    topic: "topic".into(),
                    data: json!(seq),
                },
            )
            .unwrap();
            assert_eq!(v["sequence"], seq);
        }
        assert_eq!(k.drain().len(), 2);
        k.retire(aid, "test");
        invoke(
            &mut k,
            HOST,
            Request::EventPublish {
                topic: "topic".into(),
                data: json!(3),
            },
        )
        .unwrap();
        assert!(k.drain().is_empty());
    }
    #[test]
    fn task_ownership_and_cancellation_tree() {
        let mut k = Kernel::new();
        let a = k
            .create_subject(HOST, HOST, Level::Worker, caps(&[CAP_TASK, CAP_CONTEXT]))
            .unwrap();
        let b = k
            .create_subject(HOST, HOST, Level::Worker, caps(&[CAP_TASK, CAP_CONTEXT]))
            .unwrap();
        let root: TaskId = serde_json::from_value(
            invoke(
                &mut k,
                a,
                Request::TaskCreate {
                    owner: a,
                    parent: None,
                    limits: ContextLimits::default(),
                    seed: vec![ContextItem::text("private", 3)],
                },
            )
            .unwrap()["task"]
                .clone(),
        )
        .unwrap();
        let child: TaskId = serde_json::from_value(
            invoke(
                &mut k,
                a,
                Request::TaskCreate {
                    owner: a,
                    parent: Some(root),
                    limits: ContextLimits::default(),
                    seed: vec![],
                },
            )
            .unwrap()["task"]
                .clone(),
        )
        .unwrap();
        assert!(invoke(&mut k, b, Request::ContextGet { task: root }).is_err());
        invoke(&mut k, a, Request::TaskCancel { task: root }).unwrap();
        assert_eq!(k.task(child).unwrap().state, TaskState::Cancelled);
        assert!(k.context(root).unwrap().snapshot().items.is_empty());
        assert!(!k.context(root).unwrap().history().is_empty());
    }
    #[test]
    fn parent_seeds_child_but_cannot_read_private_context() {
        let mut k = Kernel::new();
        let parent = k
            .create_subject(HOST, HOST, Level::Main, caps(&[CAP_TASK, CAP_CONTEXT]))
            .unwrap();
        let child = k
            .create_subject(HOST, parent, Level::Worker, caps(&[CAP_TASK, CAP_CONTEXT]))
            .unwrap();
        let id: TaskId = serde_json::from_value(
            invoke(
                &mut k,
                parent,
                Request::TaskCreate {
                    owner: child,
                    parent: None,
                    limits: ContextLimits::default(),
                    seed: vec![ContextItem::text("seed", 1)],
                },
            )
            .unwrap()["task"]
                .clone(),
        )
        .unwrap();
        assert!(invoke(&mut k, parent, Request::TaskGet { task: id }).is_ok());
        assert!(invoke(&mut k, parent, Request::ContextGet { task: id }).is_err());
        assert!(invoke(&mut k, child, Request::ContextGet { task: id }).is_ok());
        invoke(&mut k, parent, Request::TaskCancel { task: id }).unwrap();
        assert_eq!(k.task(id).unwrap().state, TaskState::Cancelled);
        assert!(k.drain().iter().any(|d|matches!(d,Delivery::Notice{to,notice:Notice::TaskEnded{task,..}} if *to==child&&*task==id)));
    }
    #[test]
    fn cancellation_fails_task_calls_and_notifies_provider() {
        let mut k = Kernel::new();
        k.define_operation("files", "read", "files.read").unwrap();
        let provider = k
            .create_subject(
                HOST,
                HOST,
                Level::Worker,
                caps(&[CAP_BACKEND, "provide:files"]),
            )
            .unwrap();
        k.register_backend(provider, "files").unwrap();
        let id: TaskId = serde_json::from_value(
            invoke(
                &mut k,
                HOST,
                Request::TaskCreate {
                    owner: HOST,
                    parent: None,
                    limits: ContextLimits::default(),
                    seed: vec![],
                },
            )
            .unwrap()["task"]
                .clone(),
        )
        .unwrap();
        assert!(
            k.dispatch(
                HOST,
                42,
                Request::TaskSystemCall {
                    task: id,
                    service: "files".into(),
                    operation: "read".into(),
                    payload: json!(null)
                }
            )
            .is_none()
        );
        invoke(&mut k, HOST, Request::TaskCancel { task: id }).unwrap();
        assert!(k.pending.is_empty());
        let deliveries = k.drain();
        assert!(deliveries.iter().any(|d|matches!(d,Delivery::Response{id:42,result:ApiResult::Error(e),..} if e.code=="cancelled")));
        assert!(deliveries.iter().any(|d|matches!(d,Delivery::Notice{to,notice:Notice::SystemCallCancelled{..}} if *to==provider)));
        assert!(
            k.dispatch(
                HOST,
                43,
                Request::TaskSystemCall {
                    task: id,
                    service: "files".into(),
                    operation: "read".into(),
                    payload: json!(null)
                }
            )
            .unwrap()
            .into_result()
            .is_err()
        );
    }
    #[test]
    fn read_grant_does_not_grant_write_or_undeclared_operations() {
        let mut k = Kernel::new();
        k.define_operation("files", "read", "files.read").unwrap();
        k.define_operation("files", "write", "files.write").unwrap();
        let provider = k
            .create_subject(
                HOST,
                HOST,
                Level::Worker,
                caps(&[CAP_BACKEND, "provide:files"]),
            )
            .unwrap();
        k.register_backend(provider, "files").unwrap();
        let caller = k
            .create_subject(HOST, HOST, Level::Worker, caps(&["files.read"]))
            .unwrap();
        for op in ["write", "remove"] {
            assert!(
                k.dispatch(
                    caller,
                    1,
                    Request::SystemCall {
                        service: "files".into(),
                        operation: op.into(),
                        payload: json!(null)
                    }
                )
                .unwrap()
                .into_result()
                .is_err()
            );
        }
        assert!(
            k.dispatch(
                caller,
                2,
                Request::SystemCall {
                    service: "files".into(),
                    operation: "read".into(),
                    payload: json!(null)
                }
            )
            .is_none()
        );
        assert!(k.drain().iter().any(|d|matches!(d,Delivery::Notice{notice:Notice::SystemCall{caller:source,..},..} if *source==caller)));
    }
    #[test]
    fn deep_task_chains_are_bounded_and_cancel_without_recursion() {
        let mut k = Kernel::new();
        let mut parent = None;
        let mut root = None;
        for _ in 0..256 {
            let id: TaskId = serde_json::from_value(
                invoke(
                    &mut k,
                    HOST,
                    Request::TaskCreate {
                        owner: HOST,
                        parent,
                        limits: ContextLimits::default(),
                        seed: vec![],
                    },
                )
                .unwrap()["task"]
                    .clone(),
            )
            .unwrap();
            if root.is_none() {
                root = Some(id);
            }
            parent = Some(id);
        }
        assert!(
            invoke(
                &mut k,
                HOST,
                Request::TaskCreate {
                    owner: HOST,
                    parent,
                    limits: ContextLimits::default(),
                    seed: vec![]
                }
            )
            .is_err()
        );
        invoke(
            &mut k,
            HOST,
            Request::TaskCancel {
                task: root.unwrap(),
            },
        )
        .unwrap();
        assert!(k.tasks.values().all(|t| t.state == TaskState::Cancelled));
    }
    #[test]
    fn large_context_is_pageable_and_never_a_transport_failure() {
        let mut k = Kernel::new();
        let id: TaskId = serde_json::from_value(
            invoke(
                &mut k,
                HOST,
                Request::TaskCreate {
                    owner: HOST,
                    parent: None,
                    limits: ContextLimits::default(),
                    seed: vec![],
                },
            )
            .unwrap()["task"]
                .clone(),
        )
        .unwrap();
        for revision in 0..3 {
            invoke(
                &mut k,
                HOST,
                Request::ContextAppend {
                    task: id,
                    expected_revision: revision,
                    items: vec![ContextItem::text("x".repeat(400_000), 1)],
                },
            )
            .unwrap();
        }
        assert_eq!(
            invoke(&mut k, HOST, Request::ContextGet { task: id })
                .unwrap_err()
                .code,
            "response_too_large"
        );
        assert_eq!(
            invoke(
                &mut k,
                HOST,
                Request::ContextRead {
                    task: id,
                    start: 1,
                    limit: 1
                }
            )
            .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(k.context(id).unwrap().snapshot().items.len(), 3);
    }
    #[test]
    fn provider_does_not_choose_permissions_or_spoof_results() {
        let mut k = Kernel::new();
        k.define_service("files", "files.read").unwrap();
        let provider = k
            .create_subject(
                HOST,
                HOST,
                Level::Worker,
                caps(&[CAP_BACKEND, "provide:files"]),
            )
            .unwrap();
        k.register_backend(provider, "files").unwrap();
        let other = k
            .create_subject(HOST, HOST, Level::Worker, caps(&[]))
            .unwrap();
        assert!(
            k.dispatch(
                other,
                1,
                Request::SystemCall {
                    service: "files".into(),
                    operation: "read".into(),
                    payload: json!({})
                }
            )
            .unwrap()
            .into_result()
            .is_err()
        );
        assert!(
            k.dispatch(
                HOST,
                2,
                Request::SystemCall {
                    service: "files".into(),
                    operation: "read".into(),
                    payload: json!({})
                }
            )
            .is_none()
        );
        let call = *k.pending.keys().next().unwrap();
        assert!(
            invoke(
                &mut k,
                other,
                Request::BackendComplete {
                    call,
                    result: BackendResult::Ok(json!("fake"))
                }
            )
            .is_err()
        );
        let native =
            BackendResult::Error(json!({"errno":13,"message":"native error","custom":[1,2]}));
        invoke(
            &mut k,
            provider,
            Request::BackendComplete {
                call,
                result: native.clone(),
            },
        )
        .unwrap();
        assert!(k.drain().iter().any(
            |d| matches!(d,Delivery::Response{result:ApiResult::Ok(v),..} if *v==json!(native))
        ));
    }
}
