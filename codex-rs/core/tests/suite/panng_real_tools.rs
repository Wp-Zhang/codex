//! Public Panng turns using actual finite shell processes and custom patches.
#[path = "panng_approval_tests.rs"]
mod approvals;
#[path = "panng_fork_tests.rs"]
mod forks;
#[path = "panng_long_task_tests.rs"]
mod long_tasks;
#[path = "panng_result_reader_tests.rs"]
mod result_reader;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_config::Constrained;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_panng_runtime::OperationState;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::TestTargetOs;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::sse;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::test_codex;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::sync::Notify;

fn builder() -> TestCodexBuilder {
    test_codex()
        .with_history_mode(ThreadHistoryMode::PanngV1)
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| {
            config.features.disable(Feature::CodeMode).unwrap();
            config.features.disable(Feature::ShellZshFork).unwrap();
            config.features.disable(Feature::ShellSnapshot).unwrap();
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
            config
                .permissions
                .set_permission_profile(PermissionProfile::Disabled)
                .unwrap();
        })
}

async fn harness() -> anyhow::Result<TestCodexHarness> {
    TestCodexHarness::with_auto_env_builder(builder()).await
}

fn request_body(request: &wiremock::Request) -> Value {
    let bytes = if request
        .headers
        .get("content-encoding")
        .is_some_and(|value| value == "zstd")
    {
        zstd::stream::decode_all(request.body.as_slice()).unwrap()
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&bytes).unwrap()
}
fn completions(body: &Value) -> Vec<Value> {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "message" && item["role"] == "user")
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter_map(|content| content["text"].as_str())
        .filter_map(|text| text.split_once("<panng_completion>"))
        .filter_map(|(_, text)| text.split_once("</panng_completion>"))
        .map(|(body, _)| serde_json::from_str(body.trim()).unwrap())
        .collect()
}
fn exec(call: &str, command: &str) -> Value {
    ev_function_call(
        call,
        "exec_command",
        &json!({
            "cmd":command, "login":false, "timeout_ms":30_000, "max_output_tokens":20_000,
        })
        .to_string(),
    )
}
fn response(events: Vec<Value>) -> wiremock::ResponseTemplate {
    wiremock::ResponseTemplate::new(/*s*/ 200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse(events))
}
async fn complete(test: &TestCodex) -> anyhow::Result<()> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(/*secs*/ 45), test.codex.next_event())
            .await??;
        match event.msg {
            EventMsg::TurnComplete(turn) => {
                assert!(turn.error.is_none(), "{turn:?}");
                return Ok(());
            }
            EventMsg::Error(error) => anyhow::bail!("real tool turn failed: {}", error.message),
            EventMsg::TurnAborted(_) => anyhow::bail!("real tool turn unexpectedly aborted"),
            _ => {}
        }
    }
}
async fn resume_and_compare(test: &TestCodex) -> anyhow::Result<()> {
    let id = test.session_configured.thread_id;
    let before = test.thread_store.experimental_panng_read(id).await?;
    assert!(
        before
            .runtime
            .operations()
            .iter()
            .all(|(_, operation)| matches!(operation.state(), OperationState::InContext { .. }))
    );
    ui_tests::assert_canonical_tool_items(&before);
    let live_ui = ui_tests::ui_items(test).await?;
    test.codex.shutdown_and_wait().await?;
    assert_eq!(ui_tests::ui_items(test).await?, live_ui);
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
        before.runtime.operations()
    );
    resumed.thread.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_real_commands_overlap_across_steps_then_custom_patch_changes_file()
