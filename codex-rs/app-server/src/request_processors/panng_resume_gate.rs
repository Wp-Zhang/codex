//! Select full UI projection separately from the bounded model-recovery suffix.
use codex_protocol::protocol::ThreadHistoryMode;

pub(super) fn uses_projected_history(mode: ThreadHistoryMode) -> bool {
    matches!(
        mode,
        ThreadHistoryMode::Paginated | ThreadHistoryMode::PanngV1
    )
}
