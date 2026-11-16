//! Pull-loop tests use real kernel task/context APIs. Only the model and tool
//! service responses are scripted, so no network or process timing is involved.
use ithink::agent_loop::*;
use ithink::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

fn config() -> LoopConfig {
    let mut config = LoopConfig::new(ServiceRoute::new("model", "generate"));
    config.tools = vec![
        ToolDefinition {
            name: "lookup".into(),
            description: "Look up a value".into(),
            input_schema: json!({"type":"object","properties":{"key":{"type":"string"}}}),
            route: ServiceRoute::new("tools", "lookup"),
        },
        ToolDefinition {
            name: "save".into(),
            description: "Save a value".into(),
            input_schema: json!({"type":"object"}),
            route: ServiceRoute::new("tools", "save"),
        },
    ];
    config
}
fn task(rt: &mut Runtime, seed: Vec<ContextItem>, limits: ContextLimits) -> TaskId {
    let value = host(
        rt,
        Request::TaskCreate {
            owner: HOST,
            parent: None,
            limits,
            seed,
        },
    )
    .into_result()
    .unwrap();
    serde_json::from_value(value["task"].clone()).unwrap()
}
fn seeded_task(rt: &mut Runtime, text: &str) -> TaskId {
    task(
        rt,
        vec![input_item(json!(text)).unwrap()],
        ContextLimits::default(),
    )
}
fn host(rt: &mut Runtime, request: Request) -> ApiResult {
    let id = rt.submit(request).unwrap();
    rt.take_response(id)
        .expect("non-service kernel request must finish synchronously")
}
fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}
fn reply(content: Value, tool_calls: Vec<ToolCall>) -> ApiResult {
    backend(BackendResult::Ok(json!(ModelReply {
        content,
        tool_calls,
        usage: None
    })))
}
fn final_reply(content: Value) -> ApiResult {
    reply(content, vec![])
}
fn backend(result: BackendResult) -> ApiResult {
    ApiResult::Ok(json!(result))
}
fn api_error(code: &str) -> ApiResult {
    ApiResult::Error(KernelError::new(code, format!("scripted {code}")))
}

#[derive(Default)]
struct Script {
    models: VecDeque<ApiResult>,
    tools: VecDeque<ApiResult>,
    model_inputs: Vec<ModelInput>,
    tool_inputs: Vec<(TaskId, String, String, Value)>,
    requests: Vec<Request>,
}
impl Script {
    fn models(models: Vec<ApiResult>) -> Self {
        Self {
            models: models.into(),
            ..Self::default()
        }
    }
    fn answer(&mut self, rt: &mut Runtime, request: Request) -> ApiResult {
        self.requests.push(request.clone());
        match request {
            Request::TaskSystemCall {
                task,
                service,
                operation,
                payload,
            } if service == "model" => {
                assert_eq!(operation, "generate");
                let input: ModelInput = serde_json::from_value(payload).unwrap();
                assert_eq!(input.task, task);
                self.model_inputs.push(input);
                self.models.pop_front().expect("unexpected model request")
            }
            Request::TaskSystemCall {
                task,
                service,
                operation,
                payload,
            } => {
                self.tool_inputs.push((task, service, operation, payload));
                self.tools.pop_front().expect("unexpected tool execution")
            }
            request => host(rt, request),
        }
    }
    fn drive(&mut self, rt: &mut Runtime, agent: &mut AgentLoop) {
        for _ in 0..1000 {
            let Some(request) = agent.next_request() else {
                return;
            };
            let result = self.answer(rt, request.request);
            assert!(agent.accept_response(request.id, result));
        }
        panic!("loop did not stop, wait or pause within bounded test steps");
    }
}
fn failed(agent: &AgentLoop) -> &KernelError {
    match agent.outcome().expect("loop outcome") {
        LoopOutcome::Failed { error, .. } => error,
        outcome => panic!("expected failure, got {outcome:?}"),
    }
}
fn original_items(rt: &Runtime, id: TaskId) -> Vec<ContextItem> {
    rt.kernel()
        .context(id)
        .unwrap()
        .history()
        .iter()
        .flat_map(|entry| match entry {
            ContextEntry::Seed { items, .. } | ContextEntry::Append { items, .. } => items.clone(),
            _ => vec![],
        })
        .collect()
}
fn messages(rt: &Runtime, id: TaskId) -> Vec<LoopMessage> {
    original_items(rt, id)
        .into_iter()
        .map(|item| serde_json::from_value(item.data).unwrap())
        .collect()
}

