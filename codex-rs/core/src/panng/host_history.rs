//! Local fallible Panng history entry point. Native recording and live execution gates stay unchanged.
use super::panng_host_commit::HostTransaction;
use super::*;
use crate::context::world_state::WorldStateSnapshot;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::CommitId;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::RuntimeSnapshot;
use codex_thread_store::DurableRecordPosition;

pub(crate) struct PanngHistoryTransaction<'a> {
    session: &'a Session,
    turn: &'a TurnContext,
    transaction: HostTransaction,
    baseline: ContextManager,
    candidate: ContextManager,
    items: Vec<ResponseItemEnvelope>,
    image_preparations: Vec<ImagePreparationMetadata>,
    consumed_orders: Vec<(String, u64)>,
    mcp_revision: Option<u64>,
}

impl Session {
    /// Acquires the lease before preparing IDs, annotations or candidate input order.
    /// All new Panng writers must use this lease; legacy mutation paths remain gated.
    pub(crate) async fn begin_panng_history<'a>(
        &'a self,
        turn: &'a TurnContext,
        model: &ModelInfo,
        items: Vec<ResponseItemEnvelope>,
    ) -> CodexResult<PanngHistoryTransaction<'a>> {
        let transaction = self.begin_panng_transaction().await?;
        let baseline = self.state.lock().await.clone_history();
        self.prepare_panng_history(transaction, baseline, turn, model, items)
            .await
    }

    /// Continues an existing lease so step-context derivation and history installation share a cut.
    pub(crate) async fn prepare_panng_history<'a>(
        &'a self,
        transaction: HostTransaction,
        baseline: ContextManager,
        turn: &'a TurnContext,
        model: &ModelInfo,
        items: Vec<ResponseItemEnvelope>,
    ) -> CodexResult<PanngHistoryTransaction<'a>> {
        let (mut items, image_preparations) = self
            .prepare_annotated_conversation_items_for_history(turn, model, items)
            .await;
        let policy: codex_utils_output_truncation::TruncationPolicy =
            model.truncation_policy.into();
        for envelope in &mut items {
            if let ResponseItem::Reasoning { content, .. } = &mut envelope.item
                && content.as_ref().is_some_and(|parts| {
                    !parts.iter().any(|part| {
                        matches!(
                            part,
                            codex_protocol::models::ReasoningItemContent::ReasoningText { .. }
                        )
                    })
                })
            {
                // Native serialization omits these parts, but deserialization restores None.
                // Use that stable representation before deriving the durable commit identity.
                *content = None;
            }
            if matches!(
                envelope.item,
                ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. }
            ) {
                envelope
                    .metadata
                    .get_or_insert_default()
                    .history_truncation_token_limit
                    .get_or_insert_with(|| with_serialization_allowance(policy).token_budget());
            }
        }
        let force_mcp_checkpoint = items
            .iter()
            .any(|envelope| crate::context_manager::is_user_turn_boundary(&envelope.item));
        let mcp_revision = self
            .services
            .executed_tool_calls
            .mcp_attribution_checkpoint(force_mcp_checkpoint)
            .and_then(|(attribution, revision)| {
                let first = items
                    .iter()
                    .position(|envelope| should_persist_response_item(&envelope.item))?;
                for (index, envelope) in items.iter_mut().enumerate() {
                    if index == first
                        || crate::context_manager::is_user_turn_boundary(&envelope.item)
                    {
                        envelope.metadata.get_or_insert_default().mcp_attribution =
                            Some(attribution.clone());
                    }
                }
                Some(revision)
            });
        let mut candidate = baseline.clone();
        let pending_orders = turn
            .extension_data
            .get::<retained_context::PendingAssistantMessageOrders>();
        let mut consumed_orders = Vec::new();
        for envelope in &mut items {
            if envelope
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.compaction_output)
            {
                continue;
            }
            if matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "assistant")
                || matches!(&envelope.item, ResponseItem::FunctionCall { .. })
                || crate::context::is_user_authorization_message(&envelope.item)
            {
                let reserved = pending_orders.as_ref().and_then(|orders| {
                    let id = envelope.item.id()?.as_str();
                    let order = orders
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(id)
                        .copied()?;
                    consumed_orders.push((id.to_owned(), order));
                    Some(order)
                });
                envelope
                    .metadata
                    .get_or_insert_default()
                    .user_input_order
                    .get_or_insert_with(|| {
                        reserved.unwrap_or_else(|| candidate.reserve_input_order())
                    });
            }
        }
        // This mutates only the candidate and annotates the original envelopes with retained
        // provenance. Canonical originals remain untruncated; live history follows native policy.
        candidate.record_annotated_items(&mut items, policy);
        Ok(PanngHistoryTransaction {
            session: self,
            turn,
            transaction,
            baseline,
            candidate,
            items,
            image_preparations,
            consumed_orders,
            mcp_revision,
        })
    }
}

