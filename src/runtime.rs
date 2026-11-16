use crate::api::*;
use crate::kernel::{Delivery, Kernel};
use crate::plugin::{self, Incoming, Peer, PluginSpec, PluginToken, Starting};
use crate::protocol::{KernelFrame, PluginFrame};
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LoadTicket(pub u64);
struct Staged {
    spec: PluginSpec,
    starting: Starting,
}

/// Host-owned pump. `submit` never waits for a backend. `pump` processes bounded
/// work and returns; the host controls integration with its own event loop.
pub struct Runtime {
    kernel: Kernel,
    peers: BTreeMap<String, Peer>,
    staged: BTreeMap<LoadTicket, Staged>,
    loaded: BTreeMap<LoadTicket, Result<PluginToken, KernelError>>,
    templates: BTreeMap<String, PluginSpec>,
    load_callers: BTreeMap<LoadTicket, (Aid, u64)>,
    incoming_tx: mpsc::SyncSender<Incoming>,
    incoming_rx: mpsc::Receiver<Incoming>,
    replies: BTreeMap<u64, ApiResult>,
    notices: VecDeque<Notice>,
    last_request: BTreeMap<Aid, u64>,
    next_id: u64,
    next_generation: u64,
    pub handshake_timeout: Duration,
}
impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}
impl Runtime {
    pub fn new() -> Self {
        let (incoming_tx, incoming_rx) = mpsc::sync_channel(1024);
        Self {
            kernel: Kernel::new(),
            peers: BTreeMap::new(),
            staged: BTreeMap::new(),
            loaded: BTreeMap::new(),
            templates: BTreeMap::new(),
            load_callers: BTreeMap::new(),
            incoming_tx,
            incoming_rx,
            replies: BTreeMap::new(),
            notices: VecDeque::new(),
            last_request: BTreeMap::new(),
            next_id: 1,
            next_generation: 1,
            handshake_timeout: Duration::from_secs(5),
        }
    }
    pub fn kernel(&self) -> &Kernel {
        &self.kernel
    }
    pub fn define_service(&mut self, service: &str, required: &str) -> Result<(), KernelError> {
        self.kernel.define_service(service, required)
    }
    pub fn define_operation(
        &mut self,
        service: &str,
        operation: &str,
        required: &str,
    ) -> Result<(), KernelError> {
        self.kernel.define_operation(service, operation, required)
    }
    /// Templates are host-approved executables, not commands supplied by a model.
    pub fn register_template(
        &mut self,
        name: impl Into<String>,
        spec: PluginSpec,
    ) -> Result<(), KernelError> {
        let name = name.into();
        if name.is_empty() || self.templates.contains_key(&name) {
            return Err(KernelError::invalid("invalid or duplicate template"));
        }
        self.templates.insert(name, spec);
        Ok(())
    }
    pub fn set_call_timeout(&mut self, timeout: Duration) {
        self.kernel.call_timeout = timeout;
    }
    pub fn plugin(&self, name: &str) -> Option<&PluginToken> {
        self.peers.get(name).map(|p| &p.token)
    }
    pub fn take_notice(&mut self) -> Option<Notice> {
        self.notices.pop_front()
    }