#[test]
fn direct_final_commits_original_order_usage_and_task_completion() {
    let mut rt = Runtime::new();
    let seed = input_item(json!({"text":"keep whitespace  \n and exact data"})).unwrap();
    let id = task(&mut rt, vec![seed.clone()], ContextLimits::default());
    let mut agent = AgentLoop::new(id, config()).unwrap();
    let usage = json!({"prompt_cache_hit_tokens":128,"prompt_cache_miss_tokens":3});
    let mut script = Script::models(vec![backend(BackendResult::Ok(json!(ModelReply {
        content: json!({"answer":"done"}),
        tool_calls: vec![],
        usage: Some(usage.clone()),
    })))]);
    script.drive(&mut rt, &mut agent);
    assert_eq!(
        agent.outcome(),
        Some(&LoopOutcome::Completed {
            content: json!({"answer":"done"}),
            steps: 1
        })
    );
    assert_eq!(agent.status(), LoopStatus::Ended);
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Completed);
    assert_eq!(script.model_inputs.len(), 1);
    assert_eq!(script.model_inputs[0].context, vec![seed.clone()]);
    assert_eq!(script.model_inputs[0].tools[0].name, "lookup");
    assert_eq!(
        script.model_inputs[0].tools[0].input_schema,
        config().tools[0].input_schema
    );
    assert!(script.tool_inputs.is_empty());
    assert_eq!(agent.usage, vec![usage.clone()]);
    assert_eq!(
        rt.kernel().task(id).unwrap().result.as_ref().unwrap()["usage"],
        json!([usage])
    );
    let items = original_items(&rt, id);
    assert_eq!(items[0], seed);
    assert!(
        matches!(&messages(&rt,id)[1], LoopMessage::Assistant{content,tool_calls} if *content==json!({"answer":"done"})&&tool_calls.is_empty())
    );
    assert!(agent.take_events().iter().any(|event| matches!(
        event,
        LoopEvent::Ended {
            outcome: LoopOutcome::Completed { .. }
        }
    )));
    assert!(agent.next_request().is_none());
}

#[test]
fn tool_results_are_committed_with_balanced_ids_and_seen_by_next_model_step() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "find and save");
    let lookup = call("call-1", "lookup", json!({"key":"x"}));
    let save = call("call-2", "save", json!({"value":42}));
    let mut script = Script::models(vec![
        reply(json!("checking"), vec![lookup.clone(), save.clone()]),
        final_reply(json!("saved 42")),
    ]);
    script.tools.extend([
        backend(BackendResult::Ok(json!({"value":42}))),
        backend(BackendResult::Ok(json!({"saved":true}))),
    ]);
    let mut agent = AgentLoop::new(id, config()).unwrap();
    script.drive(&mut rt, &mut agent);
    assert_eq!(
        agent.outcome(),
        Some(&LoopOutcome::Completed {
            content: json!("saved 42"),
            steps: 2
        })
    );
    assert_eq!(
        script.tool_inputs,
        vec![
            (id, "tools".into(), "lookup".into(), json!({"key":"x"})),
            (id, "tools".into(), "save".into(), json!({"value":42})),
        ]
    );
    let next = &script.model_inputs[1];
    assert_eq!(next.context.len(), 4);
    assert_eq!(next.context[1].opens, vec!["call-1", "call-2"]);
    assert_eq!(next.context[2].closes, vec!["call-1"]);
    assert_eq!(next.context[3].closes, vec!["call-2"]);
    let next_messages: Vec<LoopMessage> = next
        .context
        .iter()
        .map(|item| serde_json::from_value(item.data.clone()).unwrap())
        .collect();
    assert!(
        matches!(&next_messages[2],LoopMessage::Tool{call_id,result:BackendResult::Ok(value),..} if call_id=="call-1"&&*value==json!({"value":42}))
    );
    assert!(
        matches!(&next_messages[3],LoopMessage::Tool{call_id,result:BackendResult::Ok(value),..} if call_id=="call-2"&&*value==json!({"saved":true}))
    );
    assert_eq!(original_items(&rt, id).len(), 5);
    assert!(matches!(
        rt.kernel().context(id).unwrap().history().last(),
        Some(ContextEntry::ReleaseView { .. })
    ));
}

#[test]
fn duplicate_tool_ids_in_one_reply_are_rejected_before_any_tool_executes() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "do something");
    let mut script = Script::models(vec![reply(
        json!(null),
        vec![
            call("duplicate", "lookup", json!({})),
            call("duplicate", "save", json!({})),
        ],
    )]);
    let mut agent = AgentLoop::new(id, config()).unwrap();
    script.drive(&mut rt, &mut agent);
    assert_eq!(failed(&agent).code, "invalid_tool_call");
    assert!(script.tool_inputs.is_empty());
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Failed);
    assert_eq!(original_items(&rt, id).len(), 1);
}

