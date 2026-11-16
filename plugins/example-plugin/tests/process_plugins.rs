use ithink::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);
fn spec(name: &str, mode: &str, label: &str) -> PluginSpec {
    let mut s = PluginSpec::new(
        name,
        env!("CARGO_BIN_EXE_ithink-example-plugin"),
        Level::Worker,
    );
    s.args = vec![name.into(), mode.into(), label.into()];
    s
}
fn backend(mode: &str, label: &str) -> PluginSpec {
    let mut s = spec("bridge", mode, label);
    s.capabilities = [CAP_BACKEND, "provide:fixture"]
        .into_iter()
        .map(String::from)
        .collect();
    s.provides = vec!["fixture".into()];
    s
}
fn runtime() -> Runtime {
    let mut rt = Runtime::new();
    rt.define_service("fixture", "fixture.call").unwrap();
    rt
}
fn call(rt: &mut Runtime, op: &str, data: Value) -> Result<Value, KernelError> {
    rt.request(
        Request::SystemCall {
            service: "fixture".into(),
            operation: op.into(),
            payload: data,
        },
        WAIT,
    )
}
fn until(rt: &mut Runtime, mut predicate: impl FnMut(&mut Runtime) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        if predicate(rt) {
            return;
        }
        assert!(Instant::now() < deadline, "condition timed out");
        rt.pump(Duration::from_millis(5));
    }
}
fn subscribe(rt: &mut Runtime, topic: &str) {
    rt.request(
        Request::EventSubscribe {
            topic: topic.into(),
        },
        WAIT,
    )
    .unwrap();
}

#[test]
fn hot_reload_uses_new_process_and_unload_revokes_service() {
    let mut rt = runtime();
    let first = rt.load_plugin(backend("echo", "old")).unwrap();
    assert_eq!(
        call(&mut rt, "echo", json!(1)).unwrap()["payload"]["label"],
        "old"
    );
    let second = rt.load_plugin(backend("echo", "new")).unwrap();
    assert_ne!(first.aid, second.aid);
    assert!(second.generation > first.generation);
    assert!(!rt.kernel().subject(first.aid).unwrap().active);
    assert_eq!(
        call(&mut rt, "echo", json!(2)).unwrap()["payload"]["label"],
        "new"
    );
    rt.unload_plugin("bridge").unwrap();
    assert!(call(&mut rt, "echo", json!(3)).is_err());
}

#[test]
fn hot_replacement_explicitly_fails_old_inflight_calls() {
    let mut rt = runtime();
    rt.load_plugin(backend("echo", "old")).unwrap();
    let id = rt
        .submit(Request::SystemCall {
            service: "fixture".into(),
            operation: "hold".into(),
            payload: json!(null),
        })
        .unwrap();
    rt.load_plugin(backend("echo", "new")).unwrap();
    assert_eq!(
        rt.take_response(id)
            .unwrap()
            .into_result()
            .unwrap_err()
            .code,
        "plugin_unavailable"
    );
    assert_eq!(
        call(&mut rt, "echo", json!(null)).unwrap()["payload"]["label"],
        "new"
    );
}

#[test]
fn failed_handshake_and_no_ack_keep_old_plugin_alive() {
    let mut rt = runtime();
    rt.handshake_timeout = Duration::from_millis(400);
    let first = rt.load_plugin(backend("echo", "old")).unwrap();
    for mode in ["bad_ready", "ready_exit", "no_ack"] {
        assert!(rt.load_plugin(backend(mode, "bad")).is_err());
        assert_eq!(rt.plugin("bridge").unwrap().aid, first.aid);
        assert_eq!(
            call(&mut rt, "echo", json!(null)).unwrap()["payload"]["label"],
            "old"
        );
    }
}

