//! Accepted inputs retain their order, each with its own durable UI ordinal.
use super::*;
use crate::hook_runtime::inspect_pending_input_panng;
use crate::hook_runtime::panng_hook_context_items;

#[derive(Default)]
pub(crate) struct PendingInputs(std::sync::Mutex<std::collections::VecDeque<TurnInput>>);

impl PendingInputs {
    pub(crate) fn retain(turn: &TurnContext, inputs: Vec<TurnInput>) {
        let pending = turn.extension_data.get_or_init(Self::default);
        pending
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(inputs);
    }
}

enum InputProcessing {
    InspectHooks,
    Retain,
}

impl Session {
    pub(crate) async fn commit_panng_inputs(
        self: &Arc<Self>,
        step: &StepContext,
        inputs: Vec<TurnInput>,
        effects: Vec<RolloutItem>,
    ) -> CodexResult<bool> {
        PendingInputs::retain(&step.turn, inputs);
        crate::hook_runtime::drain_async_hook_results_checked(
            self, &step.turn, /*before_user_prompt*/ true,
        )
        .await?;
        self.commit_panng_step(step, Vec::new(), effects).await?;
        self.commit_retained_panng_inputs(&step.turn, InputProcessing::InspectHooks)
            .await
    }

    pub(crate) async fn retain_panng_input_before_completion(
        self: &Arc<Self>,
        turn: &Arc<TurnContext>,
    ) -> CodexResult<()> {
        self.commit_retained_panng_inputs(turn, InputProcessing::Retain)
            .await
            .map(|_| ())
    }

    async fn commit_retained_panng_inputs(
        self: &Arc<Self>,
        turn: &Arc<TurnContext>,
        processing: InputProcessing,
    ) -> CodexResult<bool> {
        let pending = turn.extension_data.get_or_init(PendingInputs::default);
        let mut blocked = false;
        let mut accepted_user_input = false;
        loop {
            let input = pending
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .front()
                .cloned();
            let Some(input) = input else {
                break;
            };
            let mut additional = self.state.lock().await.additional_context.clone();
            let mut items = Vec::new();
            let mut effects = Vec::new();
            let hook = match processing {
                InputProcessing::InspectHooks => {
                    inspect_pending_input_panng(self, turn, &input).await?
                }
                InputProcessing::Retain => crate::hook_runtime::HookRuntimeOutcome {
                    should_stop: false,
                    additional_contexts: Vec::new(),
                },
            };
            if hook.should_stop {
                blocked = true;
            } else {
                match input {
                    TurnInput::UserInput {
                        content,
                        client_id,
                        metadata,
                    } => {
                        accepted_user_input = true;
                        let mut visible = UserMessageItem::new(&content);
                        visible.client_id = client_id;
                        let now = now_unix_timestamp_ms();
                        effects.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(
                            ItemCompletedEvent {
                                thread_id: self.thread_id,
                                turn_id: turn.sub_id.clone(),
                                item: TurnItem::UserMessage(visible),
                                started_at_ms: Some(now),
                                completed_at_ms: now,
                            },
                        )));
                        items.push(ResponseItemEnvelope {
                            item: self.response_item_from_user_input(content),
                            metadata: metadata.acceptance_order.map(|order| CodexHarnessMetadata {
                                user_input_order: Some(order),
                                ..Default::default()
                            }),
                        });
                    }
                    TurnInput::AdditionalContext(values) => {
                        for item in additional.merge(values) {
                            if serde_json::to_vec(&item)
                                .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?
                                .len()
                                > 8192
                            {
                                return Err(CodexErr::InvalidRequest(
                                    "additional context fragment exceeds 8192 encoded bytes".into(),
                                ));
                            }
                            items.push(self.annotate_client_response_item(item));
                        }
                    }
                    TurnInput::ResponseItem(item) | TurnInput::FunctionCallOutput(item) => {
                        items.push(item)
                    }
                    TurnInput::InterAgentCommunication(_) => {
                        return Err(CodexErr::InvalidRequest(
                            "Panng agent mail is not connected".into(),
                        ));
                    }
                }
            }
            items.extend(panng_hook_context_items(hook.additional_contexts)?);
            self.begin_panng_history(turn, turn.model_info(), items)
                .await?
                .commit_with_effects(
                    codex_panng_runtime::RuntimeMutation::delta(Vec::new())
                        .map_err(|error| CodexErr::Fatal(error.to_string()))?,
                    effects,
                )
                .await?;
            // commit_and_publish has restored Ready. No cancellation point may separate
            // that acknowledgement from relinquishing this accepted input.
            pending
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front();
            // Advance this cache only after the corresponding input has its own ACK.
            self.state.lock().await.additional_context = additional;
        }
        Ok(blocked && !accepted_user_input)
    }
}