#[test]
fn previous_tool_id_cannot_execute_again_in_a_later_model_reply() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "once only");
    let reused = call("once", "lookup", json!({}));
    let mut script = Script::models(vec![
        reply(json!(null), vec![reused.clone()]),
        reply(json!(null), vec![reused]),
    ]);
    script
        .tools
        .push_back(backend(BackendResult::Ok(json!(42))));
    let mut agent = AgentLoop::new(id, config()).unwrap();
    script.drive(&mut rt, &mut agent);
    assert_eq!(failed(&agent).code, "invalid_tool_call");
    assert_eq!(script.tool_inputs.len(), 1);
}

#[test]
fn an_unknown_tool_rejects_the_entire_model_reply_without_partial_execution() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "unknown");
    let mut script = Script::models(vec![reply(
        json!(null),
        vec![
            call("allowed", "lookup", json!({})),
            call("unknown", "not-installed", json!({})),
        ],
    )]);
    let mut agent = AgentLoop::new(id, config()).unwrap();
    script.drive(&mut rt, &mut agent);
    assert_eq!(failed(&agent).code, "unknown_tool");
    assert!(script.tool_inputs.is_empty());
    assert_eq!(original_items(&rt, id).len(), 1);
}

#[test]
fn model_native_error_is_not_a_tool_result_or_success() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "question");
    let native = json!({"code":"provider_down","detail":"model response"});
    let mut script = Script::models(vec![backend(BackendResult::Error(native.clone()))]);
    let mut agent = AgentLoop::new(id, config()).unwrap();
    script.drive(&mut rt, &mut agent);
    assert_eq!(failed(&agent).code, "model_error");
    assert!(failed(&agent).message.contains("provider_down"));
    assert!(script.tool_inputs.is_empty());
    assert_eq!(script.model_inputs.len(), 1);
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Failed);
    assert_eq!(original_items(&rt, id).len(), 1);
}

#[test]
fn invalid_model_payload_fails_without_executing_tools() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "question");
    let mut script = Script::models(vec![backend(BackendResult::Ok(
        json!({"not":"a model reply"}),
    ))]);
    let mut agent = AgentLoop::new(id, config()).unwrap();
    script.drive(&mut rt, &mut agent);
    assert_eq!(failed(&agent).code, "invalid_model_reply");
    assert!(script.tool_inputs.is_empty());
}

#[test]
fn native_tool_error_is_returned_to_context_and_model_may_continue() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "try lookup");
    let native = json!({"errno":404,"message":"missing key","extra":{"opaque":[1,2,3]}});
    let mut script = Script::models(vec![
        reply(
            json!("looking"),
            vec![call("lookup-1", "lookup", json!({"key":"none"}))],
        ),
        final_reply(json!("nothing found")),
    ]);
    script
        .tools
        .push_back(backend(BackendResult::Error(native.clone())));
    let mut agent = AgentLoop::new(id, config()).unwrap();
    script.drive(&mut rt, &mut agent);
    assert!(matches!(
        agent.outcome(),
        Some(LoopOutcome::Completed { steps: 2, .. })
    ));
    let tool: LoopMessage =
        serde_json::from_value(script.model_inputs[1].context[2].data.clone()).unwrap();
    assert!(
        matches!(tool,LoopMessage::Tool{result:BackendResult::Error(ref value),..} if *value==native)
    );
    assert_eq!(script.model_inputs[1].context[2].closes, vec!["lookup-1"]);
    assert_eq!(script.tool_inputs.len(), 1);
}

#[test]
fn denied_or_timed_out_tool_calls_fail_without_retry_or_fabricated_result() {
    for code in ["permission_denied", "timeout"] {
        let mut rt = Runtime::new();
        let id = seeded_task(&mut rt, "attempt action");
        let mut script = Script::models(vec![reply(
            json!(null),
            vec![call("action", "save", json!({"v":1}))],
        )]);
        script.tools.push_back(api_error(code));
        let mut agent = AgentLoop::new(id, config()).unwrap();
        script.drive(&mut rt, &mut agent);
        assert_eq!(failed(&agent).code, code);
        assert_eq!(script.tool_inputs.len(), 1);
        assert_eq!(script.model_inputs.len(), 1);
        assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Failed);
        let history = original_items(&rt, id);
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].opens, vec!["action"]);
        assert!(history.iter().all(|item| item.closes.is_empty()));
        assert!(agent.next_request().is_none());
    }
}

