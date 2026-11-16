//! Replaceable OpenAI-compatible process. A one-use private stdin bootstrap
//! keeps credentials out of argv, public protocol, context and kernel journals.
use ithink::agent_loop::ModelInput;
use ithink::openai::{ProviderConfig, generate_async};
use ithink::protocol::read_frame;
use ithink::sdk::Client;
use ithink::*;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;

fn main() {
    if let Err(error) = run() {
        eprintln!("model provider: {}", error.code);
        std::process::exit(1);
    }
}
fn run() -> Result<(), KernelError> {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "model-provider".into());
    let config = Arc::new(read_config()?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| KernelError::new("provider_runtime", "无法启动模型网络运行时"))?;
    let mut client = Client::connect(&name)?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut pending: BTreeMap<u64, tokio::task::JoinHandle<()>> = BTreeMap::new();
    let mut replies = BTreeSet::new();
    loop {
        client.poll(Duration::from_millis(5))?;
        let done: Vec<_> = replies
            .iter()
            .copied()
            .filter(|id| client.take_response(*id).is_some())
            .collect();
        for id in done {
            replies.remove(&id);
        }
        while let Some(notice) = client.take_notice() {
            match notice {
                Notice::SystemCallCancelled { call } => {
                    if let Some(handle) = pending.remove(&call) {
                        handle.abort();
                    }
                }
                Notice::SystemCall {
                    call,
                    operation,
                    payload,
                    ..
                } => {
                    if operation != "generate" || pending.len() >= 8 {
                        let id = client.send(Request::BackendComplete {
                            call,
                            result: BackendResult::Error(
                                json!({"code":"model_busy","message":"模型并发已满或操作不支持"}),
                            ),
                        })?;
                        replies.insert(id);
                        continue;
                    }
                    let input: ModelInput = match serde_json::from_value(payload) {
                        Ok(input) => input,
                        Err(_) => {
                            let id=client.send(Request::BackendComplete{call,result:BackendResult::Error(json!({"code":"invalid_model_input","message":"模型输入格式错误"}))})?;
                            replies.insert(id);
                            continue;
                        }
                    };
                    let config = Arc::clone(&config);
                    let tx = tx.clone();
                    pending.insert(
                        call,
                        runtime.spawn(async move {
                            let result = generate_async(&config, &input).await.and_then(|reply| {
                                serde_json::to_value(reply).map_err(|_| {
                                    KernelError::new("invalid_model_reply", "模型回执无法编码")
                                })
                            });
                            let result = match result {
                                Ok(value) => BackendResult::Ok(value),
                                Err(error) => BackendResult::Error(
                                    json!({"code":error.code,"message":error.message}),
                                ),
                            };
                            // Await bounded capacity without blocking a network
                            // worker or silently dropping a terminal response.
                            let _ = tx.send((call, result)).await;
                        }),
                    );
                }
                _ => {}
            }
        }
        while let Ok((call, result)) = rx.try_recv() {
            // Cancellation may have already removed the job. Never deliver its
            // late reply, even if the HTTP response won just before abort.
            if pending.remove(&call).is_some() {
                let id = client.send(Request::BackendComplete { call, result })?;
                replies.insert(id);
            }
        }
    }
}
fn read_config() -> Result<ProviderConfig, KernelError> {
    let bytes = read_frame(&mut BufReader::new(std::io::stdin()))
        .map_err(|_| KernelError::new("config_error", "无法读取模型配置"))?
        .ok_or_else(|| KernelError::new("config_error", "缺少模型配置"))?;
    let config: ProviderConfig = serde_json::from_slice(&bytes)
        .map_err(|_| KernelError::new("config_error", "模型配置格式错误"))?;
    config.validate()?;
    Ok(config)
}
