//! Optional, non-streaming OpenAI-compatible Chat Completions adapter.
//!
//! This is user-space transport code, not an Agent Loop or kernel policy. It
//! does not log credentials, follow redirects, or expose raw provider errors.
//! `reasoning_content` is retained for subsequent tool rounds in the private
//! assistant content object; a UI must render only its `text` field by default.
use crate::agent_loop::{LoopMessage, ModelInput, ModelReply, ToolCall};
use crate::api::{KernelError, TaskId};
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::net::IpAddr;
use std::time::{Duration, Instant};

const MAX_RESPONSE_BYTES: usize = 1_048_576;
const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_TOOL_ID_BYTES: usize = 256;

/// Deliberately does not implement Debug: `api_key` is a secret.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub system_prompt: String,
    pub temperature: Option<f64>,
    pub max_tokens: u32,
    pub timeout_secs: u64,
}
impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
            api_key: String::new(),
            system_prompt: "你是一位有帮助的通用助手。请准确、清晰地回答用户；需要工具时调用已提供的工具，不要编造工具执行结果。".into(),
            temperature: None,
            max_tokens: 4096,
            timeout_secs: 60,
        }
    }
}
impl ProviderConfig {
    pub fn validate(&self) -> Result<(), KernelError> {
        completion_endpoint(&self.endpoint)?;
        if self.model.trim().is_empty()
            || self.model.chars().count() > 256
            || self.model.chars().any(char::is_control)
        {
            return Err(config_error("模型名称必须为 1–256 个非控制字符"));
        }
        if self.api_key.len() > 8192 || self.api_key.chars().any(char::is_control) {
            return Err(config_error("API Key 超过 8192 字节或包含控制字符"));
        }
        if !(1..=65_536).contains(&self.max_tokens) {
            return Err(config_error("max_tokens 必须在 1–65536 之间"));
        }
        if !(1..=120).contains(&self.timeout_secs) {
            return Err(config_error("请求超时必须在 1–120 秒之间"));
        }
        if self
            .temperature
            .is_some_and(|v| !v.is_finite() || !(0.0..=2.0).contains(&v))
        {
            return Err(config_error("temperature 必须为 0–2 的有限数值"));
        }
        if self.system_prompt.chars().count() > 16_000 {
            return Err(config_error("系统提示词不能超过 16000 个字符"));
        }
        Ok(())
    }
}

/// Generate one complete assistant response. No SSE is requested. HTTP and
/// parse errors use fixed safe messages, never provider bodies or request URLs.
pub fn generate(config: &ProviderConfig, input: &ModelInput) -> Result<ModelReply, KernelError> {
    config.validate()?;
    let payload = request_payload(config, input, config.max_tokens)?;
    let response = send(config, &payload)?;
    parse_reply(&response, input, false)
}

/// Async transport variant for cancellable provider tasks. Dropping this future
/// (for example via JoinHandle::abort) drops the in-flight HTTP request/body; no
/// blocking worker is spawned. Payload conversion and response validation are
/// identical to the synchronous adapter.
pub async fn generate_async(
    config: &ProviderConfig,
    input: &ModelInput,
) -> Result<ModelReply, KernelError> {
    config.validate()?;
    let payload = request_payload(config, input, config.max_tokens)?;
    let response = send_async(config, &payload).await?;
    parse_reply(&response, input, false)
}

/// Makes a real, small chat request. A length-limited reply proves reachability
/// too: reasoning models may spend the 32-token probe budget before final text.
/// The returned message is a fixed local description, not model/private text.
pub fn test_connection(config: &ProviderConfig) -> Result<Value, KernelError> {
    config.validate()?;
    let mut probe = config.clone();
    probe.system_prompt = "这是连接测试，请仅回复 OK。".into();
    probe.temperature = None;
    let input = ModelInput {
        task: TaskId(0),
        step: 0,
        context: vec![crate::agent_loop::input_item(json!("仅回复 OK。"))?],
        tools: vec![],
    };
    let payload = request_payload(&probe, &input, config.max_tokens.min(32))?;
    let start = Instant::now();
    let response = send(&probe, &payload)?;
    parse_reply(&response, &input, true)?;
    let truncated = response["choices"][0]["finish_reason"] == "length";
    Ok(json!({
        "ok": true,
        "latency_ms": start.elapsed().as_millis() as u64,
        "model": config.model,
        "message": if truncated { "接口已响应，连接测试通过（已达到小额测试输出上限）。" } else { "接口已响应，连接测试通过。" }
    }))
}

