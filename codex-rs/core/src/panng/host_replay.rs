//! Read-only reconstruction adapter. Never persist this projection or use its indexes as cuts.
use std::borrow::Cow;

use codex_history::HostReplayItem;
use codex_history::RolloutItem;
use codex_history::host_replay_items;

pub(crate) fn reconstruction_items(items: &[RolloutItem]) -> Cow<'_, [RolloutItem]> {
    if !items
        .iter()
        .any(|item| matches!(item, RolloutItem::PanngCommit(_)))
    {
        return Cow::Borrowed(items);
    }
    Cow::Owned(
        host_replay_items(items)
            .map(|item| match item {
                HostReplayItem::Item(item) => item.clone(),
                HostReplayItem::Compacted(compacted) => RolloutItem::Compacted(compacted.clone()),
            })
            .collect(),
    )
}

/// Prevent native fork filters from discarding the runtime and cutting inside an atomic commit.
pub(crate) fn reject_unsupported_fork(items: &[RolloutItem]) -> codex_protocol::error::Result<()> {
    if items.iter().any(|item| matches!(item, RolloutItem::PanngCommit(_))
        || matches!(item, RolloutItem::SessionMeta(meta) if meta.meta.history_mode == codex_protocol::protocol::ThreadHistoryMode::PanngV1)) {
        return Err(codex_protocol::error::CodexErr::InvalidRequest(
            "Panng history forks require a runtime-aware branch checkpoint and are not supported yet".into(),
        ));
    }
    Ok(())
}

pub(crate) fn collaboration_mode(
    items: &[RolloutItem],
    thread_id: codex_protocol::ThreadId,
) -> Option<&codex_protocol::config_types::CollaborationMode> {
    host_replay_items(items)
        .rev()
        .find_map(|item| match item {
            HostReplayItem::Item(RolloutItem::EventMsg(
                codex_protocol::protocol::EventMsg::ThreadSettingsApplied(event),
            )) if event.thread_id == Some(thread_id) => {
                Some(&event.thread_settings.collaboration_mode)
            }
            _ => None,
        })
        .or_else(|| {
            host_replay_items(items)
                .rev()
                .find_map(|item| match item {
                    HostReplayItem::Item(RolloutItem::TurnContext(context)) => {
                        Some(context.collaboration_mode.as_ref())
                    }
                    _ => None,
                })
                .flatten()
        })
}