-> anyhow::Result<()> {
    let harness = harness().await?;
    harness.write_file("source.txt", b"SOURCE_BEFORE\n").await?;
    let (a, b, c, read_changed) = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => (
            "while [ ! -f b-started ]; do sleep 0.05; done; cat source.txt",
            "printf started > b-started; while [ ! -f release-b ]; do sleep 0.05; done; printf B_FINISHED",
            "test -f b-started && test ! -f release-b && printf C_OVERLAPPED",
            "cat source.txt",
        ),
        TestTargetOs::Windows => (
            "while (!(Test-Path b-started)) { Start-Sleep -Milliseconds 50 }; Get-Content source.txt",
            "Set-Content b-started started; while (!(Test-Path release-b)) { Start-Sleep -Milliseconds 50 }; Write-Output B_FINISHED",
            "if ((Test-Path b-started) -and !(Test-Path release-b)) { Write-Output C_OVERLAPPED }",
            "Get-Content source.txt",
        ),
    };
    let c_completed = Arc::new(Notify::new());
    let signal = c_completed.clone();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let stage = Arc::new(Mutex::new(0));
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let body = request_body(request);
            let notices = completions(&body).iter().map(Value::to_string).collect::<String>();
            let mut requests = captured.lock().unwrap();
            let index = requests.len();
            assert!(index < 12, "model did not converge after real tool completion");
            let mut stage = stage.lock().unwrap();
            if *stage == 0 {
                let tools = body["tools"].as_array().unwrap();
                let command = tools.iter().find(|tool| tool["name"] == "exec_command").expect("finite exec schema");
                assert!(command["parameters"]["properties"].get("timeout_ms").is_some());
                assert!(command["parameters"]["properties"].get("yield_time_ms").is_none());
                assert!(command["parameters"]["properties"].get("tty").is_none());
                assert!(!tools.iter().any(|tool| tool["name"] == "write_stdin"));
            }
            requests.push(body);
            let waiting_for_patch = *stage == 3 && !notices.contains("patch");
            let events = match *stage {
                0 => vec![exec("a", a), exec("b", b), ev_completed("ab")],
                1 => {
                    assert!(notices.contains("SOURCE_BEFORE"), "A must finish before the second model step: {notices}");
                    assert!(!notices.contains("B_FINISHED"));
                    vec![exec("c", c), ev_completed("c")]
                }
                2 => {
                    assert!(notices.contains("C_OVERLAPPED"), "real C observed B's filesystem gate: {notices}");
                    assert!(!notices.contains("B_FINISHED"));
                    signal.notify_one();
                    vec![ev_custom_tool_call("patch", "apply_patch", "*** Begin Patch\n*** Update File: source.txt\n@@\n-SOURCE_BEFORE\n+SOURCE_AFTER\n*** End Patch"), ev_completed("patch")]
                }
                3 if waiting_for_patch => vec![ev_assistant_message("waiting", "Waiting for the patch to finish."), ev_completed("waiting")],
                3 => vec![exec("read-changed", read_changed), ev_completed("read-changed")],
                4 => {
                    assert!(notices.contains("SOURCE_AFTER"), "patched file read must reach model: {notices}");
                    assert!(notices.contains("B_FINISHED"));
                    vec![ev_assistant_message("done", "done"), ev_completed("done")]
                }
                _ => panic!("unexpected model step {index}"),
            };
            if !waiting_for_patch { *stage += 1; }
            response(events)
        }).mount(harness.server()).await;
    let test = harness.test();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Read source while B waits; run C, patch source, then read it again.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 20), c_completed.notified()).await?;
    let current = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    let (_, pending_b) = current
        .runtime
        .operations()
        .iter()
        .find(|(_, operation)| operation.identity().call_id.as_str() == "b")
        .expect("real B operation");
    assert!(matches!(pending_b.state(), OperationState::Prepared));
    harness
        .write_file("release-b", b"released by the test, outside the agent")
        .await?;
    complete(test).await?;
    assert_eq!(
        harness.read_file_text("source.txt").await?,
        "SOURCE_AFTER\n"
    );
    let durable = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    assert_eq!(durable.runtime.operations().len(), 5);
    let requests = requests.lock().unwrap().clone();
    let final_input = requests.last().unwrap()["input"].as_array().unwrap();
    for (call, family) in [
        ("a", "function_call_output"),
        ("b", "function_call_output"),
        ("c", "function_call_output"),
        ("patch", "custom_tool_call_output"),
        ("read-changed", "function_call_output"),
    ] {
        assert_eq!(
            final_input
                .iter()
                .filter(|item| item["type"] == family && item["call_id"] == call)
                .count(),
            1,
            "one wire output for {call}"
        );
    }
    resume_and_compare(test).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_real_large_output_is_retrievable_beyond_notification_after_resume()
