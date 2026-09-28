//! Public turn admission uses a real Session, local journal, provider and ToolCallRuntime.
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolContributor;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_panng_runtime::ExecutionOutcome;
use codex_panng_runtime::OperationState;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use codex_tools::JsonToolOutput;
use codex_tools::ToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolExecutorFuture;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::Semaphore;

struct Gate {
    a_finished: Notify,
    b_started: Notify,
    b_exited: Notify,
    c_started: Notify,
    b_active: AtomicBool,
    c_observed_pending_b: AtomicBool,
    release: Semaphore,
}

struct Active<'a>(&'a Gate);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.b_active.store(/*val*/ false, Ordering::SeqCst);
        self.0.b_exited.notify_one();
    }
}

#[derive(Clone)]
struct ProbeTool(Arc<Gate>);
impl ToolContributor for ProbeTool {
    fn tools(
        &self,
        _: &ExtensionData,
        _: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        vec![Arc::new(self.clone())]
    }
}
impl<'call> ToolExecutor<ToolCall<'call>> for ProbeTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("panng_probe")
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(codex_tools::ResponsesApiTool {
            name: "panng_probe".into(),
            description: "Deterministic prototype gate".into(),
            strict: false,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
            defer_loading: None,
        })
    }
    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let gate = &self.0;
            if call.call_id == "b" {
                gate.b_active.store(/*val*/ true, Ordering::SeqCst);
                let _active = Active(gate);
                gate.b_started.notify_one();
                gate.release
                    .acquire()
                    .await
                    .map_err(|error| codex_tools::FunctionCallError::Fatal(error.to_string()))?
                    .forget();
            } else if call.call_id == "c" {
                gate.c_observed_pending_b
                    .store(gate.b_active.load(Ordering::SeqCst), Ordering::SeqCst);
                gate.c_started.notify_one();
                gate.release.add_permits(/*n*/ 1);
            } else if call.call_id == "a" {
                gate.b_started.notified().await;
                gate.a_finished.notify_one();
            }
            Ok(Box::new(JsonToolOutput::new(
                serde_json::json!({"result": format!("result from {}", call.call_id)}),
            )) as Box<dyn ToolOutput>)
        })
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    Complete,
    Interrupt,
}