#[test]
fn slow_backend_and_startup_do_not_block_other_requests() {
    let mut rt = runtime();
    rt.load_plugin(backend("echo", "old")).unwrap();
    let pending = rt
        .submit(Request::SystemCall {
            service: "fixture".into(),
            operation: "slow".into(),
            payload: json!(null),
        })
        .unwrap();
    let fast = rt.submit(Request::AgentGet { aid: HOST }).unwrap();
    assert!(rt.take_response(fast).unwrap().into_result().is_ok());
    assert!(rt.take_response(pending).is_none());
    let mut slow = spec("warming", "slow_start", "v");
    slow.level = Level::Main;
    let ticket = rt.begin_load(slow).unwrap();
    let fast = rt.submit(Request::AgentGet { aid: HOST }).unwrap();
    assert!(rt.take_response(fast).is_some());
    until(&mut rt, |rt| rt.take_response(pending).is_some());
    until(&mut rt, |rt| rt.take_load_result(ticket).is_some());
}

#[test]
fn native_errors_pass_through_and_timeout_is_kernel_error() {
    let mut rt = runtime();
    rt.load_plugin(backend("echo", "v")).unwrap();
    let native = json!({"errno":987,"message":"原生错误","details":{"opaque":true}});
    let result = call(&mut rt, "native_error", native.clone()).unwrap();
    assert_eq!(result, json!({"status":"error","payload":native}));
    rt.set_call_timeout(Duration::from_millis(30));
    assert_eq!(
        call(&mut rt, "slow", json!(null)).unwrap_err().code,
        "timeout"
    );
    let fast = rt.submit(Request::AgentGet { aid: HOST }).unwrap();
    assert!(rt.take_response(fast).is_some());
}

#[test]
fn provider_crash_fails_waiters_but_not_kernel() {
    let mut rt = runtime();
    rt.load_plugin(backend("echo", "v")).unwrap();
    let error = call(&mut rt, "crash", json!(null)).unwrap_err();
    assert_eq!(error.code, "plugin_unavailable");
    assert!(rt.plugin("bridge").is_none());
    assert!(rt.request(Request::AgentGet { aid: HOST }, WAIT).is_ok());
}

#[test]
fn dependencies_are_checked_and_prevent_unsafe_unload() {
    let mut rt = runtime();
    let mut dependent = spec("dependent", "echo", "v");
    dependent.dependencies = vec!["bridge".into()];
    assert_eq!(
        rt.load_plugin(dependent.clone()).unwrap_err().code,
        "missing_dependency"
    );
    rt.load_plugin(backend("echo", "v")).unwrap();
    rt.load_plugin(dependent).unwrap();
    assert_eq!(rt.unload_plugin("bridge").unwrap_err().code, "in_use");
    rt.unload_plugin("dependent").unwrap();
    rt.unload_plugin("bridge").unwrap();
}

#[test]
fn plugin_calls_kernel_with_connection_bound_permissions() {
    let mut rt = runtime();
    rt.load_plugin(backend("echo", "v")).unwrap();
    subscribe(&mut rt, "client.result");
    let mut client = spec("client", "client", "v");
    client.capabilities = [CAP_EVENT].into_iter().map(String::from).collect();
    rt.load_plugin(client).unwrap();
    let mut result = None;
    until(&mut rt, |rt| {
        while let Some(n) = rt.take_notice() {
            if let Notice::Event { event } = n {
                if event.topic == "client.result" {
                    result = Some(event.data);
                }
            }
        }
        result.is_some()
    });
    assert_eq!(result.unwrap()["ok"], false);
}