#[test]
fn maximum_steps_ends_the_task_instead_of_requesting_another_model_turn() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "bounded work");
    let mut cfg = config();
    cfg.max_steps = 1;
    let mut script = Script::models(vec![reply(
        json!(null),
        vec![call("one", "lookup", json!({}))],
    )]);
    script.tools.push_back(backend(BackendResult::Ok(json!(1))));
    let mut agent = AgentLoop::new(id, cfg).unwrap();
    script.drive(&mut rt, &mut agent);
    assert_eq!(failed(&agent).code, "step_limit");
    assert_eq!(script.model_inputs.len(), 1);
    assert_eq!(script.tool_inputs.len(), 1);
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Failed);
    assert_eq!(original_items(&rt, id).len(), 3);
}

#[test]
fn cancellation_ignores_an_inflight_late_response_and_never_executes_its_tools() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "cancel me");
    let mut agent = AgentLoop::new(id, config()).unwrap();
    let read = agent.next_request().unwrap();
    assert!(agent.accept_response(read.id, host(&mut rt, read.request)));
    let model = agent.next_request().unwrap();
    assert!(matches!(model.request, Request::TaskSystemCall { .. }));
    assert!(agent.next_request().is_none());
    agent.cancel();
    assert!(!agent.accept_response(
        model.id,
        reply(json!(null), vec![call("late", "save", json!({}))])
    ));
    let cancel = agent.next_request().unwrap();
    assert!(matches!(cancel.request,Request::TaskCancel{task} if task==id));
    assert_ne!(cancel.id, model.id);
    assert!(agent.accept_response(cancel.id, host(&mut rt, cancel.request)));
    assert!(agent.outcome().is_none());
    let terminal = agent.next_request().unwrap();
    assert!(matches!(terminal.request, Request::TaskGet { task } if task == id));
    assert!(agent.accept_response(terminal.id, host(&mut rt, terminal.request)));
    assert_eq!(agent.outcome(), Some(&LoopOutcome::Cancelled { steps: 1 }));
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Cancelled);
    assert!(!agent.accept_response(model.id, final_reply(json!("too late"))));
    assert!(agent.next_request().is_none());
    assert_eq!(original_items(&rt, id).len(), 1);
}

#[test]
fn unrelated_response_and_unrelated_task_notice_do_not_advance_a_loop() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "matching only");
    let mut agent = AgentLoop::new(id, config()).unwrap();
    let read = agent.next_request().unwrap();
    assert!(!agent.accept_response(LoopRequestId(read.id.0 + 1), ApiResult::Ok(json!({}))));
    agent.on_notice(&Notice::TaskEnded {
        task: TaskId(id.0 + 999),
        state: TaskState::Cancelled,
        result: None,
    });
    assert_eq!(agent.status(), LoopStatus::Waiting);
    assert!(agent.outcome().is_none());
    assert!(agent.accept_response(read.id, host(&mut rt, read.request)));
    agent.on_notice(&Notice::TaskEnded {
        task: id,
        state: TaskState::Cancelled,
        result: None,
    });
    assert!(matches!(
        agent.outcome(),
        Some(LoopOutcome::Cancelled { .. })
    ));
    assert!(agent.next_request().is_none());
}

#[test]
fn initial_context_with_unanswered_tool_call_requires_explicit_recovery() {
    let mut rt = Runtime::new();
    let assistant = LoopMessage::Assistant {
        content: json!("incomplete"),
        tool_calls: vec![call("open", "lookup", json!({}))],
    };
    let seed = ContextItem {
        data: json!(assistant),
        units: 10,
        opens: vec!["open".into()],
        closes: vec![],
        pinned: false,
    };
    let id = task(&mut rt, vec![seed.clone()], ContextLimits::default());
    let mut agent = AgentLoop::new(id, config()).unwrap();
    let mut script = Script::default();
    script.drive(&mut rt, &mut agent);
    assert_eq!(failed(&agent).code, "incomplete_context");
    assert!(script.model_inputs.is_empty());
    assert!(script.tool_inputs.is_empty());
    assert_eq!(original_items(&rt, id), vec![seed]);
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Failed);
}

