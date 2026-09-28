//! Actual JSON-RPC tools remain visible after the Session has been unloaded.
use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::*;
use core_test_support::TestTargetOs;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::sse;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::time::Duration;
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn panng_cold_public_read_keeps_real_command_and_patch_items() -> Result<()> {
    let (create, read) = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => {
            ("printf 'before\\n' > ui-history.txt", "cat ui-history.txt")
        }
        TestTargetOs::Windows => (
            "Set-Content ui-history.txt before",
            "Get-Content ui-history.txt",
        ),
    };
    let command = |id: &str, cmd: &str| {
        sse(vec![
            ev_function_call(
                id,
                "exec_command",
                &json!({"cmd":cmd,"login":false,"timeout_ms":10_000}).to_string(),
            ),
            ev_completed(id),
        ])
    };
    let provider = create_mock_responses_server_sequence(vec![
        command("ui-create", create),
        sse(vec![ev_custom_tool_call("ui-patch", "apply_patch", "*** Begin Patch\n*** Update File: ui-history.txt\n@@\n-before\n+after\n*** End Patch"), ev_completed("patch")]),
        command("ui-read", read),
        sse(vec![ev_assistant_message("done", "done"), ev_completed("done")]),
    ]).await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&provider.uri())
        .with_model("gpt-5.5")
        .with_root_config("thread_unload_delay_secs = 1")
        .disable_feature(codex_features::Feature::CodeMode)
        .disable_feature(codex_features::Feature::ShellZshFork)
        .disable_feature(codex_features::Feature::ShellSnapshot)
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build()
        .await?;
    app.initialize().await?;
    let request = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::PanngV1),
            approval_policy: Some(AskForApproval::Never),
            sandbox: Some(SandboxMode::DangerFullAccess),
            ..Default::default()
        })
        .await?;
    let started: ThreadStartResponse = timeout(TIMEOUT, app.read_response(request)).await??;
    let request = app
        .send_turn_start_request(TurnStartParams {
            thread_id: started.thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Create the file, patch it, and read it back.".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn: TurnStartResponse = timeout(TIMEOUT, app.read_response(request)).await??;
    let done: TurnCompletedNotification =
        timeout(TIMEOUT, app.read_notification("turn/completed")).await??;
    assert_eq!(done.turn.id, turn.turn.id);
    assert_eq!(done.turn.status, TurnStatus::Completed);
    let request = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: started.thread.id.clone(),
            include_turns: true,
        })
        .await?;
    let live: ThreadReadResponse = timeout(TIMEOUT, app.read_response(request)).await??;
    let mut tool_ids = live
        .thread
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| match item {
            ThreadItem::CommandExecution {
                id,
                exit_code,
                aggregated_output,
                ..
            } => {
                assert_eq!(*exit_code, Some(0));
                if id == "ui-read" {
                    assert_eq!(aggregated_output.as_deref().map(str::trim), Some("after"));
                }
                Some(id.clone())
            }
            ThreadItem::FileChange { id, changes, .. } => {
                assert!(
                    serde_json::to_string(changes)
                        .unwrap()
                        .contains("ui-history.txt")
                );
                Some(id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    tool_ids.sort();
    assert_eq!(tool_ids, vec!["ui-create", "ui-patch", "ui-read"]);
    let request = app
        .send_thread_unsubscribe_request(ThreadUnsubscribeParams {
            thread_id: started.thread.id.clone(),
        })
        .await?;
    let _: ThreadUnsubscribeResponse = timeout(TIMEOUT, app.read_response(request)).await??;
    let closed: ThreadClosedNotification =
        timeout(TIMEOUT, app.read_notification("thread/closed")).await??;
    assert_eq!(closed.thread_id, started.thread.id);
    let path = started.thread.path.as_ref().context("canonical journal")?;
    let before = std::fs::read(path)?;
    let request = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: started.thread.id.clone(),
            include_turns: true,
        })
        .await?;
    let cold: ThreadReadResponse = timeout(TIMEOUT, app.read_response(request)).await??;
    assert_eq!(cold.thread.turns, live.thread.turns);
    assert_eq!(
        std::fs::read(path)?,
        before,
        "cold UI read must not append a fence"
    );
    timeout(TIMEOUT, app.shutdown_gracefully()).await??;
    Ok(())
}
