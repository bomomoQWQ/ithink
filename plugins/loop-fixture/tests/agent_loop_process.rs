use ithink::agent_loop::{LoopMessage, input_item};
use ithink::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);
fn spec(name: &str, mode: &str) -> PluginSpec {
    let mut spec = PluginSpec::new(
        name,
        env!("CARGO_BIN_EXE_ithink-loop-fixture"),
        Level::Worker,
    );
    spec.args = vec![name.into(), mode.into()];
    spec
}
fn setup(allow_tools: bool) -> (Runtime, Aid) {
    let mut rt = Runtime::new();
    rt.define_operation("model", "generate", "model.generate")
        .unwrap();
    rt.define_operation("tools", "add", "tool.add").unwrap();
    for (name, mode, service) in [
        ("model-provider", "model", "model"),
        ("tool-provider", "tools", "tools"),
    ] {
        let mut s = spec(name, mode);
        s.capabilities = [
            CAP_BACKEND.to_string(),
            CAP_EVENT.to_string(),
            format!("provide:{service}"),
        ]
        .into_iter()
        .collect();
        s.provides = vec![service.into()];
        rt.load_plugin(s).unwrap();
    }
    let mut s = spec("loop", "loop");
    s.level = Level::Main;
    s.capabilities = [CAP_TASK, CAP_CONTEXT, CAP_EVENT, "model.generate"]
        .into_iter()
        .map(String::from)
        .collect();
    if allow_tools {
        s.capabilities.insert("tool.add".into());
    }
    let aid = rt.load_plugin(s).unwrap().aid;
    (rt, aid)
}
fn task(rt: &mut Runtime, aid: Aid, scenario: &str) -> TaskId {
    let value = rt
        .request(
            Request::TaskCreate {
                owner: aid,
                parent: None,
                limits: ContextLimits::default(),
                seed: vec![input_item(json!({"scenario":scenario,"a":19,"b":23})).unwrap()],
            },
            WAIT,
        )
        .unwrap();
    serde_json::from_value(value["task"].clone()).unwrap()
}
fn until(rt: &mut Runtime, mut predicate: impl FnMut(&Runtime) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        if predicate(rt) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "loop process condition timed out"
        );
        rt.pump(Duration::from_millis(2));
    }
}
fn finish(rt: &mut Runtime, id: TaskId) -> Task {
    until(rt, |rt| {
        rt.kernel().task(id).unwrap().state != TaskState::Running
    });
    rt.kernel().task(id).unwrap().clone()
}
fn history(rt: &Runtime, id: TaskId) -> Vec<LoopMessage> {
    rt.kernel()
        .context(id)
        .unwrap()
        .history()
        .iter()
        .flat_map(|e| match e {
            ContextEntry::Seed { items, .. } | ContextEntry::Append { items, .. } => items.clone(),
            _ => vec![],
        })
        .filter_map(|i| serde_json::from_value(i.data).ok())
        .collect()
}
fn started(rt: &Runtime, operation: &str) -> usize {
    rt.kernel()
        .journal()
        .iter()
        .filter(|r| r.kind == "system.started" && r.data["operation"] == operation)
        .count()
}

#[test]
fn full_loop_runs_through_real_plugins_and_commits_tool_history() {
    let (mut rt, aid) = setup(true);
    let id = task(&mut rt, aid, "tool");
    let result = finish(&mut rt, id);
    assert_eq!(result.state, TaskState::Completed);
    assert_eq!(
        result.result.unwrap()["content"]["answer"],
        json!({"status":"ok","payload":{"sum":42}})
    );
    let messages = history(&rt, id);
    assert_eq!(messages.len(), 4);
    assert!(matches!(&messages[1],LoopMessage::Assistant{tool_calls,..} if tool_calls.len()==1));
    assert!(
        matches!(&messages[2],LoopMessage::Tool{result:BackendResult::Ok(value),..} if *value==json!({"sum":42}))
    );
    assert_eq!(started(&rt, "generate"), 2);
    assert_eq!(started(&rt, "add"), 1);
}

#[test]
fn waiting_on_one_model_does_not_block_another_task_in_the_same_loop_process() {
    let (mut rt, aid) = setup(true);
    let waiting = task(&mut rt, aid, "hold_model");
    until(&mut rt, |rt| started(rt, "generate") == 1);
    let fast = task(&mut rt, aid, "direct");
    let result = finish(&mut rt, fast);
    assert_eq!(result.state, TaskState::Completed);
    assert_eq!(result.result.unwrap()["content"], "offline direct reply");
    assert_eq!(rt.kernel().task(waiting).unwrap().state, TaskState::Running);
    rt.request(Request::TaskCancel { task: waiting }, WAIT)
        .unwrap();
    assert_eq!(
        rt.kernel().task(waiting).unwrap().state,
        TaskState::Cancelled
    );
    let followup = task(&mut rt, aid, "tool");
    assert_eq!(finish(&mut rt, followup).state, TaskState::Completed);
    assert!(rt.plugin("loop").is_some());
}

#[test]
fn cancel_during_tool_wait_stops_the_loop_and_does_not_replay_effects() {
    let (mut rt, aid) = setup(true);
    let id = task(&mut rt, aid, "hold_tool");
    until(&mut rt, |rt| started(rt, "add") == 1);
    rt.request(Request::TaskCancel { task: id }, WAIT).unwrap();
    let next = task(&mut rt, aid, "direct");
    assert_eq!(finish(&mut rt, next).state, TaskState::Completed);
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Cancelled);
    assert_eq!(started(&rt, "add"), 1);
    let messages = history(&rt, id);
    assert_eq!(messages.len(), 2);
    assert!(
        messages
            .iter()
            .all(|m| !matches!(m, LoopMessage::Tool { .. }))
    );
}

