//! Maintain bounded replay at acknowledged cuts; no second runtime or history authority.
use super::panng_host_commit::HostTransaction;
use super::*;
use codex_history::PanngHostCheckpoint;
use codex_panng_runtime::ExecutionEpoch;
use codex_panng_runtime::OperationState;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::RuntimeSnapshot;
use codex_panng_runtime::SnapshotReason;

impl Session {
    /// Called before deriving a new candidate. Replay maintenance leaves live context untouched.
    pub(crate) async fn begin_panng_transaction(&self) -> CodexResult<HostTransaction> {
        let bootstrap = self
            .services
            .thread_extension_data
            .get::<panng_bootstrap::Bootstrap>()
            .ok_or_else(|| {
                CodexErr::InvalidRequest("Panng transaction requires a bootstrapped journal".into())
            })?;
        let mut transaction = bootstrap
            .coordinator()
            .begin_transaction()
            .await
            .map_err(fatal)?;
        if transaction.checkpoint_due().map_err(fatal)? {
            let history = self.state.lock().await.clone_history();
            let checkpoint = self.panng_checkpoint_for_history(&history).await?;
            let runtime = transaction.runtime().map_err(fatal)?.clone();
            transaction
                .prepare(
                    RuntimeMutation::snapshot(runtime, SnapshotReason::ReplayBound)
                        .map_err(fatal)?,
                    Some(checkpoint),
                    Vec::new(),
                )
                .map_err(fatal)?;
            let live = self
                .live_thread_for_persistence("maintain Panng replay bound")
                .map_err(fatal)?;
            transaction
                .commit_and_publish(live, |_| async { Ok(()) })
                .await
                .map_err(fatal)?;
            transaction = bootstrap
                .coordinator()
                .begin_transaction()
                .await
                .map_err(fatal)?;
        }
        Ok(transaction)
    }

    /// Snapshot metadata and companions for the supplied candidate; caller holds the host lease.
    pub(crate) async fn panng_checkpoint_for_history(
        &self,
        history: &ContextManager,
    ) -> CodexResult<PanngHostCheckpoint> {
        let (window_number, ids, usage, last_started_turn_id, previous_turn_settings) = {
            let state = self.state.lock().await;
            (
                state.auto_compact_window_number(),
                state.auto_compact_window_ids(),
                state.latest_token_usage_record.clone(),
                state.last_started_turn_id.clone(),
                state.previous_turn_settings(),
            )
        };
        let mut companions = Vec::new();
        if let Some(world) = history.world_state_checkpoint() {
            companions.push(RolloutItem::WorldState(world));
        }
        if let Some(context) = history.reference_context_item() {
            companions.push(RolloutItem::TurnContext(context));
        }
        companions.push(RolloutItem::EventMsg(
            thread_settings::applied_event(self).await,
        ));
        Ok(PanngHostCheckpoint {
            compacted: CompactedItem {
                message: String::new(),
                replacement_history: Some(history.annotated_items().to_vec()),
                guardian_history: history.guardian_history_checkpoint(),
                retained_context: Some(history.retained_context().clone()),
                mcp_resource_origins: self.services.mcp_runtime.resource_origin_checkpoint(),
                window_number: Some(window_number),
                first_window_id: Some(ids.first_window_id.to_string()),
                previous_window_id: ids.previous_window_id.map(|id| id.to_string()),
                window_id: Some(ids.window_id.to_string()),
                compaction_response_id: None,
                latest_token_usage_record: usage,
                resume_metadata: Some(CompactionResumeMetadata {
                    multi_agent_version: self.multi_agent_version(),
                    last_started_turn_id,
                    previous_turn_settings,
                }),
            },
            companions,
            parent_cut: None,
        })
    }

    pub(crate) async fn panng_runtime_snapshot(
        &self,
        _turn: &TurnContext,
    ) -> CodexResult<RuntimeSnapshot> {
        let transaction = self.begin_panng_transaction().await?;
        let runtime = transaction.runtime().map_err(fatal)?.clone();
        transaction.cancel_unprepared().map_err(fatal)?;
        Ok(runtime)
    }

    /// Only at a drained model-step boundary. The runtime validates all old records are InContext.
    pub(crate) async fn retire_panng_delivered(&self, _turn: &TurnContext) -> CodexResult<()> {
        let mut transaction = self.begin_panng_transaction().await?;
        if transaction
            .runtime()
            .map_err(fatal)?
            .operations()
            .is_empty()
        {
            return transaction.cancel_unprepared().map_err(fatal);
        }
        if transaction
            .runtime()
            .map_err(fatal)?
            .operations()
            .iter()
            .any(|(_, operation)| !matches!(operation.state(), OperationState::InContext { .. }))
        {
            return Err(fatal(
                "Panng retirement requires every operation to be delivered",
            ));
        }
        let runtime = RuntimeSnapshot::new(
            ExecutionEpoch::new(uuid::Uuid::now_v7().to_string()).map_err(fatal)?,
            Vec::new(),
        )
        .map_err(fatal)?;
        let history = self.state.lock().await.clone_history();
        let checkpoint = self.panng_checkpoint_for_history(&history).await?;
        transaction
            .prepare(
                RuntimeMutation::snapshot(runtime, SnapshotReason::RetireDelivered)
                    .map_err(fatal)?,
                Some(checkpoint),
                Vec::new(),
            )
            .map_err(fatal)?;
        let live = self
            .live_thread_for_persistence("retire delivered Panng operations")
            .map_err(fatal)?;
        transaction
            .commit_and_publish(live, |_| async { Ok(()) })
            .await
            .map_err(fatal)?;
        Ok(())
    }
}

fn fatal(error: impl std::fmt::Display) -> CodexErr {
    CodexErr::Fatal(error.to_string())
}
