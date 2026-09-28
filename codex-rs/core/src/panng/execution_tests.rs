use super::*;
use crate::session::startup::SessionStartup;
use crate::tools::context::ToolPayload;
use codex_history::CodexHarnessMetadata;
use codex_history::RolloutItem;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

async fn fixture() -> anyhow::Result<(
    tempfile::TempDir,
    Arc<SessionStartup>,
    Arc<Session>,
    Arc<StepContext>,
)> {
    let home = tempfile::tempdir()?;
    let codex_home = AbsolutePathBuf::try_from(home.path())?;
    let startup = Arc::new(SessionStartup::default());
    let (tx, _rx) = async_channel::unbounded();
    let session = crate::session::tests::make_session_with_setup_and_events(
        |config| {
            config.codex_home = codex_home;
            config.ephemeral = false;
        },
        |_| {},
        codex_protocol::protocol::ThreadHistoryMode::PanngV1,
        Some(Arc::clone(&startup)),
        tx,
        Arc::new(codex_extension_api::ExtensionRegistryBuilder::new().build()),
    )
    .await?;
    let step = StepContext::for_test(session.new_default_turn().await);
    Ok((home, startup, session, step))
}
fn logical_step() -> anyhow::Result<LogicalStepId> {
    Ok(LogicalStepId::new(uuid::Uuid::new_v4().to_string())?)
}
fn call(id: &str, arguments: &str) -> (ToolCall, ResponseItemEnvelope) {
    (
        ToolCall {
            tool_name: codex_tools::ToolName::plain("panng_probe"),
            call_id: id.into(),
            payload: ToolPayload::Function {
                arguments: arguments.into(),
            },
            encrypted_function_args: None,
        },
        ResponseItemEnvelope::new(ResponseItem::FunctionCall {
            id: None,
            name: "panng_probe".into(),
            namespace: None,
            arguments: arguments.into(),
            encrypted_function_args: None,
            call_id: id.into(),
            internal_chat_message_metadata_passthrough: None,
        }),
    )
}
fn output(id: &str, text: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(id.into()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text(text.into()),
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata {
            delivered_assistant_message: Some("preserved metadata".into()),
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn durable_probe_factory_observes_committed_intent_and_unique_running_output()
-> anyhow::Result<()> {
    let (_home, startup, session, step) = fixture().await?;
    let pool = DurablePool::new();
    let cancel = CancellationToken::new();
    let path = session
        .live_thread()
        .unwrap()
        .local_rollout_path()
        .await?
        .unwrap();
    let result = output("a", "done");
    let expected = serde_json::to_string(&RolloutItem::ResponseItem(result.clone()))?;
    let (tool, original) = call("a", "{}");
    pool.submit(
        &session,
        step,
        logical_step()?,
        tool,
        original,
        &cancel,
        |_| {
            let persisted =
                codex_rollout::read_panng_suffix(std::fs::File::open(&path).unwrap()).unwrap();
            assert_eq!(persisted.runtime.operations().len(), 1);
            assert_eq!(
                persisted.runtime.operations()[0].1.state(),
                &OperationState::Prepared
            );
            let commit = persisted.commits.last().unwrap();
            assert_eq!(
                commit.host_effects().len(),
                2,
                "call and running output share the Prepared commit"
            );
            Box::pin(async move { Ok(result) })
        },
    )
    .await?;
    assert!(pool.advance(&session, &cancel).await?);
    let persisted = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    let (revision, operation) = &persisted.runtime.operations()[0];
    assert_eq!(*revision, 2);
    let OperationState::InContext { result, .. } = operation.state() else {
        panic!("result delivered");
    };
    assert_eq!(result.inline_payload().unwrap().1, expected);
    let wire_outputs = persisted
        .commits
        .iter()
        .flat_map(codex_history::PanngCommit::host_effects)
        .filter(|effect| {
            matches!(effect, RolloutItem::ResponseItem(envelope) if matches!(&envelope.item,
            ResponseItem::FunctionCallOutput { call_id:Some(id), .. } if id == "a"))
        })
        .count();
    assert_eq!(wire_outputs, 1);
    pool.stop(&session).await?;
    startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn durable_probe_storage_failure_and_duplicate_admission_never_construct_again()
-> anyhow::Result<()> {
    let (_home, startup, session, step) = fixture().await?;
    let pool = DurablePool::new();
    let cancel = CancellationToken::new();
    let logical = logical_step()?;
    let count = AtomicUsize::new(/*v*/ 0);
    for _ in 0..2 {
        let (tool, original) = call("same", "{}");
        pool.submit(
            &session,
            Arc::clone(&step),
            logical.clone(),
            tool,
            original,
            &cancel,
            |token| {
                count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    token.cancelled().await;
                    Err(CodexErr::TurnAborted)
                })
            },
        )
        .await?;
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let (conflict, original) = call("same", r#"{"different":true}"#);
    assert!(
        pool.submit(
            &session,
            Arc::clone(&step),
            logical,
            conflict,
            original,
            &cancel,
            |token| {
                count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    token.cancelled().await;
                    Err(CodexErr::TurnAborted)
                })
            }
        )
        .await
        .is_err()
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    pool.stop(&session).await?;
    session.live_thread().unwrap().shutdown().await?;
    let separate = DurablePool::new();
    let (tool, original) = call("not-started", "{}");
    assert!(
        separate
            .submit(
                &session,
                step,
                logical_step()?,
                tool,
                original,
                &cancel,
                |token| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async move {
                        token.cancelled().await;
                        Err(CodexErr::TurnAborted)
                    })
                }
            )
            .await
            .is_err()
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn durable_probe_fast_result_advances_while_slow_future_remains_pending() -> anyhow::Result<()>
{
    let (_home, startup, session, step) = fixture().await?;
    let pool = DurablePool::new();
    let cancel = CancellationToken::new();
    struct DropSignal(Arc<AtomicUsize>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicUsize::new(/*v*/ 0));
    for id in ["slow", "fast"] {
        let (tool, original) = call(id, "{}");
        pool.submit(
            &session,
            Arc::clone(&step),
            logical_step()?,
            tool,
            original,
            &cancel,
            |token| {
                if id == "slow" {
                    let signal = DropSignal(Arc::clone(&dropped));
                    let worker = tokio::spawn(async move {
                        let _signal = signal;
                        std::future::pending::<()>().await;
                    });
                    Box::pin(async move {
                        token.cancelled().await;
                        worker.abort();
                        let _ = worker.await;
                        Err(CodexErr::TurnAborted)
                    })
                } else {
                    Box::pin(async { Ok(output("fast", "fast result")) })
                }
            },
        )
        .await?;
    }
    assert!(pool.advance(&session, &cancel).await?);
    let persisted = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    let states = persisted
        .runtime
        .operations()
        .iter()
        .map(|(_, operation)| {
            (
                operation.identity().call_id.as_str().to_owned(),
                operation.state().clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(states["slow"], OperationState::Prepared);
    assert!(matches!(
        states["fast"],
        OperationState::InContext {
            outcome: ExecutionOutcome::Succeeded,
            ..
        }
    ));
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    pool.stop(&session).await?;
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "stop must join dispatch cleanup before returning"
    );
    let settled = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    assert!(
        settled
            .runtime
            .operations()
            .iter()
            .all(|(_, operation)| matches!(operation.state(), OperationState::InContext { .. }))
    );
    startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn durable_probe_long_notification_retains_full_result() -> anyhow::Result<()> {
    let (_home, startup, session, step) = fixture().await?;
    let pool = DurablePool::new();
    let cancel = CancellationToken::new();
    let (tool, original) = call("large", "{}");
    let observed = output("large", &"x".repeat(900));
    let expected = serde_json::to_string(&RolloutItem::ResponseItem(observed.clone()))?;
    pool.submit(
        &session,
        step,
        logical_step()?,
        tool,
        original,
        &cancel,
        |_| Box::pin(async move { Ok(observed) }),
    )
    .await?;
    assert!(pool.advance(&session, &cancel).await?);
    pool.stop(&session).await?;
    {
        let state = pool.state.lock().await;
        assert_eq!(
            (
                state.pending.len(),
                state.observed.len(),
                state.futures.len()
            ),
            (0, 0, 0)
        );
    }
    let persisted = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    let (revision, operation) = &persisted.runtime.operations()[0];
    assert_eq!(*revision, 2);
    let OperationState::InContext {
        result, outcome, ..
    } = operation.state()
    else {
        panic!("the bounded notification must deliver the retained result");
    };
    assert_eq!(*outcome, ExecutionOutcome::Succeeded);
    assert_eq!(result.inline_payload().unwrap().1, expected);
    startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_probe_result_encoding_preserves_reloadable_provenance() -> anyhow::Result<()> {
    use codex_history::PanngCommit;
    use codex_history::PanngCommitData;
    use codex_panng_runtime::BranchId;
    use codex_panng_runtime::CommitId;
    use codex_panng_runtime::ResultEncoding;
    use codex_protocol::models::ContentItemKind;
    use codex_protocol::models::ExecutedToolCall;
    use codex_protocol::models::InternalChatMessageMetadataPassthrough;
    let mut original = output("warehouse-call", "done");
    let ResponseItem::FunctionCallOutput {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut original.item
    else {
        unreachable!()
    };
    *internal_chat_message_metadata_passthrough = Some(InternalChatMessageMetadataPassthrough {
        turn_id: Some("origin-turn".into()),
        create_time: Some(serde_json::Number::from(123)),
        content_item_kinds: Some(vec![ContentItemKind("tool.result".into())]),
        cell_id: Some("warehouse-cell".into()),
        executed_tool_calls: Some(vec![ExecutedToolCall::new(
            "panng_probe".into(),
            serde_json::json!({"value":"kept in warehouse only"}),
        )]),
        tool_calls_complete: Some(true),
    });
    let unchanged = original.clone();
    let mut expected = original.clone();
    let ResponseItem::FunctionCallOutput {
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = &mut expected.item
    else {
        unreachable!()
    };
    metadata.cell_id = None;
    metadata.executed_tool_calls = None;
    metadata.tool_calls_complete = None;
    let home = tempfile::tempdir()?;
    let (outcome, stored) = persist_result(home.path(), &original).await?;
    assert_eq!(
        original, unchanged,
        "encoding must not strip the live warehouse claims"
    );
    assert_eq!(outcome, ExecutionOutcome::Succeeded);
    let (encoding, payload) = stored.inline_payload().unwrap();
    assert_eq!(encoding, ResultEncoding::CodexResponseEnvelopeV1);
    let decoded: RolloutItem = serde_json::from_str(payload)?;
    let RolloutItem::ResponseItem(envelope) = &decoded else {
        panic!("result envelope");
    };
    assert_eq!(
        envelope, &expected,
        "turn/time/content classification and all harness metadata survive reload"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(payload)?,
        serde_json::to_value(&decoded)?
    );
    let commit = PanngCommit::new(PanngCommitData {
        version: 1,
        branch: BranchId::new(uuid::Uuid::new_v4().to_string())?,
        commit_id: CommitId::new(uuid::Uuid::new_v4().to_string())?,
        sequence: 1,
        previous_sequence: Some(0),
        mutation: RuntimeMutation::delta(Vec::new())?,
        host_effects: vec![decoded],
        checkpoint: None,
    })
    .map_err(anyhow::Error::msg)?;
    let roundtrip: PanngCommit = serde_json::from_slice(&serde_json::to_vec(&commit)?)?;
    assert_eq!(
        serde_json::to_value(roundtrip)?,
        serde_json::to_value(commit)?
    );
    Ok(())
}

#[path = "execution_ui_tests.rs"]
mod ui_tests;
