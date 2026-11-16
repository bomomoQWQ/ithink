//! A deliberately restricted loopback HTTP/1.1 example, not a public server.
mod web_host;
mod web_settings;
use ithink::{KernelError, TaskId};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use web_host::{NewMessage, NewTask, WebApp};
use web_settings::SettingsUpdate;

const MAX_BODY: usize = 131_072;
const MAX_HEADERS: usize = 8192;
const READ_TIMEOUT: Duration = Duration::from_secs(3);
struct HttpRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}
struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut port = 4173_u16;
    let mut offline = false;
    let mut data_dir = Some(std::path::PathBuf::from(".ithink"));
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => port = args.next().ok_or("missing port")?.parse()?,
            "--offline" => offline = true,
            "--no-persist" => data_dir = None,
            "--data-dir" => data_dir = Some(args.next().ok_or("missing data directory")?.into()),
            "--help" => {
                println!(
                    "ithink-web [--port 4173] [--offline] [--no-persist] [--data-dir PATH] (127.0.0.1 only)"
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    let fixture = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "ithink-loop-fixture.exe"
    } else {
        "ithink-loop-fixture"
    });
    if !fixture.is_file() {
        return Err("missing ithink-loop-fixture; run cargo build --workspace --bins".into());
    }
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))?;
    listener.set_nonblocking(true)?;
    let authority = listener.local_addr()?.to_string();
    let origin = format!("http://{authority}");
    let app = Arc::new(WebApp::start(&fixture, data_dir.as_deref(), offline)?);
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Release))?;
    let (tx, rx) = mpsc::sync_channel::<TcpStream>(32);
    let rx = Arc::new(Mutex::new(rx));
    let mut workers = vec![];
    for index in 0..8 {
        let app = Arc::clone(&app);
        let stop = Arc::clone(&stop);
        let rx = Arc::clone(&rx);
        let authority = authority.clone();
        let origin = origin.clone();
        workers.push(
            std::thread::Builder::new()
                .name(format!("ithink-web-http-{index}"))
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let stream = rx
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .recv_timeout(Duration::from_millis(50));
                        if let Ok(mut stream) = stream {
                            // On Windows an accepted socket can inherit the
                            // nonblocking listener mode. Restore blocking IO so
                            // read timeouts are real deadlines, not immediate
                            // WouldBlock + reset when a browser splits its POST.
                            if stream.set_nonblocking(false).is_err() {
                                continue;
                            }
                            let response = match read_request(&mut stream) {
                                Ok(request)
                                    if request.method == "GET" && request.path == "/api/stream" =>
                                {
                                    match stream_events(
                                        &mut stream,
                                        &request,
                                        &app,
                                        &authority,
                                        &origin,
                                        &stop,
                                    ) {
                                        Ok(()) => continue,
                                        Err(response) => response,
                                    }
                                }
                                Ok(request) => route(request, &app, &authority, &origin, &stop),
                                Err((status, message)) => error(status, "invalid_http", message),
                            };
                            let _ = write_response(&mut stream, response);
                        }
                    }
                })?,
        );
    }
    println!("ITHINK_WEB {origin}");
    println!("俺寻思 Agent 工作台；仅监听本机。在设置中配置模型，Ctrl+C 退出。");
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = tx.try_send(stream);
            } // overload closes the new socket
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => {
                stop.store(true, Ordering::Release);
                eprintln!("web accept: {e}");
                break;
            }
        }
    }
    drop(tx);
    drop(listener);
    for worker in workers {
        let _ = worker.join();
    }
    drop(app);
    Ok(())
}

