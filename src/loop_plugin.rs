//! User-space process driver for AgentLoop. Multiple tasks share one Client;
//! local loop IDs are mapped to connection-wide wire IDs explicitly.
use crate::agent_loop::{AgentLoop, LoopConfig, LoopEvent, LoopRequestId, LoopStatus};
use crate::api::{ApiResult, KernelError, Notice, Request, TaskId};
use crate::sdk::Client;
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

#[derive(Debug)]
enum Pending {
    Loop { task: TaskId, id: LoopRequestId },
    Report,
}

/// No routing/worker policy is imposed: each TaskAssigned starts the configured
/// loop, and the owner may instead attach tasks or resume compaction explicitly.
pub struct LoopPlugin {
    client: Client,
    config: LoopConfig,
    loops: BTreeMap<TaskId, AgentLoop>,
    pending: BTreeMap<u64, Pending>,
    events: VecDeque<(TaskId, LoopEvent)>,
    notices: VecDeque<Notice>,
    report_errors: VecDeque<KernelError>,
    /// Optional event topic prefix for host observability. Reports are not
    /// context messages and do not enter model input or block model/tool waits.
    report_prefix: Option<String>,
    pub max_active_tasks: usize,
}
impl LoopPlugin {
    pub fn new(client: Client, config: LoopConfig) -> Result<Self, KernelError> {
        config.validate()?;
        Ok(Self {
            client,
            config,
            loops: BTreeMap::new(),
            pending: BTreeMap::new(),
            events: VecDeque::new(),
            notices: VecDeque::new(),
            report_errors: VecDeque::new(),
            report_prefix: None,
            max_active_tasks: 64,
        })
    }
    pub fn aid(&self) -> crate::Aid {
        self.client.aid()
    }
    pub fn set_report_prefix(&mut self, prefix: Option<String>) {
        self.report_prefix = prefix;
    }
    pub fn attach(&mut self, task: TaskId) -> Result<(), KernelError> {
        if self.loops.contains_key(&task) {
            return Err(KernelError::new("already_exists", "loop already attached"));
        }
        if self.loops.len() >= self.max_active_tasks {
            return Err(KernelError::new("limit_exceeded", "active loop limit"));
        }
        self.loops
            .insert(task, AgentLoop::new(task, self.config.clone())?);
        Ok(())
    }
    pub fn cancel(&mut self, task: TaskId) -> Result<(), KernelError> {
        self.loops
            .get_mut(&task)
            .ok_or_else(|| KernelError::missing("loop"))?
            .cancel();
        Ok(())
    }
    pub fn resume_after_compaction(&mut self, task: TaskId) -> Result<(), KernelError> {
        self.loops
            .get_mut(&task)
            .ok_or_else(|| KernelError::missing("loop"))?
            .resume_after_compaction()
    }
    pub fn take_event(&mut self) -> Option<(TaskId, LoopEvent)> {
        self.events.pop_front()
    }
    pub fn take_notice(&mut self) -> Option<Notice> {
        self.notices.pop_front()
    }
    pub fn take_report_error(&mut self) -> Option<KernelError> {
        self.report_errors.pop_front()
    }
    fn report_error(&mut self, error: KernelError) {
        if self.report_errors.len() < 128 {
            self.report_errors.push_back(error);
        }
    }
    pub fn active_tasks(&self) -> usize {
        self.loops.len()
    }
    pub fn client_mut(&mut self) -> &mut Client {
        &mut self.client
    }

