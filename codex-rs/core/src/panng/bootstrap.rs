//! Experimental Session bootstrap only. This does not authorize live model/tool execution.
use super::panng_host_commit::HostCommitCoordinator;
use super::*;
use anyhow::bail;
use codex_history::PanngCommit;
use codex_history::PanngCommitData;
use codex_history::PanngHostCheckpoint;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::CommitId;
use codex_panng_runtime::ExecutionEpoch;
use codex_panng_runtime::ExecutionOutcome;
use codex_panng_runtime::OperationState;
use codex_panng_runtime::ResultId;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::RuntimeSnapshot;
use codex_panng_runtime::SnapshotReason;
use codex_panng_runtime::StoredResult;
use codex_panng_runtime::UnknownResultReason;
use codex_thread_store::DurableRecordPosition;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct Bootstrap {
    coordinator: HostCommitCoordinator,
    // Immutable source data for the early-reader equality check, never a second runtime ledger.
    recovered: Option<Arc<codex_rollout::RecoveredPanngSuffix>>,
}

pub(crate) fn initial_checkpoint(
    thread_id: ThreadId,
    window_id: String,
    settings: codex_protocol::protocol::ThreadSettingsSnapshot,
    multi_agent_version: Option<MultiAgentVersion>,
) -> anyhow::Result<PanngCommit> {
    let runtime =
        RuntimeSnapshot::new(ExecutionEpoch::new(Uuid::now_v7().to_string())?, Vec::new())?;
    PanngCommit::new(PanngCommitData {
        version: 1,
        branch: BranchId::new(Uuid::now_v7().to_string())?,
        commit_id: CommitId::new(Uuid::now_v7().to_string())?,
        sequence: 0,
        previous_sequence: None,
        mutation: RuntimeMutation::snapshot(runtime, SnapshotReason::Initial)?,
        host_effects: Vec::new(),
        checkpoint: Some(PanngHostCheckpoint {
            compacted: CompactedItem {
                message: String::new(),
                replacement_history: Some(Vec::new()),
                guardian_history: None,
                retained_context: Some(Default::default()),
                mcp_resource_origins: None,
                window_number: Some(0),
                first_window_id: Some(window_id.clone()),
                previous_window_id: None,
                window_id: Some(window_id),
                compaction_response_id: None,
                latest_token_usage_record: None,
                resume_metadata: Some(CompactionResumeMetadata {
                    multi_agent_version,
                    last_started_turn_id: None,
                    previous_turn_settings: None,
                }),
            },
            companions: vec![RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                codex_protocol::protocol::ThreadSettingsAppliedEvent {
                    thread_id: Some(thread_id),
                    thread_settings: settings,
                },
            ))],
            parent_cut: None,
        }),
    })
    .map_err(anyhow::Error::msg)
}

