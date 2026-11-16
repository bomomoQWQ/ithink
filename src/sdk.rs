//! Minimal native-plugin SDK using only the public wire API.
//!
//! One plugin process must own at most one `Client`: stdin/stdout are the
//! process-wide protocol connection. After the synchronous activation handshake,
//! a bounded reader thread owns stdin. [`Client::poll`] never reads stdin itself,
//! so a driver can interleave responses and notices for many tasks.
//!
//! Only reads are moved off-thread. Writes remain synchronous on the caller's
//! thread, preserving request order; this is not a fully async transport. A
//! blocked ordinary stdin read cannot be safely killed by Rust. Dropping the
//! client does not wait for such a reader: final cleanup belongs to the plugin
//! process lifecycle, not to reconnecting clients inside one process.
use crate::api::{Aid, ApiResult, KernelError, Notice, Request};
use crate::protocol::{KernelFrame, PROTOCOL_VERSION, PluginFrame, read_frame, write_frame};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, BufRead, BufReader, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_READER_FRAMES: usize = 128;
const MAX_RESPONSES: usize = 1024;
const MAX_NOTICES: usize = 1024;

type ReaderFrame = Result<KernelFrame, KernelError>;

pub struct Client {
    incoming: mpsc::Receiver<ReaderFrame>,
    reader_thread: Option<JoinHandle<()>>,
    reader_stopping: Arc<AtomicBool>,
    writer: Stdout,
    aid: Aid,
    generation: u64,
    next_id: u64,
    responses: BTreeMap<u64, ApiResult>,
    notices: VecDeque<Notice>,
    terminal: Option<KernelError>,
}
impl Client {
    /// Complete the activation handshake before starting the background reader.
    /// This is the only blocking connection setup; API traffic starts after Active.
    pub fn connect(name: &str) -> Result<Self, KernelError> {
        let mut reader = BufReader::new(io::stdin());
        let mut writer = io::stdout();
        write_frame(
            &mut writer,
            &PluginFrame::Ready {
                protocol_version: PROTOCOL_VERSION,
                name: name.into(),
            },
        )
        .map_err(io_error)?;
        let frame = receive(&mut reader)?;
        let KernelFrame::Welcome {
            protocol_version,
            aid,
            generation,
        } = frame
        else {
            return Err(KernelError::new("protocol_error", "expected welcome"));
        };
        if protocol_version != PROTOCOL_VERSION {
            return Err(KernelError::new("protocol_error", "version mismatch"));
        }
        write_frame(&mut writer, &PluginFrame::Activated).map_err(io_error)?;
        if !matches!(receive(&mut reader)?, KernelFrame::Active) {
            return Err(KernelError::new(
                "protocol_error",
                "expected active confirmation",
            ));
        }
        // Move this exact BufReader, including any bytes read ahead during the
        // handshake. Creating another stdin reader here would lose those bytes.
        let reader_stopping = Arc::new(AtomicBool::new(false));
        let (incoming, reader_thread) = spawn_reader(reader, Arc::clone(&reader_stopping))?;
        Ok(Self {
            incoming,
            reader_thread: Some(reader_thread),
            reader_stopping,
            writer,
            aid,
            generation,
            next_id: 1,
            responses: BTreeMap::new(),
            notices: VecDeque::new(),
            terminal: None,
        })
    }
    pub fn aid(&self) -> Aid {
        self.aid
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn send(&mut self, request: Request) -> Result<u64, KernelError> {
        if let Some(error) = &self.terminal {
            return Err(error.clone());
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| KernelError::new("limit_exceeded", "request IDs exhausted"))?;
        if let Err(error) = write_frame(&mut self.writer, &PluginFrame::Request { id, request }) {
            // write_frame rejects an oversized request before touching stdout.
            // That local validation error must not poison an otherwise live client.
            if error.kind() == io::ErrorKind::InvalidInput {
                return Err(io_error(error));
            }
            return self.fail(io_error(error));
        }
        Ok(id)
    }
    pub fn request(&mut self, request: Request) -> Result<Value, KernelError> {
        let id = self.send(request)?;
        self.wait(id)
    }
    pub fn wait(&mut self, id: u64) -> Result<Value, KernelError> {
        loop {
            if let Some(result) = self.take_response(id) {
                return result.into_result();
            }
            // recv_timeout with a practically unbounded wait blocks, rather
            // than repeatedly testing stdin. Other responses/notices stay queued.
            self.poll(Duration::MAX)?;
        }
    }
    pub fn next_notice(&mut self) -> Result<Notice, KernelError> {
        loop {
            if let Some(notice) = self.take_notice() {
                return Ok(notice);
            }
            self.poll(Duration::MAX)?;
        }
    }

    /// Receive and route at most one protocol frame.
    ///
    /// Returns `true` after queuing one response or notice, `false` if the wait
    /// expires without a frame. `Duration::ZERO` is a nonblocking check. EOF,
    /// malformed input, shutdown, abnormal reader exit and queue overflow become
    /// sticky errors. Already queued responses/notices remain available through
    /// their `take_*` methods after an error; no unrelated item is consumed.
    pub fn poll(&mut self, wait: Duration) -> Result<bool, KernelError> {
        if let Some(error) = &self.terminal {
            return Err(error.clone());
        }
        let frame = match self.incoming.recv_timeout(wait) {
            Ok(frame) => frame,
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(false),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return self.fail(KernelError::new(
                    "reader_failed",
                    "SDK reader stopped without a terminal frame",
                ));
            }
        };
        match frame {
            Err(error) => self.fail(error),
            Ok(KernelFrame::Response { id, result }) => {
                if self.responses.contains_key(&id) {
                    return self.fail(KernelError::new(
                        "protocol_error",
                        "duplicate response ID already queued",
                    ));
                }
                if self.responses.len() >= MAX_RESPONSES {
                    return self.fail(KernelError::new(
                        "limit_exceeded",
                        "plugin response queue full",
                    ));
                }
                self.responses.insert(id, result);
                Ok(true)
            }
            Ok(KernelFrame::Notice { notice }) => {
                if self.notices.len() >= MAX_NOTICES {
                    return self.fail(KernelError::new(
                        "limit_exceeded",
                        "plugin notice queue full",
                    ));
                }
                self.notices.push_back(notice);
                Ok(true)
            }
            Ok(KernelFrame::Shutdown { reason }) => self.fail(KernelError::new("shutdown", reason)),
            Ok(KernelFrame::Welcome { .. } | KernelFrame::Active) => {
                self.fail(KernelError::new("protocol_error", "duplicate handshake"))
            }
        }
    }