#[test]
fn context_pressure_pauses_until_user_commits_a_smaller_replacement() {
    let mut rt = Runtime::new();
    let seed = ContextItem::text("original transcript", 100);
    let limits = ContextLimits {
        capacity: 200,
        output_reserve: 20,
        headroom: 20,
        pressure_bps: 5000,
    };
    let id = task(&mut rt, vec![seed.clone()], limits);
    let mut agent = AgentLoop::new(id, config()).unwrap();
    let mut script = Script::models(vec![final_reply(json!("resumed"))]);
    script.drive(&mut rt, &mut agent);
    assert_eq!(agent.status(), LoopStatus::NeedsCompaction);
    assert!(agent.outcome().is_none());
    assert!(script.model_inputs.is_empty());
    let before = rt.kernel().context(id).unwrap().snapshot();
    assert_eq!(before.items, vec![seed.clone()]);
    assert!(agent.take_events().iter().any(|event| matches!(
        event,
        LoopEvent::NeedsCompaction {
            used: 100,
            required: 0,
            ..
        }
    )));
    let summary = ContextItem::text("summary", 5);
    host(
        &mut rt,
        Request::ContextReplace {
            task: id,
            expected_revision: before.revision,
            start: 0,
            end: 1,
            replacement: vec![summary.clone()],
        },
    )
    .into_result()
    .unwrap();
    agent.resume_after_compaction().unwrap();
    script.drive(&mut rt, &mut agent);
    assert!(matches!(
        agent.outcome(),
        Some(LoopOutcome::Completed { steps: 1, .. })
    ));
    assert_eq!(script.model_inputs[0].context, vec![summary]);
    assert!(rt.kernel().context(id).unwrap().history().iter().any(
        |entry| matches!(entry,ContextEntry::Replace{original,..} if *original==vec![seed.clone()])
    ));
}

#[test]
fn external_task_failure_is_not_mistaken_for_cancel_when_old_call_reply_arrives_first() {
    let mut rt = Runtime::new();
    rt.define_operation("model", "generate", "model.call")
        .unwrap();
    host(
        &mut rt,
        Request::BackendRegister {
            service: "model".into(),
        },
    )
    .into_result()
    .unwrap();
    let id = seeded_task(&mut rt, "fail while model is running");
    let mut agent = AgentLoop::new(id, config()).unwrap();
    let read = agent.next_request().unwrap();
    assert!(agent.accept_response(read.id, host(&mut rt, read.request)));
    let pending = agent.next_request().unwrap();
    assert!(matches!(pending.request, Request::TaskSystemCall { task, .. } if task == id));
    let wire_id = rt.submit(pending.request).unwrap();
    assert!(rt.take_response(wire_id).is_none());
    let external = KernelError::new("supervisor_failed", "worker invalidated externally");
    host(
        &mut rt,
        Request::TaskFail {
            task: id,
            error: external.clone(),
        },
    )
    .into_result()
    .unwrap();

    // Runtime emits the cancelled old-call response before the TaskEnded notice.
    // Deliberately deliver only that response first; it does not mean Cancelled.
    let old = rt.take_response(wire_id).unwrap();
    assert!(matches!(&old, ApiResult::Error(error) if error.code == "cancelled"));
    assert!(agent.accept_response(pending.id, old));
    assert!(agent.outcome().is_none());
    let terminal = agent.next_request().unwrap();
    assert!(matches!(terminal.request, Request::TaskGet { task } if task == id));
    assert!(agent.accept_response(terminal.id, host(&mut rt, terminal.request)));
    assert_eq!(failed(&agent), &external);
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Failed);
    assert_eq!(agent.status(), LoopStatus::Ended);
    assert!(!agent.accept_response(pending.id, final_reply(json!("too late"))));
    while let Some(notice) = rt.take_notice() {
        agent.on_notice(&notice);
    }
    assert_eq!(failed(&agent), &external);
}

#[test]
fn cancellation_racing_with_committed_finish_reports_completion_not_cancellation() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "finish won the race");
    let mut agent = AgentLoop::new(id, config()).unwrap();
    let read = agent.next_request().unwrap();
    assert!(agent.accept_response(read.id, host(&mut rt, read.request)));
    let model = agent.next_request().unwrap();
    assert!(agent.accept_response(model.id, final_reply(json!("already finished"))));
    let append = agent.next_request().unwrap();
    assert!(matches!(append.request, Request::ContextAppend { .. }));
    assert!(agent.accept_response(append.id, host(&mut rt, append.request)));
    let finish = agent.next_request().unwrap();
    assert!(matches!(finish.request, Request::TaskFinish { task, .. } if task == id));
    let finish_response = host(&mut rt, finish.request);
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Completed);

    // The completion is committed, but its response has not reached this loop.
    agent.cancel();
    assert!(!agent.accept_response(finish.id, finish_response));
    let cancel = agent.next_request().unwrap();
    assert!(matches!(cancel.request, Request::TaskCancel { task } if task == id));
    assert!(agent.accept_response(cancel.id, host(&mut rt, cancel.request)));
    assert!(agent.outcome().is_none());
    let terminal = agent.next_request().unwrap();
    assert!(matches!(terminal.request, Request::TaskGet { task } if task == id));
    assert!(agent.accept_response(terminal.id, host(&mut rt, terminal.request)));
    assert_eq!(
        agent.outcome(),
        Some(&LoopOutcome::Completed {
            content: json!("already finished"),
            steps: 1
        })
    );
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Completed);
    assert!(agent.next_request().is_none());
}

