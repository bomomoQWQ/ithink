use crate::api::{Aid, Capabilities, KernelError, Level};
use crate::process_tree::{self, ProcessTree};
use crate::protocol::{KernelFrame, PROTOCOL_VERSION, PluginFrame, read_frame, write_frame};
use std::collections::BTreeMap;
use std::io::{self, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// One-use private stdin bootstrap, intentionally redacted from Debug/logs.
#[derive(Clone)]
pub struct BootstrapInput(Vec<u8>);
impl std::fmt::Debug for BootstrapInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BootstrapInput([REDACTED])")
    }
}
impl BootstrapInput {
    pub fn json(value: &impl serde::Serialize) -> Result<Self, KernelError> {
        let mut bytes = serde_json::to_vec(value)
            .map_err(|_| KernelError::invalid("invalid bootstrap data"))?;
        if bytes.len() > 65_536 {
            return Err(KernelError::invalid("bootstrap exceeds 64 KiB"));
        }
        bytes.push(b'\n');
        Ok(Self(bytes))
    }
}

#[derive(Debug, Clone)]
pub struct PluginSpec {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub current_dir: Option<PathBuf>,
    pub level: Level,
    pub capabilities: Capabilities,
    pub dependencies: Vec<String>,
    /// Service routes installed atomically with activation.
    pub provides: Vec<String>,
    /// Sent on private child stdin before public protocol. Never journaled.
    pub bootstrap: Option<BootstrapInput>,
}
impl PluginSpec {
    pub fn new(name: impl Into<String>, program: impl Into<PathBuf>, level: Level) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: vec![],
            env: BTreeMap::new(),
            current_dir: None,
            level,
            capabilities: Capabilities::new(),
            dependencies: vec![],
            provides: vec![],
            bootstrap: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginToken {
    pub name: String,
    pub aid: Aid,
    pub generation: u64,
}

#[derive(Debug)]
pub(crate) struct Incoming {
    pub name: String,
    pub generation: u64,
    pub frame: PluginFrame,
}

/// Startup is separate from activation: Runtime continues pumping active peers
/// while a new peer performs its handshake.
pub(crate) struct Starting {
    pub peer: Peer,
    ready: mpsc::Receiver<Result<Handshake, String>>,
    welcomed: bool,
    deadline: Instant,
}
enum Handshake {
    Ready(u32, String),
    Activated,
}
impl Starting {
    pub fn poll_ready(&mut self) -> Option<Result<(), KernelError>> {
        if let Some(fault) = self.peer.fault() {
            return Some(Err(KernelError::new("handshake_failed", fault)));
        }
        match self.ready.try_recv() {
            Ok(Ok(Handshake::Ready(version, name)))
                if !self.welcomed
                    && version == PROTOCOL_VERSION
                    && name == self.peer.token.name =>
            {
                if let Err(error) = self.peer.send(KernelFrame::Welcome {
                    protocol_version: PROTOCOL_VERSION,
                    aid: self.peer.token.aid,
                    generation: self.peer.token.generation,
                }) {
                    return Some(Err(error));
                }
                self.welcomed = true;
                None
            }
            Ok(Ok(Handshake::Activated)) if self.welcomed => Some(Ok(())),
            Ok(Ok(_)) => Some(Err(KernelError::new(
                "handshake_failed",
                "invalid activation handshake",
            ))),
            Ok(Err(e)) => Some(Err(KernelError::new("handshake_failed", e))),
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(KernelError::new(
                "handshake_failed",
                "plugin closed before activation",
            ))),
            Err(mpsc::TryRecvError::Empty) if Instant::now() >= self.deadline => Some(Err(
                KernelError::new("handshake_timeout", "plugin did not activate"),
            )),
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}

pub(crate) struct Peer {
    pub token: PluginToken,
    pub dependencies: Vec<String>,
    child: Option<Child>,
    tree: Option<ProcessTree>,
    threads: Vec<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
    outgoing: Option<mpsc::SyncSender<KernelFrame>>,
    fault: Arc<Mutex<Option<String>>>,
    stopped: bool,
}
impl Peer {
    pub fn send(&self, frame: KernelFrame) -> Result<(), KernelError> {
        self.outgoing
            .as_ref()
            .ok_or_else(|| KernelError::new("plugin_unavailable", "plugin is stopped"))?
            .try_send(frame)
            .map_err(|e| KernelError::new("plugin_backpressure", e.to_string()))
    }
    pub fn fault(&mut self) -> Option<String> {
        if let Some(fault) = self.fault.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Some(fault);
        }
        match self.child.as_mut()?.try_wait() {
            Ok(Some(status)) => Some(format!("plugin exited: {status}")),
            Err(e) => Some(format!("plugin status failed: {e}")),
            Ok(None) => None,
        }
    }
    pub fn stop(&mut self, reason: &str) {
        if self.stopped {
            return;
        }
        let _ = self.send(KernelFrame::Shutdown {
            reason: reason.into(),
        });
        self.stopping.store(true, Ordering::Release);
        self.outgoing.take();
        self.stopped = true;
        if let (Some(tree), Some(child)) = (self.tree.take(), self.child.take()) {
            // Process groups / Job Objects are lifecycle cleanup, NOT an OS
            // security sandbox. Potentially slow exit/reap never blocks pump.
            process_tree::stop(tree, child, std::mem::take(&mut self.threads));
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop("runtime dropped");
    }
}

// CancelSynchronousIo interrupts a pending Windows pipe operation; these guards
// also stop the next operation (including read_frame's partial-frame loop).
struct StoppableIo<T> {
    inner: T,
    stopping: Arc<AtomicBool>,
}
impl<T> StoppableIo<T> {
    fn check(&self) -> io::Result<()> {
        if self.stopping.load(Ordering::Acquire) {
            Err(io::Error::other("plugin stopped"))
        } else {
            Ok(())
        }
    }
}
impl<T: Read> Read for StoppableIo<T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.read(buffer)
    }
}
impl<T: Write> Write for StoppableIo<T> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.check()?;
        self.inner.flush()
    }
}