    /// Take only this request's queued response, without polling or waiting.
    pub fn take_response(&mut self, id: u64) -> Option<ApiResult> {
        self.responses.remove(&id)
    }

    /// Take the oldest queued notice, without polling or waiting.
    pub fn take_notice(&mut self) -> Option<Notice> {
        self.notices.pop_front()
    }

    #[cfg(test)]
    pub(crate) fn disconnected_test_client() -> Self {
        let (_, incoming) = mpsc::sync_channel(1);
        Self {
            incoming,
            reader_thread: None,
            reader_stopping: Arc::new(AtomicBool::new(false)),
            writer: io::stdout(),
            aid: Aid(7),
            generation: 1,
            next_id: 1,
            responses: BTreeMap::new(),
            notices: VecDeque::new(),
            terminal: None,
        }
    }

    fn fail<T>(&mut self, error: KernelError) -> Result<T, KernelError> {
        self.reader_stopping.store(true, Ordering::Release);
        self.terminal = Some(error.clone());
        Err(error)
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.reader_stopping.store(true, Ordering::Release);
        if let Some(reader) = self.reader_thread.take() {
            if reader.is_finished() {
                let _ = reader.join();
            }
            // Otherwise detach: joining a thread blocked on ordinary stdin (or
            // its bounded send) would hang the plugin. Dropping incoming below
            // wakes blocked sends; process termination closes blocked stdin.
        }
    }
}