#[test]
fn parent_launches_worker_assigns_context_and_collects_result_via_public_api() {
    let mut rt = Runtime::new();
    subscribe(&mut rt, "orchestrator.done");
    let mut worker = spec("unused", "assigned_worker", "v");
    worker.args[0] = "{name}".into();
    worker.capabilities = [CAP_TASK, CAP_CONTEXT]
        .into_iter()
        .map(String::from)
        .collect();
    rt.register_template("worker-template", worker).unwrap();
    let mut main = spec("main", "orchestrator", "v");
    main.level = Level::Main;
    main.capabilities = [
        CAP_AGENT,
        CAP_PLUGIN,
        CAP_TASK,
        CAP_CONTEXT,
        CAP_EVENT,
        "launch:worker-template",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let main_token = rt.load_plugin(main).unwrap();
    let mut result = None;
    until(&mut rt, |rt| {
        while let Some(n) = rt.take_notice() {
            if let Notice::Event { event } = n {
                if event.topic == "orchestrator.done" {
                    result = Some(event.data);
                }
            }
        }
        result.is_some()
    });
    let result = result.unwrap();
    assert_eq!(result["context_denied"], true);
    assert_eq!(result["result"]["seed"], "explicit seed");
    let child: Aid = serde_json::from_value(result["aid"].clone()).unwrap();
    assert_eq!(
        rt.kernel().subject(child).unwrap().parent,
        Some(main_token.aid)
    );
    until(&mut rt, |rt| rt.plugin("spawned-worker").is_none());
}

#[test]
fn unloading_with_a_blocked_pipe_does_not_block_the_kernel() {
    let mut rt = Runtime::new();
    subscribe(&mut rt, "ready");
    let mut plugin = spec("blocked", "blocked_reader", "v");
    plugin.capabilities = [CAP_EVENT].into_iter().map(String::from).collect();
    rt.load_plugin(plugin).unwrap();
    until(
        &mut rt,
        |rt| matches!(rt.take_notice(),Some(Notice::Event{event}) if event.topic=="ready"),
    );
    for _ in 0..10 {
        rt.request(
            Request::EventPublish {
                topic: "flood".into(),
                data: json!("x".repeat(100_000)),
            },
            WAIT,
        )
        .unwrap();
    }
    let start = Instant::now();
    rt.unload_plugin("blocked").unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(rt.request(Request::AgentGet { aid: HOST }, WAIT).is_ok());
}

#[cfg(windows)]
#[test]
fn unload_terminates_descendants_that_inherit_protocol_handles() {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let mut rt = Runtime::new();
    subscribe(&mut rt, "grandchild.pid");
    let mut plugin = spec("parent", "grandchild", "v");
    plugin.capabilities = [CAP_EVENT].into_iter().map(String::from).collect();
    rt.load_plugin(plugin).unwrap();
    let mut pid = None;
    until(&mut rt, |rt| {
        while let Some(n) = rt.take_notice() {
            if let Notice::Event { event } = n {
                if event.topic == "grandchild.pid" {
                    pid = event.data.as_u64();
                }
            }
        }
        pid.is_some()
    });
    // SAFETY: request a wait-only handle for the fixture's reported child PID.
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid.unwrap() as u32) };
    assert!(!raw.is_null());
    // SAFETY: OpenProcess returned an owned handle.
    let _owned = unsafe { OwnedHandle::from_raw_handle(raw) };
    rt.unload_plugin("parent").unwrap();
    // SAFETY: the handle remains valid, and wait is bounded at 2 seconds.
    assert_eq!(unsafe { WaitForSingleObject(raw, 2000) }, 0);
}

#[test]
fn replacement_cannot_create_dependency_cycles() {
    let mut rt = Runtime::new();
    rt.load_plugin(spec("a", "echo", "v")).unwrap();
    let mut b = spec("b", "echo", "v");
    b.dependencies = vec!["a".into()];
    rt.load_plugin(b).unwrap();
    let mut a = spec("a", "echo", "new");
    a.dependencies = vec!["b".into()];
    assert_eq!(rt.load_plugin(a).unwrap_err().code, "dependency_cycle");
    assert!(rt.plugin("a").is_some());
}

#[test]
fn event_subscriber_receives_ordered_direct_events() {
    let mut rt = Runtime::new();
    subscribe(&mut rt, "ready");
    subscribe(&mut rt, "observed");
    let mut sub = spec("sub", "subscriber", "v");
    sub.capabilities = [CAP_EVENT].into_iter().map(String::from).collect();
    rt.load_plugin(sub).unwrap();
    until(
        &mut rt,
        |rt| matches!(rt.take_notice(),Some(Notice::Event{event}) if event.topic=="ready"),
    );
    for n in 1..=3 {
        rt.request(
            Request::EventPublish {
                topic: "input".into(),
                data: json!(n),
            },
            WAIT,
        )
        .unwrap();
    }
    let mut got = vec![];
    until(&mut rt, |rt| {
        while let Some(n) = rt.take_notice() {
            if let Notice::Event { event } = n {
                if event.topic == "observed" {
                    got.push(event.data["sequence"].as_u64().unwrap());
                }
            }
        }
        got.len() == 3
    });
    assert_eq!(got, vec![1, 2, 3]);
}
