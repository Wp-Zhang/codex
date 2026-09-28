//! Borrowed host projection; canonical outer records remain the only persistence and cut units.

use crate::CompactedItem;
use crate::RolloutItem;

#[derive(Clone, Copy, Debug)]
pub enum HostReplayItem<'a> {
    Item(&'a RolloutItem),
    Compacted(&'a CompactedItem),
}

/// Expands one atomic checkpoint into its compaction and companions without copying its context.
/// Replacement history is restored by the consumer, never replayed as fresh message events.
/// The reverse view visits companions before their checkpoint, preserving canonical ordering.
pub fn host_replay_items(
    items: &[RolloutItem],
) -> impl DoubleEndedIterator<Item = HostReplayItem<'_>> + Clone {
    items.iter().flat_map(|item| {
        let (compacted, items): (Option<&CompactedItem>, &[RolloutItem]) = match item {
            RolloutItem::PanngCommit(commit) => match commit.checkpoint() {
                Some(checkpoint) => (Some(&checkpoint.compacted), &checkpoint.companions),
                None => (None, commit.host_effects()),
            },
            RolloutItem::Compacted(compacted) => (Some(compacted), &[]),
            item => (None, std::slice::from_ref(item)),
        };
        compacted
            .into_iter()
            .map(HostReplayItem::Compacted)
            .chain(items.iter().map(HostReplayItem::Item))
    })
}

#[cfg(test)]
#[path = "host_replay_tests.rs"]
mod tests;
