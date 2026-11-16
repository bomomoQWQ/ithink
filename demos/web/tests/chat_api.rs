//! Real local HTTP -> process provider -> mocked compatible API tests.
//! No external model, key or network service is used.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SECRET: &str = "chat-test-secret-should-never-leak";
const WAIT: Duration = Duration::from_secs(8);

struct Host {
    child: Child,
    origin: String,
    authority: String,
}
impl Host {
    fn start(directory: Option<&Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ithink-web"));
        command.args(["--port", "0"]);
        if let Some(directory) = directory {
            command.arg("--data-dir").arg(directory);
        } else {
            command.arg("--no-persist");
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let _ = tx.send(line);
            }
            for _ in reader.lines() {}
        });
        let line = match rx.recv_timeout(WAIT) {
            Ok(line) => line,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("chat host startup failed: {error}");
            }
        };
        let origin = line
            .trim()
            .strip_prefix("ITHINK_WEB ")
            .expect("host URL line")
            .to_string();
        let authority = origin.strip_prefix("http://").unwrap().to_string();
        Self {
            child,
            origin,
            authority,
        }
    }
    fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let bytes = body
            .map(|body| serde_json::to_vec(&body).unwrap())
            .unwrap_or_default();
        let headers = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.authority,
            self.origin,
            bytes.len()
        );
        let mut socket = TcpStream::connect(&self.authority).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        socket.write_all(headers.as_bytes()).unwrap();
        socket.write_all(&bytes).unwrap();
        let mut response = vec![];
        socket.read_to_end(&mut response).unwrap();
        let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let status = std::str::from_utf8(&response[..split])
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (
            status,
            serde_json::from_slice(&response[split + 4..]).unwrap(),
        )
    }
    fn state(&self) -> Value {
        let (status, value) = self.request("GET", "/api/state", None);
        assert_eq!(status, 200);
        value
    }
    fn conversation(&self) -> u64 {
        let (status, value) = self.request("POST", "/api/conversations", Some(json!({})));
        assert_eq!(status, 201, "{value}");
        value["conversation"].as_u64().unwrap()
    }
    fn message(&self, conversation: u64, content: &str) -> u64 {
        let (status, value) = self.request(
            "POST",
            &format!("/api/conversations/{conversation}/messages"),
            Some(json!({"content":content})),
        );
        assert_eq!(status, 201, "{value}");
        assert_eq!(value["conversation"], conversation);
        value["task"].as_u64().unwrap()
    }
    fn ended(&self, id: u64) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let state = self.state();
            let task = state["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == id)
                .expect("created task listed")
                .clone();
            if task["state"] != "running" {
                return task;
            }
            assert!(
                Instant::now() < deadline,
                "API conversation task timed out: {task}"
            );
            thread::yield_now();
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.request("POST", "/api/shutdown", Some(json!({})));
        let deadline = Instant::now() + WAIT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::yield_now(),
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone)]
struct Capture {
    headers: String,
    body: Value,
}
struct Mock {
    endpoint: String,
    captures: Arc<Mutex<Vec<Capture>>>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Mock {
    fn start(handler: impl Fn(&Value) -> (u16, Value, String) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let captures = Arc::new(Mutex::new(vec![]));
        let stopping = Arc::new(AtomicBool::new(false));
        let records = Arc::clone(&captures);
        let stop = Arc::clone(&stopping);
        let worker = thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => return,
                };
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(WAIT)).unwrap();
                socket.set_write_timeout(Some(WAIT)).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    let read = reader.read_line(&mut line).unwrap();
                    if read == 0 || line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                assert!(length <= 1_048_576);
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                records.lock().unwrap().push(Capture {
                    headers,
                    body: body.clone(),
                });
                let (status, body, extra_headers) = handler(&body);
                let bytes = serde_json::to_vec(&body).unwrap();
                let headers = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
                    bytes.len()
                );
                let _ = socket.write_all(headers.as_bytes());
                let _ = socket.write_all(&bytes);
            }
        });
        Self {
            endpoint,
            captures,
            stopping,
            worker: Some(worker),
        }
    }
    fn captures(&self) -> Vec<Capture> {
        self.captures.lock().unwrap().clone()
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                assert!(result.is_ok(), "mock provider thread failed");
            }
        }
    }
}
fn reply(content: Value) -> Value {
    json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":content}}],"usage":{"prompt_tokens":10,"completion_tokens":2}})
}
fn settings(endpoint: &str, key: Option<&str>) -> Value {
    let mut value = json!({"mode":"api","endpoint":endpoint,"model":"mock-compatible","system_prompt":"测试系统提示","temperature":null,"max_tokens":128,"timeout_secs":5});
    if let Some(key) = key {
        value["api_key"] = json!(key);
    }
    value
}
fn configure(host: &Host, endpoint: &str) {
    let (status, value) = host.request(
        "POST",
        "/api/settings",
        Some(settings(endpoint, Some(SECRET))),
    );
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["configured"], true);
    assert_eq!(value["api_key_set"], true);
    assert!(!value.to_string().contains(SECRET));
}
struct Temp {
    path: PathBuf,
}
impl Temp {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ithink-chat-api-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self { path }
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        // Verify the exact allocated child before removing only this test's data.
        assert_eq!(self.path.parent(), Some(std::env::temp_dir().as_path()));
        assert!(
            self.path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ithink-chat-api-")
        );
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn api_is_unconfigured_by_default_and_never_silently_uses_offline_reply() {
    let host = Host::start(None);
    let state = host.state();
    assert_eq!(state["mode"], "api");
    assert_eq!(state["provider"]["configured"], false);
    assert_eq!(state["agents"]["model"], false);
    let (status, public) = host.request("GET", "/api/settings", None);
    assert_eq!(status, 200);
    assert!(public.get("api_key").is_none());
    assert_eq!(public["api_key_set"], false);
    let conversation = host.conversation();
    let (status, error) = host.request(
        "POST",
        &format!("/api/conversations/{conversation}/messages"),
        Some(json!({"content":"hello"})),
    );
    assert_eq!(status, 400);
    assert_eq!(error["error"]["code"], "not_configured");
    assert_eq!(host.state()["tasks"], json!([]));
}

#[test]
fn save_and_probe_use_real_auth_but_public_state_and_disk_never_contain_key() {
    let mock = Mock::start(|_| (200, reply(json!("OK")), String::new()));
    let temp = Temp::new();
    let host = Host::start(Some(&temp.path));
    let (status, probe) = host.request(
        "POST",
        "/api/settings/test",
        Some(settings(&mock.endpoint, Some(SECRET))),
    );
    assert_eq!(status, 200, "{probe}");
    assert_eq!(probe["ok"], true);
    assert_eq!(mock.captures().len(), 1);
    let initial_captures = mock.captures();
    let first = &initial_captures[0];
    assert!(
        first
            .headers
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {SECRET}"))
    );
    assert!(first.body["max_tokens"].as_u64().unwrap() <= 32);
    assert_eq!(first.body["stream"], false);
    assert_eq!(
        host.state()["provider"]["configured"],
        false,
        "test must not implicitly save candidate settings"
    );
    configure(&host, &mock.endpoint);
    let (status, public) = host.request("GET", "/api/settings", None);
    assert_eq!(status, 200);
    assert!(public.get("api_key").is_none());
    assert!(!public.to_string().contains(SECRET));
    let disk = std::fs::read_to_string(temp.path.join("provider.json")).unwrap();
    assert!(!disk.contains(SECRET));
    assert!(!disk.contains("api_key"));
    let conversation = host.conversation();
    let task = host.message(conversation, "private auth is not chat content");
    assert_eq!(host.ended(task)["state"], "completed");
    assert!(!host.state().to_string().contains(SECRET));
    let (status, saved) = host.request(
        "POST",
        "/api/settings",
        Some(settings(&mock.endpoint, Some(""))),
    );
    assert_eq!(status, 200);
    assert_eq!(
        saved["api_key_set"], true,
        "blank key retains the existing in-memory key"
    );
    let mut clear = settings(&mock.endpoint, None);
    clear["clear_api_key"] = json!(true);
    let (status, saved) = host.request("POST", "/api/settings", Some(clear));
    assert_eq!(status, 200);
    assert_eq!(saved["api_key_set"], false);
    assert_eq!(saved["configured"], true, "local backend permits empty key");
    let task = host.message(conversation, "after clearing the key");
    assert_eq!(host.ended(task)["state"], "completed");
    let captures = mock.captures();
    assert!(
        !captures
            .last()
            .unwrap()
            .headers
            .to_ascii_lowercase()
            .contains("authorization:")
    );
    assert!(
        !std::fs::read_to_string(temp.path.join("provider.json"))
            .unwrap()
            .contains(SECRET)
    );
}

#[test]
fn real_tool_roundtrip_and_followup_preserve_history_without_crossing_conversations() {
    let mock = Mock::start(|body| {
        let messages = body["messages"].as_array().unwrap();
        let users = messages.iter().filter(|m| m["role"] == "user").count();
        let tools = messages.iter().filter(|m| m["role"] == "tool").count();
        if users == 1 && tools == 0 {
            (
                200,
                json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"add","arguments":"{\"a\":17,\"b\":25}"}}]}}]}),
                String::new(),
            )
        } else if users == 1 {
            let content: Value = serde_json::from_str(
                messages.iter().find(|m| m["role"] == "tool").unwrap()["content"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(content["payload"]["sum"], 42);
            (200, reply(json!("42")), String::new())
        } else {
            (200, reply(json!("followup remembered")), String::new())
        }
    });
    let host = Host::start(None);
    configure(&host, &mock.endpoint);
    let first_conversation = host.conversation();
    let first_task = host.message(first_conversation, "first conversation: add 17 and 25");
    let first = host.ended(first_task);
    assert_eq!(first["state"], "completed");
    assert_eq!(first["result"]["content"], "42");
    let second_task = host.message(first_conversation, "first conversation followup");
    let second = host.ended(second_task);
    assert_eq!(second["state"], "completed");
    assert_eq!(second["result"]["content"], "followup remembered");
    let independent = host.conversation();
    let third_task = host.message(independent, "independent conversation");
    assert_eq!(host.ended(third_task)["state"], "completed");
    let captures = mock.captures();
    assert_eq!(captures.len(), 5);
    let followup = &captures[2].body["messages"];
    let followup = followup.as_array().unwrap();
    assert_eq!(followup.iter().filter(|m| m["role"] == "user").count(), 2);
    assert_eq!(
        followup.iter().filter(|m| m["role"] == "assistant").count(),
        2
    );
    assert_eq!(followup.iter().filter(|m| m["role"] == "tool").count(), 1);
    let call_id = captures[0].body["messages"].as_array().unwrap().len();
    assert_eq!(call_id, 2);
    let inherited_id = captures[1].body["messages"][2]["tool_calls"][0]["id"]
        .as_str()
        .unwrap();
    assert!(inherited_id.starts_with(&format!("t{first_task}-s1-")));
    assert_eq!(followup[2]["tool_calls"][0]["id"], inherited_id);
    let independent_messages = captures[3].body["messages"].as_array().unwrap();
    assert_eq!(
        independent_messages
            .iter()
            .filter(|m| m["role"] == "user")
            .count(),
        1
    );
    assert!(
        !Value::Array(independent_messages.clone())
            .to_string()
            .contains("first conversation")
    );
    let state = host.state();
    let conversations = state["conversations"].as_array().unwrap();
    assert_eq!(
        conversations
            .iter()
            .find(|c| c["id"] == first_conversation)
            .unwrap()["turns"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        conversations
            .iter()
            .find(|c| c["id"] == independent)
            .unwrap()["turns"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn running_chat_rejects_provider_switch_until_task_is_cancelled() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release = Mutex::new(release_rx);
    let mock = Mock::start(move |_| {
        let _ = entered_tx.try_send(());
        let _ = release.lock().unwrap().recv_timeout(Duration::from_secs(5));
        (200, reply(json!("late reply")), String::new())
    });
    let host = Host::start(None);
    configure(&host, &mock.endpoint);
    let conversation = host.conversation();
    let task = host.message(conversation, "keep this request active");
    entered_rx.recv_timeout(WAIT).unwrap();
    let (status, error) = host.request(
        "POST",
        "/api/settings",
        Some(settings(&mock.endpoint, None)),
    );
    assert_eq!(status, 409);
    assert_eq!(error["error"]["code"], "busy");
    let (status, _) = host.request(
        "POST",
        &format!("/api/tasks/{task}/cancel"),
        Some(json!({})),
    );
    assert_eq!(status, 200);
    assert_eq!(host.ended(task)["state"], "cancelled");
    release_tx.send(()).unwrap();
    let (status, _) = host.request(
        "POST",
        "/api/settings",
        Some(settings(&mock.endpoint, None)),
    );
    assert_eq!(status, 200);
}

#[test]
fn remote_http_and_redirect_probe_fail_without_credential_or_body_leaks() {
    let host = Host::start(None);
    let (status, error) = host.request(
        "POST",
        "/api/settings",
        Some(settings("http://remote.example/v1", Some(SECRET))),
    );
    assert_eq!(status, 400);
    assert_eq!(error["error"]["code"], "invalid_provider_config");
    assert!(!error.to_string().contains(SECRET));
    let mock = Mock::start(|_| {
        (
            302,
            json!({"error":{"message":format!("Authorization Bearer {SECRET} https://secret.example/private")}}),
            "Location: http://127.0.0.1:9/private\r\n".into(),
        )
    });
    let (status, error) = host.request(
        "POST",
        "/api/settings/test",
        Some(settings(&mock.endpoint, Some(SECRET))),
    );
    assert_eq!(status, 400);
    assert_eq!(error["error"]["code"], "provider_http_error");
    for secret in [SECRET, "Authorization", "secret.example", "/private"] {
        assert!(!error.to_string().contains(secret));
    }
    assert_eq!(mock.captures().len(), 1);
    assert_eq!(host.state()["provider"]["configured"], false);
}

#[test]
fn cancelling_ten_real_provider_requests_closes_upstream_and_leaves_new_work_available() {
    // Each accepted connection owns its own thread. A blocked/forgotten request
    // must not prevent the mock from accepting the ninth and tenth requests.
    enum Event {
        Holding(usize),
        Closed(usize),
        Normal,
        Error(String),
    }
    struct ConcurrentMock {
        endpoint: String,
        authority: String,
        events: mpsc::Receiver<Event>,
        stopping: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }
    fn read_body(socket: &mut TcpStream, stopping: &AtomicBool) -> std::io::Result<Value> {
        let deadline = Instant::now() + WAIT;
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            if stopping.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "mock request deadline",
                ));
            }
            match socket.read(&mut buffer) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "mock request closed early",
                    ));
                }
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
            if bytes.len() > 1_048_576 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "mock request too large",
                ));
            }
            let Some(split) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&bytes[..split]).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "mock header encoding")
            })?;
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                })
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "mock missing body length")
                })?;
            if length > 1_048_576 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "mock body too large",
                ));
            }
            let start = split + 4;
            if bytes.len() >= start + length {
                return serde_json::from_slice(&bytes[start..start + length]).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "mock invalid body")
                });
            }
        }
    }
    fn connection(
        mut socket: TcpStream,
        index: usize,
        stopping: &AtomicBool,
        events: &mpsc::Sender<Event>,
    ) -> std::io::Result<()> {
        socket.set_nonblocking(false)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        let body = read_body(&mut socket, stopping)?;
        let content = body["messages"]
            .as_array()
            .and_then(|messages| {
                messages
                    .iter()
                    .rev()
                    .find(|message| message["role"] == "user")
            })
            .and_then(|message| message["content"].as_str());
        if content == Some("hold") {
            // Send only headers. The real async provider must await the body;
            // task cancellation must drop that future and close this socket.
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n")?;
            let _ = events.send(Event::Holding(index));
            let deadline = Instant::now() + Duration::from_secs(4);
            let mut byte = [0];
            while !stopping.load(Ordering::Acquire) {
                match socket.read(&mut byte) {
                    Ok(0) => {
                        let _ = events.send(Event::Closed(index));
                        return Ok(());
                    }
                    Ok(_) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "unexpected bytes on held response",
                        ));
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::BrokenPipe
                        ) =>
                    {
                        let _ = events.send(Event::Closed(index));
                        return Ok(());
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) => {}
                    Err(error) => return Err(error),
                }
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "cancel did not close upstream response",
                    ));
                }
            }
            return Ok(());
        }
        if content != Some("normal") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected mock chat content",
            ));
        }
        let response =
            serde_json::to_vec(&reply(json!("normal completed after ten cancellations"))).unwrap();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        );
        socket.write_all(headers.as_bytes())?;
        socket.write_all(&response)?;
        let _ = events.send(Event::Normal);
        Ok(())
    }
    impl ConcurrentMock {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let authority = listener.local_addr().unwrap().to_string();
            let endpoint = format!("http://{authority}/v1");
            let stopping = Arc::new(AtomicBool::new(false));
            let stop = Arc::clone(&stopping);
            let (events_tx, events) = mpsc::channel();
            let worker = thread::spawn(move || {
                let mut connections = vec![];
                for index in 0..16 {
                    let (socket, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(error) => {
                            let _ = events_tx.send(Event::Error(format!("mock accept: {error}")));
                            break;
                        }
                    };
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let stop = Arc::clone(&stop);
                    let events = events_tx.clone();
                    connections.push(thread::spawn(move || {
                        if let Err(error) = connection(socket, index, &stop, &events) {
                            if !stop.load(Ordering::Acquire) {
                                let _ = events.send(Event::Error(format!(
                                    "mock connection {index}: {error}"
                                )));
                            }
                        }
                    }));
                }
                // Never unwrap a join while a failing assertion is already
                // unwinding in the test. Stop + socket deadlines bound cleanup.
                for worker in connections {
                    let _ = worker.join();
                }
            });
            Self {
                endpoint,
                authority,
                events,
                stopping,
                worker: Some(worker),
            }
        }
        fn event(&self) -> Event {
            match self.events.recv_timeout(Duration::from_secs(3)) {
                Ok(Event::Error(error)) => panic!("{error}"),
                Ok(event) => event,
                Err(error) => panic!("provider cancellation event deadline: {error}"),
            }
        }
    }
    impl Drop for ConcurrentMock {
        fn drop(&mut self) {
            self.stopping.store(true, Ordering::Release);
            // Wake blocking accept, then join every bounded connection worker.
            let _ = TcpStream::connect(&self.authority);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    let mock = ConcurrentMock::start();
    let host = Host::start(None);
    let mut config = settings(&mock.endpoint, Some(SECRET));
    // Upstream closure must be caused by cancellation, not the configured HTTP
    // timeout expiring while this test waits for its explicit socket event.
    config["timeout_secs"] = json!(120);
    let (status, result) = host.request("POST", "/api/settings", Some(config));
    assert_eq!(status, 200, "{result}");
    for index in 0..10 {
        let conversation = host.conversation();
        let task = host.message(conversation, "hold");
        assert!(
            matches!(mock.event(), Event::Holding(held) if held == index),
            "expected request {index} to reach the actual provider"
        );
        let (status, result) = host.request(
            "POST",
            &format!("/api/tasks/{task}/cancel"),
            Some(json!({})),
        );
        assert_eq!(status, 200, "{result}");
        assert!(
            matches!(mock.event(), Event::Closed(closed) if closed == index),
            "cancel must close upstream request {index}, not leave a blocked worker"
        );
        assert_eq!(host.ended(task)["state"], "cancelled");
    }
    let conversation = host.conversation();
    let task = host.message(conversation, "normal");
    assert!(matches!(mock.event(), Event::Normal));
    let result = host.ended(task);
    assert_eq!(result["state"], "completed");
    assert_eq!(
        result["result"]["content"],
        "normal completed after ten cancellations"
    );
    assert_eq!(host.state()["tasks"].as_array().unwrap().len(), 11);
}
