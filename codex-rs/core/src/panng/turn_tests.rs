//! Host-only durable probe integration; public Op admission intentionally remains gated.
use super::*;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolContributor;
use codex_panng_runtime::OperationState;
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
use pretty_assertions::assert_eq;
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

#[tokio::test]
async fn panng_durable_probe_crosses_real_model_steps() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = requests.clone();
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
                zstd::stream::decode_all(request.body.as_slice()).expect("decode request")
            } else {
                request.body.clone()
            };
            let mut requests = captured.lock().unwrap();
            let index = requests.len();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if index == 1 {
                assert!(observing_gate.b_active.load(Ordering::SeqCst));
                assert!(body["input"].to_string().contains("result from a"));
                assert!(!body["input"].to_string().contains("result from b"));
            }
            requests.push(body);
            assert!(index < 4, "unexpected extra model step");
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
            wiremock::ResponseTemplate::new(/*s*/ 200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(events))
        })
        .mount(&server)
        .await;
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_contributor(Arc::new(ProbeTool(gate.clone())));
    let home = tempfile::tempdir()?;
    let startup = Arc::new(startup::SessionStartup::default());
    let (tx, events) = async_channel::unbounded();
    let session = tests::make_session_with_setup_and_events(
        |config| {
            config.codex_home =
                codex_utils_absolute_path::AbsolutePathBuf::try_from(home.path()).unwrap();
            config.ephemeral = false;
            config.model = Some("test-gpt-5.1-codex".into());
            config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
            config.model_provider.supports_websockets = false;
            config
                .features
                .disable(Feature::CodeMode)
                .expect("disable code mode for probe fixture");
        },
        |_| {},
        ThreadHistoryMode::PanngV1,
        Some(startup.clone()),
        tx,
        Arc::new(extensions.build()),
    )
    .await?;
    let turn = session.new_default_turn().await;
    let mut client = session.services.model_client.new_session();
    let result = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 20),
        crate::panng::turn::run_probe_turn(
            session.clone(),
            turn,
            "Run A and B, then C while B is pending.".into(),
            &mut client,
            CancellationToken::new(),
        ),
    )
    .await??;
    assert_eq!(result, Some("done".into()));
    assert!(gate.c_observed_pending_b.load(Ordering::SeqCst));
    assert!(!gate.b_active.load(Ordering::SeqCst));
    let requests = requests.lock().unwrap().clone();
    assert!((3..=4).contains(&requests.len()));
    let final_input = requests.last().unwrap()["input"].as_array().unwrap();
    assert_eq!(
        final_input
            .iter()
            .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "b")
            .count(),
        1
    );
    assert_eq!(
        final_input
            .iter()
            .filter(|item| item.to_string().contains("result from b"))
            .count(),
        1
    );
    let durable = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    assert_eq!(durable.runtime.operations().len(), 3);
    assert!(
        durable
            .runtime
            .operations()
            .iter()
            .all(|(_, operation)| matches!(operation.state(), OperationState::InContext { .. }))
    );
    let effects = durable
        .commits
        .iter()
        .flat_map(codex_history::PanngCommit::host_effects)
        .collect::<Vec<_>>();
    let usage = effects
        .iter()
        .filter_map(|item| match item {
            RolloutItem::TokenUsageRecord(record) => Some(record),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), requests.len());
    assert_eq!(
        usage
            .iter()
            .map(|record| record.usage.total_tokens)
            .sum::<i64>(),
        11
    );
    assert_eq!(usage.last().unwrap().thread_token_usage.total_tokens, 11);
    assert_eq!(
        session
            .state
            .lock()
            .await
            .latest_token_usage_record
            .as_ref(),
        usage.last().copied()
    );
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
            .filter(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnComplete(_))))
            .count(),
        1
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event.msg, EventMsg::Error(_)));
    }
    startup.cleanup().await;
    Ok(())
}
