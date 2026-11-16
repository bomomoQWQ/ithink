//! Offline end-to-end Agent Loop with real process plugins.
use ithink::agent_loop::input_item;
use ithink::*;
use serde_json::json;
use std::time::{Duration, Instant};

fn plugin(name: &str, mode: &str, level: Level) -> Result<PluginSpec, std::io::Error> {
    let exe = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "ithink-loop-fixture.exe"
    } else {
        "ithink-loop-fixture"
    });
    let mut spec = PluginSpec::new(name, exe, level);
    spec.args = vec![name.into(), mode.into()];
    Ok(spec)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::new();
    runtime.define_operation("model", "generate", "model.generate")?;
    runtime.define_operation("tools", "add", "tool.add")?;
    for (name, mode, service) in [
        ("model-provider", "model", "model"),
        ("tools-provider", "tools", "tools"),
    ] {
        let mut spec = plugin(name, mode, Level::Worker)?;
        spec.capabilities = [CAP_BACKEND.to_string(), format!("provide:{service}")]
            .into_iter()
            .collect();
        spec.provides = vec![service.into()];
        runtime.load_plugin(spec)?;
    }
    let mut spec = plugin("agent-loop", "loop", Level::Main)?;
    spec.capabilities = [
        CAP_TASK,
        CAP_CONTEXT,
        CAP_EVENT,
        "model.generate",
        "tool.add",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let aid = runtime.load_plugin(spec)?.aid;
    let task = runtime.request(
        Request::TaskCreate {
            owner: aid,
            parent: None,
            limits: ContextLimits::default(),
            seed: vec![input_item(json!({"scenario":"tool","a":17,"b":25}))?],
        },
        Duration::from_secs(3),
    )?;
    let task: TaskId = serde_json::from_value(task["task"].clone())?;
    println!("Agent Loop started, task={}", task.0);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        runtime.pump(Duration::from_millis(5));
        let state = runtime.kernel().task(task).unwrap();
        if state.state != TaskState::Running {
            println!(
                "state={:?}, result={}",
                state.state,
                state.result.as_ref().unwrap_or(&serde_json::Value::Null)
            );
            if state.state != TaskState::Completed {
                return Err("loop failed".into());
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err("loop demo timed out".into());
        }
    }
    println!("Model -> tool(add) -> result written to context -> model -> final answer");
    Ok(())
}