fn config_error(message: &str) -> KernelError {
    KernelError::new("invalid_provider_config", message)
}
fn invalid_reply() -> KernelError {
    KernelError::new(
        "invalid_model_reply",
        "模型接口未返回有效的 Chat Completions 回复",
    )
}
fn invalid_context() -> KernelError {
    KernelError::new(
        "invalid_model_context",
        "上下文消息或工具配对不能转换为 Chat Completions",
    )
}

/// Only appends /chat/completions; it never guesses /v1 or rewrites a provider
/// path. URL syntax that normalizes traversal, escaped separators or credentials
/// is rejected before parsing, rather than silently changing its meaning.
fn completion_endpoint(endpoint: &str) -> Result<Url, KernelError> {
    if endpoint.len() > 4096
        || endpoint.is_empty()
        || endpoint.trim() != endpoint
        || endpoint.chars().any(char::is_control)
        || endpoint.contains('\\')
    {
        return Err(config_error("API 地址格式无效"));
    }
    let (_, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| config_error("API 地址必须使用 http 或 https"))?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') || authority.contains('%') {
        return Err(config_error("API 地址不允许内嵌账号或转义主机名"));
    }
    let raw_path = &rest[authority_end..];
    if raw_path.contains('?') || raw_path.contains('#') {
        return Err(config_error("API 地址不允许查询参数或片段"));
    }
    // Standard compatible endpoints use plain path segments. Refuse encoded
    // paths rather than decode them, especially %2e/%2f/%5c traversal variants.
    if !raw_path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/-._~".contains(&b))
    {
        return Err(config_error("API 路径仅支持未转义的标准路径段"));
    }
    let path = raw_path.trim_end_matches('/');
    if path.split('/').any(|part| part == "." || part == "..") || raw_path.contains("//") {
        return Err(config_error("API 路径不能包含空段或相对跳转"));
    }
    let mut url = Url::parse(endpoint).map_err(|_| config_error("API 地址格式无效"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(config_error(
            "API 地址必须是无账号、查询参数或片段的 HTTP(S) 地址",
        ));
    }
    if url.scheme() == "http" && !is_loopback(&url) {
        return Err(config_error("非本机模型接口必须使用 HTTPS"));
    }
    let path = if path.ends_with("/chat/completions") {
        path.to_string()
    } else {
        format!("{path}/chat/completions")
    };
    url.set_path(&path);
    Ok(url)
}
fn is_loopback(url: &Url) -> bool {
    let host = url.host_str().unwrap_or("");
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok().is_some_and(|ip| {
        ip.is_loopback() || matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()))
    })
}

