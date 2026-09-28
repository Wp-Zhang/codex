use super::*;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::ThreadSettingsOverrides;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_real_approval_grant_executes_and_denial_has_no_side_effect() -> anyhow::Result<()> {
    for decision in [
        ReviewDecision::Approved,
        ReviewDecision::denied("user declined"),
    ] {
        let harness = harness().await?;
        let command = match test_target_os() {
            TestTargetOs::Linux | TestTargetOs::MacOs => "printf approved > approval.txt",
            TestTargetOs::Windows => "Set-Content -NoNewline approval.txt approved",
        };
        let mock = core_test_support::responses::mount_sse_sequence(harness.server(), vec![
            sse(vec![ev_function_call("approval", "exec_command", &json!({
                "cmd":command, "login":false, "timeout_ms":10_000,
                "sandbox_permissions":"require_escalated", "justification":"Exercise the user's approval decision",
            }).to_string()), ev_completed("command")]),
            sse(vec![ev_assistant_message("done", "done"), ev_completed("done")]),
        ]).await;
        let test = harness.test();
        test.codex
            .start_or_steer_turn(
                TurnInputRequest::user_input(vec![UserInput::Text {
                    text: "Write the file only if I approve.".into(),
                    text_elements: Vec::new(),
                }])
                .with_thread_settings(ThreadSettingsOverrides {
                    approval_policy: Some(AskForApproval::OnRequest),
                    sandbox_policy: Some(SandboxPolicy::ReadOnly {
                        network_access: false,
                    }),
                    ..Default::default()
                }),
            )
            .await?;
        let mut approvals = 0;
        loop {
            let event =
                tokio::time::timeout(Duration::from_secs(45), test.codex.next_event()).await??;
            match event.msg {
                EventMsg::ExecApprovalRequest(approval) => {
                    approvals += 1;
                    assert!(
                        !harness.path_exists("approval.txt").await?,
                        "no effect before approval"
                    );
                    test.codex
                        .submit(Op::ExecApproval {
                            id: approval.effective_approval_id(),
                            turn_id: Some(approval.turn_id),
                            decision: decision.clone(),
                        })
                        .await?;
                }
                EventMsg::TurnComplete(turn) => {
                    assert!(turn.error.is_none(), "{turn:?}");
                    break;
                }
                EventMsg::Error(error) => anyhow::bail!("approval turn failed: {}", error.message),
                EventMsg::TurnAborted(_) => anyhow::bail!("approval turn aborted"),
                _ => {}
            }
        }
        assert_eq!(approvals, 1);
        let approved = matches!(decision, ReviewDecision::Approved);
        assert_eq!(harness.path_exists("approval.txt").await?, approved);
        if approved {
            assert_eq!(harness.read_file_text("approval.txt").await?, "approved");
        }
        assert_eq!(mock.requests().len(), 2);
        let history = test
            .thread_store
            .experimental_panng_read(test.session_configured.thread_id)
            .await?;
        assert_eq!(history.runtime.operations().len(), 1);
        assert!(
            history
                .runtime
                .operations()
                .iter()
                .all(|(_, operation)| matches!(
                    operation.state(),
                    OperationState::InContext { .. }
                ))
        );
        test.codex.shutdown_and_wait().await?;
    }
    Ok(())
}