async fn scenario(scenario: Scenario) -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = requests.clone();
    let second_request = Arc::new(Notify::new());
    let observed_second = second_request.clone();
    let gate = Arc::new(Gate {
        a_finished: Notify::new(),
        b_started: Notify::new(),
        b_exited: Notify::new(),
        c_started: Notify::new(),
        b_active: AtomicBool::new(/*v*/ false),
        c_observed_pending_b: AtomicBool::new(/*v*/ false),
        release: Semaphore::new(/*permits*/ 0),
    });
    let observing_gate = gate.clone();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let bytes = if request
                .headers
                .get("content-encoding")
                .is_some_and(|value| value == "zstd")
            {
                zstd::stream::decode_all(request.body.as_slice()).expect("request decode")
            } else {
                request.body.clone()
            };
            let mut requests = captured.lock().expect("request capture");
            let index = requests.len();
            let body: serde_json::Value =
                serde_json::from_slice(&bytes).expect("provider request JSON");
            if index == 1 {
                assert!(observing_gate.b_active.load(Ordering::SeqCst));
                assert!(body["input"].to_string().contains("result from a"));
                assert!(!body["input"].to_string().contains("result from b"));
                observed_second.notify_one();
            }
            requests.push(body);
            assert!(index < 4, "unexpected model request");
            let events = match index {
                0 => vec![
                    ev_function_call("a", "panng_probe", "{}"),
                    ev_function_call("b", "panng_probe", "{}"),
                    ev_completed_with_tokens("first", /*total_tokens*/ 11),
                ],
                1 => vec![
                    ev_function_call("c", "panng_probe", "{}"),
                    ev_completed("second"),
                ],
                _ => vec![
                    ev_assistant_message(&format!("done-{index}"), "done"),
                    ev_completed(&format!("end-{index}")),
                ],
            };
            let response = wiremock::ResponseTemplate::new(/*s*/ 200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(events));
            if index == 1 && matches!(scenario, Scenario::Interrupt) {
                response.set_delay(Duration::from_secs(/*secs*/ 30))
            } else {
                response
            }
        })
        .mount(&server)
        .await;
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_contributor(Arc::new(ProbeTool(gate.clone())));
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::PanngV1)
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| {
            assert!(config.features.disable(Feature::CodeMode).is_ok());
        })
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Run A and B, then C while B is pending.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), second_request.notified()).await?;
    if matches!(scenario, Scenario::Interrupt) {
        test.codex.submit(Op::Interrupt).await?;
    }
    let id = test.session_configured.thread_id;
    let mut durable = None;
    let mut terminal_count = 0;
    let mut shutdown_submitted = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), test.codex.next_event())
            .await??;
        match event.msg {
            EventMsg::Error(error) => anyhow::bail!("unexpected error: {}", error.message),
            EventMsg::TurnComplete(completed) => {
                assert!(matches!(scenario, Scenario::Complete));
                assert_eq!(
                    (completed.last_agent_message, completed.error),
                    (Some("done".into()), None)
                );
                terminal_count += 1;
            }
            EventMsg::TurnAborted(_) => {
                assert!(matches!(scenario, Scenario::Interrupt));
                terminal_count += 1;
            }
            EventMsg::ShutdownComplete => break,
            _ => {}
        }
        if terminal_count > 0 && !shutdown_submitted {
            durable = Some(test.thread_store.experimental_panng_read(id).await?);
            test.codex.submit(Op::Shutdown).await?;
            shutdown_submitted = true;
        }
    }
    test.codex.wait_until_terminated().await;
    assert_eq!(terminal_count, 1);
    assert!(!gate.b_active.load(Ordering::SeqCst));
    let durable = durable.expect("canonical terminal captured before closing writer");
    let effects = durable
        .commits
        .iter()
        .flat_map(codex_history::PanngCommit::host_effects)
        .collect::<Vec<_>>();
    assert_eq!(
        effects
            .iter()
            .filter(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
            .count(),
        1
    );
    assert_eq!(
        effects
            .iter()
            .filter(|item| matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
            ))
            .count(),
        1
    );
    assert!(
        durable
            .runtime
            .operations()
            .iter()
            .all(|(_, operation)| matches!(operation.state(), OperationState::InContext { .. }))
    );
    let usage = effects
        .iter()
        .filter_map(|item| match item {
            RolloutItem::TokenUsageRecord(record) => Some(record),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        usage
            .iter()
            .map(|record| record.usage.total_tokens)
            .sum::<i64>(),
        11
    );
    assert_eq!(
        usage
            .last()
            .expect("usage record")
            .thread_token_usage
            .total_tokens,
        11
    );
    let requests = requests.lock().expect("captured requests").clone();
    match scenario {
        Scenario::Interrupt => {
            assert_eq!(requests.len(), 2);
            assert_eq!(durable.runtime.operations().len(), 2);
            let (_, b) = durable
                .runtime
                .operations()
                .iter()
                .find(|(_, operation)| operation.identity().call_id.as_str() == "b")
                .expect("B operation");
            assert!(matches!(
                b.state(),
                OperationState::InContext {
                    outcome: ExecutionOutcome::Indeterminate,
                    ..
                }
            ));
        }
        Scenario::Complete => {
            assert!(gate.c_observed_pending_b.load(Ordering::SeqCst));
            assert_eq!(durable.runtime.operations().len(), 3);
            assert!((3..=4).contains(&requests.len()));
            let input = requests.last().expect("final request")["input"]
                .as_array()
                .expect("request input");
            assert_eq!(
                input
                    .iter()
                    .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "b")
                    .count(),
                1
            );
            assert_eq!(
                input
                    .iter()
                    .filter(|item| item.to_string().contains("result from b"))
                    .count(),
                1
            );
        }
    }
    let loaded = test
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: id,
            include_archived: true,
        })
        .await?;
    let resumed = test
        .thread_manager
        .resume_thread_with_history(
            test.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: id,
                history: Arc::new(loaded.items),
                rollout_path: test.session_configured.rollout_path.clone(),
            }),
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await?;
    assert_eq!(
        test.thread_store
            .experimental_panng_read(id)
            .await?
            .runtime
            .operations(),
        durable.runtime.operations()
    );
    resumed.thread.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_public_turn_advances_while_b_is_pending() -> anyhow::Result<()> {
    scenario(Scenario::Complete).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_public_interrupt_settles_b_before_its_single_terminal() -> anyhow::Result<()> {
    scenario(Scenario::Interrupt).await
}

struct IdleObserver(async_channel::Sender<codex_extension_api::ThreadIdleCause>);

impl codex_extension_api::ThreadLifecycleContributor<Config> for IdleObserver {
    fn on_thread_idle<'a>(
        &'a self,
        input: codex_extension_api::ThreadIdleInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let sent = self.0.send(input.cause).await;
            assert!(sent.is_ok(), "idle observer closed: {sent:?}");
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_public_commit_failure_delivers_failed_terminal_without_changing_journal()
-> anyhow::Result<()> {
    let server = start_mock_server().await;
    let (idle_tx, idle_rx) = async_channel::unbounded();
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.thread_lifecycle_contributor(Arc::new(IdleObserver(idle_tx)));
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::PanngV1)
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| {
            assert!(config.features.disable(Feature::CodeMode).is_ok());
        })
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    let id = test.session_configured.thread_id;
    let path = test
        .session_configured
        .rollout_path
        .as_ref()
        .expect("persistent journal");
    // Remove the actual writer, without substituting a mock store or changing disk bytes.
    test.thread_store.shutdown_thread(id).await?;
    let before = tokio::fs::read(path).await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "This input must not reach the model without a durable commit.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let idle = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), idle_rx.recv()).await??;
    assert_eq!(idle, codex_extension_api::ThreadIdleCause::Failed);
    // Idle is emitted only after clearing active-turn ownership; shutdown must still finish.
    test.codex.submit(Op::Shutdown).await?;
    let mut errors = Vec::new();
    let mut failed_terminals = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), test.codex.next_event())
            .await??;
        match event.msg {
            EventMsg::Error(error) => errors.push(error.message),
            EventMsg::TurnComplete(terminal) => {
                assert!(terminal.error.is_some(), "failed commit claimed success");
                assert_eq!(terminal.last_agent_message, None);
                failed_terminals.push(terminal);
            }
            EventMsg::TurnStarted(_) | EventMsg::TurnAborted(_) => {
                anyhow::bail!("failed initial commit published a successful/aborted boundary");
            }
            EventMsg::ShutdownComplete => break,
            _ => {}
        }
    }
    test.codex.wait_until_terminated().await;
    assert_eq!(failed_terminals.len(), 1);
    assert!(
        errors
            .iter()
            .any(|message| message.contains("Panng terminal could not be durably recorded")),
        "errors: {errors:?}"
    );
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path().ends_with("/responses"))
    );
    assert_eq!(tokio::fs::read(path).await?, before);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_repeated_client_settings_and_durable_changes_reach_the_next_round()