    /// Stage a process without blocking existing plugins. Dependencies must
    /// already be active. No library ABI/dlopen dependency is used.
    pub fn begin_load(&mut self, spec: PluginSpec) -> Result<LoadTicket, KernelError> {
        self.begin_load_as(HOST, spec)
    }
    fn begin_load_as(&mut self, actor: Aid, spec: PluginSpec) -> Result<LoadTicket, KernelError> {
        if actor != HOST {
            self.kernel.require(actor, CAP_PLUGIN)?;
            if let Some(old) = self.peers.get(&spec.name) {
                if old.token.aid == actor || !self.kernel.manages(actor, old.token.aid) {
                    return Err(KernelError::denied());
                }
            }
        }
        if spec.name.is_empty() || spec.name.len() > 256 || spec.name.chars().any(char::is_control)
        {
            return Err(KernelError::invalid("invalid plugin name"));
        }
        if self.staged.values().any(|s| s.spec.name == spec.name) {
            return Err(KernelError::new("busy", "plugin already loading"));
        }
        if self.peers.len() + self.staged.len() >= 256 {
            return Err(KernelError::new("limit_exceeded", "plugin limit"));
        }
        if self.loaded.len() >= 4096 {
            return Err(KernelError::new(
                "limit_exceeded",
                "uncollected plugin load results",
            ));
        }
        for dep in &spec.dependencies {
            if dep == &spec.name || !self.peers.contains_key(dep) {
                return Err(KernelError::new(
                    "missing_dependency",
                    format!("dependency not active: {dep}"),
                ));
            }
            let mut stack = vec![dep.clone()];
            let mut seen = std::collections::BTreeSet::new();
            while let Some(name) = stack.pop() {
                if name == spec.name {
                    return Err(KernelError::new(
                        "dependency_cycle",
                        "replacement would create a dependency cycle",
                    ));
                }
                if seen.insert(name.clone()) {
                    if let Some(peer) = self.peers.get(&name) {
                        stack.extend(peer.dependencies.iter().cloned());
                    }
                }
            }
        }
        // A launch:<template> grant delegates this host-approved template's
        // capabilities; the coordinator need not hold the worker's tools.
        if actor != HOST {
            let parent = self.kernel.subject(actor).ok_or_else(KernelError::denied)?;
            if spec.level as u8 <= parent.level as u8 {
                return Err(KernelError::denied());
            }
        }
        let aid = self
            .kernel
            .create_subject(HOST, actor, spec.level, spec.capabilities.clone())?;
        let old = self.peers.get(&spec.name).map(|p| p.token.aid);
        if let Err(e) = self.kernel.can_register_services(aid, old, &spec.provides) {
            self.kernel.retire(aid, "activation rejected");
            return Err(e);
        }
        let ticket = LoadTicket(self.next_generation);
        self.next_generation += 1;
        let token = PluginToken {
            name: spec.name.clone(),
            aid,
            generation: ticket.0,
        };
        let starting = match plugin::start(
            &spec,
            token,
            self.incoming_tx.clone(),
            self.handshake_timeout,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.kernel.retire(aid, "spawn failed");
                return Err(e);
            }
        };
        self.staged.insert(ticket, Staged { spec, starting });
        Ok(ticket)
    }
    pub fn take_load_result(
        &mut self,
        ticket: LoadTicket,
    ) -> Option<Result<PluginToken, KernelError>> {
        self.loaded.remove(&ticket)
    }
    pub fn load_plugin(&mut self, spec: PluginSpec) -> Result<PluginToken, KernelError> {
        let ticket = self.begin_load(spec)?;
        loop {
            self.pump(Duration::from_millis(10));
            if let Some(result) = self.take_load_result(ticket) {
                return result;
            }
        }
    }
    pub fn unload_plugin(&mut self, name: &str) -> Result<(), KernelError> {
        self.unload_as(HOST, name)
    }
    fn unload_as(&mut self, actor: Aid, name: &str) -> Result<(), KernelError> {
        if actor != HOST {
            self.kernel.require(actor, CAP_PLUGIN)?;
            let target = self
                .peers
                .get(name)
                .ok_or_else(|| KernelError::missing("plugin"))?
                .token
                .aid;
            if target == actor || !self.kernel.manages(actor, target) {
                return Err(KernelError::denied());
            }
        }
        if self.staged.values().any(|s| s.spec.name == name) {
            return Err(KernelError::new("busy", "plugin has a staged replacement"));
        }
        if self
            .peers
            .values()
            .any(|p| p.dependencies.iter().any(|d| d == name))
        {
            return Err(KernelError::new(
                "in_use",
                "an active plugin depends on this plugin",
            ));
        }
        if !self.peers.contains_key(name) {
            return Err(KernelError::missing("plugin"));
        }
        self.stop_peer(name, "unloaded");
        self.flush();
        Ok(())
    }

    pub fn submit(&mut self, request: Request) -> Result<u64, KernelError> {
        if self.replies.len() >= 4096 {
            return Err(KernelError::new(
                "limit_exceeded",
                "uncollected host replies",
            ));
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| KernelError::new("limit_exceeded", "request id exhausted"))?;
        if let Some(reply) = self.kernel.dispatch(HOST, id, request) {
            self.replies.insert(id, reply);
        }
        self.flush();
        Ok(id)
    }
    pub fn take_response(&mut self, id: u64) -> Option<ApiResult> {
        self.replies.remove(&id)
    }
    pub fn request(
        &mut self,
        request: Request,
        timeout: Duration,
    ) -> Result<serde_json::Value, KernelError> {
        let id = self.submit(request)?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(result) = self.take_response(id) {
                return result.into_result();
            }
            if Instant::now() >= deadline {
                self.kernel.cancel_request(HOST, id);
                self.replies.remove(&id);
                return Err(KernelError::new(
                    "timeout",
                    "host request timed out; external effects are not rolled back",
                ));
            }
            self.pump(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(10)),
            );
        }
    }

    /// Does not run plugin code in the kernel thread. A bounded batch prevents
    /// one talkative plugin from keeping the host inside pump indefinitely.
    pub fn pump(&mut self, wait: Duration) {
        self.poll_starting();
        self.reap_faults();
        self.kernel.expire_calls();
        self.flush();
        match self.incoming_rx.recv_timeout(wait) {
            Ok(input) => self.handle(input),
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
        }
        for _ in 0..127 {
            match self.incoming_rx.try_recv() {
                Ok(input) => self.handle(input),
                Err(_) => break,
            }
        }
        self.poll_starting();
        self.finish_plugin_calls();
        self.reap_faults();
        self.kernel.expire_calls();
        self.flush();
    }
    fn handle(&mut self, input: Incoming) {
        let Some(peer) = self.peers.get(&input.name) else {
            return;
        };
        if peer.token.generation != input.generation {
            return;
        }
        let aid = peer.token.aid;
        let PluginFrame::Request { id, request } = input.frame else {
            return;
        };
        let last = self.last_request.entry(aid).or_default();
        if id <= *last {
            self.stop_peer(&input.name, "non-increasing request id");
            return;
        }
        *last = id;
        let response = match request {
            Request::PluginStart { template, name } => {
                let result = self
                    .kernel
                    .require(aid, CAP_PLUGIN)
                    .and_then(|()| self.kernel.require(aid, &format!("launch:{template}")))
                    .and_then(|()| {
                        self.templates
                            .get(&template)
                            .cloned()
                            .ok_or_else(|| KernelError::missing("plugin template"))
                    })
                    .and_then(|mut spec| {
                        for arg in &mut spec.args {
                            if arg == "{name}" {
                                *arg = name.clone();
                            }
                        }
                        spec.name = name;
                        self.begin_load_as(aid, spec)
                    });
                match result {
                    Ok(ticket) => {
                        self.load_callers.insert(ticket, (aid, id));
                        None
                    }
                    Err(e) => Some(ApiResult::Error(e)),
                }
            }
            Request::PluginStop { name } => {
                Some(self.unload_as(aid, &name).map(|()| json!(true)).into())
            }
            request => self.kernel.dispatch(aid, id, request),
        };
        if let Some(result) = response {
            let frame = bounded_response(id, result);
            if self
                .peers
                .get(&input.name)
                .is_some_and(|p| p.send(frame).is_err())
            {
                self.stop_peer(&input.name, "plugin response queue full");
            }
        }
        self.flush();
    }
    fn poll_starting(&mut self) {
        let mut ready = vec![];
        for (&ticket, staged) in &mut self.staged {
            if let Some(result) = staged.starting.poll_ready() {
                ready.push((ticket, result));
            }
        }
        for (ticket, result) in ready {
            let mut staged = self.staged.remove(&ticket).unwrap();
            let aid = staged.starting.peer.token.aid;
            let old = self.peers.get(&staged.spec.name).map(|p| p.token.aid);
            let result = result.and_then(|()| {
                if !self.kernel.subject(aid).is_some_and(|s| s.active) {
                    return Err(KernelError::new(
                        "cancelled",
                        "plugin parent retired during startup",
                    ));
                }
                for dep in &staged.spec.dependencies {
                    let mut stack = vec![dep.clone()];
                    let mut seen = std::collections::BTreeSet::new();
                    while let Some(name) = stack.pop() {
                        if name == staged.spec.name {
                            return Err(KernelError::new(
                                "dependency_cycle",
                                "activation would create a dependency cycle",
                            ));
                        }
                        if seen.insert(name.clone()) {
                            if let Some(peer) = self.peers.get(&name) {
                                stack.extend(peer.dependencies.iter().cloned());
                            }
                        }
                    }
                }
                if let Some(old) = old {
                    let mut parent = self.kernel.subject(aid).and_then(|s| s.parent);
                    while let Some(id) = parent {
                        if id == old {
                            return Err(KernelError::new(
                                "invalid_parent",
                                "replacement cannot be a child of its old instance",
                            ));
                        }
                        parent = self.kernel.subject(id).and_then(|s| s.parent);
                    }
                }
                for dep in &staged.spec.dependencies {
                    if !self.peers.contains_key(dep) {
                        return Err(KernelError::new("missing_dependency", dep.clone()));
                    }
                }
                self.kernel
                    .can_register_services(aid, old, &staged.spec.provides)?;
                if let Some(fault) = staged.starting.peer.fault() {
                    return Err(KernelError::new("handshake_failed", fault));
                }
                Ok(())
            });
            match result {
                Err(e) => {
                    staged.starting.peer.stop("failed activation");
                    self.kernel.retire(aid, "failed activation");
                    self.loaded.insert(ticket, Err(e));
                }
                Ok(()) => {
                    let name = staged.spec.name.clone();
                    if self.peers.contains_key(&name) {
                        self.stop_peer(&name, "hot replacement");
                    }
                    for service in &staged.spec.provides {
                        self.kernel
                            .register_backend(aid, service)
                            .expect("activation was validated");
                    }
                    let token = staged.starting.peer.token.clone();
                    let _ = staged.starting.peer.send(KernelFrame::Active);
                    self.kernel.record(
                        HOST,
                        "plugin.activated",
                        json!({"name":name,"aid":aid,"generation":ticket.0}),
                    );
                    self.peers.insert(name, staged.starting.peer);
                    self.loaded.insert(ticket, Ok(token));
                }
            }
        }
    }
    fn finish_plugin_calls(&mut self) {
        let completed: Vec<_> = self
            .load_callers
            .keys()
            .filter(|t| self.loaded.contains_key(t))
            .copied()
            .collect();
        for ticket in completed {
            let (aid, id) = self.load_callers.remove(&ticket).unwrap();
            let result = self
                .loaded
                .remove(&ticket)
                .unwrap()
                .map(|t| json!({"aid":t.aid,"generation":t.generation,"name":t.name}))
                .into();
            if let Some((_, peer)) = self.peers.iter().find(|(_, p)| p.token.aid == aid) {
                let _ = peer.send(bounded_response(id, result));
            }
        }
    }
    fn stop_peer(&mut self, name: &str, reason: &str) {
        if let Some(mut peer) = self.peers.remove(name) {
            let aid = peer.token.aid;
            peer.stop(reason);
            self.last_request.remove(&aid);
            self.kernel.retire(aid, reason);
        }
    }
    fn reap_faults(&mut self) {
        let faults: Vec<_> = self
            .peers
            .iter_mut()
            .filter_map(|(n, p)| {
                if !self.kernel.subject(p.token.aid).is_some_and(|s| s.active) {
                    Some((n.clone(), "owner retired".into()))
                } else {
                    p.fault().map(|f| (n.clone(), f))
                }
            })
            .collect();
        for (name, reason) in faults {
            self.stop_peer(&name, &reason);
        }
        // Dependencies are references to an active plugin name, not its PID.
        loop {
            let broken = self
                .peers
                .iter()
                .find(|(_, p)| p.dependencies.iter().any(|d| !self.peers.contains_key(d)))
                .map(|(n, _)| n.clone());
            match broken {
                Some(n) => self.stop_peer(&n, "dependency lost"),
                None => break,
            }
        }
    }
    fn flush(&mut self) {
        // A failed route may enqueue failure responses; drain until resolved.
        loop {
            let deliveries = self.kernel.drain();
            if deliveries.is_empty() {
                break;
            }
            let mut failed = vec![];
            for delivery in deliveries {
                let (to, frame) = match delivery {
                    Delivery::Response { to, id, result } if to == HOST => {
                        self.replies.insert(id, result);
                        continue;
                    }
                    Delivery::Notice { to, notice } if to == HOST => {
                        if self.notices.len() < 4096 {
                            self.notices.push_back(notice);
                        }
                        continue;
                    }
                    Delivery::Response { to, id, result } => (to, bounded_response(id, result)),
                    Delivery::Notice { to, notice } => (to, KernelFrame::Notice { notice }),
                };
                if let Some((name, peer)) = self.peers.iter().find(|(_, p)| p.token.aid == to) {
                    if peer.send(frame).is_err() {
                        failed.push(name.clone());
                    }
                }
            }
            for name in failed {
                self.stop_peer(&name, "outgoing queue full or closed");
            }
        }
    }
}
fn bounded_response(id: u64, result: ApiResult) -> KernelFrame {
    let frame = KernelFrame::Response { id, result };
    match serde_json::to_vec(&frame) {
        Ok(bytes) if bytes.len() < crate::protocol::MAX_FRAME_BYTES => frame,
        _ => KernelFrame::Response {
            id,
            result: ApiResult::Error(KernelError::new(
                "response_too_large",
                "response exceeds frame limit; read smaller context pages",
            )),
        },
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        for (_, mut staged) in std::mem::take(&mut self.staged) {
            staged.starting.peer.stop("runtime shutdown");
        }
        while !self.peers.is_empty() {
            let name = self
                .peers
                .iter()
                .find(|(n, _)| !self.peers.values().any(|p| p.dependencies.contains(n)))
                .map(|(n, _)| n.clone())
                .unwrap_or_else(|| self.peers.keys().next().unwrap().clone());
            self.stop_peer(&name, "runtime shutdown");
        }
    }
}