impl Bootstrap {
    pub(crate) fn created(
        initial: &PanngCommit,
        receipt: DurableRecordPosition,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            coordinator: HostCommitCoordinator::created(initial, receipt)?,
            recovered: None,
        })
    }
    pub(crate) fn resumed(recovered: codex_rollout::RecoveredPanngSuffix) -> anyhow::Result<Self> {
        Ok(Self {
            coordinator: HostCommitCoordinator::recovered(&recovered)?,
            recovered: Some(Arc::new(recovered)),
        })
    }
    pub(crate) fn coordinator(&self) -> &HostCommitCoordinator {
        &self.coordinator
    }

    /// Early configuration was derived before acquiring the writer. Fail if that view is stale.
    pub(crate) async fn validate_resume(&self, supplied: &[RolloutItem]) -> anyhow::Result<()> {
        let recovered = self
            .recovered
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing cold recovery"))?;
        let header = codex_history::PanngSessionHeader::from_slice(&recovered.header_bytes)
            .map_err(anyhow::Error::msg)?;
        let mut canonical = vec![RolloutItem::SessionMeta(header.session_meta().clone())];
        canonical.extend(
            recovered
                .commits
                .iter()
                .cloned()
                .map(|commit| RolloutItem::PanngCommit(Box::new(commit))),
        );
        if serde_json::to_value(supplied)? != serde_json::to_value(&canonical)? {
            bail!(
                "Panng initialization history changed before strict writer acquisition; reload before resuming"
            );
        }
        Ok(())
    }

    pub(crate) async fn finish(
        &self,
        session: &Session,
        supplied: &[RolloutItem],
    ) -> anyhow::Result<()> {
        let live = session.live_thread_for_persistence("finish Panng bootstrap")?;
        let mut transaction = self.coordinator().begin_transaction().await?;
        if transaction.needs_resume_fence()? {
            // Reconstruct the original envelopes, before media preparation changes their representation.
            let turn = session.new_default_turn().await;
            let replay = session
                .reconstruct_history_from_rollout(&turn, supplied)
                .await;
            let mut operations = Vec::new();
            for (revision, operation) in transaction.runtime()?.operations() {
                operations.push(if operation.state() == &OperationState::Prepared {
                    (
                        revision
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("operation revision overflow"))?,
                        operation.ready(
                            ResultId::new(Uuid::now_v7().to_string())?,
                            ExecutionOutcome::Indeterminate,
                            StoredResult::unknown(UnknownResultReason::ResumeFence),
                        )?,
                    )
                } else {
                    (*revision, operation.clone())
                });
            }
            let runtime =
                RuntimeSnapshot::new(ExecutionEpoch::new(Uuid::now_v7().to_string())?, operations)?;
            let mut companions = Vec::new();
            if let Some(world) = replay.world_state_baseline {
                companions.push(RolloutItem::WorldState(WorldStateItem::full(
                    world.into_object(),
                )));
            }
            if let Some(context) = replay.reference_context_item {
                companions.push(RolloutItem::TurnContext(context));
            }
            let saved_settings = host_replay_items(supplied)
                .rev()
                .find_map(|item| match item {
                    HostReplayItem::Item(RolloutItem::EventMsg(
                        EventMsg::ThreadSettingsApplied(event),
                    )) => Some(event.clone()),
                    _ => None,
                })
                .ok_or_else(|| anyhow::anyhow!("missing recovered thread settings"))?;
            companions.push(RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                saved_settings,
            )));
            let checkpoint = PanngHostCheckpoint {
                compacted: CompactedItem {
                    message: String::new(),
                    replacement_history: Some(replay.history),
                    guardian_history: replay.guardian_history,
                    retained_context: Some(replay.retained_context),
                    mcp_resource_origins: session.services.mcp_runtime.resource_origin_checkpoint(),
                    window_number: Some(replay.window_number),
                    first_window_id: replay.first_window_id.map(|id| id.to_string()),
                    previous_window_id: replay.previous_window_id.map(|id| id.to_string()),
                    window_id: replay.window_id.map(|id| id.to_string()),
                    compaction_response_id: None,
                    latest_token_usage_record: Session::last_token_usage_record_from_rollout(
                        supplied,
                    ),
                    resume_metadata: Some(CompactionResumeMetadata {
                        multi_agent_version: session.multi_agent_version(),
                        last_started_turn_id: replay.last_started_turn_id,
                        previous_turn_settings: replay.previous_turn_settings,
                    }),
                },
                companions,
                parent_cut: None,
            };
            transaction.prepare(
                RuntimeMutation::snapshot(runtime, SnapshotReason::ResumeFence)?,
                Some(checkpoint),
                Vec::new(),
            )?;
            transaction
                .commit_and_publish(live, |_| async { Ok(()) })
                .await?;
            transaction = self.coordinator().begin_transaction().await?;
        }
        // Effective settings and any interrupted inherited boundary are local to the child.
        let mut effects = vec![RolloutItem::EventMsg(
            thread_settings::applied_event(session).await,
        )];
        if self.recovered.is_none() {
            effects.extend(
                supplied
                    .iter()
                    .filter(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnAborted(_))))
                    .cloned(),
            );
        }
        transaction.prepare(RuntimeMutation::delta(Vec::new())?, None, effects)?;
        transaction
            .commit_and_publish(live, |_| async { Ok(()) })
            .await?;
        Ok(())
    }
}

/// Explicit stage gate, shared by public paths which bypass the submission queue.
pub(crate) fn reject_live_execution(session: &Session) -> CodexResult<()> {
    if session
        .services
        .thread_extension_data
        .get::<Bootstrap>()
        .is_some()
    {
        return Err(CodexErr::InvalidRequest(
            "This operation is not supported by Panng yet".into(),
        ));
    }
    Ok(())
}