#[test]
fn kernel_tool_permission_is_enforced_even_when_model_requests_the_tool() {
    let (mut rt, aid) = setup(false);
    let id = task(&mut rt, aid, "tool");
    let result = finish(&mut rt, id);
    assert_eq!(result.state, TaskState::Failed);
    assert_eq!(result.result.unwrap()["error"]["code"], "permission_denied");
    assert_eq!(started(&rt, "add"), 0);
    assert!(rt.plugin("loop").is_some());
}

#[test]
fn backend_tool_error_is_visible_to_the_next_model_call() {
    let (mut rt, aid) = setup(true);
    let id = task(&mut rt, aid, "tool_error");
    let result = finish(&mut rt, id);
    assert_eq!(result.state, TaskState::Completed);
    assert_eq!(
        result.result.unwrap()["content"]["answer"],
        json!({"status":"error","payload":{"errno":77,"message":"native tool error"}})
    );
    assert!(history(&rt, id).iter().any(
        |m| matches!(m,LoopMessage::Tool{result:BackendResult::Error(v),..} if v["errno"]==77)
    ));
}

#[test]
fn model_errors_bad_payloads_and_unknown_tools_end_only_their_tasks() {
    let (mut rt, aid) = setup(true);
    for (scenario, code) in [
        ("model_error", "model_error"),
        ("malformed", "invalid_model_reply"),
        ("unknown_tool", "unknown_tool"),
    ] {
        let id = task(&mut rt, aid, scenario);
        let result = finish(&mut rt, id);
        assert_eq!(result.state, TaskState::Failed);
        assert_eq!(result.result.unwrap()["error"]["code"], code);
    }
    let id = task(&mut rt, aid, "direct");
    assert_eq!(finish(&mut rt, id).state, TaskState::Completed);
    assert_eq!(started(&rt, "add"), 0);
}

#[test]
fn model_timeout_fails_the_task_without_retrying_or_stopping_the_loop_plugin() {
    let (mut rt, aid) = setup(true);
    rt.set_call_timeout(Duration::from_millis(80));
    let id = task(&mut rt, aid, "hold_model");
    let result = finish(&mut rt, id);
    assert_eq!(result.state, TaskState::Failed);
    assert_eq!(result.result.unwrap()["error"]["code"], "timeout");
    assert_eq!(started(&rt, "generate"), 1);
    let next = task(&mut rt, aid, "direct");
    assert_eq!(finish(&mut rt, next).state, TaskState::Completed);
}

#[test]
fn unload_of_the_loop_cancels_task_calls_and_keeps_kernel_available() {
    let (mut rt, aid) = setup(true);
    let id = task(&mut rt, aid, "hold_model");
    until(&mut rt, |rt| started(rt, "generate") == 1);
    rt.unload_plugin("loop").unwrap();
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Failed);
    assert!(rt.request(Request::AgentGet { aid: HOST }, WAIT).is_ok());
    assert!(
        rt.kernel()
            .journal()
            .iter()
            .any(|r| r.kind == "system.failed")
    );
}

#[test]
fn external_compaction_wakes_a_paused_loop_without_editing_raw_history() {
    let (mut rt, aid) = setup(true);
    let mut seed = input_item(json!({"scenario":"direct"})).unwrap();
    seed.units = 80;
    let value = rt
        .request(
            Request::TaskCreate {
                owner: aid,
                parent: None,
                limits: ContextLimits {
                    capacity: 200,
                    output_reserve: 20,
                    headroom: 20,
                    pressure_bps: 4000,
                },
                seed: vec![seed],
            },
            WAIT,
        )
        .unwrap();
    let id: TaskId = serde_json::from_value(value["task"].clone()).unwrap();
    rt.request(
        Request::EventSubscribe {
            topic: format!("loop.{}", id.0),
        },
        WAIT,
    )
    .unwrap();
    let deadline = Instant::now() + WAIT;
    let mut paused = false;
    while !paused {
        assert!(Instant::now() < deadline);
        rt.pump(Duration::from_millis(2));
        while let Some(Notice::Event { event }) = rt.take_notice() {
            if event.data["event"]["kind"] == "needs_compaction" {
                paused = true;
            }
        }
    }
    assert_eq!(started(&rt, "generate"), 0);
    let replacement = input_item(json!({"scenario":"direct"})).unwrap();
    rt.request(
        Request::ContextReplace {
            task: id,
            expected_revision: 0,
            start: 0,
            end: 1,
            replacement: vec![replacement],
        },
        WAIT,
    )
    .unwrap();
    assert_eq!(finish(&mut rt, id).state, TaskState::Completed);
    assert!(
        rt.kernel()
            .context(id)
            .unwrap()
            .history()
            .iter()
            .any(|e| matches!(e,ContextEntry::Replace{original,..} if original[0].units==80))
    );
    assert_eq!(started(&rt, "generate"), 1);
}

#[test]
fn provider_crash_while_model_pending_fails_task_not_the_kernel() {
    let (mut rt, aid) = setup(true);
    let id = task(&mut rt, aid, "hold_model");
    until(&mut rt, |rt| started(rt, "generate") == 1);
    rt.unload_plugin("model-provider").unwrap();
    let result = finish(&mut rt, id);
    assert_eq!(result.state, TaskState::Failed);
    let code = result.result.unwrap()["error"]["code"].clone();
    assert_eq!(code, Value::String("plugin_unavailable".into()));
    assert!(rt.plugin("loop").is_some());
}
