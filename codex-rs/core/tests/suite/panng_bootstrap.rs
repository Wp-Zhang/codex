//! Real store → Session bootstrap coverage and remaining unsupported-path admission.
use anyhow::Result;
use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_history::InitialHistory;
use codex_history::PanngCommit;
use codex_history::PanngCommitData;
use codex_history::ResponseItemEnvelope;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_panng_runtime::*;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageRecord;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panng_real_session_bootstrap_fences_pending_and_restores_context_tokens() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_history_mode(ThreadHistoryMode::PanngV1);
    let initial = builder.build_with_auto_env(&server).await?;
    let id = initial.session_configured.thread_id;
    let before = initial.thread_store.experimental_panng_read(id).await?;
    assert_eq!(
        before.commits.len(),
        2,
        "initial checkpoint and effective settings"
    );
    let last = before.commits.last().unwrap();
    let digest = Digest::new("a".repeat(64))?;
    let pending = OperationRecord::prepared(
        OperationIdentity {
            id: OperationId::new(Uuid::now_v7().to_string())?,
            origin_branch: before.branch.clone(),
            owner_epoch: before.runtime.execution_epoch().clone(),
            turn_id: TurnId::new("saved-turn".into())?,
            logical_step_id: LogicalStepId::new(Uuid::now_v7().to_string())?,
            call_id: CallId::new("pending-call".into())?,
            provider_response_id: None,
        },
        DispatchDescriptor::new(
            ToolName::new("panng_probe".into())?,
            BoundedJson::new("{}".into())?,
            digest.clone(),
            digest.clone(),
            digest,
        ),
    );
    let remembered: ResponseItem = serde_json::from_value(json!({
        "type":"message", "id":"saved-user", "role":"user", "content":[{"type":"input_text","text":"Remember the pending operation"}]
    }))?;
    let usage = TokenUsageRecord {
        thread_id: id,
        session_id: id.into(),
        turn_id: "saved-turn".into(),
        root_turn_id: "saved-turn".into(),
        response_id: "saved-response".into(),
        usage: TokenUsage {
            total_tokens: 17,
            ..Default::default()
        },
        turn_token_usage: TokenUsage::default(),
        thread_token_usage: TokenUsage {
            total_tokens: 42,
            ..Default::default()
        },
    };
    let commit = PanngCommit::new(PanngCommitData {
        version: 1,
        branch: before.branch.clone(),
        commit_id: CommitId::new(Uuid::now_v7().to_string())?,
        sequence: last.sequence() + 1,
        previous_sequence: Some(last.sequence()),
        mutation: RuntimeMutation::delta(vec![OperationChange::new(
            /*expected_revision*/ None,
            /*revision*/ 0,
            pending.clone(),
        )?])?,
        host_effects: vec![
            RolloutItem::ResponseItem(ResponseItemEnvelope::new(remembered.clone())),
            RolloutItem::TokenUsageRecord(usage.clone()),
        ],
        checkpoint: None,
    })
    .map_err(anyhow::Error::msg)?;
    initial
        .thread_store
        .experimental_panng_append(id, commit.clone())
        .await?;
    initial.codex.shutdown_and_wait().await?;
    let loaded = initial
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: id,
            include_archived: true,
        })
        .await?;
    let stored_commit = loaded
        .items
        .iter()
        .find_map(|item| match item {
            RolloutItem::PanngCommit(stored) if stored.commit_id() == commit.commit_id() => {
                Some(stored)
            }
            _ => None,
        })
        .expect("canonical source commit survives loading");
    assert_eq!(
        serde_json::to_value(stored_commit)?,
        serde_json::to_value(&commit)?
    );
    let resumed = initial
        .thread_manager
        .resume_thread_with_history(
            initial.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: id,
                history: Arc::new(loaded.items),
                rollout_path: initial.session_configured.rollout_path.clone(),
            }),
            initial.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await?;
    assert_eq!(
        resumed.thread.config_snapshot().await.history_mode,
        ThreadHistoryMode::PanngV1
    );
    let snapshot = resumed.thread.conversation_history_snapshot().await;
    // Native replay media preparation preserves each content item's classification;
    // legacy text without a classification becomes `unknown`, even when there is no image.
    let mut normalized = remembered.clone();
    let content = to_annotated_content(&mut normalized).expect("message content");
    set_annotated_content(&mut normalized, content).expect("message content");
    assert_eq!(
        snapshot.items().cloned().collect::<Vec<_>>(),
        vec![normalized]
    );
    let info = resumed.thread.token_usage_info().await.unwrap();
    assert_eq!(
        (info.total_token_usage, info.last_token_usage),
        (usage.thread_token_usage, usage.usage)
    );
    let after = initial.thread_store.experimental_panng_read(id).await?;
    assert_eq!(
        after.commits.len(),
        2,
        "bounded replay starts at new ResumeFence followed by settings"
    );
    assert_eq!(
        after.commits[0].mutation().snapshot_reason(),
        Some(SnapshotReason::ResumeFence)
    );
    // The fence reconstructs canonical messages before live media preparation. The original
    // passthrough metadata remains intact instead of persisting live normalization back to disk.
    let checkpoint = after.commits[0].checkpoint().unwrap();
    assert_eq!(
        checkpoint
            .compacted
            .replacement_history
            .as_ref()
            .unwrap()
            .iter()
            .map(|envelope| envelope.item.clone())
            .collect::<Vec<_>>(),
        vec![remembered]
    );
    assert_ne!(
        after.runtime.execution_epoch(),
        before.runtime.execution_epoch()
    );
    let (_, recovered) = &after.runtime.operations()[0];
    assert_eq!(recovered.identity(), pending.identity());
    assert!(
        matches!(recovered.state(), OperationState::Ready { outcome: ExecutionOutcome::Indeterminate, result, .. } if result.unknown_reason() == Some(UnknownResultReason::ResumeFence))
    );
    let current_settings = codex_history::host_replay_items(&[RolloutItem::PanngCommit(Box::new(
        after.commits[1].clone(),
    ))])
    .filter_map(|item| match item {
        codex_history::HostReplayItem::Item(RolloutItem::EventMsg(
            EventMsg::ThreadSettingsApplied(event),
        )) => Some(event.thread_settings.clone()),
        _ => None,
    })
    .next()
    .unwrap();
    assert_eq!(
        current_settings,
        resumed.thread.thread_settings_snapshot().await
    );
    assert!(
        resumed
            .thread
            .call_mcp_tool(
                "unused", "unused", /*arguments*/ None, /*meta*/ None
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("not supported by Panng")
    );
    resumed
        .thread
        .submit(Op::RunUserShellCommand {
            command: "printf bypass".into(),
            timeout_ms: Some(1_000),
        })
        .await?;
    wait_for_event(
        &resumed.thread,
        |event| matches!(event, EventMsg::Error(error) if error.message.contains("not supported by Panng")),
    )
    .await;
    assert_eq!(
        initial.thread_store.experimental_panng_read(id).await?.cut,
        after.cut,
        "gated input must not append"
    );
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path().ends_with("/responses"))
    );
    resumed.thread.shutdown_and_wait().await?;
    Ok(())
}
