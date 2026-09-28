//! Panng notifications and bounded native tool UI snapshots. Journal ownership stays in the pool.
use super::*;
use codex_protocol::protocol::FileChange;

const UI_BYTES: usize = 32 * 1024;
const OMITTED: &str =
    "\n[Panng UI output omitted; use read_operation_result for retained tool output]";

/// Notifications with another canonical source, or transient progress with no replay meaning.
/// Canonical boundaries/settings/items intentionally do not fall through this classification.
pub(super) fn is_notification(event: &EventMsg) -> bool {
    matches!(
        event,
        EventMsg::SessionConfigured(_)
            | EventMsg::EnvironmentConnected(_)
            | EventMsg::EnvironmentDisconnected(_)
            | EventMsg::ItemStarted(_)
            | EventMsg::ExecCommandBegin(_)
            | EventMsg::ExecCommandEnd(_)
            | EventMsg::ExecCommandOutputDelta(_)
            | EventMsg::PatchApplyBegin(_)
            | EventMsg::PatchApplyEnd(_)
            | EventMsg::PatchApplyUpdated(_)
            | EventMsg::TurnDiff(_)
            | EventMsg::TokenCount(_)
            | EventMsg::RawResponseItem(_)
            | EventMsg::RawResponseCompleted(_)
            | EventMsg::UserMessage(_)
            | EventMsg::AgentMessage(_)
            | EventMsg::AgentReasoning(_)
            | EventMsg::AgentReasoningRawContent(_)
            | EventMsg::AgentMessageContentDelta(_)
            | EventMsg::ReasoningContentDelta(_)
            | EventMsg::ReasoningRawContentDelta(_)
            | EventMsg::AgentReasoningSectionBreak(_)
            | EventMsg::PlanDelta(_)
            | EventMsg::ExecApprovalRequest(_)
            | EventMsg::ApplyPatchApprovalRequest(_)
            | EventMsg::GuardianAssessment(_)
            | EventMsg::Error(_)
            | EventMsg::Warning(_)
            | EventMsg::StreamError(_)
            | EventMsg::DeprecationNotice(_)
            | EventMsg::AuthRecoveryStarted(_)
            | EventMsg::AuthRecoveryCompleted(_)
            | EventMsg::GuardianWarning(_)
            | EventMsg::SafetyBuffering(_)
            | EventMsg::ModelReroute(_)
            | EventMsg::ModelVerification(_)
            | EventMsg::TurnModerationMetadata(_)
            | EventMsg::McpStartupUpdate(_)
            | EventMsg::McpStartupComplete(_)
            | EventMsg::ThreadQueueChanged(_)
            | EventMsg::RequestUserInput(_)
            | EventMsg::HookStarted(_)
            | EventMsg::HookCompleted(_)
            | EventMsg::ShutdownComplete
    )
}

fn truncate_ui(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut end = limit.saturating_sub(OMITTED.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(OMITTED);
}

/// Bound the serialized *whole* UI event, including JSON escaping and repeated output fields.
/// This is a display snapshot only; execution result bytes are independently retained in blobs.
pub(crate) fn bound_tool_completed(source: ItemCompletedEvent) -> CodexResult<ItemCompletedEvent> {
    let mut limit = 4096;
    loop {
        let mut event = source.clone();
        match &mut event.item {
            TurnItem::CommandExecution(item) => {
                for value in [
                    &mut item.stdout,
                    &mut item.stderr,
                    &mut item.aggregated_output,
                    &mut item.formatted_output,
                ]
                .into_iter()
                .flatten()
                {
                    truncate_ui(value, limit);
                }
            }
            TurnItem::FileChange(item) => {
                for value in [&mut item.stdout, &mut item.stderr].into_iter().flatten() {
                    truncate_ui(value, limit);
                }
                for change in item.changes.values_mut() {
                    let text = match change {
                        FileChange::Add { content } | FileChange::Delete { content } => content,
                        FileChange::Update { unified_diff, .. } => unified_diff,
                    };
                    truncate_ui(text, limit);
                }
            }
            _ => {
                return Err(CodexErr::Fatal(
                    "Panng tool capture requires a native command or file-change item".into(),
                ));
            }
        }
        if serde_json::to_vec(&event)
            .map_err(|error| CodexErr::Fatal(error.to_string()))?
            .len()
            <= UI_BYTES
        {
            return Ok(event);
        }
        if limit <= 128 {
            return Err(CodexErr::Fatal(
                "Panng tool UI metadata exceeds 32 KiB".into(),
            ));
        }
        limit /= 2;
    }
}

impl Session {
    /// Caller already owns a durability ACK (or explicitly classified transient notification).
    /// Never calls native persistence or re-enters the commit coordinator.
    pub(crate) async fn deliver_panng_event(&self, turn: &TurnContext, msg: EventMsg) {
        if let EventMsg::ItemCompleted(event) = &msg
            && matches!(
                event.item,
                TurnItem::CommandExecution(_) | TurnItem::FileChange(_)
            )
        {
            record_turn_ttfm_metric(turn, &event.item).await;
            for contributor in self.services.extensions.turn_lifecycle_contributors() {
                contributor
                    .on_item_completed(
                        &self.services.thread_extension_data,
                        turn.extension_data.as_ref(),
                        &event.item,
                    )
                    .await;
            }
        }
        self.services.mcp_runtime.observe_event(&msg);
        self.services
            .rollout_thread_trace
            .record_protocol_event(&msg);
        let legacy = msg.as_legacy_events(self.show_raw_agent_reasoning());
        self.deliver_event_raw(Event {
            id: turn.sub_id.clone(),
            msg,
        })
        .await;
        for msg in legacy {
            self.services
                .rollout_thread_trace
                .record_protocol_event(&msg);
            self.deliver_event_raw(Event {
                id: turn.sub_id.clone(),
                msg,
            })
            .await;
        }
    }
}
