use codex_core::PanngPrototype;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolContributor;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_tools::JsonToolOutput;
use codex_tools::ToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolExecutorFuture;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use futures::FutureExt;
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
                gate.release.acquire().await.expect("gate").forget();
            } else if call.call_id == "c" {
                gate.c_observed_pending_b
                    .store(gate.b_active.load(Ordering::SeqCst), Ordering::SeqCst);
                gate.c_started.notify_one();
                gate.release.add_permits(/*n*/ 1);
            } else if call.call_id == "a" {
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
    Native,
    Async,
    Interrupt,
    ModelError,
}

async fn scenario(mode: Scenario) -> anyhow::Result<()> {
    let prototype = !matches!(mode, Scenario::Native);
    let server = start_mock_server().await;
    let bodies = [
        sse(vec![
            ev_function_call("a", "panng_probe", "{}"),
            ev_function_call("b", "panng_probe", "{}"),
            ev_completed("first"),
        ]),
        sse(vec![
            ev_function_call("c", "panng_probe", "{}"),
            ev_completed("second"),
        ]),
    ];
    let responses = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = responses.clone();
    // Completion batching intentionally permits either three or four model requests.
    // Keep the server's existing request invariant checks and cap requests explicitly below.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let bytes = if request.headers.get("content-encoding").is_some_and(|value| value == "zstd") {
                zstd::stream::decode_all(request.body.as_slice()).expect("zstd body")
            } else { request.body.clone() };
            let mut requests = captured.lock().expect("requests");
            let index = requests.len();
            requests.push(serde_json::from_slice(&bytes).expect("request JSON"));
            if matches!(mode, Scenario::ModelError) && index == 1 {
                return wiremock::ResponseTemplate::new(/*s*/ 400)
                    .set_body_json(serde_json::json!({"error":{"message":"injected non-retryable model error","type":"invalid_request_error"}}));
            }
            wiremock::ResponseTemplate::new(/*s*/ 200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(bodies.get(index).cloned().unwrap_or_else(|| {
                    sse(vec![ev_assistant_message(&format!("done-{index}"), "done"), ev_completed(&format!("end-{index}"))])
                }))
        }).mount(&server).await;
    let gate = Arc::new(Gate {
        a_finished: Notify::new(),
        b_started: Notify::new(),
        b_exited: Notify::new(),
        c_started: Notify::new(),
        b_active: AtomicBool::new(/*v*/ false),
        c_observed_pending_b: AtomicBool::new(/*v*/ false),
        release: Semaphore::new(/*permits*/ 0),
    });
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    if prototype {
        extensions.turn_lifecycle_contributor(Arc::new(PanngPrototype));
    }
    extensions.tool_contributor(Arc::new(ProbeTool(gate.clone())));
    let test = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Run A and B, then use A to run C while B is pending.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), gate.b_started.notified()).await?;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), gate.a_finished.notified()).await?;
    if matches!(mode, Scenario::Interrupt) {
        test.codex.submit(Op::Interrupt).await?;
        tokio::time::timeout(
            Duration::from_secs(/*secs*/ 10),
            wait_for_event(&test.codex, |event| {
                matches!(event, EventMsg::TurnAborted(_))
            }),
        )
        .await?;
        // Releasing no permit: cancellation must drop the blocked tool future itself.
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), gate.b_exited.notified()).await?;
        assert!(!gate.b_active.load(Ordering::SeqCst));
        return Ok(());
    }
    if !prototype {
        assert!(gate.c_started.notified().now_or_never().is_none());
        assert_eq!(responses.lock().expect("requests").len(), 1);
        gate.release.add_permits(/*n*/ 1);
    }
    if matches!(mode, Scenario::ModelError) {
        let EventMsg::Error(error) =
            wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await
        else {
            unreachable!("event predicate guarantees an error");
        };
        let EventMsg::TurnComplete(completed) = wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await
        else {
            unreachable!("event predicate guarantees turn completion");
        };
        assert_eq!(completed.error, Some(error));
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), gate.b_exited.notified()).await?;
        assert!(!gate.b_active.load(Ordering::SeqCst));
        assert_eq!(responses.lock().expect("requests").len(), 2);
        assert!(gate.c_started.notified().now_or_never().is_none());
        return Ok(());
    }
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), gate.c_started.notified()).await?;
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 10),
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        }),
    )
    .await?;
    assert_eq!(gate.c_observed_pending_b.load(Ordering::SeqCst), prototype);
    let requests = responses.lock().expect("requests").clone();
    assert!(
        (3..=4).contains(&requests.len()),
        "unexpected sampling count: {}",
        requests.len()
    );
    let second = &requests[1];
    let a_outputs: Vec<_> = second["input"]
        .as_array()
        .expect("input")
        .iter()
        .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "a")
        .map(|item| {
            serde_json::from_str::<serde_json::Value>(
                item["output"].as_str().expect("A text output"),
            )
            .expect("A JSON result")
        })
        .collect();
    assert_eq!(
        a_outputs,
        vec![serde_json::json!({"result": "result from a"})]
    );
    let outputs: Vec<_> = second["input"]
        .as_array()
        .expect("input")
        .iter()
        .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "b")
        .collect();
    assert_eq!(outputs.len(), 1);
    assert_eq!(
        outputs[0]["output"]
            .as_str()
            .expect("text output")
            .contains("still running"),
        prototype
    );
    if prototype {
        let last = requests.last().expect("last request");
        let input = last["input"].as_array().expect("input");
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "b")
                .count(),
            1
        );
        assert!(
            input
                .iter()
                .any(|item| item.to_string().contains("<panng_completion>"))
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_prototype_advances_with_a_real_pending_tool_future() -> anyhow::Result<()> {
    scenario(Scenario::Async).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_native_control_waits_for_the_same_pending_future() -> anyhow::Result<()> {
    scenario(Scenario::Native).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_prototype_interrupt_drops_pending_tools() -> anyhow::Result<()> {
    scenario(Scenario::Interrupt).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_prototype_model_error_drops_pending_tools() -> anyhow::Result<()> {
    scenario(Scenario::ModelError).await
}
