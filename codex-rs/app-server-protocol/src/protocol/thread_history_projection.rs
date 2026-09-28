//! Stateless projection from canonical paginated rollout records to thread-history changes.
//!
//! This module is only for the new paginated rollout format that persists canonical
//! `ItemCompleted(TurnItem)` records, not legacy event-only rollouts.

use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;

use crate::protocol::thread_history::ThreadHistoryChangeSet;
use crate::protocol::thread_history::ThreadHistoryItemChange;
use crate::protocol::thread_history::ThreadHistoryTurnMetadata;
use crate::protocol::v2::ThreadItem;
use crate::protocol::v2::TurnError;
use crate::protocol::v2::TurnStatus;

/// A commit must fit a single ordinal's pagination and lifecycle semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RolloutProjectionError {
    #[error("Panng commit contains multiple completed item events")]
    MultipleCompletedItems,
    #[error("Panng commit contains multiple turn boundary events")]
    MultipleTurnBoundaries,
    #[error("Panng item and turn boundary belong to different turns")]
    ConflictingTurns,
}

/// Project one canonical rollout line, rejecting changes that cannot share one ordinal.
///
/// Snapshots carry recovery state, not newly completed UI items. Callers must propagate errors
/// before advancing their projection checkpoint; they must not substitute an empty changeset.
pub fn project_rollout_line(
    line: &RolloutLine,
) -> Result<ThreadHistoryChangeSet, RolloutProjectionError> {
    let RolloutItem::PanngCommit(commit) = &line.item else {
        return Ok(project_native_item(&line.item));
    };
    if commit.is_checkpoint() {
        return Ok(ThreadHistoryChangeSet::default());
    }
    let mut item_turn = None;
    let mut boundary_turn = None;
    let mut seen_boundary = false;
    let mut changes = ThreadHistoryChangeSet::default();
    for effect in commit.host_effects() {
        match effect {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if item_turn.replace(event.turn_id.as_str()).is_some() =>
            {
                return Err(RolloutProjectionError::MultipleCompletedItems);
            }
            RolloutItem::EventMsg(
                event @ (EventMsg::TurnStarted(_)
                | EventMsg::TurnComplete(_)
                | EventMsg::TurnAborted(_)),
            ) => {
                if std::mem::replace(&mut seen_boundary, true) {
                    return Err(RolloutProjectionError::MultipleTurnBoundaries);
                }
                boundary_turn = match event {
                    EventMsg::TurnStarted(event) => Some(event.turn_id.as_str()),
                    EventMsg::TurnComplete(event) => Some(event.turn_id.as_str()),
                    EventMsg::TurnAborted(event) => event.turn_id.as_deref(),
                    _ => unreachable!("only turn boundary events enter this branch"),
                };
            }
            _ => {}
        }
        let native = project_native_item(effect);
        changes.changed_items.extend(native.changed_items);
        changes.changed_turns.extend(native.changed_turns);
        changes.removed_turn_ids.extend(native.removed_turn_ids);
    }
    if item_turn
        .zip(boundary_turn)
        .is_some_and(|(item, boundary)| item != boundary)
    {
        return Err(RolloutProjectionError::ConflictingTurns);
    }
    Ok(changes)
}

/// Original native mapping; validated Panng append effects cannot contain nested commits.
fn project_native_item(item: &RolloutItem) -> ThreadHistoryChangeSet {
    match item {
        RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnMetadata {
                turn_id: event.turn_id.clone(),
                root_turn_id: event.root_turn_id.clone(),
                status: TurnStatus::InProgress,
                error: None,
                started_at: event.started_at,
                completed_at: None,
                duration_ms: None,
            }],
            ..Default::default()
        },
        RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnMetadata {
                turn_id: event.turn_id.clone(),
                root_turn_id: None,
                status: if event.error.is_some() {
                    TurnStatus::Failed
                } else {
                    TurnStatus::Completed
                },
                error: event.error.as_ref().map(|error| TurnError {
                    misalignment: error.misalignment.clone().map(Into::into),
                    message: error.message.clone(),
                    codex_error_info: error.codex_error_info.clone().map(Into::into),
                    additional_details: None,
                }),
                started_at: event.started_at,
                completed_at: event.completed_at,
                duration_ms: event.duration_ms,
            }],
            ..Default::default()
        },
        RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
            let Some(turn_id) = event.turn_id.as_ref() else {
                return ThreadHistoryChangeSet::default();
            };
            ThreadHistoryChangeSet {
                changed_turns: vec![ThreadHistoryTurnMetadata {
                    turn_id: turn_id.clone(),
                    root_turn_id: None,
                    status: TurnStatus::Interrupted,
                    error: None,
                    started_at: event.started_at,
                    completed_at: event.completed_at,
                    duration_ms: event.duration_ms,
                }],
                ..Default::default()
            }
        }
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => ThreadHistoryChangeSet {
            changed_items: vec![ThreadHistoryItemChange {
                turn_id: event.turn_id.clone(),
                item: ThreadItem::from(event.item.clone()),
                started_at_ms: event.started_at_ms,
                completed_at_ms: (event.completed_at_ms != 0).then_some(event.completed_at_ms),
            }],
            ..Default::default()
        },
        RolloutItem::PanngCommit(_) => unreachable!("outer commits use the fallible dispatcher"),
        RolloutItem::SessionMeta(_)
        | RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::TokenUsageRecord(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::RealtimeItem(_)
        | RolloutItem::RetainedContext(_)
        | RolloutItem::SecurityRiskScore(_)
        | RolloutItem::EventMsg(_) => ThreadHistoryChangeSet::default(),
    }
}

#[cfg(test)]
#[path = "thread_history_projection_tests.rs"]
mod tests;