-> anyhow::Result<()> {
    use codex_protocol::protocol::ThreadSettingsOverrides;
    use codex_protocol::turn_input::TurnStartOptions;
    use core_test_support::responses::mount_sse_sequence;
    use core_test_support::wait_for_event;

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("first", "accepted first"),
                ev_completed("first"),
            ]),
            sse(vec![
                ev_assistant_message("second", "accepted second"),
                ev_completed("second"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::PanngV1)
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| {
            config.features.disable(Feature::CodeMode).unwrap();
        })
        .build_with_auto_env(&server)
        .await?;
    let original = test.codex.thread_settings_snapshot().await;
    let restorable = test.codex.restorable_thread_settings().await;
    let repeated = ThreadSettingsOverrides {
        environments: restorable.environments,
        approval_policy: Some(original.approval_policy),
        model: Some(original.model.clone()),
        effort: Some(original.reasoning_effort.clone()),
        ..Default::default()
    };
    let request = |text: &str, settings: ThreadSettingsOverrides| {
        TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }])
        .with_thread_settings(settings)
        .on_start(TurnStartOptions {
            turn_trigger: Some("cli".into()),
            ..Default::default()
        })
    };
    test.codex
        .start_or_steer_turn(request("first accepted input", repeated.clone()))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(test.codex.thread_settings_snapshot().await, original);

    let path = test
        .session_configured
        .rollout_path
        .as_ref()
        .expect("canonical journal");
    let mut changed = repeated;
    changed.model = Some("gpt-5.2".into());
    changed.effort = Some(Some(codex_protocol::openai_models::ReasoningEffort::High));
    test.codex
        .start_or_steer_turn(request(
            "second accepted input with changed settings",
            changed,
        ))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let mut expected = original.clone();
    expected.model = "gpt-5.2".into();
    expected.collaboration_mode.settings.model = expected.model.clone();
    expected.reasoning_effort = Some(codex_protocol::openai_models::ReasoningEffort::High);
    expected.collaboration_mode.settings.reasoning_effort = expected.reasoning_effort.clone();
    assert_eq!(test.codex.thread_settings_snapshot().await, expected);
    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body_json()["model"], original.model);
    assert_eq!(requests[1].body_json()["model"], expected.model);
    assert_eq!(requests[1].body_json()["reasoning"]["effort"], "high");
    let canonical = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    let saved_settings = canonical
        .commits
        .iter()
        .rev()
        .flat_map(|commit| commit.host_effects().iter().rev())
        .find_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) => {
                Some(&event.thread_settings)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(saved_settings, &expected);
    test.thread_store
        .shutdown_thread(test.session_configured.thread_id)
        .await?;
    let before_failure = tokio::fs::read(path).await?;
    let failed = test
        .codex
        .start_or_steer_turn(request(
            "input must remain unaccepted",
            ThreadSettingsOverrides {
                model: Some(original.model),
                ..Default::default()
            },
        ))
        .await;
    assert!(failed.is_err());
    assert_eq!(test.codex.thread_settings_snapshot().await, expected);
    assert_eq!(responses.requests().len(), 2);
    assert_eq!(tokio::fs::read(path).await?, before_failure);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_plan_mode_commits_native_parsed_plan_with_its_source() -> anyhow::Result<()> {
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::items::TurnItem;
    use codex_protocol::protocol::ThreadSettingsOverrides;
    use core_test_support::responses::mount_sse_once;
    use core_test_support::wait_for_event;
    let server = start_mock_server().await;
    let response = mount_sse_once(&server, sse(vec![
        ev_assistant_message("plan-message", "<proposed_plan>\n1. Inspect the source.\n2. Add regression coverage.\n</proposed_plan>"),
        ev_completed("plan-response"),
    ])).await;
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::PanngV1)
        .with_config(|config| {
            config.features.disable(Feature::CodeMode).unwrap();
        })
        .build_with_auto_env(&server)
        .await?;
    let mut mode = test
        .codex
        .thread_settings_snapshot()
        .await
        .collaboration_mode;
    mode.mode = ModeKind::Plan;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Make a plan without editing files.".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                collaboration_mode: Some(mode),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let canonical = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    let plans = canonical
        .commits
        .iter()
        .flat_map(codex_history::PanngCommit::host_effects)
        .filter_map(|effect| match effect {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => match &event.item {
                TurnItem::Plan(plan) => Some(plan.text.trim()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        plans,
        vec!["1. Inspect the source.\n2. Add regression coverage."]
    );
    assert_eq!(response.requests().len(), 1);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_steer_and_additional_context_wake_a_pending_finite_tool() -> anyhow::Result<()> {
    use codex_protocol::protocol::AdditionalContextEntry;
    use codex_protocol::protocol::AdditionalContextKind;
    use codex_protocol::turn_input::TurnInputSubmission;
    use core_test_support::wait_for_event;
    let server = start_mock_server().await;
    let gate = Arc::new(Gate {
        a_finished: Notify::new(),
        b_started: Notify::new(),
        b_exited: Notify::new(),
        c_started: Notify::new(),
        b_active: AtomicBool::new(/*v*/ false),
        c_observed_pending_b: AtomicBool::new(/*v*/ false),
        release: Semaphore::new(/*permits*/ 0),
    });
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = requests.clone();
    let observing = gate.clone();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let bytes = if request
                .headers
                .get("content-encoding")
                .is_some_and(|value| value == "zstd")
            {
                zstd::stream::decode_all(request.body.as_slice()).unwrap()
            } else {
                request.body.clone()
            };
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let mut requests = captured.lock().unwrap();
            let index = requests.len();
            if index == 1 {
                assert!(observing.b_active.load(Ordering::SeqCst));
                assert!(body["input"].to_string().contains("STEER_NOW"));
                assert!(body["input"].to_string().contains("EXTRA_CONTEXT_VALUE"));
                assert!(!body["input"].to_string().contains("result from b"));
            }
            requests.push(body);
            assert!(index < 4);
            let events = match index {
                0 => vec![
                    ev_function_call("b", "panng_probe", "{}"),
                    ev_completed("waiting"),
                ],
                1 => vec![
                    ev_function_call("c", "panng_probe", "{}"),
                    ev_completed("steered"),
                ],
                _ => vec![
                    ev_assistant_message(&format!("done-{index}"), "done"),
                    ev_completed(&format!("complete-{index}")),
                ],
            };
            wiremock::ResponseTemplate::new(/*s*/ 200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(events))
        })
        .mount(&server)
        .await;
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_contributor(Arc::new(ProbeTool(gate.clone())));
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::PanngV1)
        .with_config(|config| {
            config.features.disable(Feature::CodeMode).unwrap();
        })
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    let initial = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Start waiting for my next instruction.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = initial else {
        anyhow::bail!("expected a new turn");
    };
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), gate.b_started.notified()).await?;
    let steer = test
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "STEER_NOW: run C while B is pending.".into(),
                text_elements: Vec::new(),
            }])
            .with_additional_context(std::collections::BTreeMap::from([(
                "selection".into(),
                AdditionalContextEntry {
                    value: "EXTRA_CONTEXT_VALUE".into(),
                    kind: AdditionalContextKind::Application,
                },
            )])),
        )
        .await?;
    assert_eq!(steer, TurnInputSubmission::Steered { turn_id });
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 10),
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        }),
    )
    .await?;
    assert!(gate.c_observed_pending_b.load(Ordering::SeqCst));
    let canonical = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    assert_eq!(canonical.commits.iter().flat_map(codex_history::PanngCommit::host_effects).filter(|effect| matches!(effect, RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) if matches!(event.item, codex_protocol::items::TurnItem::UserMessage(_)))).count(), 2);
    assert!(
        canonical
            .runtime
            .operations()
            .iter()
            .all(|(_, operation)| matches!(operation.state(), OperationState::InContext { .. }))
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_mixed_plan_commits_each_ui_item_and_source_once() -> anyhow::Result<()> {
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::items::TurnItem;
    use codex_protocol::protocol::ThreadSettingsOverrides;
    use core_test_support::responses::mount_sse_once;
    use core_test_support::wait_for_event;
    let server = start_mock_server().await;
    let response = mount_sse_once(&server, sse(vec![
        ev_assistant_message("plan-message", "Here is the plan.\n<proposed_plan>\n1. Inspect the source.\n2. Add regression coverage.\n</proposed_plan>"),
        ev_completed("plan-response"),
    ])).await;
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::PanngV1)
        .with_config(|config| {
            config.features.disable(Feature::CodeMode).unwrap();
        })
        .build_with_auto_env(&server)
        .await?;
    let mut mode = test
        .codex
        .thread_settings_snapshot()
        .await
        .collaboration_mode;
    mode.mode = ModeKind::Plan;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Make a plan without editing files.".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                collaboration_mode: Some(mode),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let canonical = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    let plans = canonical
        .commits
        .iter()
        .flat_map(codex_history::PanngCommit::host_effects)
        .filter_map(|effect| match effect {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => match &event.item {
                TurnItem::Plan(plan) => Some(plan.text.trim()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        plans,
        vec!["1. Inspect the source.\n2. Add regression coverage."]
    );
    let effects = canonical
        .commits
        .iter()
        .flat_map(codex_history::PanngCommit::host_effects)
        .collect::<Vec<_>>();
    assert_eq!(effects.iter().filter(|effect| matches!(effect, RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) if matches!(event.item, TurnItem::AgentMessage(_)))).count(), 1);
    assert_eq!(effects.iter().filter(|effect| matches!(effect, RolloutItem::ResponseItem(item) if matches!(&item.item, codex_protocol::models::ResponseItem::Message { role, .. } if role == "assistant"))).count(), 1);
    for commit in &canonical.commits {
        assert!(
            commit
                .host_effects()
                .iter()
                .filter(|effect| matches!(
                    effect,
                    RolloutItem::EventMsg(EventMsg::ItemCompleted(_))
                ))
                .count()
                <= 1
        );
    }
    assert_eq!(response.requests().len(), 1);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[test_case::test_case(false; "both accepted")]
#[test_case::test_case(true; "first blocked by hook")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_two_accepted_steers_keep_separate_ui_ordinals(
    block_first: bool,
) -> anyhow::Result<()> {
    use codex_protocol::turn_input::TurnInputSubmission;
    use core_test_support::wait_for_event;
    let server = start_mock_server().await;
    let entered = Arc::new(Notify::new());
    let provider_entered = entered.clone();
    let (release, receive) = std::sync::mpsc::channel();
    let receive = Mutex::new(receive);
    let count = std::sync::atomic::AtomicUsize::new(0);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let index = count.fetch_add(1, Ordering::SeqCst);
            if index == 0 {
                provider_entered.notify_one();
                receive
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(/*secs*/ 10))
                    .unwrap();
            } else {
                let bytes = if request
                    .headers
                    .get("content-encoding")
                    .is_some_and(|value| value == "zstd")
                {
                    zstd::stream::decode_all(request.body.as_slice()).unwrap()
                } else {
                    request.body.clone()
                };
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let input = body["input"].to_string();
                assert!(input.contains("STEER_SECOND"));
                if block_first {
                    assert!(!input.contains("STEER_FIRST"));
                } else {
                    assert!(
                        input.find("STEER_FIRST").unwrap() < input.find("STEER_SECOND").unwrap()
                    );
                }
            }
            assert!(index < 2);
            wiremock::ResponseTemplate::new(/*s*/ 200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(vec![
                    ev_assistant_message(&format!("answer-{index}"), "done"),
                    ev_completed(&format!("response-{index}")),
                ]))
        })
        .mount(&server)
        .await;
    let test = test_codex().with_history_mode(ThreadHistoryMode::PanngV1)
        .with_pre_build_hook(move |home| {
            if block_first {
                let script = home.join("block_first_steer.py");
                std::fs::write(&script, "import json,sys
p=json.load(sys.stdin)
print(json.dumps({'decision':'block','reason':'first input rejected'} if 'STEER_FIRST' in p.get('prompt','') else {}))
").unwrap();
                let hooks = serde_json::json!({"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":format!("python3 {}",script.display())}]}]}});
                std::fs::write(home.join("hooks.json"), hooks.to_string()).unwrap();
            }
        })
        .with_config(move |config| {
            config.features.disable(Feature::CodeMode).unwrap();
            if block_first { core_test_support::hooks::trust_discovered_hooks(config); }
        })
        .build_with_auto_env(&server).await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Wait for my instructions.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), entered.notified()).await?;
    for text in ["STEER_FIRST", "STEER_SECOND"] {
        let accepted = test
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: text.into(),
                text_elements: Vec::new(),
            }]))
            .await?;
        assert!(matches!(accepted, TurnInputSubmission::Steered { .. }));
    }
    release.send(())?;
    let complete = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(matches!(complete, EventMsg::TurnComplete(event) if event.error.is_none()));
    let canonical = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    let users = canonical.commits.iter().flat_map(codex_history::PanngCommit::host_effects).filter(|effect| matches!(effect, RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) if matches!(event.item, codex_protocol::items::TurnItem::UserMessage(_)))).count();
    assert_eq!(users, if block_first { 2 } else { 3 });
    for commit in &canonical.commits {
        assert!(
            commit
                .host_effects()
                .iter()
                .filter(|effect| matches!(
                    effect,
                    RolloutItem::EventMsg(EventMsg::ItemCompleted(_))
                ))
                .count()
                <= 1
        );
    }
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_cancelled_prompt_hook_retains_accepted_input_without_replaying_hook()
-> anyhow::Result<()> {
    use core_test_support::responses::mount_sse_once;
    use core_test_support::wait_for_event;
    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("after-cancel", "continued"),
            ev_completed("after-cancel"),
        ]),
    )
    .await;
    let test = test_codex().with_history_mode(ThreadHistoryMode::PanngV1)
        .with_pre_build_hook(|home| {
            let script = home.join("slow_prompt.py");
            std::fs::write(&script, r#"import json,sys,time
from pathlib import Path
home=Path(__file__).parent
p=json.load(sys.stdin)['prompt']
with (home/'prompt_calls.jsonl').open('a') as f: f.write(json.dumps(p)+'\n')
if 'RETAIN_ME' in p:
    (home/'prompt_started').write_text('started')
    for _ in range(1000):
        if (home/'release_prompt').exists(): break
        time.sleep(.01)
print('{}')
"#).unwrap();
            std::fs::write(home.join("hooks.json"), serde_json::json!({"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":format!("python3 {}", script.display())}]}]}}).to_string()).unwrap();
        })
        .with_config(|config| {
            config.features.disable(Feature::CodeMode).unwrap();
            core_test_support::hooks::trust_discovered_hooks(config);
        }).build_with_auto_env(&server).await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "RETAIN_ME".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let home = &test.config.codex_home;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
        while !home.join("prompt_started").exists() {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    tokio::fs::write(home.join("release_prompt"), "release").await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Continue now.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let terminal = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(matches!(terminal, EventMsg::TurnComplete(event) if event.error.is_none()));
    let request = response.single_request();
    assert_eq!(
        request.body_json()["input"]
            .to_string()
            .matches("RETAIN_ME")
            .count(),
        1
    );
    let hook_calls = tokio::fs::read_to_string(home.join("prompt_calls.jsonl")).await?;
    assert_eq!(hook_calls.matches("RETAIN_ME").count(), 1);
    let id = test.session_configured.thread_id;
    test.codex.shutdown_and_wait().await?;
    let cold = test
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: id,
            include_archived: true,
        })
        .await?;
    let source_count = codex_history::host_replay_items(&cold.items).filter(|item| matches!(item, codex_history::HostReplayItem::Item(RolloutItem::ResponseItem(envelope)) if serde_json::to_string(&envelope.item).unwrap().contains("RETAIN_ME"))).count();
    assert_eq!(source_count, 1);
    Ok(())
}