// Kept generic only to test framing/EOF without taking ownership of real stdin.
// The production reader is the original BufReader<Stdin> from connect().
fn spawn_reader<R: BufRead + Send + 'static>(
    mut reader: R,
    stopping: Arc<AtomicBool>,
) -> Result<(mpsc::Receiver<ReaderFrame>, JoinHandle<()>), KernelError> {
    let (sender, receiver) = mpsc::sync_channel(MAX_READER_FRAMES);
    let handle = thread::Builder::new()
        .name("ithink-sdk-reader".into())
        .spawn(move || {
            loop {
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                let frame = receive(&mut reader);
                let terminal = matches!(
                    &frame,
                    Err(_)
                        | Ok(KernelFrame::Shutdown { .. }
                            | KernelFrame::Welcome { .. }
                            | KernelFrame::Active)
                );
                // Backpressure bounds queued frames; dropping Client drops the
                // receiver and releases a sender waiting for capacity.
                if sender.send(frame).is_err() || terminal {
                    return;
                }
            }
        })
        .map_err(|error| KernelError::new("reader_start_failed", error.to_string()))?;
    Ok((receiver, handle))
}
fn receive<R: BufRead>(reader: &mut R) -> Result<KernelFrame, KernelError> {
    let bytes = read_frame(reader)
        .map_err(io_error)?
        .ok_or_else(|| KernelError::new("disconnected", "kernel closed connection"))?;
    serde_json::from_slice(&bytes).map_err(|e| KernelError::new("protocol_error", e.to_string()))
}
fn io_error(e: io::Error) -> KernelError {
    KernelError::new("transport_error", e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::TaskId;
    use serde_json::json;
    use std::io::Cursor;

    fn client_with_receiver(incoming: mpsc::Receiver<ReaderFrame>) -> Client {
        Client {
            incoming,
            reader_thread: None,
            reader_stopping: Arc::new(AtomicBool::new(false)),
            writer: io::stdout(),
            aid: Aid(7),
            generation: 2,
            next_id: 1,
            responses: BTreeMap::new(),
            notices: VecDeque::new(),
            terminal: None,
        }
    }
    fn channel_client() -> (mpsc::SyncSender<ReaderFrame>, Client) {
        let (sender, receiver) = mpsc::sync_channel(MAX_READER_FRAMES);
        (sender, client_with_receiver(receiver))
    }
    fn response(id: u64) -> ReaderFrame {
        Ok(KernelFrame::Response {
            id,
            result: ApiResult::Ok(json!(id)),
        })
    }
    fn notice(task: u64) -> ReaderFrame {
        Ok(KernelFrame::Notice {
            notice: Notice::TaskAssigned { task: TaskId(task) },
        })
    }

    #[test]
    fn zero_wait_is_empty_without_consuming_queues() {
        let (_sender, mut client) = channel_client();
        assert!(!client.poll(Duration::ZERO).unwrap());
        assert!(client.take_notice().is_none());
        assert!(client.take_response(1).is_none());
    }

    #[test]
    fn poll_routes_at_most_one_frame() {
        let (sender, mut client) = channel_client();
        sender.send(response(1)).unwrap();
        sender.send(notice(2)).unwrap();
        assert!(client.poll(Duration::ZERO).unwrap());
        assert!(client.take_notice().is_none());
        assert_eq!(client.take_response(1), Some(ApiResult::Ok(json!(1))));
        assert!(client.poll(Duration::ZERO).unwrap());
        assert_eq!(
            client.take_notice(),
            Some(Notice::TaskAssigned { task: TaskId(2) })
        );
    }

    #[test]
    fn wait_preserves_unrelated_responses_and_notices() {
        let (sender, mut client) = channel_client();
        sender.send(notice(7)).unwrap();
        sender.send(response(2)).unwrap();
        sender.send(response(1)).unwrap();
        assert_eq!(client.wait(1).unwrap(), json!(1));
        assert_eq!(client.take_response(2), Some(ApiResult::Ok(json!(2))));
        assert_eq!(
            client.take_notice(),
            Some(Notice::TaskAssigned { task: TaskId(7) })
        );
    }

    #[test]
    fn next_notice_preserves_responses_and_notice_order() {
        let (sender, mut client) = channel_client();
        sender.send(response(9)).unwrap();
        sender.send(notice(1)).unwrap();
        sender.send(notice(2)).unwrap();
        assert_eq!(
            client.next_notice().unwrap(),
            Notice::TaskAssigned { task: TaskId(1) }
        );
        assert_eq!(
            client.next_notice().unwrap(),
            Notice::TaskAssigned { task: TaskId(2) }
        );
        assert_eq!(client.take_response(9), Some(ApiResult::Ok(json!(9))));
    }

    #[test]
    fn transport_failure_is_sticky_without_losing_buffered_items() {
        let (sender, mut client) = channel_client();
        sender.send(response(1)).unwrap();
        sender.send(notice(2)).unwrap();
        sender
            .send(Err(KernelError::new("transport_error", "broken pipe")))
            .unwrap();
        assert!(client.poll(Duration::ZERO).unwrap());
        assert!(client.poll(Duration::ZERO).unwrap());
        assert_eq!(
            client.poll(Duration::ZERO).unwrap_err().code,
            "transport_error"
        );
        assert_eq!(
            client.poll(Duration::ZERO).unwrap_err().message,
            "broken pipe"
        );
        assert_eq!(client.wait(1).unwrap(), json!(1));
        assert_eq!(
            client.next_notice().unwrap(),
            Notice::TaskAssigned { task: TaskId(2) }
        );
        assert_eq!(client.wait(99).unwrap_err().code, "transport_error");
    }

    #[test]
    fn unexpected_reader_exit_is_reported() {
        let (sender, mut client) = channel_client();
        drop(sender);
        assert_eq!(
            client.poll(Duration::ZERO).unwrap_err().code,
            "reader_failed"
        );
    }

    #[test]
    fn shutdown_and_duplicate_handshake_are_terminal() {
        for frame in [
            KernelFrame::Shutdown {
                reason: "unloaded".into(),
            },
            KernelFrame::Active,
        ] {
            let (sender, mut client) = channel_client();
            sender.send(Ok(frame)).unwrap();
            let error = client.poll(Duration::ZERO).unwrap_err();
            assert!(matches!(error.code.as_str(), "shutdown" | "protocol_error"));
            assert_eq!(client.poll(Duration::ZERO).unwrap_err(), error);
        }
    }

    #[test]
    fn duplicate_response_does_not_overwrite_queued_result() {
        let (sender, mut client) = channel_client();
        sender.send(response(1)).unwrap();
        sender
            .send(Ok(KernelFrame::Response {
                id: 1,
                result: ApiResult::Ok(json!("replacement")),
            }))
            .unwrap();
        client.poll(Duration::ZERO).unwrap();
        assert_eq!(
            client.poll(Duration::ZERO).unwrap_err().code,
            "protocol_error"
        );
        assert_eq!(client.take_response(1), Some(ApiResult::Ok(json!(1))));
    }

    #[test]
    fn response_queue_is_bounded_and_keeps_existing_results() {
        let (sender, mut client) = channel_client();
        for id in 1..=MAX_RESPONSES as u64 {
            sender.send(response(id)).unwrap();
            client.poll(Duration::ZERO).unwrap();
        }
        sender.send(response(MAX_RESPONSES as u64 + 1)).unwrap();
        assert_eq!(
            client.poll(Duration::ZERO).unwrap_err().code,
            "limit_exceeded"
        );
        assert_eq!(client.responses.len(), MAX_RESPONSES);
        assert_eq!(client.take_response(1), Some(ApiResult::Ok(json!(1))));
    }

    #[test]
    fn notice_queue_is_bounded_and_keeps_existing_notices() {
        let (sender, mut client) = channel_client();
        for task in 0..MAX_NOTICES as u64 {
            sender.send(notice(task)).unwrap();
            client.poll(Duration::ZERO).unwrap();
        }
        sender.send(notice(MAX_NOTICES as u64)).unwrap();
        assert_eq!(
            client.poll(Duration::ZERO).unwrap_err().code,
            "limit_exceeded"
        );
        assert_eq!(client.notices.len(), MAX_NOTICES);
        assert_eq!(
            client.take_notice(),
            Some(Notice::TaskAssigned { task: TaskId(0) })
        );
    }

    fn client_with_bytes(bytes: Vec<u8>) -> Client {
        let stopping = Arc::new(AtomicBool::new(false));
        let (incoming, reader) = spawn_reader(Cursor::new(bytes), Arc::clone(&stopping)).unwrap();
        let mut client = client_with_receiver(incoming);
        client.reader_thread = Some(reader);
        client.reader_stopping = stopping;
        client
    }

    #[test]
    fn reader_reports_eof_after_delivering_complete_frames() {
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &KernelFrame::Response {
                id: 1,
                result: ApiResult::Ok(json!(1)),
            },
        )
        .unwrap();
        let mut client = client_with_bytes(bytes);
        assert!(client.poll(Duration::from_secs(1)).unwrap());
        assert_eq!(client.take_response(1), Some(ApiResult::Ok(json!(1))));
        assert_eq!(
            client.poll(Duration::from_secs(1)).unwrap_err().code,
            "disconnected"
        );
    }

    #[test]
    fn reader_reports_invalid_json_and_incomplete_frames() {
        let mut invalid = client_with_bytes(b"not json\n".to_vec());
        assert_eq!(
            invalid.poll(Duration::from_secs(1)).unwrap_err().code,
            "protocol_error"
        );
        let mut partial = client_with_bytes(b"{\"type\":".to_vec());
        assert_eq!(
            partial.poll(Duration::from_secs(1)).unwrap_err().code,
            "transport_error"
        );
    }

    #[test]
    fn background_frame_queue_is_bounded() {
        let (sender, _client) = channel_client();
        for id in 1..=MAX_READER_FRAMES as u64 {
            sender.try_send(response(id)).unwrap();
        }
        assert!(matches!(
            sender.try_send(response(999)),
            Err(mpsc::TrySendError::Full(_))
        ));
    }
}