fn request_payload(
    config: &ProviderConfig,
    input: &ModelInput,
    max_tokens: u32,
) -> Result<Value, KernelError> {
    let mut messages = vec![];
    if !config.system_prompt.is_empty() {
        messages.push(json!({"role":"system", "content":config.system_prompt}));
    }
    let mut seen = BTreeSet::new();
    let mut pending = BTreeMap::<String, String>::new();
    for item in &input.context {
        let message: LoopMessage =
            serde_json::from_value(item.data.clone()).map_err(|_| invalid_context())?;
        match message {
            LoopMessage::User { content } => {
                if !pending.is_empty() {
                    return Err(invalid_context());
                }
                messages.push(json!({"role":"user", "content": user_content(content)}));
            }
            LoopMessage::Assistant {
                content,
                tool_calls,
            } => {
                if !pending.is_empty() {
                    return Err(invalid_context());
                }
                let (content, reasoning) = assistant_content(content)?;
                let mut message = json!({"role":"assistant", "content":content});
                if let Some(reasoning) = reasoning {
                    message["reasoning_content"] = json!(reasoning);
                }
                if !tool_calls.is_empty() {
                    let mut calls = vec![];
                    for call in tool_calls {
                        if !identifier(&call.id)
                            || !identifier(&call.name)
                            || !seen.insert(call.id.clone())
                        {
                            return Err(invalid_context());
                        }
                        pending.insert(call.id.clone(), call.name.clone());
                        calls.push(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}}));
                    }
                    message["tool_calls"] = json!(calls);
                }
                messages.push(message);
            }
            LoopMessage::Tool {
                call_id,
                name,
                result,
            } => {
                if pending.remove(&call_id).as_deref() != Some(name.as_str()) {
                    return Err(invalid_context());
                }
                // Both successes and errors remain JSON with the original
                // status/payload. A model can decide what to do with an error.
                messages.push(json!({"role":"tool","tool_call_id":call_id,"content":serde_json::to_string(&result).map_err(|_| invalid_context())?}));
            }
        }
    }
    if !pending.is_empty() || messages.iter().all(|m| m["role"] == "system") {
        return Err(invalid_context());
    }
    let mut payload =
        json!({"model":config.model,"messages":messages,"max_tokens":max_tokens,"stream":false});
    if let Some(temperature) = config.temperature {
        payload["temperature"] = json!(temperature);
    }
    if !input.tools.is_empty() {
        let mut names = BTreeSet::new();
        let mut tools = vec![];
        for tool in &input.tools {
            if !identifier(&tool.name)
                || !tool.input_schema.is_object()
                || !names.insert(&tool.name)
            {
                return Err(KernelError::new(
                    "invalid_model_tools",
                    "工具定义格式错误或名称重复",
                ));
            }
            tools.push(json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}}));
        }
        payload["tools"] = json!(tools);
        payload["tool_choice"] = json!("auto");
    }
    if serde_json::to_vec(&payload)
        .map_err(|_| invalid_context())?
        .len()
        > MAX_REQUEST_BYTES
    {
        return Err(KernelError::new(
            "model_request_too_large",
            "模型请求超过 1 MiB，请压缩上下文后重试",
        ));
    }
    Ok(payload)
}
fn user_content(content: Value) -> Value {
    match content {
        Value::String(_) => content,
        Value::Object(ref object) if object.get("text").is_some_and(Value::is_string) => {
            object["text"].clone()
        }
        // Retain old structured fixture inputs without presenting them as an
        // unsupported provider-native message shape.
        other => Value::String(other.to_string()),
    }
}
fn assistant_content(content: Value) -> Result<(Value, Option<String>), KernelError> {
    match content {
        Value::String(_) | Value::Null => Ok((content, None)),
        Value::Array(ref parts) if text_parts(parts) => Ok((content, None)),
        Value::Object(ref object)
            if object.contains_key("reasoning_content") || object.contains_key("text") =>
        {
            let text = object.get("text").cloned().unwrap_or(Value::Null);
            if !text.is_string()
                && !text.is_null()
                && !text.as_array().is_some_and(|parts| text_parts(parts))
            {
                return Err(invalid_context());
            }
            let reasoning = match object.get("reasoning_content") {
                None | Some(Value::Null) => None,
                Some(Value::String(value)) => Some(value.clone()),
                _ => return Err(invalid_context()),
            };
            Ok((text, reasoning))
        }
        other => Ok((Value::String(other.to_string()), None)),
    }
}
fn text_parts(parts: &[Value]) -> bool {
    !parts.is_empty()
        && parts
            .iter()
            .all(|p| p["type"] == "text" && p["text"].is_string())
}
fn identifier(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_TOOL_ID_BYTES
        && !value.chars().any(char::is_control)
}

fn send(config: &ProviderConfig, payload: &Value) -> Result<Value, KernelError> {
    let endpoint = completion_endpoint(&config.endpoint)?;
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(config.timeout_secs))
        .connect_timeout(Duration::from_secs(config.timeout_secs.min(15)));
    if is_loopback(&endpoint) {
        builder = builder.no_proxy();
    }
    let client = builder
        .build()
        .map_err(|_| KernelError::new("provider_client_error", "无法初始化模型 HTTPS 客户端"))?;
    let mut request = client
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(payload);
    if !config.api_key.is_empty() {
        request = request.bearer_auth(&config.api_key);
    }
    let response = request.send().map_err(|error| {
        if error.is_timeout() {
            KernelError::new("provider_timeout", "模型接口请求超时")
        } else {
            KernelError::new(
                "provider_network_error",
                "模型接口连接失败，请检查地址、网络及 TLS 配置",
            )
        }
    })?;
    let status = response.status();
    if !status.is_success() {
        // Deliberately do not parse or echo the body. Providers and proxies can
        // reflect Authorization, URLs and submitted messages in error text.
        return Err(KernelError::new(
            "provider_http_error",
            format!(
                "模型接口返回 HTTP {}，请检查 API 配置与模型访问权限",
                status.as_u16()
            ),
        ));
    }
    let mut bytes = vec![];
    response
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            let timeout = matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) || error
                .get_ref()
                .and_then(|e| e.downcast_ref::<reqwest::Error>())
                .is_some_and(reqwest::Error::is_timeout);
            if timeout {
                KernelError::new("provider_timeout", "模型接口响应读取超时")
            } else {
                KernelError::new("provider_read_error", "读取模型接口响应失败")
            }
        })?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(KernelError::new(
            "provider_response_too_large",
            "模型接口响应超过 1 MiB 上限",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid_reply())
}

