//! Native compaction producers prepare candidates; Panng installs them only after a joint ACK.
use super::*;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::SnapshotReason;

impl Session {
    /// Native windows advance as before. Panng only plans IDs until its joint checkpoint ACK.
    pub(crate) async fn prepare_compaction_window(&self) -> (u64, AutoCompactWindowIds) {
        let planned = {
            let state = self.state.lock().await;
            if state.session_configuration.history_mode == ThreadHistoryMode::PanngV1 {
                let current = state.auto_compact_window_ids();
                Some((
                    state.auto_compact_window_number().saturating_add(1),
                    AutoCompactWindowIds {
                        first_window_id: current.first_window_id,
                        previous_window_id: Some(current.window_id),
                        window_id: uuid::Uuid::now_v7(),
                    },
                ))
            } else {
                None
            }
        };
        match planned {
            Some(window) => window,
            None => self.advance_auto_compact_window().await,
        }
    }

    pub(crate) async fn replace_compacted_history_checked(
        &self,
        mut items: Vec<ResponseItemEnvelope>,
        reference: Option<TurnContextItem>,
        world: Option<Arc<WorldState>>,
        metadata: CompactedHistoryMetadata,
    ) -> CodexResult<()> {
        if self.state.lock().await.session_configuration.history_mode != ThreadHistoryMode::PanngV1
        {
            self.replace_compacted_history(items, reference, world, metadata)
                .await;
            return Ok(());
        }
        let bootstrap = self
            .services
            .thread_extension_data
            .get::<panng_bootstrap::Bootstrap>()
            .ok_or_else(|| fatal("Panng compaction requires a bootstrapped journal"))?;
        let _settings = thread_settings::acquire_persistence_lock(self).await;
        let mut transaction = bootstrap
            .coordinator()
            .begin_transaction()
            .await
            .map_err(fatal)?;
        let (baseline, old_number, old_ids) = {
            let state = self.state.lock().await;
            (
                state.clone_history(),
                state.auto_compact_window_number(),
                state.auto_compact_window_ids(),
            )
        };
        if metadata.window_number != old_number.saturating_add(1)
            || metadata.window_ids.previous_window_id != Some(old_ids.window_id)
            || metadata.window_ids.first_window_id != old_ids.first_window_id
        {
            return Err(fatal(
                "Panng compaction window changed before candidate preparation",
            ));
        }
        for envelope in &mut items {
            Self::assign_missing_response_item_id(&mut envelope.item);
        }
        let mcp_revision = self
            .services
            .executed_tool_calls
            .mcp_attribution_checkpoint(true)
            .and_then(|(attribution, revision)| {
                items.last_mut().map(|envelope| {
                    envelope.metadata.get_or_insert_default().mcp_attribution = Some(attribution);
                    revision
                })
            });
        if let Some(checkpoint) = items.iter_mut().rev().find(|envelope| {
            matches!(
                envelope.item,
                ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
            )
        }) {
            checkpoint
                .metadata
                .get_or_insert_default()
                .compaction_model_hash = metadata.compaction_model_hash;
        }
        let snapshot = world.map(|world| world.snapshot()).unwrap_or_else(|| {
            let mut retained = serde_json::Map::new();
            if let Some(previous) = baseline.world_state_checkpoint() {
                for contributor in self.services.extensions.context_contributors() {
                    retained
                        .extend(contributor.retain_world_state_after_compaction(&previous.state));
                }
            }
            WorldStateSnapshot::from(&retained)
        });
        let mut candidate = baseline.clone();
        let invalidate_reviews =
            candidate.replace_compacted(items, metadata.reviewer_compaction_hash.as_deref());
        candidate.set_reference_context_item(reference);
        candidate.set_world_state_baseline(snapshot);
        let mut checkpoint = self.panng_checkpoint_for_history(&candidate).await?;
        checkpoint.compacted.message = metadata.message;
        checkpoint.compacted.window_number = Some(metadata.window_number);
        checkpoint.compacted.first_window_id =
            Some(metadata.window_ids.first_window_id.to_string());
        checkpoint.compacted.previous_window_id = metadata
            .window_ids
            .previous_window_id
            .map(|id| id.to_string());
        checkpoint.compacted.window_id = Some(metadata.window_ids.window_id.to_string());
        checkpoint.compacted.compaction_response_id = metadata.compaction_response_id;
        let runtime = transaction.runtime().map_err(fatal)?.clone();
        transaction
            .prepare(
                RuntimeMutation::snapshot(runtime, SnapshotReason::Compaction).map_err(fatal)?,
                Some(checkpoint),
                Vec::new(),
            )
            .map_err(fatal)?;
        let live = self
            .live_thread_for_persistence("install Panng compaction")
            .map_err(fatal)?;
        transaction
            .commit_and_publish(live, |_| async move {
                let mut state = self.state.lock().await;
                if state.history.history_version() != baseline.history_version()
                    || !std::ptr::eq(state.history.annotated_items(), baseline.annotated_items())
                    || !std::ptr::eq(
                        state.history.retained_context(),
                        baseline.retained_context(),
                    )
                    || state.history.token_info() != baseline.token_info()
                    || state.history.reference_context_item() != baseline.reference_context_item()
                    || state.history.world_state_checkpoint() != baseline.world_state_checkpoint()
                    || state.auto_compact_window_number() != old_number
                    || state.auto_compact_window_ids() != old_ids
                {
                    anyhow::bail!(
                        "Panng history changed during compaction ACK; strict reopen required"
                    );
                }
                if invalidate_reviews {
                    std::mem::take(&mut state.history_reset).cancel();
                }
                state.history = candidate;
                // Clear reminder/request/prefill state only after ACK, then install the recorded IDs.
                state.start_new_context_window();
                state.restore_auto_compact_window(metadata.window_number, metadata.window_ids);
                state.reasoning_effort_pin = ReasoningEffortPin::Compacted;
                state.queue_pending_session_start_source(codex_hooks::SessionStartSource::Compact);
                if let Some(revision) = mcp_revision {
                    self.services
                        .executed_tool_calls
                        .mark_mcp_attribution_persisted(revision);
                }
                Ok(())
            })
            .await
            .map_err(fatal)?;
        Ok(())
    }

