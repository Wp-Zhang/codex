//! Usage accounting for the durable probe path; native accounting is unchanged.
use super::*;
use codex_panng_runtime::RuntimeMutation;

impl Session {
    pub(crate) async fn record_panng_response_completed(
        &self,
        step: &StepContext,
        response_id: &str,
        usage: Option<&TokenUsage>,
        usage_metadata: Option<&ResponseUsageMetadata>,
    ) -> CodexResult<()> {
        if let Some(usage) = usage {
            let transaction = self
                .begin_panng_history(&step.turn, &step.settings.model_info, Vec::new())
                .await?;
            let previous = self.state.lock().await.latest_token_usage_record.clone();
            let mut turn_token_usage = previous
                .as_ref()
                .filter(|record| record.turn_id == step.turn.sub_id)
                .map_or_else(TokenUsage::default, |record| {
                    record.turn_token_usage.clone()
                });
            let mut thread_token_usage = previous
                .as_ref()
                .map_or_else(TokenUsage::default, |record| {
                    record.thread_token_usage.clone()
                });
            turn_token_usage.add_assign(usage);
            thread_token_usage.add_assign(usage);
            let record = TokenUsageRecord {
                thread_id: self.thread_id,
                session_id: self.session_id(),
                turn_id: step.turn.sub_id.clone(),
                root_turn_id: step
                    .turn
                    .turn_metadata_state
                    .root_turn_id()
                    .unwrap_or_else(|| step.turn.sub_id.clone()),
                response_id: response_id.to_owned(),
                usage: usage.clone(),
                turn_token_usage,
                thread_token_usage,
            };
            transaction
                .commit_with_effects(
                    RuntimeMutation::delta(Vec::new())
                        .map_err(|error| CodexErr::Fatal(error.to_string()))?,
                    vec![RolloutItem::TokenUsageRecord(record)],
                )
                .await?;
            self.record_rollout_budget_usage(usage).await?;
        }
        self.deliver_event_raw(Event {
            id: step.turn.sub_id.clone(),
            msg: EventMsg::RawResponseCompleted(RawResponseCompletedEvent {
                response_id: response_id.to_owned(),
                token_usage: usage.cloned(),
                usage_metadata: usage_metadata.cloned(),
            }),
        })
        .await;
        Ok(())
    }
}