#[test]
fn resume_outside_compaction_never_changes_ready_waiting_or_ended_stages() {
    fn reject_resume(agent: &mut AgentLoop) {
        let before = agent.status();
        let outcome = agent.outcome().cloned();
        assert_eq!(
            agent.resume_after_compaction().unwrap_err().code,
            "invalid_state"
        );
        assert_eq!(agent.status(), before);
        assert_eq!(agent.outcome(), outcome.as_ref());
    }
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "invalid resume must be harmless");
    let mut agent = AgentLoop::new(id, config()).unwrap();
    reject_resume(&mut agent);
    let read = agent.next_request().unwrap();
    assert!(matches!(read.request, Request::ContextRead { .. }));
    reject_resume(&mut agent);
    assert!(agent.next_request().is_none());
    assert!(agent.accept_response(read.id, host(&mut rt, read.request)));
    reject_resume(&mut agent);
    let model = agent.next_request().unwrap();
    assert!(matches!(model.request, Request::TaskSystemCall { .. }));
    reject_resume(&mut agent);
    assert!(agent.accept_response(model.id, final_reply(json!("done"))));
    reject_resume(&mut agent);
    let append = agent.next_request().unwrap();
    assert!(matches!(append.request, Request::ContextAppend { .. }));
    reject_resume(&mut agent);
    assert!(agent.accept_response(append.id, host(&mut rt, append.request)));
    reject_resume(&mut agent);
    let finish = agent.next_request().unwrap();
    assert!(matches!(finish.request, Request::TaskFinish { .. }));
    reject_resume(&mut agent);
    assert!(agent.accept_response(finish.id, host(&mut rt, finish.request)));
    reject_resume(&mut agent);
    assert_eq!(
        agent.outcome(),
        Some(&LoopOutcome::Completed {
            content: json!("done"),
            steps: 1
        })
    );
    assert!(agent.next_request().is_none());
}

fn thirty_units(_: &Value) -> Result<u64, KernelError> {
    Ok(30)
}

#[test]
fn compaction_after_a_tool_effect_preserves_its_output_without_executing_it_twice() {
    let mut rt = Runtime::new();
    let seed = ContextItem::text("old request", 60);
    let limits = ContextLimits {
        capacity: 100,
        output_reserve: 0,
        headroom: 10,
        pressure_bps: 9000,
    };
    let id = task(&mut rt, vec![seed], limits);
    let mut cfg = config();
    cfg.meter = thirty_units;
    let mut script = Script::models(vec![
        reply(
            json!("doing effect"),
            vec![call("effect", "save", json!({"value":1}))],
        ),
        final_reply(json!("done")),
    ]);
    script.tools.push_back(backend(BackendResult::Ok(
        json!({"external_effect":"already happened"}),
    )));
    let mut agent = AgentLoop::new(id, cfg).unwrap();
    script.drive(&mut rt, &mut agent);
    assert_eq!(agent.status(), LoopStatus::NeedsCompaction);
    assert_eq!(script.tool_inputs.len(), 1);
    let before = rt.kernel().context(id).unwrap().snapshot();
    assert_eq!(before.used, 90);
    assert_eq!(before.items.len(), 2);
    assert_eq!(before.items[1].opens, vec!["effect"]);
    assert!(
        agent
            .take_events()
            .iter()
            .any(|event| matches!(event, LoopEvent::NeedsCompaction { required: 30, .. }))
    );
    host(
        &mut rt,
        Request::ContextReplace {
            task: id,
            expected_revision: before.revision,
            start: 0,
            end: 1,
            replacement: vec![ContextItem::text("short", 5)],
        },
    )
    .into_result()
    .unwrap();
    agent.resume_after_compaction().unwrap();
    script.drive(&mut rt, &mut agent);
    assert!(matches!(
        agent.outcome(),
        Some(LoopOutcome::Completed { steps: 2, .. })
    ));
    assert_eq!(script.tool_inputs.len(), 1);
    let next = &script.model_inputs[1].context;
    assert_eq!(next.len(), 3);
    assert_eq!(next[2].closes, vec!["effect"]);
    let output: LoopMessage = serde_json::from_value(next[2].data.clone()).unwrap();
    assert!(
        matches!(output,LoopMessage::Tool{result:BackendResult::Ok(value),..} if value==json!({"external_effect":"already happened"}))
    );
}