-> anyhow::Result<()> {
    let harness = harness().await?;
    let text = format!("{}LATE_RESULT_MARKER\n", "x".repeat(8192));
    harness.write_file("large.txt", &text).await?;
    let command = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => "cat large.txt",
        TestTargetOs::Windows => "Get-Content -Raw large.txt",
    };
    let step = Arc::new(Mutex::new(0));
    let retrieved = Arc::new(Mutex::new(None::<Value>));
    let captured = retrieved.clone();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let body = request_body(request);
            let mut step = step.lock().unwrap();
            let events =
                match *step {
                    0 => vec![exec("large", command), ev_completed("large")],
                    1 => {
                        let notices = completions(&body);
                        let notice = notices.last().expect("completion notice");
                        assert!(notice.to_string().len() <= 768);
                        assert_eq!(notice["truncated"], true);
                        assert_eq!(notice["read_tool"], "read_operation_result");
                        assert!(!notice.to_string().contains("LATE_RESULT_MARKER"));
                        vec![ev_function_call("read-page", "read_operation_result", &json!({
                        "operation_id":notice["operation_id"], "offset":7000, "limit":4096,
                    }).to_string()), ev_completed("page")]
                    }
                    2 => {
                        let output = body["input"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|item| {
                                item["type"] == "function_call_output"
                                    && item["call_id"] == "read-page"
                            })
                            .expect("synchronous page output");
                        let page: Value =
                            serde_json::from_str(output["output"].as_str().expect("page text"))
                                .unwrap();
                        assert!(
                            page["output"]
                                .as_str()
                                .unwrap()
                                .contains("LATE_RESULT_MARKER")
                        );
                        assert_eq!(page["offset"], 7000);
                        assert!(page["next_offset"].is_null());
                        *captured.lock().unwrap() = Some(page);
                        vec![ev_assistant_message("done", "done"), ev_completed("done")]
                    }
                    _ => panic!("recursive result retrieval instead of a synchronous barrier"),
                };
            *step += 1;
            response(events)
        })
        .mount(harness.server())
        .await;
    let test = harness.test();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Read the large file, then retrieve the tail of its retained result.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    complete(test).await?;
    assert!(retrieved.lock().unwrap().is_some());
    let durable = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    assert_eq!(
        durable.runtime.operations().len(),
        1,
        "reader must not create recursive deferred operations"
    );
    let (_, operation) = durable.runtime.operations().iter().next().unwrap();
    let OperationState::InContext { result, .. } = operation.state() else {
        panic!("large result was not delivered")
    };
    assert!(result.artifact_reference().is_some());
    let payload = codex_rollout::load_panng_result_blob(test.codex_home_path(), result).await?;
    assert!(String::from_utf8(payload)?.contains("LATE_RESULT_MARKER"));
    resume_and_compare(test).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_result_pages_remain_complete_json_under_small_native_tool_budget()