async fn send_async(config: &ProviderConfig, payload: &Value) -> Result<Value, KernelError> {
    let endpoint = completion_endpoint(&config.endpoint)?;
    let mut builder = reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(config.timeout_secs))
        .connect_timeout(Duration::from_secs(config.timeout_secs.min(15)));
    if is_loopback(&endpoint) {
        builder = builder.no_proxy();
    }
    let client = builder
        .build()
        .map_err(|_| KernelError::new("provider_client_error", "无法初始化模型 HTTPS 客户端"))?;
    let mut request = client
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(payload);
    if !config.api_key.is_empty() {
        request = request.bearer_auth(&config.api_key);
    }
    let mut response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            KernelError::new("provider_timeout", "模型接口请求超时")
        } else {
            KernelError::new(
                "provider_network_error",
                "模型接口连接失败，请检查地址、网络及 TLS 配置",
            )
        }
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(KernelError::new(
            "provider_http_error",
            format!(
                "模型接口返回 HTTP {}，请检查 API 配置与模型访问权限",
                status.as_u16()
            ),
        ));
    }
    // Do not trust Content-Length or use Response::json/bytes, which would
    // collect an unbounded body. Check before copying each network chunk.
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(KernelError::new(
            "provider_response_too_large",
            "模型接口响应超过 1 MiB 上限",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        if error.is_timeout() {
            KernelError::new("provider_timeout", "模型接口响应读取超时")
        } else {
            KernelError::new("provider_read_error", "读取模型接口响应失败")
        }
    })? {
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err(KernelError::new(
                "provider_response_too_large",
                "模型接口响应超过 1 MiB 上限",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid_reply())
}

fn parse_reply(
    value: &Value,
    input: &ModelInput,
    allow_length: bool,
) -> Result<ModelReply, KernelError> {
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .filter(|choices| !choices.is_empty())
        .ok_or_else(invalid_reply)?;
    let choice = &choices[0];
    let reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .ok_or_else(invalid_reply)?;
    match reason {
        "length" if !allow_length => {
            return Err(KernelError::new(
                "model_output_truncated",
                "模型达到输出上限，回复未完整生成；请调整输出预算",
            ));
        }
        "content_filter" => {
            return Err(KernelError::new(
                "model_content_filtered",
                "模型接口拒绝输出该内容",
            ));
        }
        "stop" | "tool_calls" | "length" => {}
        _ => return Err(invalid_reply()),
    }
    let message = choice
        .get("message")
        .filter(|m| m.is_object())
        .ok_or_else(invalid_reply)?;
    if message["role"] != "assistant" {
        return Err(invalid_reply());
    }
    let content = match message.get("content") {
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) if text_parts(parts) => Value::Array(parts.clone()),
        None | Some(Value::Null) => {
            if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
                json!(refusal)
            } else {
                Value::Null
            }
        }
        _ => return Err(invalid_reply()),
    };
    let raw_calls = match message.get("tool_calls") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(calls)) => calls.as_slice(),
        _ => return Err(invalid_reply()),
    };
    if raw_calls.len() > 128
        || (reason == "tool_calls" && raw_calls.is_empty())
        || (reason == "stop" && !raw_calls.is_empty())
        || (reason == "length" && !raw_calls.is_empty())
        || (reason != "length" && content.is_null() && raw_calls.is_empty())
    {
        return Err(invalid_reply());
    }
    let mut tool_calls = vec![];
    let mut ids = BTreeSet::new();
    for call in raw_calls {
        if call["type"] != "function" {
            return Err(invalid_reply());
        }
        let raw_id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| identifier(id))
            .ok_or_else(invalid_reply)?;
        let function = call
            .get("function")
            .filter(|f| f.is_object())
            .ok_or_else(invalid_reply)?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| identifier(name))
            .ok_or_else(invalid_reply)?;
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .ok_or_else(invalid_reply)?;
        let arguments: Value = serde_json::from_str(arguments).map_err(|_| invalid_reply())?;
        if !arguments.is_object() {
            return Err(invalid_reply());
        }
        let id = format!("t{}-s{}-{raw_id}", input.task.0, input.step);
        if !identifier(&id) || !ids.insert(id.clone()) {
            return Err(invalid_reply());
        }
        tool_calls.push(ToolCall {
            id,
            name: name.into(),
            arguments,
        });
    }
    let reasoning = match message.get("reasoning_content") {
        None | Some(Value::Null) => None,
        Some(Value::String(reasoning)) => Some(reasoning.clone()),
        _ => return Err(invalid_reply()),
    };
    let content = if let Some(reasoning_content) = reasoning {
        json!({"text":content,"reasoning_content":reasoning_content})
    } else {
        content
    };
    let usage = match value.get("usage") {
        None | Some(Value::Null) => None,
        Some(Value::Object(_)) => Some(value["usage"].clone()),
        _ => return Err(invalid_reply()),
    };
    Ok(ModelReply {
        content,
        tool_calls,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::{ModelTool, input_item};
    use crate::api::{BackendResult, ContextItem};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};

    fn input() -> ModelInput {
        ModelInput {
            task: TaskId(42),
            step: 3,
            context: vec![input_item(json!("hello")).unwrap()],
            tools: vec![ModelTool {
                name: "lookup".into(),
                description: "Find a value".into(),
                input_schema: json!({"type":"object"}),
            }],
        }
    }
    fn final_response() -> Value {
        json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"hello back"}}],"usage":{"prompt_tokens":7,"prompt_cache_hit_tokens":4}})
    }
    fn item(message: LoopMessage) -> ContextItem {
        ContextItem {
            data: serde_json::to_value(message).unwrap(),
            units: 1,
            opens: vec![],
            closes: vec![],
            pinned: false,
        }
    }
    struct Capture {
        headers: String,
        body: Value,
    }
    fn mock(
        status: u16,
        body: Vec<u8>,
        extra_headers: &str,
        delay: Duration,
    ) -> (String, mpsc::Receiver<Capture>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::sync_channel(1);
        let extra_headers = extra_headers.to_string();
        let worker = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse().unwrap())
                })
                .unwrap();
            let mut request_body = vec![0; length];
            reader.read_exact(&mut request_body).unwrap();
            tx.send(Capture {
                headers,
                body: serde_json::from_slice(&request_body).unwrap(),
            })
            .unwrap();
            thread::sleep(delay);
            let header = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
                body.len()
            );
            let _ = socket.write_all(header.as_bytes());
            let _ = socket.write_all(&body);
        });
        (endpoint, rx, worker)
    }
    fn local_config(endpoint: String) -> ProviderConfig {
        ProviderConfig {
            endpoint,
            model: "local-test".into(),
            api_key: "test-secret-do-not-reflect".into(),
            timeout_secs: 3,
            ..ProviderConfig::default()
        }
    }

    #[test]
    fn defaults_validate_and_endpoint_normalization_only_appends_completion_path() {
        ProviderConfig::default().validate().unwrap();
        for (base, normalized) in [
            (
                "https://api.deepseek.com/v1",
                "https://api.deepseek.com/v1/chat/completions",
            ),
            (
                "https://api.deepseek.com/v1/",
                "https://api.deepseek.com/v1/chat/completions",
            ),
            (
                "https://api.deepseek.com",
                "https://api.deepseek.com/chat/completions",
            ),
            (
                "https://host.example/custom/v2/chat/completions",
                "https://host.example/custom/v2/chat/completions",
            ),
            (
                "http://localhost:8080/v1",
                "http://localhost:8080/v1/chat/completions",
            ),
            (
                "http://[::1]:8080/v1",
                "http://[::1]:8080/v1/chat/completions",
            ),
        ] {
            assert_eq!(completion_endpoint(base).unwrap().as_str(), normalized);
        }
    }
    #[test]
    fn unsafe_endpoints_are_rejected_without_echoing_the_input() {
        for endpoint in [
            "http://remote.example/v1",
            "ftp://example.com/v1",
            "https://user:secret@host.example/v1",
            "https://host.example/v1?api_key=secret",
            "https://host.example/v1#secret",
            "https://host.example/a/../v1",
            "https://host.example/a/%2e%2e/v1",
            "https://host.example/a%2fb",
            "https://host.example/a\\b",
            "https://host.example/a//b",
            " https://host.example/v1",
            "https://host.example/v1\n",
        ] {
            let error = completion_endpoint(endpoint).unwrap_err();
            assert_eq!(error.code, "invalid_provider_config");
            assert!(!error.message.contains("secret"));
            assert!(!error.message.contains(endpoint));
        }
    }
    #[test]
    fn config_ranges_and_header_injection_are_validated() {
        let mut config = ProviderConfig {
            model: " ".into(),
            ..ProviderConfig::default()
        };
        assert!(config.validate().is_err());
        config.model = "x".repeat(257);
        assert!(config.validate().is_err());
        config.model = "model".into();
        config.api_key = "secret\r\nAuthorization: x".into();
        assert!(config.validate().is_err());
        config.api_key = "x".repeat(8193);
        assert!(config.validate().is_err());
        config.api_key.clear();
        for max_tokens in [0, 65_537] {
            config.max_tokens = max_tokens;
            assert!(config.validate().is_err());
        }
        config.max_tokens = 4096;
        for timeout in [0, 121] {
            config.timeout_secs = timeout;
            assert!(config.validate().is_err());
        }
        config.timeout_secs = 60;
        for temperature in [f64::NAN, f64::INFINITY, -0.1, 2.1] {
            config.temperature = Some(temperature);
            assert!(config.validate().is_err());
        }
        config.temperature = Some(2.0);
        config.system_prompt = "中".repeat(16_001);
        assert!(config.validate().is_err());
        config.system_prompt = "中".repeat(16_000);
        assert!(config.validate().is_ok());
    }
    #[test]
    fn message_conversion_preserves_strings_fixture_text_reasoning_and_tool_error_json() {
        let mut input = input();
        input.context = vec![
            item(LoopMessage::User {
                content: json!("  原文\n不改  "),
            }),
            item(LoopMessage::Assistant {
                content: json!({"text":"thinking via tool","reasoning_content":"private retained reasoning"}),
                tool_calls: vec![ToolCall {
                    id: "t42-s2-call1".into(),
                    name: "lookup".into(),
                    arguments: json!({"key":"a"}),
                }],
            }),
            item(LoopMessage::Tool {
                call_id: "t42-s2-call1".into(),
                name: "lookup".into(),
                result: BackendResult::Error(json!({"errno":7,"message":"native"})),
            }),
            item(LoopMessage::User {
                content: json!({"text":"只取这份正文","scenario":"old-fixture"}),
            }),
        ];
        let payload = request_payload(&ProviderConfig::default(), &input, 4096).unwrap();
        assert_eq!(payload["messages"][1]["content"], "  原文\n不改  ");
        assert_eq!(
            payload["messages"][2]["reasoning_content"],
            "private retained reasoning"
        );
        assert_eq!(
            payload["messages"][2]["tool_calls"][0]["id"],
            "t42-s2-call1"
        );
        assert_eq!(
            serde_json::from_str::<Value>(
                payload["messages"][2]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap()
            )
            .unwrap(),
            json!({"key":"a"})
        );
        assert_eq!(payload["messages"][3]["tool_call_id"], "t42-s2-call1");
        assert_eq!(
            serde_json::from_str::<Value>(payload["messages"][3]["content"].as_str().unwrap())
                .unwrap(),
            json!({"status":"error","payload":{"errno":7,"message":"native"}})
        );
        assert_eq!(payload["messages"][4]["content"], "只取这份正文");
        assert_eq!(payload["tools"][0]["function"]["name"], "lookup");
        assert_eq!(payload["stream"], false);
    }
    #[test]
    fn unmatched_or_duplicate_history_tool_ids_fail_before_transport() {
        let mut input = input();
        input.context.push(item(LoopMessage::Tool {
            call_id: "orphan".into(),
            name: "lookup".into(),
            result: BackendResult::Ok(json!(1)),
        }));
        assert_eq!(
            request_payload(&ProviderConfig::default(), &input, 10)
                .unwrap_err()
                .code,
            "invalid_model_context"
        );
    }
    #[test]
    fn tool_ids_are_task_step_scoped_and_reasoning_round_trips() {
        let input = input();
        let response = json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"reasoning_content":"private","tool_calls":[{"id":"call1","type":"function","function":{"name":"lookup","arguments":"{\"key\":\"a\"}"}}]}}]});
        let reply = parse_reply(&response, &input, false).unwrap();
        assert_eq!(reply.tool_calls[0].id, "t42-s3-call1");
        assert_eq!(
            reply.content,
            json!({"text":null,"reasoning_content":"private"})
        );
        let mut next = input.clone();
        next.step += 1;
        next.context.push(item(LoopMessage::Assistant {
            content: reply.content,
            tool_calls: reply.tool_calls.clone(),
        }));
        next.context.push(item(LoopMessage::Tool {
            call_id: reply.tool_calls[0].id.clone(),
            name: "lookup".into(),
            result: BackendResult::Ok(json!(5)),
        }));
        let payload = request_payload(&ProviderConfig::default(), &next, 10).unwrap();
        assert_eq!(
            payload["messages"][2]["tool_calls"][0]["id"],
            "t42-s3-call1"
        );
        assert_eq!(payload["messages"][2]["reasoning_content"], "private");
        assert_eq!(payload["messages"][3]["tool_call_id"], "t42-s3-call1");
        let next_reply = parse_reply(&response, &next, false).unwrap();
        assert_ne!(next_reply.tool_calls[0].id, reply.tool_calls[0].id);
    }
    #[test]
    fn malformed_empty_truncated_and_inconsistent_replies_are_not_successes() {
        for value in [
            json!({}),
            json!({"choices":[]}),
            json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":null}}]}),
            json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":"x","tool_calls":[]}}]}),
            json!({"choices":[{"finish_reason":"stop","message":{"role":"user","content":"x"}}]}),
        ] {
            assert!(parse_reply(&value, &input(), false).is_err());
        }
        let truncated = json!({"choices":[{"finish_reason":"length","message":{"role":"assistant","content":null}}]});
        assert_eq!(
            parse_reply(&truncated, &input(), false).unwrap_err().code,
            "model_output_truncated"
        );
        assert!(parse_reply(&truncated, &input(), true).is_ok());
        let broken = json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"lookup","arguments":"not json"}}]}}]});
        assert!(parse_reply(&broken, &input(), false).is_err());
    }
    #[test]
    fn http_generate_sends_authorization_stream_false_and_maps_usage() {
        let (endpoint, rx, worker) = mock(
            200,
            serde_json::to_vec(&final_response()).unwrap(),
            "",
            Duration::ZERO,
        );
        let reply = generate(&local_config(endpoint), &input()).unwrap();
        let capture = rx.recv().unwrap();
        worker.join().unwrap();
        assert!(
            capture
                .headers
                .starts_with("POST /v1/chat/completions HTTP/1.1")
        );
        assert!(
            capture
                .headers
                .to_ascii_lowercase()
                .contains("authorization: bearer test-secret-do-not-reflect")
        );
        assert_eq!(capture.body["stream"], false);
        assert_eq!(reply.content, "hello back");
        assert_eq!(reply.usage.unwrap()["prompt_cache_hit_tokens"], 4);
    }
    #[test]
    fn provider_error_and_redirect_do_not_leak_body_url_or_key() {
        for (status, headers) in [(401, ""), (302, "Location: http://127.0.0.1:9/secret\r\n")] {
            let(endpoint,rx,worker)=mock(status,b"{\"error\":{\"message\":\"Authorization: Bearer test-secret-do-not-reflect https://secret.example/private\"}}".to_vec(),headers,Duration::ZERO);
            let error = generate(&local_config(endpoint.clone()), &input()).unwrap_err();
            let _ = rx.recv().unwrap();
            worker.join().unwrap();
            assert_eq!(error.code, "provider_http_error");
            assert!(error.message.contains(&status.to_string()));
            for secret in [
                "Authorization",
                "test-secret-do-not-reflect",
                "secret.example",
                endpoint.as_str(),
            ] {
                assert!(!error.message.contains(secret));
            }
        }
    }
    #[test]
    fn local_empty_key_and_probe_have_no_authorization_and_small_output_budget() {
        let (endpoint, rx, worker) = mock(
            200,
            serde_json::to_vec(&final_response()).unwrap(),
            "",
            Duration::ZERO,
        );
        let mut config = local_config(endpoint);
        config.api_key.clear();
        let result = test_connection(&config).unwrap();
        let capture = rx.recv().unwrap();
        worker.join().unwrap();
        assert!(
            !capture
                .headers
                .to_ascii_lowercase()
                .contains("authorization:")
        );
        assert!(capture.body["max_tokens"].as_u64().unwrap() <= 32);
        assert!(capture.body.get("tools").is_none());
        assert_eq!(result["ok"], true);
        assert_eq!(result["model"], "local-test");
        assert!(!result.to_string().contains("hello back"));
    }
    #[test]
    fn response_limit_and_invalid_json_are_safe_errors() {
        for (body, code) in [
            (
                vec![b'x'; MAX_RESPONSE_BYTES + 1],
                "provider_response_too_large",
            ),
            (
                b"not json test-secret-do-not-reflect".to_vec(),
                "invalid_model_reply",
            ),
        ] {
            let (endpoint, rx, worker) = mock(200, body, "", Duration::ZERO);
            let error = generate(&local_config(endpoint), &input()).unwrap_err();
            let _ = rx.recv().unwrap();
            worker.join().unwrap();
            assert_eq!(error.code, code);
            assert!(!error.message.contains("test-secret"));
        }
    }
    #[test]
    fn asynchronous_generation_uses_the_same_payload_reply_and_redaction_rules() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (endpoint, rx, worker) = mock(
            200,
            serde_json::to_vec(&final_response()).unwrap(),
            "",
            Duration::ZERO,
        );
        let config = local_config(endpoint);
        let reply = runtime.block_on(generate_async(&config, &input())).unwrap();
        let capture = rx.recv().unwrap();
        worker.join().unwrap();
        assert_eq!(reply.content, "hello back");
        assert_eq!(capture.body["stream"], false);
        assert!(
            capture
                .headers
                .to_ascii_lowercase()
                .contains("authorization: bearer test-secret-do-not-reflect")
        );
        let (endpoint, rx, worker) = mock(
            302,
            b"test-secret-do-not-reflect".to_vec(),
            "Location: http://127.0.0.1:9/private\r\n",
            Duration::ZERO,
        );
        let error = runtime
            .block_on(generate_async(&local_config(endpoint), &input()))
            .unwrap_err();
        let _ = rx.recv().unwrap();
        worker.join().unwrap();
        assert_eq!(error.code, "provider_http_error");
        assert!(!error.message.contains("test-secret"));
        assert!(!error.message.contains("private"));
    }
    #[test]
    fn asynchronous_response_collection_enforces_limit_and_total_timeout() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (endpoint, rx, worker) =
            mock(200, vec![b'x'; MAX_RESPONSE_BYTES + 1], "", Duration::ZERO);
        let error = runtime
            .block_on(generate_async(&local_config(endpoint), &input()))
            .unwrap_err();
        let _ = rx.recv().unwrap();
        worker.join().unwrap();
        assert_eq!(error.code, "provider_response_too_large");
        let (endpoint, rx, worker) = mock(
            200,
            serde_json::to_vec(&final_response()).unwrap(),
            "",
            Duration::from_millis(1400),
        );
        let mut config = local_config(endpoint);
        config.timeout_secs = 1;
        let error = runtime
            .block_on(generate_async(&config, &input()))
            .unwrap_err();
        let _ = rx.recv().unwrap();
        worker.join().unwrap();
        assert_eq!(error.code, "provider_timeout");
    }
    #[test]
    fn aborting_async_generation_releases_the_inflight_http_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (closed_tx, closed_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
            }
            let mut request = vec![0; length];
            reader.read_exact(&mut request).unwrap();
            // Leave a declared JSON body unfinished so generate_async awaits it.
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{").unwrap();
            started_tx.send(()).unwrap();
            let mut byte = [0];
            let closed = match socket.read(&mut byte) {
                Ok(0) => true,
                Err(error)
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    true
                }
                _ => false,
            };
            closed_tx.send(closed).unwrap();
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let mut config = local_config(endpoint);
        config.timeout_secs = 10;
        let task = runtime.spawn(async move { generate_async(&config, &input()).await });
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        task.abort();
        assert!(runtime.block_on(task).unwrap_err().is_cancelled());
        assert!(
            closed_rx.recv_timeout(Duration::from_secs(4)).unwrap(),
            "aborted future must close the pending HTTP exchange"
        );
        worker.join().unwrap();
    }

    #[test]
    fn timeout_is_explicit_and_does_not_echo_request_url() {
        let (endpoint, rx, worker) = mock(
            200,
            serde_json::to_vec(&final_response()).unwrap(),
            "",
            Duration::from_millis(1400),
        );
        let mut config = local_config(endpoint.clone());
        config.timeout_secs = 1;
        let error = generate(&config, &input()).unwrap_err();
        let _ = rx.recv().unwrap();
        worker.join().unwrap();
        assert_eq!(error.code, "provider_timeout");
        assert!(!error.message.contains(&endpoint));
        assert!(!error.message.contains(&config.api_key));
    }
}