#[test]
fn uncompressible_context_remains_intact_and_no_model_is_called() {
    let mut rt = Runtime::new();
    let mut pinned = ContextItem::text("must remain", 90);
    pinned.pinned = true;
    let limits = ContextLimits {
        capacity: 100,
        output_reserve: 0,
        headroom: 10,
        pressure_bps: 9000,
    };
    let id = task(&mut rt, vec![pinned.clone()], limits);
    let mut agent = AgentLoop::new(id, config()).unwrap();
    let mut script = Script::default();
    script.drive(&mut rt, &mut agent);
    assert_eq!(agent.status(), LoopStatus::NeedsCompaction);
    let before = rt.kernel().context(id).unwrap().snapshot();
    assert!(
        host(
            &mut rt,
            Request::ContextReplace {
                task: id,
                expected_revision: before.revision,
                start: 0,
                end: 1,
                replacement: vec![ContextItem::text("drop", 1)]
            }
        )
        .into_result()
        .is_err()
    );
    assert_eq!(rt.kernel().context(id).unwrap().snapshot(), before);
    agent.resume_after_compaction().unwrap();
    script.drive(&mut rt, &mut agent);
    assert_eq!(agent.status(), LoopStatus::NeedsCompaction);
    assert_eq!(rt.kernel().context(id).unwrap().snapshot(), before);
    assert!(script.model_inputs.is_empty());
    assert!(script.tool_inputs.is_empty());
    agent.cancel();
    script.drive(&mut rt, &mut agent);
    assert!(matches!(
        agent.outcome(),
        Some(LoopOutcome::Cancelled { .. })
    ));
    assert_eq!(original_items(&rt, id), vec![pinned]);
}

#[test]
fn large_native_usage_across_steps_does_not_make_a_normal_task_fail() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "three model steps with large provider metadata");
    let usage = json!({"native_blob":"x".repeat(200 * 1024),"prompt_cache_hit_tokens":123});
    let model_with_usage = |content: Value, tool_calls: Vec<ToolCall>| {
        backend(BackendResult::Ok(json!(ModelReply {
            content,
            tool_calls,
            usage: Some(usage.clone()),
        })))
    };
    let mut script = Script::models(vec![
        model_with_usage(
            json!("first action"),
            vec![call("step-1", "lookup", json!({}))],
        ),
        model_with_usage(
            json!("second action"),
            vec![call("step-2", "save", json!({}))],
        ),
        model_with_usage(json!("complete"), vec![]),
    ]);
    script.tools.extend([
        backend(BackendResult::Ok(json!(1))),
        backend(BackendResult::Ok(json!(2))),
    ]);
    let mut agent = AgentLoop::new(id, config()).unwrap();
    script.drive(&mut rt, &mut agent);
    assert_eq!(
        agent.outcome(),
        Some(&LoopOutcome::Completed {
            content: json!("complete"),
            steps: 3
        })
    );
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Completed);
    assert_eq!(script.model_inputs.len(), 3);
    assert_eq!(script.tool_inputs.len(), 2);
    assert!(agent.usage.is_empty());
    assert_eq!(agent.usage_dropped, 3);
    let result = rt.kernel().task(id).unwrap().result.as_ref().unwrap();
    assert_eq!(result["content"], json!("complete"));
    assert_eq!(result["usage"], json!([]));
    assert_eq!(result["usage_dropped"], json!(3));
    assert!(serde_json::to_vec(result).unwrap().len() < 1024);
    let usages: Vec<Value> = agent
        .take_events()
        .into_iter()
        .filter_map(|event| match event {
            LoopEvent::ModelOutput { reply, .. } => reply.usage,
            _ => None,
        })
        .collect();
    assert_eq!(usages, vec![usage.clone(), usage.clone(), usage]);
    // Accounting metadata belongs to events/final accounting, not the next
    // model's conversation context; tool results remain the real context.
    assert!(script.model_inputs.iter().all(|input| {
        input
            .context
            .iter()
            .all(|item| item.data.get("usage").is_none())
    }));
}

