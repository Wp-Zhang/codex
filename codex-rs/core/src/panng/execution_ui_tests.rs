use super::*;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn panng_stop_commits_one_unconfirmed_command_ui_with_ready() -> anyhow::Result<()> {
    for preserve_native in [false, true] {
        let (_home, startup, session, step) = fixture().await?;
        let pool = DurablePool::new();
        let cancel = CancellationToken::new();
        let (mut tool, mut original) = call("interrupted", r#"{"cmd":"sleep 100","login":false}"#);
        tool.tool_name = codex_tools::ToolName::plain("exec_command");
        let ResponseItem::FunctionCall { name, .. } = &mut original.item else {
            unreachable!()
        };
        *name = "exec_command".into();
        pool.submit(
            &session,
            Arc::clone(&step),
            logical_step()?,
            tool,
            original,
            &cancel,
            |token| {
                Box::pin(async move {
                    token.cancelled().await;
                    Err(CodexErr::TurnAborted)
                })
            },
        )
        .await?;
        let native = if preserve_native {
            let before = session
                .live_thread()
                .unwrap()
                .experimental_panng_read()
                .await?;
            let operation = &before.runtime.operations()[0].1;
            let mut event = ui::unconfirmed_command(&session, &step, operation).await?;
            let TurnItem::CommandExecution(item) = &mut event.item else {
                unreachable!()
            };
            item.exit_code = Some(130);
            item.aggregated_output = Some("native interruption".into());
            pool.capture_tool_completed(event.clone()).await?;
            Some(event)
        } else {
            None
        };
        pool.stop(&session).await?;
        pool.stop(&session).await?;
        let recovered = session
            .live_thread()
            .unwrap()
            .experimental_panng_read()
            .await?;
        let mut runtime = None;
        let mut completed = Vec::new();
        for commit in &recovered.commits {
            let next = commit.mutation().apply(
                commit.branch(),
                runtime.as_ref().map(|state| (commit.branch(), state)),
            )?;
            for effect in commit.host_effects() {
                if let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = effect {
                    assert!(matches!(
                        next.operations()[0].1.state(),
                        OperationState::Ready {
                            outcome: ExecutionOutcome::Indeterminate,
                            ..
                        }
                    ));
                    completed.push(event.clone());
                }
            }
            runtime = Some(next);
        }
        assert_eq!(completed.len(), 1);
        if let Some(native) = native {
            assert_eq!(
                serde_json::to_value(&completed)?,
                serde_json::to_value(vec![native])?
            );
        } else {
            let TurnItem::CommandExecution(item) = &completed[0].item else {
                panic!("native command UI")
            };
            assert_eq!(
                (
                    &item.id,
                    &item.command,
                    item.status,
                    item.exit_code,
                    &item.process_id,
                    item.aggregated_output.as_deref()
                ),
                (
                    &"interrupted".to_string(),
                    &vec!["sleep 100".to_string()],
                    CommandExecutionStatus::Failed,
                    None,
                    &None,
                    Some(ui::UNCONFIRMED)
                ),
            );
        }
        assert!(
            recovered
                .runtime
                .operations()
                .iter()
                .all(|(_, operation)| matches!(
                    operation.state(),
                    OperationState::InContext { .. }
                ))
        );
        startup.cleanup().await;
    }
    Ok(())
}