pub(crate) fn start(
    spec: &PluginSpec,
    token: PluginToken,
    inbox: mpsc::SyncSender<Incoming>,
    timeout: Duration,
) -> Result<Starting, KernelError> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .envs(&spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(path) = &spec.current_dir {
        cmd.current_dir(path);
    }
    process_tree::ensure_reaper()
        .map_err(|e| KernelError::new("cleanup_start_failed", e.to_string()))?;
    let mut tree = ProcessTree::prepare(&mut cmd)
        .map_err(|e| KernelError::new("process_tree_failed", e.to_string()))?;
    let mut child = cmd
        .spawn()
        .map_err(|e| KernelError::new("plugin_spawn_failed", e.to_string()))?;
    if let Err(error) = tree.attach(&child) {
        // Fail closed: never activate a process whose lifetime is unmanaged.
        process_tree::stop(tree, child, vec![]);
        return Err(KernelError::new("process_tree_failed", error.to_string()));
    }
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let fault = Arc::new(Mutex::new(None));
    let stopping = Arc::new(AtomicBool::new(false));
    // There are exactly two handshake results. Neither may block the reader
    // during shutdown while Runtime is dropping its staged receiver.
    let (ready_tx, ready) = mpsc::sync_channel(2);
    let (outgoing, out_rx) = mpsc::sync_channel::<KernelFrame>(128);
    let reader_fault = Arc::clone(&fault);
    let name = token.name.clone();
    let generation = token.generation;
    let reader_stopping = Arc::clone(&stopping);
    let reader = thread::Builder::new()
        .name(format!("{name}-stdout"))
        .spawn(move || {
            let mut reader = BufReader::new(StoppableIo {
                inner: stdout,
                stopping: Arc::clone(&reader_stopping),
            });
            let handshake = read_frame(&mut reader)
                .map_err(|e| e.to_string())
                .and_then(|line| line.ok_or_else(|| "EOF before ready".into()))
                .and_then(|line| {
                    serde_json::from_slice::<PluginFrame>(&line).map_err(|e| e.to_string())
                })
                .and_then(|frame| match frame {
                    PluginFrame::Ready {
                        protocol_version,
                        name,
                    } => Ok(Handshake::Ready(protocol_version, name)),
                    _ => Err("first frame must be ready".into()),
                });
            let valid = handshake.is_ok();
            if ready_tx.try_send(handshake).is_err()
                || !valid
                || reader_stopping.load(Ordering::Acquire)
            {
                return;
            }
            let activation = read_frame(&mut reader)
                .map_err(|e| e.to_string())
                .and_then(|line| line.ok_or_else(|| "EOF before activation".into()))
                .and_then(|line| {
                    serde_json::from_slice::<PluginFrame>(&line).map_err(|e| e.to_string())
                })
                .and_then(|frame| match frame {
                    PluginFrame::Activated => Ok(Handshake::Activated),
                    _ => Err("expected activated acknowledgement".into()),
                });
            let valid = activation.is_ok();
            if ready_tx.try_send(activation).is_err() || !valid {
                return;
            }
            let fail = loop {
                if reader_stopping.load(Ordering::Acquire) {
                    return;
                }
                match read_frame(&mut reader) {
                    Ok(Some(bytes)) => match serde_json::from_slice::<PluginFrame>(&bytes) {
                        Ok(PluginFrame::Ready { .. } | PluginFrame::Activated) => {
                            break "duplicate handshake".into();
                        }
                        Ok(frame) => {
                            if inbox
                                .try_send(Incoming {
                                    name: name.clone(),
                                    generation,
                                    frame,
                                })
                                .is_err()
                            {
                                break "kernel inbox full or disconnected".into();
                            }
                        }
                        Err(e) => break format!("invalid protocol: {e}"),
                    },
                    Ok(None) => break "plugin stdout closed".into(),
                    Err(e) => break format!("plugin protocol read: {e}"),
                }
            };
            *reader_fault.lock().unwrap_or_else(|p| p.into_inner()) = Some(fail);
        });
    let reader = match reader {
        Ok(reader) => reader,
        Err(error) => {
            stopping.store(true, Ordering::Release);
            process_tree::stop(tree, child, vec![]);
            return Err(KernelError::new("thread_spawn_failed", error.to_string()));
        }
    };
    let bootstrap = spec.bootstrap.clone();
    let writer_fault = Arc::clone(&fault);
    let writer_stopping = Arc::clone(&stopping);
    let writer = thread::Builder::new()
        .name(format!("{}-stdin", token.name))
        .spawn(move || {
            let mut writer = StoppableIo {
                inner: stdin,
                stopping: Arc::clone(&writer_stopping),
            };
            if let Some(input) = bootstrap {
                if writer
                    .write_all(&input.0)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    *writer_fault.lock().unwrap_or_else(|p| p.into_inner()) =
                        Some("private bootstrap failed".into());
                    return;
                }
            }
            while !writer_stopping.load(Ordering::Acquire) {
                match out_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(frame) => {
                        if writer_stopping.load(Ordering::Acquire) {
                            return;
                        }
                        if let Err(e) = write_frame(&mut writer, &frame) {
                            if !writer_stopping.load(Ordering::Acquire) {
                                *writer_fault.lock().unwrap_or_else(|p| p.into_inner()) =
                                    Some(format!("plugin write: {e}"));
                            }
                            return;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
    let writer = match writer {
        Ok(writer) => writer,
        Err(error) => {
            stopping.store(true, Ordering::Release);
            drop(outgoing);
            process_tree::stop(tree, child, vec![reader]);
            return Err(KernelError::new("thread_spawn_failed", error.to_string()));
        }
    };
    Ok(Starting {
        peer: Peer {
            token,
            dependencies: spec.dependencies.clone(),
            child: Some(child),
            tree: Some(tree),
            threads: vec![reader, writer],
            stopping,
            outgoing: Some(outgoing),
            fault,
            stopped: false,
        },
        ready,
        welcomed: false,
        deadline: Instant::now() + timeout,
    })
}