    /// The compact checkpoint itself owns this derived completion notification.
    pub(crate) async fn emit_compaction_completed(&self, turn: &TurnContext, item: TurnItem) {
        if turn.history_mode != ThreadHistoryMode::PanngV1 {
            self.emit_turn_item_completed(turn, item).await;
            return;
        }
        record_turn_ttfm_metric(turn, &item).await;
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_item_completed(
                    &self.services.thread_extension_data,
                    turn.extension_data.as_ref(),
                    &item,
                )
                .await;
        }
        let completed_at_ms = now_unix_timestamp_ms();
        let started_at_ms = turn.turn_timing_state.take_item_started(&item.id()).await;
        self.deliver_panng_event(
            turn,
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: self.thread_id,
                turn_id: turn.sub_id.clone(),
                item,
                started_at_ms,
                completed_at_ms,
            }),
        )
        .await;
    }

    pub(crate) async fn start_new_context_window_checked(
        &self,
        step: &StepContext,
        world: Arc<WorldState>,
    ) -> CodexResult<u64> {
        if step.turn.history_mode != ThreadHistoryMode::PanngV1 {
            return Ok(self.start_new_context_window(step, world).await);
        }
        let retained = if self.enabled(Feature::RetainClientDeveloperMessages) {
            crate::compact_remote_v2::truncate_retained_messages_for_remote_compaction(
                self.clone_history()
                    .await
                    .annotated_items()
                    .iter()
                    .filter(|item| {
                        crate::compact_remote_v2::is_client_authored_developer_message(item)
                    })
                    .cloned()
                    .collect(),
                crate::compact_remote_v2::RETAINED_MESSAGE_TOKEN_BUDGET,
            )
        } else {
            Vec::new()
        };
        let (window_number, window_ids) = self.prepare_compaction_window().await;
        let items = self
            .build_initial_context_with_world_state(step, world.as_ref())
            .await
            .into_iter()
            .map(ResponseItemEnvelope::new)
            .chain(retained)
            .collect();
        self.replace_compacted_history_checked(
            items,
            Some(step.to_turn_context_item()),
            Some(world),
            CompactedHistoryMetadata {
                message: String::new(),
                window_number,
                window_ids,
                compaction_response_id: None,
                compaction_model_hash: None,
                reviewer_compaction_hash: None,
            },
        )
        .await?;
        self.recompute_token_usage(&step.turn).await;
        Ok(window_number)
    }
}
fn fatal(error: impl std::fmt::Display) -> CodexErr {
    CodexErr::Fatal(error.to_string())
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
