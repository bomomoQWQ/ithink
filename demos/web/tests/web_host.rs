use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Host {
    child: Child,
    origin: String,
    authority: String,
}
impl Host {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ithink-web"))
            .args(["--port", "0", "--offline", "--no-persist"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let _ = tx.send(line);
            }
            for _ in reader.lines() {}
        });
        let line = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(line) => line,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("web host did not start: {error}");
            }
        };
        let origin = line
            .trim()
            .strip_prefix("ITHINK_WEB ")
            .expect("host startup url")
            .to_string();
        let authority = origin.strip_prefix("http://").unwrap().to_string();
        Self {
            child,
            origin,
            authority,
        }
    }
    fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Vec<u8>, String) {
        let body = body
            .map(|v| serde_json::to_vec(&v).unwrap())
            .unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.authority,
            self.origin,
            body.len()
        );
        let mut bytes = request.into_bytes();
        bytes.extend_from_slice(&body);
        self.raw(&bytes)
    }
    fn raw(&self, bytes: &[u8]) -> (u16, Vec<u8>, String) {
        let mut socket = TcpStream::connect(&self.authority).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket.write_all(bytes).unwrap();
        let mut response = vec![];
        socket.read_to_end(&mut response).unwrap();
        let header_end = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8(response[..header_end].to_vec()).unwrap();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, response[header_end + 4..].to_vec(), headers)
    }
    fn json(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let (status, body, _) = self.request(method, path, body);
        (status, serde_json::from_slice(&body).unwrap())
    }
    fn create(&self, prompt: &str, scenario: &str) -> u64 {
        let (status, value) = self.json(
            "POST",
            "/api/tasks",
            Some(json!({"prompt":prompt,"scenario":scenario})),
        );
        assert_eq!(status, 201, "{value}");
        value["task"].as_u64().unwrap()
    }
    fn state(&self) -> Value {
        let (status, value) = self.json("GET", "/api/state", None);
        assert_eq!(status, 200);
        value
    }
    fn ended(&self, id: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = self.state();
            let task = state["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == id)
                .unwrap()
                .clone();
            if task["state"] != "running" {
                return task;
            }
            assert!(Instant::now() < deadline, "web task timed out");
            std::thread::yield_now();
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.json("POST", "/api/shutdown", Some(json!({})));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => std::thread::yield_now(),
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn web_assets_api_and_real_loop_are_wired_end_to_end() {
    let host = Host::start();
    for (path, expected) in [
        ("/", "text/html"),
        ("/app.js", "text/javascript"),
        ("/style.css", "text/css"),
    ] {
        let (status, body, headers) = host.request("GET", path, None);
        assert_eq!(status, 200);
        assert!(!body.is_empty());
        assert!(headers.contains(expected));
        assert!(headers.contains("nosniff"));
    }
    let state = host.state();
    assert_eq!(state["mode"], "offline");
    assert_eq!(state["tasks"], json!([]));
    assert_eq!(state["agents"]["loop"], true);
    let id = host.create("帮我计算 17 + 25", "auto");
    let task = host.ended(id);
    assert_eq!(task["state"], "completed");
    assert_eq!(task["result"]["content"]["answer"]["payload"]["sum"], 42);
    assert_eq!(task["context"].as_array().unwrap().len(), 4);
    assert!(
        task["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"]["kind"] == "tool_finished")
    );
    let id = host.create("<script>alert('x')</script>你好", "auto");
    let task = host.ended(id);
    assert!(
        task["result"]["content"]
            .as_str()
            .unwrap()
            .contains("尚未接入真实 LLM")
    );
    assert_eq!(task["prompt"], "<script>alert('x')</script>你好");
}
#[test]
fn waiting_task_does_not_block_new_work_and_can_be_cancelled() {
    let host = Host::start();
    let waiting = host.create("等待一下", "hold_model");
    let fast = host.create("-7 + 9", "auto");
    assert_eq!(host.ended(fast)["state"], "completed");
    assert!(
        host.state()["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == waiting && t["state"] == "running")
    );
    let (status, _) = host.json(
        "POST",
        &format!("/api/tasks/{waiting}/cancel"),
        Some(json!({})),
    );
    assert_eq!(status, 200);
    assert_eq!(host.ended(waiting)["state"], "cancelled");
    let (status, value) = host.json("POST", "/api/tasks/999999/cancel", Some(json!({})));
    assert_eq!(status, 404);
    assert_eq!(value["error"]["code"], "not_found");
}
#[test]
fn huge_lengths_and_partial_bodies_are_bounded_without_hanging_shutdown() {
    let mut host = Host::start();
    let huge = format!(
        "POST /api/tasks HTTP/1.1\r\nHost: {}\r\nContent-Length: 999999999999\r\nConnection: close\r\n\r\n",
        host.authority
    );
    assert_eq!(host.raw(huge.as_bytes()).0, 413);
    let duplicate = format!(
        "POST /api/tasks HTTP/1.1\r\nHost: {}\r\nContent-Length: 2\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
        host.authority
    );
    assert_eq!(host.raw(duplicate.as_bytes()).0, 400);
    let mut partial = TcpStream::connect(&host.authority).unwrap();
    partial.write_all(format!("POST /api/tasks HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{",host.authority,host.origin).as_bytes()).unwrap();
    let start = Instant::now();
    assert_eq!(host.json("POST", "/api/shutdown", Some(json!({}))).0, 200);
    loop {
        if let Some(status) = host.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "shutdown hung on partial body"
        );
        std::thread::yield_now();
    }
}

#[test]
fn browser_style_split_post_body_waits_for_data_instead_of_resetting_connection() {
    let host = Host::start();
    let body = serde_json::to_vec(&json!({"prompt":"17 + 25","scenario":"auto"})).unwrap();
    let headers = format!(
        "POST /api/tasks HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        host.authority,
        host.origin,
        body.len()
    );
    let mut socket = TcpStream::connect(&host.authority).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket.write_all(headers.as_bytes()).unwrap();
    socket.write_all(&body[..1]).unwrap();
    // Real browser headers/body can land in different packets. Inherited
    // nonblocking accept on Windows used to close/reset this before the body.
    std::thread::sleep(Duration::from_millis(75));
    socket.write_all(&body[1..]).unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 201 "), "{response}");
}

#[test]
fn cross_site_links_can_open_shell_but_cannot_read_or_mutate_task_api() {
    let host = Host::start();
    for site in ["cross-site", "same-site"] {
        for path in ["/", "/index.html"] {
            let (status, body, headers) = host.raw(format!(
                "GET {path} HTTP/1.1\r\nHost: {}\r\nOrigin: http://other.example\r\nSec-Fetch-Site: {site}\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\nConnection: close\r\n\r\n", host.authority
            ).as_bytes());
            assert_eq!(status, 200);
            assert!(headers.contains("text/html"));
            assert!(String::from_utf8(body).unwrap().contains("message-form"));
        }
        for path in ["/api/state", "/api/health"] {
            let (status, _, _) = host.raw(format!(
                "GET {path} HTTP/1.1\r\nHost: {}\r\nSec-Fetch-Site: {site}\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\nConnection: close\r\n\r\n", host.authority
            ).as_bytes());
            assert_eq!(status, 403, "cross-site navigation must not exempt APIs");
        }
    }
    let (status, _, _) = host.raw(b"GET / HTTP/1.1\r\nHost: evil.example\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nConnection: close\r\n\r\n");
    assert_eq!(status, 403, "shell still requires the bound loopback Host");
    let (status, _, _) = host.raw(format!(
        "POST /api/tasks HTTP/1.1\r\nHost: {}\r\nOrigin: http://other.example\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}", host.authority
    ).as_bytes());
    assert_eq!(status, 403);
    assert_eq!(host.state()["tasks"], json!([]));
}

#[test]
fn local_mutations_reject_cross_origin_wrong_content_and_invalid_input() {
    let host = Host::start();
    let response=host.raw(format!("POST /api/tasks HTTP/1.1\r\nHost: {}\r\nOrigin: http://evil.example\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",host.authority).as_bytes());
    assert_eq!(response.0, 403);
    let response =
        host.raw(b"GET /api/state HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n");
    assert_eq!(response.0, 403);
    let response=host.raw(format!("POST /api/tasks HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",host.authority,host.origin).as_bytes());
    assert_eq!(response.0, 415);
    let (status, _) = host.json(
        "POST",
        "/api/tasks",
        Some(json!({"prompt":"","scenario":"auto"})),
    );
    assert_eq!(status, 400);
    let (status, _) = host.json(
        "POST",
        "/api/tasks",
        Some(json!({"prompt":"test","scenario":"unsupported"})),
    );
    assert_eq!(status, 400);
    let (status, _) = host.json(
        "POST",
        "/api/tasks",
        Some(json!({"prompt":"hello","program":"cmd.exe"})),
    );
    assert_eq!(status, 400);
    assert_eq!(host.state()["tasks"], json!([]));
}
