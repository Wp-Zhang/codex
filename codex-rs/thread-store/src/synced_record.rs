use codex_protocol::protocol::HistoryPosition;

/// Optional canonical single-record sync support, not a durable agent runtime protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SingleRecordSyncCapability {
    Unsupported,
    /// Supports paginated records on live writers. Does not imply reconciliation or deduplication.
    SingleRecordSyncV1,
}

/// Canonical cut acknowledged after file and directory synchronization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurableRecordPosition {
    pub end: HistoryPosition,
}