-> anyhow::Result<()> {
    let harness = TestCodexHarness::with_auto_env_builder(builder().with_config(|config| {
        config.tool_output_token_limit = Some(128);
    }))
    .await?;
    // Escapes and multi-byte characters exercise the serialized JSON cap, not only raw length.
    harness
        .write_file("escaped.txt", "piece-\"\\λ\n".repeat(1024))
        .await?;
    let command = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => "cat escaped.txt",
        TestTargetOs::Windows => "Get-Content -Raw escaped.txt",
    };
    let pages = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = pages.clone();
    let stage = Arc::new(Mutex::new(0));
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let body = request_body(request);
            let mut stage = stage.lock().unwrap();
            let events = match *stage {
                0 => vec![exec("escaped", command), ev_completed("escaped")],
                1 => {
                    let notices = completions(&body);
                    let notice = notices.last().expect("result completion");
                    vec![
                        ev_function_call(
                            "page-one",
                            "read_operation_result",
                            &json!({
                                "operation_id":notice["operation_id"], "offset":0, "limit":8192,
                            })
                            .to_string(),
                        ),
                        ev_completed("page-one"),
                    ]
                }
                2 | 3 => {
                    let call_id = if *stage == 2 { "page-one" } else { "page-two" };
                    let output = body["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|item| {
                            item["type"] == "function_call_output" && item["call_id"] == call_id
                        })
                        .expect("actual outbound result page");
                    let page: Value =
                        serde_json::from_str(output["output"].as_str().expect("JSON page text"))
                            .expect("native history must not truncate the reader's JSON page");
                    let offset = page["offset"].as_u64().unwrap();
                    if *stage == 2 {
                        assert!(
                            page["next_offset"].is_u64(),
                            "small budget must require a second page"
                        );
                    }
                    let next = page["next_offset"]
                        .as_u64()
                        .unwrap_or_else(|| page["total_bytes"].as_u64().unwrap());
                    assert_eq!(next, offset + page["output"].as_str().unwrap().len() as u64);
                    assert!(next > offset, "a page must make progress");
                    let mut pages = captured.lock().unwrap();
                    assert_eq!(
                        offset,
                        pages
                            .last()
                            .map_or(0, |previous| previous["next_offset"].as_u64().unwrap())
                    );
                    if *stage == 3 {
                        let old_output = body["input"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|item| {
                                item["type"] == "function_call_output"
                                    && item["call_id"] == "page-one"
                            })
                            .unwrap();
                        assert_eq!(
                            serde_json::from_str::<Value>(old_output["output"].as_str().unwrap())
                                .unwrap(),
                            pages[0]
                        );
                    }
                    pages.push(page.clone());
                    if *stage == 2 {
                        vec![ev_function_call("page-two", "read_operation_result", &json!({
                            "operation_id":page["operation_id"], "offset":next, "limit":8192,
                        }).to_string()), ev_completed("page-two")]
                    } else {
                        vec![
                            ev_assistant_message("done", "two intact pages"),
                            ev_completed("done"),
                        ]
                    }
                }
                _ => panic!("unexpected deferred reader operation"),
            };
            *stage += 1;
            response(events)
        })
        .mount(harness.server())
        .await;
    let test = harness.test();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Read the file, then request two consecutive pages of its retained result."
                .into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    complete(test).await?;
    let pages = pages.lock().unwrap().clone();
    assert_eq!(pages.len(), 2);
    let durable = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    assert_eq!(
        durable.runtime.operations().len(),
        1,
        "reader calls must not create deferred operations"
    );
    let (_, operation) = durable.runtime.operations().iter().next().unwrap();
    let OperationState::InContext { result, .. } = operation.state() else {
        panic!("result not delivered")
    };
    let bytes = match result.inline_payload() {
        Some((_, payload)) => payload.as_bytes().to_vec(),
        None => codex_rollout::load_panng_result_blob(test.codex_home_path(), result).await?,
    };
    let codex_history::RolloutItem::ResponseItem(envelope) = serde_json::from_slice(&bytes)? else {
        panic!("stored envelope expected")
    };
    let codex_protocol::models::ResponseItem::FunctionCallOutput { output, .. } = envelope.item
    else {
        panic!("native function output expected")
    };
    let text = output.text_content().expect("native result text");
    let end = pages[1]["next_offset"]
        .as_u64()
        .unwrap_or_else(|| pages[1]["total_bytes"].as_u64().unwrap()) as usize;
    let delivered = pages
        .iter()
        .map(|page| page["output"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(delivered.as_bytes(), &text.as_bytes()[..end]);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[path = "panng_real_tools_ui_tests.rs"]
mod ui_tests;

#[path = "panng_hook_tests.rs"]
mod hook_tests;

#[path = "panng_control_tool_tests.rs"]
mod control_tool_tests;
