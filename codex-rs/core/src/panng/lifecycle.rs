//! Durable terminal publication at the existing task finish and abort boundaries.
use super::*;
use crate::panng::execution::DurablePool;
use crate::tasks::InterruptedTurnHistoryMarker;
use crate::tasks::interrupted_turn_history_marker;
use codex_panng_runtime::RuntimeMutation;

impl Session {
    pub(crate) async fn report_panng_task_error(&self, turn: &TurnContext, error: &CodexErr) {
        self.track_turn_codex_error(turn, error);
        let event = error.to_error_event(/*message_prefix*/ None);
        *turn.terminal_error.lock().await = Some(event.clone());
        self.deliver_event_raw(Event {
            id: turn.sub_id.clone(),
            msg: EventMsg::Error(event),
        })
        .await;
    }

    /// The existing task owner calls this after execution has exited. A failed commit
    /// publishes a transient failed terminal so clients can stop waiting. The journal
    /// remains unfinished and must be recovered; no durability is claimed.
    pub(crate) async fn finish_panng_terminal(
        self: &Arc<Self>,
        turn: &Arc<TurnContext>,
        terminal: EventMsg,
    ) {
        let result = async {
            if let Some(pool) = turn.extension_data.get::<DurablePool>() {
                pool.stop(self).await?;
            }
            self.retain_panng_input_before_completion(turn).await?;
            let items = if matches!(&terminal, EventMsg::TurnAborted(event) if event.reason == TurnAbortReason::Interrupted) {
                interrupted_turn_history_marker(InterruptedTurnHistoryMarker::from_config_and_version(
                    &turn.config, turn.multi_agent_version,
                )).map(ResponseItemEnvelope::new).into_iter().collect()
            } else { Vec::new() };
            let effects = vec![RolloutItem::EventMsg(terminal.clone())];
            self.begin_panng_history(turn, turn.model_info(), items).await?
                .commit_with_effects(
                    RuntimeMutation::delta(Vec::new()).map_err(|error| CodexErr::Fatal(error.to_string()))?,
                    effects,
                ).await?;
            Ok::<(), CodexErr>(())
        }.await;
        if let Err(error) = result {
            let error = CodexErr::Fatal(format!(
                "Panng terminal could not be durably recorded: {error}"
            ));
            self.report_panng_task_error(turn, &error).await;
            let (started_at, completed_at, duration_ms) = match terminal {
                EventMsg::TurnComplete(event) => {
                    (event.started_at, event.completed_at, event.duration_ms)
                }
                EventMsg::TurnAborted(event) => {
                    (event.started_at, event.completed_at, event.duration_ms)
                }
                _ => unreachable!("Panng finalizer accepts only terminal events"),
            };
            // Native clients use TurnComplete.error to distinguish failure from success.
            // This is a delivery-only failure notification, never a canonical ACK.
            self.deliver_event_raw(Event {
                id: turn.sub_id.clone(),
                msg: EventMsg::TurnComplete(codex_protocol::protocol::TurnCompleteEvent {
                    turn_id: turn.sub_id.clone(),
                    last_agent_message: None,
                    error: Some(error.to_error_event(/*message_prefix*/ None)),
                    started_at,
                    completed_at,
                    duration_ms,
                    time_to_first_token_ms: turn.turn_timing_state.time_to_first_token_ms().await,
                }),
            })
            .await;
        }
    }
}
