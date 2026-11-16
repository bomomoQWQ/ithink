use ithink::*;
use serde_json::json;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::new();
    runtime.define_service("fixture", "fixture.call")?;
    let executable = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "ithink-example-plugin.exe"
    } else {
        "ithink-example-plugin"
    });
    let mut plugin = PluginSpec::new("bridge", executable, Level::Worker);
    plugin.args = vec!["bridge".into(), "echo".into(), "v1".into()];
    plugin.capabilities = [CAP_BACKEND, "provide:fixture"]
        .into_iter()
        .map(String::from)
        .collect();
    plugin.provides = vec!["fixture".into()];
    let first = runtime.load_plugin(plugin.clone())?;
    let result = runtime.request(
        Request::SystemCall {
            service: "fixture".into(),
            operation: "echo".into(),
            payload: json!({"message":"hello"}),
        },
        Duration::from_secs(3),
    )?;
    println!("first generation: {}, response: {result}", first.generation);
    plugin.args[2] = "v2".into();
    let second = runtime.load_plugin(plugin)?;
    let result = runtime.request(
        Request::SystemCall {
            service: "fixture".into(),
            operation: "echo".into(),
            payload: json!({"message":"after reload"}),
        },
        Duration::from_secs(3),
    )?;
    println!(
        "replacement generation: {}, response: {result}",
        second.generation
    );
    runtime.unload_plugin("bridge")?;
    println!(
        "unloaded; journal records: {}",
        runtime.kernel().journal().len()
    );
    Ok(())
}