/// No unbounded read_line/read_to_end, no keep-alive, chunks or request drain on
/// rejection. Deadline is absolute, so drip-fed bytes cannot renew it forever.
fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, (u16, &'static str)> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            if index + 4 > MAX_HEADERS {
                return Err((431, "请求头超过 8 KiB"));
            }
            break index + 4;
        }
        if bytes.len() >= MAX_HEADERS {
            return Err((431, "请求头超过 8 KiB"));
        }
        let count = read_before(stream, &mut chunk, deadline)?;
        if count == 0 {
            return Err((400, "请求头不完整"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    };
    let text =
        std::str::from_utf8(&bytes[..head_end - 4]).map_err(|_| (400, "请求头必须是有效文本"))?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().ok_or((400, "缺少请求行"))?.split_whitespace();
    let method = first.next().ok_or((400, "缺少方法"))?.to_string();
    let target = first.next().ok_or((400, "缺少路径"))?;
    if !target.starts_with('/') || first.next() != Some("HTTP/1.1") || first.next().is_some() {
        return Err((400, "仅支持本机 HTTP/1.1 路径请求"));
    }
    let path = target.split('?').next().unwrap().to_string();
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or((400, "请求头格式错误"))?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err((400, "请求头名称错误"));
        }
        if value.bytes().any(|b| b < 32 && b != b'\t') {
            return Err((400, "请求头值错误"));
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim().to_string())
            .is_some()
        {
            return Err((400, "不接受重复请求头"));
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err((411, "不支持分块传输"));
    }
    if headers.contains_key("expect") {
        return Err((417, "不支持 Expect 请求"));
    }
    let length = match headers.get("content-length") {
        Some(value) => {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err((400, "Content-Length 错误"));
            }
            value.parse::<u64>().map_err(|_| (413, "正文过大"))?
        }
        None if method == "POST" => return Err((411, "需要 Content-Length")),
        None => 0,
    };
    if length > MAX_BODY as u64 {
        return Err((413, "请求体超过 128 KiB"));
    }
    if method == "GET" && length != 0 {
        return Err((400, "GET 不接受正文"));
    }
    let length = length as usize;
    let mut body = bytes[head_end..].to_vec();
    if body.len() > length {
        return Err((400, "不接受管线或额外正文"));
    }
    while body.len() < length {
        let take = (length - body.len()).min(chunk.len());
        let count = read_before(stream, &mut chunk[..take], deadline)?;
        if count == 0 {
            return Err((400, "正文不完整"));
        }
        body.extend_from_slice(&chunk[..count]);
    }
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}
fn read_before(
    stream: &mut TcpStream,
    bytes: &mut [u8],
    deadline: Instant,
) -> Result<usize, (u16, &'static str)> {
    let timeout = deadline.saturating_duration_since(Instant::now());
    if timeout.is_zero() {
        return Err((408, "请求读取超时"));
    }
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| (400, "读取失败"))?;
    stream.read(bytes).map_err(|e| {
        if matches!(
            e.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) {
            (408, "请求读取超时")
        } else {
            (400, "读取失败")
        }
    })
}
fn route(
    request: HttpRequest,
    app: &WebApp,
    authority: &str,
    origin: &str,
    stop: &AtomicBool,
) -> HttpResponse {
    if request.header("host") != Some(authority) {
        return error(403, "invalid_host", "请使用启动日志中的 127.0.0.1 地址");
    }
    // A cross-site link opens only the public static shell. It must not be
    // rejected like a cross-origin API request: the shell then makes its own
    // same-origin requests. No task data or mutation is exposed by this branch.
    if request.method == "GET" && matches!(request.path.as_str(), "/" | "/index.html") {
        return asset(
            "text/html; charset=utf-8",
            include_bytes!("../frontend/index.html"),
        );
    }
    if request
        .header("sec-fetch-site")
        .is_some_and(|s| s != "same-origin" && s != "none")
        || request.header("origin").is_some_and(|s| s != origin)
    {
        return error(403, "cross_origin", "只允许同源请求");
    }
    if request.method == "GET" {
        return match request.path.as_str() {
            "/" | "/index.html" => asset(
                "text/html; charset=utf-8",
                include_bytes!("../frontend/index.html"),
            ),
            "/app.js" => asset(
                "text/javascript; charset=utf-8",
                include_bytes!("../frontend/app.js"),
            ),
            "/style.css" => asset(
                "text/css; charset=utf-8",
                include_bytes!("../frontend/style.css"),
            ),
            "/api/state" => json_response(200, app.state()),
            "/api/health" => json_response(200, json!({"ok":true,"mode":app.settings()["mode"]})),
            "/api/settings" => json_response(200, app.settings()),
            "/favicon.ico" => HttpResponse {
                status: 204,
                content_type: "image/x-icon",
                body: vec![],
            },
            _ => error(404, "not_found", "接口或资源不存在"),
        };
    }
    if request.method != "POST" {
        return error(405, "method_not_allowed", "仅支持 GET 与 POST");
    }
    if request.header("origin") != Some(origin) {
        return error(403, "cross_origin", "写操作需要精确同源 Origin");
    }
    let content_type = request
        .header("content-type")
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap()
        .trim();
    if !content_type.eq_ignore_ascii_case("application/json") {
        return error(415, "unsupported_media_type", "请求必须为 application/json");
    }
    if matches!(
        request.path.as_str(),
        "/api/settings" | "/api/settings/test"
    ) {
        let input = match serde_json::from_slice::<SettingsUpdate>(&request.body) {
            Ok(input) => input,
            Err(_) => return error(400, "invalid_settings", "设置格式错误"),
        };
        return if request.path == "/api/settings/test" {
            outcome(200, app.test(input))
        } else {
            outcome(200, app.configure(input))
        };
    }
    if let Some(id) = request
        .path
        .strip_prefix("/api/conversations/")
        .and_then(|p| p.strip_suffix("/messages"))
        .and_then(|id| id.parse::<u64>().ok())
    {
        let input = match serde_json::from_slice::<NewMessage>(&request.body) {
            Ok(input) => input,
            Err(_) => return error(400, "invalid_message", "消息格式错误"),
        };
        return outcome(201, app.message(id, input));
    }
    if request.path == "/api/tasks" {
        let input = match serde_json::from_slice::<NewTask>(&request.body) {
            Ok(input) => input,
            Err(_) => return error(400, "invalid_json", "任务格式错误"),
        };
        return outcome(201, app.create(input));
    }
    if serde_json::from_slice::<Value>(&request.body).ok() != Some(json!({})) {
        return error(400, "invalid_json", "请求体必须为空 JSON 对象");
    }
    if request.path == "/api/conversations" {
        return outcome(201, app.conversation());
    }
    if request.path == "/api/shutdown" {
        stop.store(true, Ordering::Release);
        return json_response(200, json!({"ok":true}));
    }
    if let Some(id) = request
        .path
        .strip_prefix("/api/tasks/")
        .and_then(|p| p.strip_suffix("/cancel"))
        .and_then(|id| id.parse::<u64>().ok())
    {
        return outcome(200, app.cancel(TaskId(id)));
    }
    error(404, "not_found", "接口不存在")
}
fn stream_events(
    stream: &mut TcpStream,
    request: &HttpRequest,
    app: &WebApp,
    authority: &str,
    origin: &str,
    stop: &AtomicBool,
) -> Result<(), HttpResponse> {
    if request.header("host") != Some(authority) {
        return Err(error(
            403,
            "invalid_host",
            "请使用启动日志中的 127.0.0.1 地址",
        ));
    }
    if request
        .header("sec-fetch-site")
        .is_some_and(|site| site != "same-origin" && site != "none")
        || request
            .header("origin")
            .is_some_and(|value| value != origin)
    {
        return Err(error(403, "cross_origin", "只允许同源请求"));
    }
    let receiver = app.subscribe_stream();
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-store\r\nConnection: keep-alive\r\nTransfer-Encoding: chunked\r\nX-Accel-Buffering: no\r\nX-Content-Type-Options: nosniff\r\n\r\n";
    if stream.write_all(headers.as_bytes()).is_err() {
        return Ok(());
    }
    let _ = stream.flush();
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut idle = 0_u32;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(value) => {
                idle = 0;
                let Ok(payload) = serde_json::to_string(&value) else {
                    continue;
                };
                let event = format!("data: {payload}\n\n");
                if write_sse_chunk(stream, event.as_bytes()).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                idle += 1;
                if idle >= 10 {
                    idle = 0;
                    if write_sse_chunk(stream, b": keep-alive\n\n").is_err() {
                        break;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = write_sse_chunk(stream, b"");
    Ok(())
}

fn write_sse_chunk(stream: &mut TcpStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        stream.write_all(b"0\r\n\r\n")?;
        return stream.flush();
    }
    stream.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn outcome(status: u16, result: Result<Value, KernelError>) -> HttpResponse {
    match result {
        Ok(v) => json_response(status, v),
        Err(e) => error(
            match e.code.as_str() {
                "not_found" => 404,
                "task_limit" | "conversation_limit" | "busy" => 409,
                "unavailable" => 503,
                "timeout" => 504,
                _ => 400,
            },
            &e.code,
            &e.message,
        ),
    }
}
fn error(status: u16, code: &str, message: &str) -> HttpResponse {
    json_response(status, json!({"error":{"code":code,"message":message}}))
}
fn json_response(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        content_type: "application/json; charset=utf-8",
        body: serde_json::to_vec(&value).unwrap(),
    }
}
fn asset(content_type: &'static str, bytes: &[u8]) -> HttpResponse {
    HttpResponse {
        status: 200,
        content_type,
        body: bytes.to_vec(),
    }
}
fn write_response(stream: &mut TcpStream, response: HttpResponse) -> io::Result<()> {
    let text = match response.status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let headers = format!(
        "HTTP/1.1 {} {text}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'\r\nReferrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    let mut bytes = headers.into_bytes();
    bytes.extend_from_slice(&response.body);
    let mut remaining = bytes.as_slice();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !remaining.is_empty() {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "response timeout"));
        }
        stream.set_write_timeout(Some(timeout))?;
        let n = stream.write(remaining)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "response closed"));
        }
        remaining = &remaining[n..];
    }
    Ok(())
}