impl PanngHistoryTransaction<'_> {
    pub(crate) fn projection_target(&self) -> CodexResult<codex_rollout::PanngProjectionTarget> {
        Ok(codex_rollout::PanngProjectionTarget {
            thread_id: self.session.thread_id,
            branch: self
                .transaction
                .branch()
                .map_err(|error| CodexErr::Fatal(error.to_string()))?
                .clone(),
            end: self
                .transaction
                .acknowledged()
                .map_err(|error| CodexErr::Fatal(error.to_string()))?
                .end,
        })
    }

    pub(crate) fn commit_id(&self) -> &CommitId {
        self.transaction.commit_id()
    }

    pub(crate) fn branch(&self) -> CodexResult<&BranchId> {
        self.transaction
            .branch()
            .map_err(|error| CodexErr::Fatal(error.to_string()))
    }

    pub(crate) fn cancel_unprepared(self) -> CodexResult<()> {
        self.transaction
            .cancel_unprepared()
            .map_err(|error| CodexErr::Fatal(error.to_string()))
    }

    pub(crate) fn runtime(&self) -> CodexResult<&RuntimeSnapshot> {
        self.transaction
            .runtime()
            .map_err(|error| CodexErr::Fatal(error.to_string()))
    }

    /// Commits the supplied runtime delta with these exact history envelopes, then installs once.
    pub(crate) async fn commit(
        self,
        mutation: RuntimeMutation,
    ) -> CodexResult<DurableRecordPosition> {
        self.commit_with_effects(mutation, Vec::new()).await
    }

    /// Adds metadata/UI effects; response envelopes must use the prepared history input above.
    pub(crate) async fn commit_with_effects(
        self,
        mutation: RuntimeMutation,
        mut effects: Vec<RolloutItem>,
    ) -> CodexResult<DurableRecordPosition> {
        let Self {
            session,
            turn,
            mut transaction,
            baseline,
            mut candidate,
            items,
            image_preparations,
            consumed_orders,
            mcp_revision,
        } = self;
        let mut latest_token_usage = None;
        for effect in &mut effects {
            match effect {
                RolloutItem::ResponseItem(_) => {
                    return Err(CodexErr::InvalidRequest(
                        "Panng response effects must be prepared through begin_panng_history"
                            .into(),
                    ));
                }
                RolloutItem::TurnContext(context) => {
                    candidate.set_reference_context_item(Some(context.clone()))
                }
                RolloutItem::WorldState(world) => {
                    let snapshot = if world.full {
                        WorldStateSnapshot::from(&world.state)
                    } else {
                        let baseline = candidate.world_state_checkpoint().ok_or_else(|| {
                            CodexErr::InvalidRequest(
                                "Panng world patch requires a full baseline".into(),
                            )
                        })?;
                        let mut snapshot = WorldStateSnapshot::from(&baseline.state);
                        snapshot.apply_merge_patch(&world.state);
                        snapshot
                    };
                    candidate.set_world_state_baseline(snapshot);
                }
                RolloutItem::RetainedContext(event) => {
                    let codex_history::RetainedContextEvent::VerifiedAnswer {
                        acceptance_order,
                        ..
                    } = event;
                    if acceptance_order.is_none() {
                        *acceptance_order = Some(candidate.reserve_input_order());
                    }
                    candidate.record_retained_context(event);
                }
                RolloutItem::TokenUsageRecord(record) => {
                    candidate.set_token_info(Some(TokenUsageInfo {
                        total_token_usage: record.thread_token_usage.clone(),
                        last_token_usage: record.usage.clone(),
                        model_context_window: turn.model_context_window(),
                    }));
                    latest_token_usage = Some(record.clone());
                }
                _ => {} // Codec/projector owns the canonical whitelist and cardinality checks.
            }
        }
        let live = session
            .live_thread_for_persistence("commit Panng history")
            .map_err(|error| CodexErr::Fatal(error.to_string()))?;
        let response_items = items
            .iter()
            .map(|envelope| envelope.item.clone())
            .collect::<Vec<_>>();
        let mut canonical: Vec<_> = items.into_iter().map(RolloutItem::ResponseItem).collect();
        canonical.extend(effects.iter().cloned());
        transaction
            .prepare(mutation, /*checkpoint*/ None, canonical)
            .map_err(|error| CodexErr::Fatal(error.to_string()))?;
        transaction.commit_and_publish(live, |_| async move {
            {
                let mut state = session.state.lock().await;
                // A legacy reservation can mutate retained order without bumping history_version.
                // Reject that bypass rather than replace it with a stale candidate after ACK.
                if state.history.history_version() != baseline.history_version()
                    || !std::ptr::eq(state.history.annotated_items(), baseline.annotated_items())
                    || !std::ptr::eq(state.history.retained_context(), baseline.retained_context())
                    || state.history.token_info() != baseline.token_info()
                    || state.history.reference_context_item() != baseline.reference_context_item()
                    || state.history.world_state_checkpoint() != baseline.world_state_checkpoint()
                {
                    anyhow::bail!("Panng history changed outside the transaction lease; strict reopen required");
                }
                if let Some(pending_orders) = turn.extension_data.get::<retained_context::PendingAssistantMessageOrders>() {
                    let mut orders = pending_orders.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if consumed_orders.iter().any(|(id, order)| orders.get(id) != Some(order)) {
                        anyhow::bail!("Panng pending input order changed during commit");
                    }
                    for (id, _) in &consumed_orders { orders.remove(id); }
                } else if !consumed_orders.is_empty() {
                    anyhow::bail!("Panng pending input orders disappeared during commit");
                }
                state.history = candidate;
                if let Some(record) = latest_token_usage { state.latest_token_usage_record = Some(record); }
                for effect in &effects {
                    if let RolloutItem::EventMsg(EventMsg::TurnStarted(event)) = effect {
                        state.last_started_turn_id = Some(event.turn_id.clone());
                    }
                }
                state.current_time_reminder.note_recorded_items(&response_items);
            }
            if let Some(revision) = mcp_revision {
                session.services.executed_tool_calls.mark_mcp_attribution_persisted(revision);
            }
            for image in image_preparations {
                session.services.analytics_events_client.track_image_preparation(ImagePreparationFact {
                    turn_id: turn.sub_id.clone(), metadata: image,
                });
            }
            // These are notifications of the acknowledged originals, not another canonical write.
            for item in response_items {
                let msg = EventMsg::RawResponseItem(RawResponseItemEvent { item });
                session.services.rollout_thread_trace.record_protocol_event(&msg);
                session.deliver_event_raw(Event { id: turn.sub_id.clone(), msg }).await;
            }
            for effect in effects {
                if let RolloutItem::EventMsg(msg) = effect {
                    session.deliver_panng_event(turn, msg).await;
                }
            }
            Ok(())
        }).await.map_err(|error| CodexErr::Fatal(error.to_string()))
    }
}

#[cfg(test)]
#[path = "host_history_tests.rs"]
mod tests;
