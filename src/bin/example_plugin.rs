//! Example user-space plugins and fault fixtures. No special internal API.
use ithink::api::*;
use ithink::protocol::{PROTOCOL_VERSION, PluginFrame, write_frame};
use ithink::sdk::Client;
use serde_json::json;
use std::time::Duration;

fn main() {
    if let Err(error) = run() {
        eprintln!("plugin: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), KernelError> {
    let args: Vec<_> = std::env::args().collect();
    let name = args.get(1).map(String::as_str).unwrap_or("example");
    let mode = args.get(2).map(String::as_str).unwrap_or("echo");
    let label = args.get(3).cloned().unwrap_or_else(|| "v1".into());
    if mode == "bad_ready" {
        write_frame(
            &mut std::io::stdout(),
            &PluginFrame::Ready {
                protocol_version: PROTOCOL_VERSION + 1,
                name: name.into(),
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(5));
        return Ok(());
    }
    if mode == "ready_exit" {
        write_frame(
            &mut std::io::stdout(),
            &PluginFrame::Ready {
                protocol_version: PROTOCOL_VERSION,
                name: name.into(),
            },
        )
        .unwrap();
        return Ok(());
    }
    if mode == "no_ack" {
        write_frame(
            &mut std::io::stdout(),
            &PluginFrame::Ready {
                protocol_version: PROTOCOL_VERSION,
                name: name.into(),
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(10));
        return Ok(());
    }
    if mode == "silent" {
        std::thread::sleep(Duration::from_secs(30));
        return Ok(());
    }
    if mode == "slow_start" {
        std::thread::sleep(Duration::from_millis(300));
    }
    let mut client = Client::connect(name)?;
    if mode == "grandchild" {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["descendant", "silent"])
            .spawn()
            .unwrap();
        let result = (|| {
            client.request(Request::EventPublish {
                topic: "grandchild.pid".into(),
                data: json!(child.id()),
            })?;
            loop {
                client.next_notice()?;
            }
        })();
        let _ = child.kill();
        let _ = child.wait();
        return result;
    }
    if mode == "blocked_reader" {
        client.request(Request::EventSubscribe {
            topic: "flood".into(),
        })?;
        client.request(Request::EventPublish {
            topic: "ready".into(),
            data: json!(name),
        })?;
        std::thread::sleep(Duration::from_secs(30));
        return Ok(());
    }
    if mode == "orchestrator" {
        let child = client.request(Request::PluginStart {
            template: "worker-template".into(),
            name: "spawned-worker".into(),
        })?;
        let owner: Aid = serde_json::from_value(child["aid"].clone()).unwrap();
        let task = client.request(Request::TaskCreate {
            owner,
            parent: None,
            limits: ContextLimits::default(),
            seed: vec![ContextItem::text("explicit seed", 10)],
        })?;
        let task: TaskId = serde_json::from_value(task["task"].clone()).unwrap();
        let denied = client.request(Request::ContextGet { task }).is_err();
        loop {
            if let Notice::TaskEnded {
                task: finished,
                result,
                ..
            } = client.next_notice()?
            {
                if finished == task {
                    client.request(Request::EventPublish{topic:"orchestrator.done".into(),data:json!({"aid":owner,"task":task,"result":result,"context_denied":denied})})?;
                    client.request(Request::PluginStop {
                        name: "spawned-worker".into(),
                    })?;
                    break;
                }
            }
        }
        loop {
            client.next_notice()?;
        }
    }
    if mode == "client" {
        let result = client.request(Request::SystemCall {
            service: "fixture".into(),
            operation: "echo".into(),
            payload: json!({"sent_by":client.aid()}),
        });
        client.request(Request::EventPublish {
            topic: "client.result".into(),
            data: json!({"ok":result.is_ok(),"result":result.ok()}),
        })?;
        loop {
            client.next_notice()?;
        }
    }
    if mode == "subscriber" {
        client.request(Request::EventSubscribe {
            topic: "input".into(),
        })?;
        client.request(Request::EventPublish {
            topic: "ready".into(),
            data: json!(name),
        })?;
    }
    loop {
        match client.next_notice()? {
            Notice::SystemCall {
                call,
                operation,
                payload,
                ..
            } => {
                if operation == "crash" {
                    std::process::exit(17);
                }
                if operation == "hold" {
                    // Fault fixture: leave the call pending until cancellation
                    // or instance replacement, without a timing-dependent sleep.
                    continue;
                }
                if operation == "slow" {
                    std::thread::sleep(Duration::from_millis(250));
                }
                let result = match operation.as_str() {
                    "native_error" => BackendResult::Error(payload),
                    "read" => {
                        let path = payload["path"].as_str().unwrap_or("");
                        match std::fs::read(path) {
                            Ok(bytes) => BackendResult::Ok(json!({"bytes":bytes})),
                            Err(error) => BackendResult::Error(
                                json!({"os_code":error.raw_os_error(),"message":error.to_string()}),
                            ),
                        }
                    }
                    _ => BackendResult::Ok(json!({"label":label,"echo":payload})),
                };
                match client.request(Request::BackendComplete { call, result }) {
                    Ok(_) => {}
                    // A cancellation/timeout may have won while the external
                    // operation was finishing. Do not crash on a late reply.
                    Err(error) if error.code == "not_found" => {}
                    Err(error) => return Err(error),
                }
            }
            Notice::TaskAssigned { task } if mode == "assigned_worker" => {
                let context = client.request(Request::ContextGet { task })?;
                client.request(Request::TaskFinish {
                    task,
                    result: json!({"seed":context["items"][0]["data"]}),
                })?;
            }
            Notice::Event { event } if mode == "subscriber" => {
                client.request(Request::EventPublish {
                    topic: "observed".into(),
                    data: json!({"sequence":event.sequence,"data":event.data}),
                })?;
            }
            _ => {}
        }
    }
}
