//! Normal Codex context construction, committed with accepted Panng step input at one cut.
use super::step_context::StepContext;
use super::*;
use codex_panng_runtime::RuntimeMutation;

impl Session {
    /// Empty input refreshes the captured step without appending a duplicate user message.
    pub(crate) async fn commit_panng_step(
        &self,
        step: &StepContext,
        input: Vec<ResponseItemEnvelope>,
        effects: Vec<RolloutItem>,
    ) -> CodexResult<()> {
        let transaction = self.begin_panng_transaction().await?;
        let baseline = self.state.lock().await.clone_history();
        let mut context_candidate = baseline.clone();
        let reference = baseline.reference_context_item();
        let context = step.to_turn_context_item();
        let context_changed = reference.as_ref() != Some(&context);
        let world = self.build_world_state_for_step(step).await?;
        let (mut context_items, world_item) = if reference.is_none() {
            (
                self.build_initial_context_with_world_state(step, &world)
                    .await,
                Some(WorldStateItem::full(world.snapshot().into_object())),
            )
        } else {
            let (fragments, world_item) = context_candidate.update_world_state(&world);
            (
                crate::context_manager::updates::merge_contextual_fragments(fragments),
                world_item,
            )
        };
        if reference.is_some() && context_changed {
            context_items.extend(self.build_turn_context_contribution_items(step).await);
        }
        let write_context = context_changed || !context_items.is_empty();
        if !write_context && world_item.is_none() && input.is_empty() && effects.is_empty() {
            return transaction
                .cancel_unprepared()
                .map_err(|error| CodexErr::Fatal(error.to_string()));
        }
        // Preserve upstream ordering: contextual instructions precede this step's accepted input.
        let mut envelopes = context_items
            .into_iter()
            .map(ResponseItemEnvelope::new)
            .collect::<Vec<_>>();
        envelopes.extend(input);
        let mut combined_effects = Vec::new();
        if let Some(world_item) = world_item {
            combined_effects.push(RolloutItem::WorldState(world_item));
        }
        if write_context {
            combined_effects.push(RolloutItem::TurnContext(context));
        }
        combined_effects.extend(effects);
        self.prepare_panng_history(
            transaction,
            baseline,
            &step.turn,
            &step.settings.model_info,
            envelopes,
        )
        .await?
        .commit_with_effects(
            RuntimeMutation::delta(Vec::new())
                .map_err(|error| CodexErr::Fatal(error.to_string()))?,
            combined_effects,
        )
        .await?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
#[path = "context_tests.rs"]
mod tests;
