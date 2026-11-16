//! Offline model, tool and loop process examples. All use public APIs only.
use ithink::agent_loop::*;
use ithink::loop_plugin::LoopPlugin;
use ithink::sdk::Client;
use ithink::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::Duration;

fn main() {
    if let Err(error) = run() {
        eprintln!("loop example: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), KernelError> {
    let args: Vec<_> = std::env::args().collect();
    let name = args.get(1).map(String::as_str).unwrap_or("loop");
    let mode = args.get(2).map(String::as_str).unwrap_or("loop");
    let mut client = Client::connect(name)?;
    if mode == "loop" {
        let mut config = LoopConfig::new(ServiceRoute::new("model", "generate"));
        config.tools = vec![ToolDefinition {
            name: "add".into(),
            description: "Add two integer values".into(),
            input_schema: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
            route: ServiceRoute::new("tools", "add"),
        }];
        let mut plugin = LoopPlugin::new(client, config)?;
        plugin.set_report_prefix(Some("loop".into()));
        loop {
            plugin.tick(Duration::from_millis(5))?;
            // Reports are also published as kernel events. An application may
            // instead consume these queues locally or attach a renderer.
            while plugin.take_event().is_some() {}
            while plugin.take_notice().is_some() {}
        }
    }
    let mut stream_replies = BTreeSet::new();
    loop {
        drain_stream_responses(&mut client, &mut stream_replies);
        let Notice::SystemCall {
            call,
            operation,
            payload,
            ..
        } = client.next_notice()?
        else {
            continue;
        };
        let result = if mode == "model" {
            model_reply(&payload)
        } else {
            tool_reply(&operation, &payload)
        };
        if mode == "model" {
            if let Some(result) = &result {
                publish_fixture_stream(&mut client, &payload, result, &mut stream_replies);
            }
        }
        if let Some(result) = result {
            match client.request(Request::BackendComplete { call, result }) {
                Ok(_) => {}
                Err(error) if error.code == "not_found" => {}
                Err(error) => return Err(error),
            }
        }
    }
}
fn drain_stream_responses(client: &mut Client, replies: &mut BTreeSet<u64>) {
    let done: Vec<u64> = replies
        .iter()
        .copied()
        .filter(|id| client.take_response(*id).is_some())
        .collect();
    for id in done {
        replies.remove(&id);
    }
}

fn publish_fixture_stream(
    client: &mut Client,
    payload: &Value,
    result: &BackendResult,
    replies: &mut BTreeSet<u64>,
) {
    let BackendResult::Ok(value) = result else {
        return;
    };
    let Ok(input) = serde_json::from_value::<ModelInput>(payload.clone()) else {
        return;
    };
    let Ok(reply) = serde_json::from_value::<ModelReply>(value.clone()) else {
        return;
    };
    let Some(content) = reply.content.as_str() else {
        return;
    };
    let chars: Vec<char> = content.chars().collect();
    for chunk in chars.chunks(8) {
        let delta: String = chunk.iter().collect();
        if let Ok(id) = client.send(Request::EventPublishStream {
            topic: "model.stream".into(),
            data: json!({
                "task": input.task,
                "step": input.step,
                "kind": "content",
                "delta": delta
            }),
        }) {
            replies.insert(id);
        }
    }
}

fn model_reply(payload: &Value) -> Option<BackendResult> {
    let input: ModelInput = match serde_json::from_value(payload.clone()) {
        Ok(input) => input,
        Err(error) => return Some(BackendResult::Error(json!({"message":error.to_string()}))),
    };
    let turn_start = input
        .context
        .iter()
        .rposition(|item| {
            matches!(
                serde_json::from_value::<LoopMessage>(item.data.clone()),
                Ok(LoopMessage::User { .. })
            )
        })
        .unwrap_or(0);
    let user = input
        .context
        .iter()
        .skip(turn_start)
        .filter_map(|i| serde_json::from_value::<LoopMessage>(i.data.clone()).ok())
        .find_map(|m| {
            if let LoopMessage::User { content } = m {
                Some(content)
            } else {
                None
            }
        })
        .unwrap_or(Value::Null);
    let scenario = user["scenario"].as_str().unwrap_or("tool");
    if scenario == "hold_model" {
        return None;
    }
    if scenario == "model_error" {
        return Some(BackendResult::Error(
            json!({"native_code":"fixture_failure","message":"offline model failed"}),
        ));
    }
    if scenario == "malformed" {
        return Some(BackendResult::Ok(json!({"unexpected":"not a model reply"})));
    }
    let result =
        input.context[turn_start..].iter().rev().find_map(|i| {
            match serde_json::from_value::<LoopMessage>(i.data.clone()) {
                Ok(LoopMessage::Tool { result, .. }) => Some(result),
                _ => None,
            }
        });
    let reply = if let Some(result) = result {
        ModelReply {
            content: json!({"answer":result}),
            tool_calls: vec![],
            usage: Some(json!({"fixture":true,"step":input.step})),
        }
    } else if scenario == "echo" {
        ModelReply {
            content: json!(format!(
                "[离线假模型] 已收到：{}。这只是接口回显，尚未接入真实 LLM。",
                user["text"].as_str().unwrap_or("")
            )),
            tool_calls: vec![],
            usage: None,
        }
    } else if scenario == "direct" {
        ModelReply {
            content: json!("offline direct reply"),
            tool_calls: vec![],
            usage: None,
        }
    } else {
        let tool = if scenario == "unknown_tool" {
            "not_registered"
        } else {
            "add"
        };
        ModelReply {
            content: json!("I will use a tool"),
            tool_calls: vec![ToolCall {
                id: format!("t{}-call-{}", input.task.0, input.step),
                name: tool.into(),
                arguments: json!({"a":user["a"].as_i64().unwrap_or(2),"b":user["b"].as_i64().unwrap_or(3),"hold":scenario=="hold_tool","fail":scenario=="tool_error"}),
            }],
            usage: None,
        }
    };
    Some(BackendResult::Ok(serde_json::to_value(reply).unwrap()))
}
fn tool_reply(operation: &str, payload: &Value) -> Option<BackendResult> {
    if payload["hold"] == true {
        return None;
    }
    if payload["fail"] == true {
        return Some(BackendResult::Error(
            json!({"errno":77,"message":"native tool error"}),
        ));
    }
    if operation != "add" {
        return Some(BackendResult::Error(json!({"message":"unknown operation"})));
    }
    let result = payload["a"]
        .as_i64()
        .zip(payload["b"].as_i64())
        .and_then(|(a, b)| a.checked_add(b));
    Some(match result {
        Some(sum) => BackendResult::Ok(json!({"sum":sum})),
        None => BackendResult::Error(json!({"message":"invalid integers or overflow"})),
    })
}