#[test]
fn failure_finalization_denied_requires_intervention_until_external_terminal_notice() {
    let mut rt = Runtime::new();
    let id = seeded_task(&mut rt, "failure must really be committed");
    let mut agent = AgentLoop::new(id, config()).unwrap();
    let read = agent.next_request().unwrap();
    assert!(agent.accept_response(read.id, host(&mut rt, read.request)));
    let model = agent.next_request().unwrap();
    assert!(agent.accept_response(model.id, api_error("model_transport_failed")));
    let fail = agent.next_request().unwrap();
    assert!(
        matches!(&fail.request, Request::TaskFail { task, error } if *task == id && error.code == "model_transport_failed")
    );
    // Do not send TaskFail to the host: emulate its actual caller lacking the
    // lifecycle capability, leaving the authoritative task state Running.
    assert!(agent.accept_response(fail.id, api_error("permission_denied")));
    assert!(agent.outcome().is_none());
    let terminal = agent.next_request().unwrap();
    assert!(matches!(terminal.request, Request::TaskGet { task } if task == id));
    assert!(agent.accept_response(terminal.id, host(&mut rt, terminal.request)));
    assert_eq!(agent.status(), LoopStatus::NeedsIntervention);
    assert!(agent.outcome().is_none());
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Running);
    let events = agent.take_events();
    assert!(events.iter().any(|event| matches!(event, LoopEvent::NeedsIntervention { error } if error.code == "finalization_failed" && error.message.contains("permission_denied"))));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LoopEvent::Ended { .. }))
    );
    for _ in 0..3 {
        assert!(agent.next_request().is_none());
    }
    assert_eq!(original_items(&rt, id).len(), 1);

    host(&mut rt, Request::TaskCancel { task: id })
        .into_result()
        .unwrap();
    while let Some(notice) = rt.take_notice() {
        agent.on_notice(&notice);
    }
    assert_eq!(agent.status(), LoopStatus::Ended);
    assert_eq!(agent.outcome(), Some(&LoopOutcome::Cancelled { steps: 1 }));
    assert_eq!(rt.kernel().task(id).unwrap().state, TaskState::Cancelled);
    assert!(agent.next_request().is_none());
    assert!(!agent.accept_response(fail.id, ApiResult::Ok(json!({"task":id}))));
}

#[test]
fn two_loops_share_a_driver_without_crossing_local_ids_context_or_outcomes() {
    let mut rt = Runtime::new();
    let one = seeded_task(&mut rt, "task one");
    let two = seeded_task(&mut rt, "task two");
    let mut loops = [
        AgentLoop::new(one, config()).unwrap(),
        AgentLoop::new(two, config()).unwrap(),
    ];
    let mut scripts = [
        Script::models(vec![final_reply(json!("answer one"))]),
        Script::models(vec![final_reply(json!("answer two"))]),
    ];
    // Local IDs deliberately overlap. A real plugin driver must correlate the
    // distinct wire ID with (task/loop, local ID), not dispatch by local ID alone.
    let mut overlaps = 0;
    for _ in 0..100 {
        let mut routed = BTreeMap::new();
        let mut locals = vec![];
        for index in 0..loops.len() {
            if let Some(request) = loops[index].next_request() {
                locals.push(request.id);
                let response = scripts[index].answer(&mut rt, request.request);
                let wire_id = (routed.len() + 1) as u64;
                routed.insert(wire_id, (index, request.id, response));
            }
        }
        if locals.len() == 2 && locals[0] == locals[1] {
            overlaps += 1;
        }
        if routed.is_empty() {
            break;
        }
        for (_, (index, id, response)) in routed.into_iter().rev() {
            assert!(loops[index].accept_response(id, response));
        }
    }
    assert!(overlaps > 0);
    assert_eq!(
        loops[0].outcome(),
        Some(&LoopOutcome::Completed {
            content: json!("answer one"),
            steps: 1
        })
    );
    assert_eq!(
        loops[1].outcome(),
        Some(&LoopOutcome::Completed {
            content: json!("answer two"),
            steps: 1
        })
    );
    assert_eq!(scripts[0].model_inputs[0].task, one);
    assert_eq!(scripts[1].model_inputs[0].task, two);
    assert_eq!(
        scripts[0].model_inputs[0].context[0].data,
        json!(LoopMessage::User {
            content: json!("task one")
        })
    );
    assert_eq!(
        scripts[1].model_inputs[0].context[0].data,
        json!(LoopMessage::User {
            content: json!("task two")
        })
    );
    assert!(
        messages(&rt, one).iter().any(
            |m| matches!(m,LoopMessage::Assistant{content,..} if *content==json!("answer one"))
        )
    );
    assert!(
        messages(&rt, two).iter().any(
            |m| matches!(m,LoopMessage::Assistant{content,..} if *content==json!("answer two"))
        )
    );
}
