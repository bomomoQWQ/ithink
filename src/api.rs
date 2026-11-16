use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);
    };
}
id_type!(Aid);
id_type!(TaskId);
id_type!(EventId);

pub const HOST: Aid = Aid(0);
pub const CAP_AGENT: &str = "kernel.agent";
pub const CAP_TASK: &str = "kernel.task";
pub const CAP_EVENT: &str = "kernel.event";
pub const CAP_CONTEXT: &str = "kernel.context";
pub const CAP_BACKEND: &str = "kernel.backend";
pub const CAP_PLUGIN: &str = "kernel.plugin";

pub type Capabilities = BTreeSet<String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Level {
    User = 0,
    Main = 1,
    Worker = 2,
    Ephemeral = 3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub id: Aid,
    pub parent: Option<Aid>,
    pub level: Level,
    pub capabilities: Capabilities,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub owner: Aid,
    pub parent: Option<TaskId>,
    pub state: TaskState,
    pub result: Option<Value>,
}

/// Opaque bytes are never normalized. `units` are supplied by a trusted meter
/// or plugin, not advertised as exact provider token counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextItem {
    pub data: Value,
    pub units: u64,
    #[serde(default)]
    pub opens: Vec<String>,
    #[serde(default)]
    pub closes: Vec<String>,
    #[serde(default)]
    pub pinned: bool,
}

impl ContextItem {
    pub fn text(text: impl Into<String>, units: u64) -> Self {
        Self {
            data: Value::String(text.into()),
            units,
            opens: vec![],
            closes: vec![],
            pinned: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextLimits {
    pub capacity: u64,
    pub output_reserve: u64,
    pub headroom: u64,
    /// Integer basis points, 8000 = 80%.
    pub pressure_bps: u16,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            capacity: 131_072,
            output_reserve: 8192,
            headroom: 8192,
            pressure_bps: 8000,
        }
    }
}

impl ContextLimits {
    pub fn threshold(&self) -> Result<u64, KernelError> {
        if self.capacity == 0 || self.pressure_bps == 0 || self.pressure_bps > 10_000 {
            return Err(KernelError::invalid("invalid context limits"));
        }
        let input = self
            .capacity
            .checked_sub(self.output_reserve)
            .ok_or_else(|| KernelError::invalid("output reserve exceeds capacity"))?;
        let pressure = input
            .checked_sub(self.headroom)
            .filter(|n| *n > 0)
            .ok_or_else(|| KernelError::invalid("no context headroom remains"))?;
        let ratio = ((u128::from(self.capacity) * u128::from(self.pressure_bps)) / 10_000) as u64;
        Ok(ratio.min(pressure))
    }

    pub fn max_input(&self) -> u64 {
        self.capacity.saturating_sub(self.output_reserve)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub source: Aid,
    pub topic: String,
    pub sequence: u64,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    Event {
        event: Event,
    },
    ContextPressure {
        task: TaskId,
        used: u64,
        threshold: u64,
        max_input: u64,
    },
    ContextReplaced {
        task: TaskId,
        revision: u64,
    },
    TaskAssigned {
        task: TaskId,
    },
    TaskEnded {
        task: TaskId,
        state: TaskState,
        result: Option<Value>,
    },
    SystemCallCancelled {
        call: u64,
    },
    SystemCall {
        call: u64,
        caller: Aid,
        task: Option<TaskId>,
        service: String,
        operation: String,
        payload: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    PluginStart {
        template: String,
        name: String,
    },
    PluginStop {
        name: String,
    },
    AgentCreate {
        parent: Aid,
        level: Level,
        capabilities: Capabilities,
    },
    AgentGet {
        aid: Aid,
    },
    TaskCreate {
        owner: Aid,
        parent: Option<TaskId>,
        limits: ContextLimits,
        seed: Vec<ContextItem>,
    },
    TaskGet {
        task: TaskId,
    },
    TaskFinish {
        task: TaskId,
        result: Value,
    },
    TaskCancel {
        task: TaskId,
    },
    TaskFail {
        task: TaskId,
        error: KernelError,
    },
    ContextAppend {
        task: TaskId,
        expected_revision: u64,
        items: Vec<ContextItem>,
    },
    ContextGet {
        task: TaskId,
    },
    ContextRead {
        task: TaskId,
        start: usize,
        limit: usize,
    },
    ContextReplace {
        task: TaskId,
        expected_revision: u64,
        start: usize,
        end: usize,
        replacement: Vec<ContextItem>,
    },
    EventSubscribe {
        topic: String,
    },
    EventUnsubscribe {
        topic: String,
    },
    EventPublish {
        topic: String,
        data: Value,
    },
    BackendRegister {
        service: String,
    },
    BackendComplete {
        call: u64,
        result: BackendResult,
    },
    SystemCall {
        service: String,
        operation: String,
        payload: Value,
    },
    TaskSystemCall {
        task: TaskId,
        service: String,
        operation: String,
        payload: Value,
    },
}

/// Provider-native success/error payloads pass through unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum BackendResult {
    Ok(Value),
    Error(Value),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelError {
    pub code: String,
    pub message: String,
}

impl KernelError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_request", message)
    }
    pub fn denied() -> Self {
        Self::new("permission_denied", "caller is not authorized")
    }
    pub fn missing(kind: &str) -> Self {
        Self::new("not_found", format!("{kind} not found"))
    }
}
impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for KernelError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum ApiResult {
    Ok(Value),
    Error(KernelError),
}
impl ApiResult {
    pub fn into_result(self) -> Result<Value, KernelError> {
        match self {
            Self::Ok(v) => Ok(v),
            Self::Error(e) => Err(e),
        }
    }
}
impl From<Result<Value, KernelError>> for ApiResult {
    fn from(value: Result<Value, KernelError>) -> Self {
        match value {
            Ok(v) => Self::Ok(v),
            Err(e) => Self::Error(e),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub sequence: u64,
    pub actor: Aid,
    pub kind: String,
    pub data: Value,
}
