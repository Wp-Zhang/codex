//! Identified appends share the recorder queue and retain one exact uncertain attempt.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use codex_history::PanngCommit;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::CommitId;
use codex_panng_runtime::RuntimeSnapshot;
use codex_panng_runtime::SnapshotReason;
use codex_protocol::ThreadId;
use tokio::sync::oneshot;

use super::*;
use crate::RecoveredPanngSuffix;
use crate::read_panng_suffix;

/// Only Durable is a synchronized acknowledgement; Absent permits resubmitting the same intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconciledAppend {
    Durable(SyncedRecord),
    Absent,
}

struct LastCommit {
    id: CommitId,
    payload: Vec<u8>,
    receipt: SyncedRecord,
}
struct Attempt {
    id: CommitId,
    payload: Vec<u8>,
    line: Vec<u8>,
    sequence: u64,
    ordinal: u64,
    candidate: RuntimeSnapshot,
    checkpoint: bool,
}

pub(super) struct StrictState {
    branch: BranchId,
    thread_id: ThreadId,
    runtime: RuntimeSnapshot,
    sequence: u64,
    ordinal: u64,
    end: u64,
    needs_resume_fence: bool,
    poisoned: bool,
    pending: Option<Attempt>,
    last: Option<LastCommit>,
    bytes_since_checkpoint: usize,
    records_since_checkpoint: usize,
    root_id: CommitId,
    seen_ids: std::collections::BTreeSet<CommitId>,
    #[cfg(test)]
    fault: Option<WriteStage>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteStage {
    BeforeWrite,
    #[cfg(test)]
    PartialWrite,
    BeforeSync,
    AfterSync,
    #[cfg(test)]
    ReconcileBeforeTruncate,
    #[cfg(test)]
    ReconcileAfterTruncate,
    #[cfg(test)]
    ReconcileBeforeSync,
    #[cfg(test)]
    ReconcileAfterSync,
}

impl RolloutRecorder {
    /// Reopen an uncompressed local marked journal. Repair follows identity and suffix validation.
    /// The returned writer accepts only a new-epoch ResumeFence before any ordinary mutation.
    pub async fn resume_panng_with_writer_lock(
        config: &impl crate::config::RolloutConfigView,
        path: PathBuf,
        writer_lock: Arc<crate::WriterLockGuard>,
        expected_branch: BranchId,
    ) -> io::Result<(Self, RecoveredPanngSuffix)> {
        if !cfg!(unix) || path.extension().is_some_and(|extension| extension == "zst") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "strict reopen requires an uncompressed Unix journal",
            ));
        }
        let home = config.codex_home().to_path_buf();
        let open_path = path.clone();
        let lock = Arc::clone(&writer_lock);
        let (file, recovered, canonical_path) = tokio::task::spawn_blocking(move || {
            let canonical_home = std::fs::canonicalize(&home)?;
            let canonical_path = std::fs::canonicalize(&open_path)?;
            if ![super::SESSIONS_SUBDIR, super::ARCHIVED_SESSIONS_SUBDIR]
                .into_iter()
                .any(|directory| canonical_path.starts_with(canonical_home.join(directory)))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "strict journal must remain inside its writer home's session directories",
                ));
            }
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .append(true)
                .open(&canonical_path)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "strict journal already has a file writer",
                    ));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error),
            }
            let recovered = read_panng_suffix(&mut file)?;
            lock.validate_owner(&home, recovered.thread_id)?;
            if recovered.branch != expected_branch {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "strict branch mismatch",
                ));
            }
            recovered
                .cut
                .ordinal
                .checked_add(1)
                .ok_or_else(|| io::Error::other("ordinal exhausted"))?;
            if file.metadata()?.len() != recovered.cut.end_byte_offset + recovered.torn_tail_bytes {
                return Err(io::Error::other("journal changed during strict reopen"));
            }
            if recovered.torn_tail_bytes != 0 {
                file.set_len(recovered.cut.end_byte_offset)?;
            }
            file.sync_all()?;
            super::synced::sync_ancestors(&canonical_path)?;
            Ok::<_, io::Error>((file, recovered, canonical_path))
        })
        .await
        .map_err(io::Error::other)??;
        let next = recovered
            .cut
            .ordinal
            .checked_add(1)
            .ok_or_else(|| io::Error::other("ordinal exhausted"))?;
        let state = RolloutWriterState {
            writer: Some(JsonlWriter {
                file: tokio::fs::File::from_std(file),
            }),
            deferred_creation: false,
            pending_items: Vec::new(),
            meta: None,
            cwd: config.cwd().to_path_buf(),
            rollout_path: canonical_path,
            ordinal_state: RolloutOrdinalState::Paginated { next: Some(next) },
            last_logged_error: None,
            synced: super::synced::SyncState::default(),
            strict: Some(StrictState::recovered(&recovered)?),
        };
        Ok((
            Self::start_writer(state, Some(writer_lock), RecorderMode::Strict),
            recovered,
        ))
    }

    pub async fn append_panng(&self, commit: PanngCommit) -> Result<SyncedRecord, SyncRecordError> {
        if self.mode != RecorderMode::Strict {
            return Err(rejected("native recorder cannot append Panng commits"));
        }
        let (ack, result) = oneshot::channel();
        self.tx
            .send(RolloutCmd::AppendPanng {
                commit: Box::new(commit),
                ack,
            })
            .await
            .map_err(|error| rejected(error.to_string()))?;
        result.await.map_err(|error| uncertain(error.to_string()))?
    }

    pub async fn reconcile_panng(
        &self,
        expected: PanngCommit,
    ) -> Result<ReconciledAppend, SyncRecordError> {
        if self.mode != RecorderMode::Strict {
            return Err(rejected("native recorder cannot reconcile Panng commits"));
        }
        let (ack, result) = oneshot::channel();
        self.tx
            .send(RolloutCmd::ReconcilePanng {
                commit: Box::new(expected),
                ack,
            })
            .await
            .map_err(|error| rejected(error.to_string()))?;
        result.await.map_err(|error| uncertain(error.to_string()))?
    }
}

impl RolloutWriterState {
    pub(super) async fn append_panng(
        &mut self,
        commit: PanngCommit,
    ) -> Result<SyncedRecord, SyncRecordError> {
        let strict = self
            .strict
            .as_mut()
            .ok_or_else(|| rejected("not a strict writer"))?;
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| rejected("strict file is closed"))?;
        strict.append(&mut writer.file, commit).await
    }
    pub(super) async fn reconcile_panng(
        &mut self,
        commit: PanngCommit,
    ) -> Result<ReconciledAppend, SyncRecordError> {
        let strict = self
            .strict
            .as_mut()
            .ok_or_else(|| rejected("not a strict writer"))?;
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| rejected("strict file is closed"))?;
        strict.reconcile(&mut writer.file, commit).await
    }
}

#[path = "strict_state.rs"]
mod state;

fn rejected(message: impl Into<String>) -> SyncRecordError {
    SyncRecordError::DefinitelyRejected(io::Error::other(message.into()))
}
fn uncertain(message: impl Into<String>) -> SyncRecordError {
    SyncRecordError::Uncertain(io::Error::other(message.into()))
}

#[cfg(test)]
#[path = "strict_tests.rs"]
mod tests;
