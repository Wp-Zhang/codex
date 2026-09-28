//! Exercise automatic pressure detection, native summarization, joint installation and cold resume.
use std::sync::Arc;
use std::time::Duration;

use codex_config::Constrained;
use codex_core::TurnInputRequest;
use codex_core::compact::SUMMARIZATION_PROMPT;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_panng_runtime::OperationState;
use codex_panng_runtime::SnapshotReason;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::TestTargetOs;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_public_context_pressure_compacts_continues_real_tools_and_resumes_same_window()
-> anyhow::Result<()> {
    let server = start_mock_server().await;
    let command = |call: &str, text: &str| {
        let cmd = match test_target_os() {
            TestTargetOs::Linux | TestTargetOs::MacOs => format!("printf {text}"),
            TestTargetOs::Windows => format!("Write-Output {text}"),
        };
        ev_function_call(
            call,
            "exec_command",
            &json!({"cmd":cmd,"login":false,"timeout_ms":10_000}).to_string(),
        )
    };
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                command("before-compact", "BEFORE_COMPACT"),
                ev_completed_with_tokens("first", 100_000),
            ]),
            sse(vec![
                ev_assistant_message(
                    "summary",
                    "AUTO_COMPACT_SUMMARY: continue the requested work.",
                ),
                ev_completed_with_tokens("summary", 10),
            ]),
            sse(vec![
                command("after-compact", "AFTER_COMPACT"),
                ev_completed_with_tokens("continued", 100),
            ]),
            sse(vec![
                ev_assistant_message("done", "work completed after compaction"),
                ev_completed_with_tokens("done", 100),
            ]),
            sse(vec![
                ev_assistant_message("cold-done", "cold window retained"),
                ev_completed_with_tokens("cold-done", 100),
            ]),
        ],
    )
    .await;
    // Same OpenAI-compatible local-compaction provider shape used by suite/compact.rs.
    let mut provider = codex_model_provider_info::built_in_model_providers(None)["openai"].clone();
    provider.name = "OpenAI-compatible test provider".into();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::PanngV1)
        .with_model("test-gpt-5.1-codex")
        .with_config(move |config| {
            config.model_provider = provider;
            config.model_context_window = Some(200_000);
            config.model_auto_compact_token_limit = Some(20_000);
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.into());
            config.features.disable(Feature::CodeMode).unwrap();
            config.features.disable(Feature::TokenBudget).unwrap();
            config.features.disable(Feature::ShellZshFork).unwrap();
            config.features.disable(Feature::ShellSnapshot).unwrap();
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
            config
                .permissions
                .set_permission_profile(PermissionProfile::Disabled)
                .unwrap();
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex.start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
        text: "Run the first command, continue after automatic compaction, and run the second command.".into(), text_elements: Vec::new(),
    }])).await?;
    let mut compaction_completed = 0;
    loop {
        let event =
            tokio::time::timeout(Duration::from_secs(40), test.codex.next_event()).await??;
        match event.msg {
            EventMsg::ItemCompleted(event)
                if matches!(
                    event.item,
                    codex_protocol::items::TurnItem::ContextCompaction(_)
                ) =>
            {
                compaction_completed += 1
            }
            EventMsg::TurnComplete(turn) => {
                assert!(turn.error.is_none(), "{turn:?}");
                break;
            }
            EventMsg::Error(error) => anyhow::bail!("auto compaction failed: {}", error.message),
            EventMsg::TurnAborted(_) => anyhow::bail!("auto compaction unexpectedly aborted"),
            _ => {}
        }
    }
    assert_eq!(
        compaction_completed, 1,
        "must enter the real automatic compaction lifecycle"
    );
    let requests = responses.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[1].body_contains_text(SUMMARIZATION_PROMPT),
        "second request must be native summarization, not another normal sample"
    );
    assert!(requests[2].body_contains_text("AUTO_COMPACT_SUMMARY"));
    assert!(requests[3].body_contains_text("AFTER_COMPACT"));
    let id = test.session_configured.thread_id;
    let before = test.thread_store.experimental_panng_read(id).await?;
    assert_eq!(before.runtime.operations().len(), 2);
    assert!(
        before
            .runtime
            .operations()
            .iter()
            .all(|(_, operation)| matches!(operation.state(), OperationState::InContext { .. }))
    );
    let compact = before
        .commits
        .iter()
        .find(|commit| commit.mutation().snapshot_reason() == Some(SnapshotReason::Compaction))
        .expect("joint compaction checkpoint");
    let checkpoint = compact.checkpoint().unwrap();
    assert!(checkpoint.compacted.window_number.unwrap() > 0);
    assert!(
        checkpoint
            .compacted
            .message
            .contains("AUTO_COMPACT_SUMMARY")
    );
    let window = (
        checkpoint.compacted.window_number,
        checkpoint.compacted.window_id.clone(),
    );
    let before_items = test
        .codex
        .conversation_history_snapshot()
        .await
        .items()
        .cloned()
        .collect::<Vec<_>>();
    test.codex.shutdown_and_wait().await?;
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
            None,
            ClientMcpExtensions::default(),
        )
        .await?;
    assert_eq!(
        resumed
            .thread
            .conversation_history_snapshot()
            .await
            .items()
            .cloned()
            .collect::<Vec<_>>(),
        before_items
    );
    let recovered = test.thread_store.experimental_panng_read(id).await?;
    assert_eq!(recovered.runtime.operations(), before.runtime.operations());
    let restored = recovered
        .commits
        .iter()
        .find_map(|commit| commit.checkpoint())
        .unwrap();
    assert_eq!(
        (
            restored.compacted.window_number,
            restored.compacted.window_id.clone()
        ),
        window
    );
    resumed
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Continue using the retained compacted window.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    loop {
        let event =
            tokio::time::timeout(Duration::from_secs(40), resumed.thread.next_event()).await??;
        match event.msg {
            EventMsg::TurnComplete(turn) => {
                assert!(turn.error.is_none());
                break;
            }
            EventMsg::Error(error) => {
                anyhow::bail!("cold continued turn failed: {}", error.message)
            }
            _ => {}
        }
    }
    let requests = responses.requests();
    assert_eq!(requests.len(), 5);
    assert!(requests[4].body_contains_text("AUTO_COMPACT_SUMMARY"));
    assert!(requests[4].body_contains_text("AFTER_COMPACT"));
    resumed.thread.shutdown_and_wait().await?;
    Ok(())
}
