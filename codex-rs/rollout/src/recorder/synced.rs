//! Explicit single-record durability; ordinary rollout appends remain best effort.

use std::io;
use std::path::Path;

use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

use super::RolloutCmd;
use super::RolloutLineRef;
use super::RolloutRecorder;
use super::RolloutWriterState;
use crate::RolloutItem;

/// Canonical position after one acknowledged, synced record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncedRecord {
    pub ordinal: Option<u64>,
    pub end_byte_offset: u64,
}

/// Whether a failed submission may already have changed canonical storage.
#[derive(Debug)]
pub enum SyncRecordError {
    DefinitelyRejected(io::Error),
    Uncertain(io::Error),
}

impl std::fmt::Display for SyncRecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DefinitelyRejected(error) => write!(f, "synced record rejected: {error}"),
            Self::Uncertain(error) => write!(f, "synced record outcome uncertain: {error}"),
        }
    }
}

impl std::error::Error for SyncRecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DefinitelyRejected(error) | Self::Uncertain(error) => Some(error),
        }
    }
}

#[derive(Default)]
pub(super) struct SyncState {
    fenced: bool,
    directories_synced: bool,
    #[cfg(test)]
    fault: Option<Stage>,
    #[cfg(test)]
    observed: Vec<Stage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    #[cfg(test)]
    PartialWrite,
    BeforeFileSync,
    FileSynced,
    BeforeDirectorySync,
    DirectoriesSynced,
}

impl SyncState {
    pub(super) fn ensure_writable(&self) -> io::Result<()> {
        if self.fenced {
            return Err(io::Error::other(
                "rollout writer is fenced after an uncertain synced append",
            ));
        }
        Ok(())
    }

    fn observe(&mut self, stage: Stage) -> io::Result<()> {
        #[cfg(test)]
        {
            self.observed.push(stage);
            if self.fault == Some(stage) {
                return Err(io::Error::other("injected synced append failure"));
            }
        }
        #[cfg(not(test))]
        let _ = stage;
        Ok(())
    }
}

impl RolloutRecorder {
    /// Append a canonical item and acknowledge only after file and directory sync.
    ///
    /// Requires a recorder created with `new_with_writer_lock`; its guard must protect this
    /// rollout for the recorder lifetime. Currently supported on Unix. An uncertain outcome fences this writer, including native
    /// writes, to prevent appending behind a partial record. Reconciliation is not provided here.
    /// Cancellation after enqueueing may commit the record without delivering its receipt.
    pub async fn append_synced(&self, item: RolloutItem) -> Result<SyncedRecord, SyncRecordError> {
        if self.writer_task._writer_lock.is_none() {
            return Err(SyncRecordError::DefinitelyRejected(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "synced append requires exclusive rollout writer ownership",
            )));
        }
        let (ack, result) = oneshot::channel();
        self.tx
            .send(RolloutCmd::AppendSynced {
                item: Box::new(item),
                ack,
            })
            .await
            .map_err(|error| SyncRecordError::DefinitelyRejected(io::Error::other(error)))?;
        result
            .await
            .map_err(|error| SyncRecordError::Uncertain(io::Error::other(error)))?
    }
}

impl RolloutWriterState {
    pub(super) async fn append_synced(
        &mut self,
        item: RolloutItem,
    ) -> Result<SyncedRecord, SyncRecordError> {
        if !cfg!(unix) {
            return Err(SyncRecordError::DefinitelyRejected(io::Error::new(
                io::ErrorKind::Unsupported,
                "synced rollout creation requires Unix directory sync",
            )));
        }
        self.synced
            .ensure_writable()
            .map_err(SyncRecordError::DefinitelyRejected)?;
        if self.last_logged_error.is_some() {
            return Err(SyncRecordError::DefinitelyRejected(io::Error::other(
                "native rollout writer requires recovery before synced append",
            )));
        }
        let history_mode = match self.ordinal_state {
            crate::ordinal::RolloutOrdinalState::Legacy => {
                codex_protocol::protocol::ThreadHistoryMode::Legacy
            }
            crate::ordinal::RolloutOrdinalState::Paginated { .. } => {
                codex_protocol::protocol::ThreadHistoryMode::Paginated
            }
        };
        if !crate::is_persisted_rollout_item(&item, history_mode) {
            return Err(SyncRecordError::DefinitelyRejected(io::Error::new(
                io::ErrorKind::InvalidInput,
                "item is excluded by canonical persistence policy",
            )));
        }
        // Preflight the payload before touching disk. The actual ordinal is assigned after the
        // queued prefix is written; serializing twice is bounded by the caller's item-size policy.
        let payload = serde_json::to_string(&item)
            .map_err(|error| SyncRecordError::DefinitelyRejected(io::Error::other(error)))?;
        if payload.len() > 16 * 1024 * 1024 {
            return Err(SyncRecordError::DefinitelyRejected(io::Error::new(
                io::ErrorKind::InvalidInput,
                "synced record payload exceeds 16 MiB",
            )));
        }
        drop(payload);
        let result = self.append_synced_once(item).await;
        if result.is_err() {
            // Never reopen/retry a partially written line via the native recovery loop.
            self.synced.fenced = true;
        }
        result.map_err(SyncRecordError::Uncertain)
    }

    async fn append_synced_once(&mut self, item: RolloutItem) -> io::Result<SyncedRecord> {
        // Process the previously queued prefix exactly once. A prefix error is also uncertain and
        // fences the writer; the new item is never added to the native retry buffer.
        self.write_pending_once().await?;
        let ordinal = self.ordinal_state.current()?;
        if ordinal == Some(u64::MAX) {
            return Err(io::Error::other("synced record end ordinal would overflow"));
        }
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(io::Error::other)?;
        let mut bytes = serde_json::to_vec(&RolloutLineRef {
            timestamp,
            ordinal,
            item: &item,
        })?;
        bytes.push(b'\n');
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other("missing rollout writer"))?;
        // O_APPEND governs writes even when previous readers left the shared file cursor elsewhere.
        #[cfg(test)]
        if self.synced.fault == Some(Stage::PartialWrite) {
            writer.file.write_all(&bytes[..bytes.len() / 2]).await?;
            writer.file.flush().await?;
            self.synced.observe(Stage::PartialWrite)?;
        }
        writer.file.write_all(&bytes).await?;
        writer.file.flush().await?;
        self.synced.observe(Stage::BeforeFileSync)?;
        writer.file.sync_all().await?;
        self.synced.observe(Stage::FileSynced)?;
        let end_byte_offset = writer.file.metadata().await?.len();
        if !self.synced.directories_synced {
            self.synced.observe(Stage::BeforeDirectorySync)?;
            let path = self.rollout_path.clone();
            tokio::task::spawn_blocking(move || sync_ancestors(&path))
                .await
                .map_err(io::Error::other)??;
            self.synced.observe(Stage::DirectoriesSynced)?;
            self.synced.directories_synced = true;
        }
        self.ordinal_state.advance();
        Ok(SyncedRecord {
            ordinal,
            end_byte_offset,
        })
    }
}

fn sync_ancestors(path: &Path) -> io::Result<()> {
    // Sync bottom-up through the existing root too: native deferred creation may have created
    // several parents before this command, and their creation history is not retained.
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("rollout has no parent"))?;
    for ancestor in parent
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
    {
        std::fs::File::open(ancestor)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "synced_tests.rs"]
mod tests;