    /// Process one input frame and advance ready tasks without waiting on a
    /// model/tool result. stdout writes remain synchronous; this is not a
    /// hard realtime or fully asynchronous transport guarantee.
    pub fn tick(&mut self, wait: Duration) -> Result<(), KernelError> {
        self.collect_responses();
        self.collect_notices()?;
        self.advance()?;
        self.client.poll(wait)?;
        self.collect_responses();
        self.collect_notices()?;
        self.advance()
    }
    fn collect_responses(&mut self) {
        let ids: Vec<_> = self.pending.keys().copied().collect();
        for wire_id in ids {
            if let Some(response) = self.client.take_response(wire_id) {
                match self.pending.remove(&wire_id).unwrap() {
                    Pending::Loop { task, id } => {
                        if let Some(agent) = self.loops.get_mut(&task) {
                            agent.accept_response(id, response);
                        }
                    }
                    Pending::Report => {
                        if let ApiResult::Error(error) = response {
                            self.report_error(error);
                        }
                    }
                }
            }
        }
    }
    fn collect_notices(&mut self) -> Result<(), KernelError> {
        while let Some(notice) = self.client.take_notice() {
            match &notice {
                Notice::TaskAssigned { task } => {
                    if self.loops.contains_key(task) {
                        continue;
                    }
                    if let Err(error) = self.attach(*task) {
                        let id = self.client.send(Request::TaskFail { task: *task, error })?;
                        self.pending.insert(id, Pending::Report);
                    }
                }
                Notice::ContextReplaced { task, .. } => {
                    if let Some(agent) = self.loops.get_mut(task) {
                        if agent.status() == LoopStatus::NeedsCompaction {
                            agent.resume_after_compaction()?;
                        }
                    }
                }
                Notice::TaskEnded { task, .. } => {
                    if let Some(agent) = self.loops.get_mut(task) {
                        agent.on_notice(&notice);
                    }
                }
                _ => {
                    if self.notices.len() >= 1024 {
                        return Err(KernelError::new(
                            "limit_exceeded",
                            "unconsumed loop plugin notices",
                        ));
                    }
                    self.notices.push_back(notice);
                }
            }
        }
        Ok(())
    }
    fn advance(&mut self) -> Result<(), KernelError> {
        let tasks: Vec<_> = self.loops.keys().copied().collect();
        for task in tasks {
            let request = self.loops.get_mut(&task).unwrap().next_request();
            if let Some(request) = request {
                match self.client.send(request.request) {
                    Ok(wire_id) => {
                        self.pending.insert(
                            wire_id,
                            Pending::Loop {
                                task,
                                id: request.id,
                            },
                        );
                    }
                    Err(error) => {
                        self.loops
                            .get_mut(&task)
                            .unwrap()
                            .accept_response(request.id, ApiResult::Error(error));
                    }
                }
            }
            let agent = self.loops.get_mut(&task).unwrap();
            if self.events.len().saturating_add(agent.pending_events()) > 4096 {
                // Leave all unconsumed events attached to the loop so callers
                // can drain and retry; never lose an Ended or pressure event.
                return Err(KernelError::new("limit_exceeded", "unconsumed loop events"));
            }
            let ended = agent.status() == LoopStatus::Ended;
            let events = agent.take_events();
            for event in events {
                self.events.push_back((task, event.clone()));
                if let Some(prefix) = &self.report_prefix {
                    if self.pending.len() >= 2048 {
                        self.report_error(KernelError::new(
                            "limit_exceeded",
                            "loop diagnostic reports are not draining",
                        ));
                        continue;
                    }
                    let topic = format!("{prefix}.{}", task.0);
                    let mut data = json!({"task":task,"event":event});
                    if serde_json::to_vec(&data).map_or(true, |bytes| {
                        bytes.len() > crate::protocol::MAX_FRAME_BYTES / 4
                    }) {
                        data = json!({"task":task,"event":{"kind":"large_event","details":"full payload available from local loop event API"}});
                    }
                    match self.client.send(Request::EventPublish { topic, data }) {
                        Ok(id) => {
                            self.pending.insert(id, Pending::Report);
                        }
                        Err(error) => self.report_error(error),
                    }
                }
            }
            if ended {
                self.loops.remove(&task);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::{LoopOutcome, ServiceRoute};
    use crate::{KernelError, TaskState};

    #[test]
    fn full_event_queue_preserves_terminal_event_until_drain_and_retry() {
        let config = LoopConfig::new(ServiceRoute::new("model", "generate"));
        let mut driver =
            LoopPlugin::new(Client::disconnected_test_client(), config.clone()).unwrap();
        let mut agent = AgentLoop::new(TaskId(1), config).unwrap();
        agent.on_notice(&Notice::TaskEnded {
            task: TaskId(1),
            state: TaskState::Cancelled,
            result: None,
        });
        driver.loops.insert(TaskId(1), agent);
        for _ in 0..4096 {
            driver.events.push_back((
                TaskId(99),
                LoopEvent::NeedsIntervention {
                    error: KernelError::new("test", "filler"),
                },
            ));
        }
        assert_eq!(driver.advance().unwrap_err().code, "limit_exceeded");
        assert_eq!(driver.loops[&TaskId(1)].pending_events(), 1);
        while driver.take_event().is_some() {}
        driver.advance().unwrap();
        assert_eq!(driver.active_tasks(), 0);
        assert!(matches!(
            driver.take_event(),
            Some((
                TaskId(1),
                LoopEvent::Ended {
                    outcome: LoopOutcome::Cancelled { .. }
                }
            ))
        ));
        assert!(driver.take_event().is_none());
    }
}
