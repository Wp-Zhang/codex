//! Experimental initialization publishes a complete two-record journal without replacing files.
use super::*;
use codex_history::PanngCommit;
use codex_history::PanngSessionHeader;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::CommitId;
use codex_panng_runtime::SnapshotReason;
use std::io;

/// Stable identity and target retained across a creation attempt; not permission to execute.
#[derive(Clone, Debug)]
pub struct PanngCreatePlan {
    path: PathBuf,
    header: PanngSessionHeader,
    initial: PanngCommit,
}
impl PanngCreatePlan {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn header(&self) -> &PanngSessionHeader {
        &self.header
    }
    pub fn initial(&self) -> &PanngCommit {
        &self.initial
    }
}

/// A failed create retains its exact target and identity; uncertain outcomes require strict reopen.
#[derive(Debug)]
pub struct PanngCreateError {
    pub path: PathBuf,
    pub branch: BranchId,
    pub commit_id: CommitId,
    pub source: SyncRecordError,
}
impl std::fmt::Display for PanngCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "strict creation at {} failed: {}",
            self.path.display(),
            self.source
        )
    }
}
impl std::error::Error for PanngCreateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl RolloutRecorder {
    /// Prepare a stable target without opening a writer or exposing a partially initialized file.
    pub async fn prepare_panng_create(
        config: &impl RolloutConfigView,
        params: RolloutRecorderParams,
        initial: PanngCommit,
    ) -> io::Result<PanngCreatePlan> {
        let mut state = Self::prepare_create_state(config, params)?;
        let meta = state
            .meta
            .take()
            .ok_or_else(|| io::Error::other("missing creation metadata"))?;
        if meta.history_mode != ThreadHistoryMode::PanngV1
            || meta.subagent_history_start_ordinal.is_some()
            || initial.sequence() != 0
            || !matches!(
                initial.mutation().snapshot_reason(),
                Some(SnapshotReason::Initial | SnapshotReason::Fork)
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "experimental creation requires a fresh Panng Initial checkpoint",
            ));
        }
        crate::panng_recovery::validate_host_ownership(&initial, meta.id)?;
        if initial.mutation().snapshot_reason() == Some(SnapshotReason::Fork) {
            let cut = meta
                .history_base
                .ok_or_else(|| io::Error::other("fork header lacks frozen parent cut"))?;
            if meta.forked_from_id.is_none()
                || cut.thread_id == meta.id
                || initial
                    .checkpoint()
                    .and_then(|checkpoint| checkpoint.parent_cut)
                    != Some(cut)
            {
                return Err(io::Error::other("fork root and header provenance differ"));
            }
            let parent = crate::find_rollout_path_by_rollout_id(config.codex_home(), cut.thread_id)
                .await?
                .ok_or_else(|| io::Error::other("fork parent journal is missing"))?;
            let parent = tokio::task::spawn_blocking(move || {
                crate::read_panng_prefix(crate::open_rollout_seekable_reader(&parent)?, cut)
            })
            .await
            .map_err(io::Error::other)??;
            initial
                .mutation()
                .apply(initial.branch(), Some((&parent.branch, &parent.runtime)))
                .map_err(io::Error::other)?;
        } else {
            if meta.forked_from_id.is_some() || meta.history_base.is_some() {
                return Err(io::Error::other(
                    "Initial checkpoint cannot claim a fork parent",
                ));
            }
            initial
                .mutation()
                .apply(initial.branch(), None)
                .map_err(io::Error::other)?;
        }
        let window = meta
            .context_window
            .as_ref()
            .map(|window| window.window_id.as_str());
        if initial
            .checkpoint()
            .and_then(|checkpoint| checkpoint.compacted.window_id.as_deref())
            != window
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "initial checkpoint and session window differ",
            ));
        }
        let git = if get_git_repo_root(&state.cwd).is_some() {
            collect_git_info(&state.cwd)
                .await
                .map(|info| ProtocolGitInfo {
                    commit_hash: info.commit_hash,
                    branch: info.branch,
                    repository_url: info.repository_url,
                })
        } else {
            None
        };
        let header =
            PanngSessionHeader::new(SessionMetaLine { meta, git }, initial.branch().clone())
                .map_err(io::Error::other)?;
        Ok(PanngCreatePlan {
            path: state.rollout_path,
            header,
            initial,
        })
    }

    /// Publish a fully synchronized Initial checkpoint and return a fresh, unfenced writer.
    /// A retry must reuse the plan; an existing target is never overwritten or treated as absent.
    pub async fn create_panng_with_writer_lock(
        config: &impl RolloutConfigView,
        plan: PanngCreatePlan,
        writer_lock: Arc<crate::WriterLockGuard>,
    ) -> Result<(Self, SyncedRecord), PanngCreateError> {
        let error = |source| PanngCreateError {
            path: plan.path.clone(),
            branch: plan.initial.branch().clone(),
            commit_id: plan.initial.commit_id().clone(),
            source,
        };
        if !cfg!(unix) {
            return Err(error(SyncRecordError::DefinitelyRejected(io::Error::new(
                io::ErrorKind::Unsupported,
                "strict creation requires Unix synchronization",
            ))));
        }
        writer_lock
            .validate_owner(config.codex_home(), plan.header.session_meta().meta.id)
            .map_err(|failure| error(SyncRecordError::DefinitelyRejected(failure)))?;
        let home = config.codex_home().to_path_buf();
        let cwd = config.cwd().to_path_buf();
        let task_plan = plan.clone();
        let lock = Arc::clone(&writer_lock);
        let result = tokio::task::spawn_blocking(move || {
            let _lock = lock;
            let mut published = false;
            let result = (|| {
                let canonical_home = fs::canonicalize(&home)?;
                let parent = task_plan.path.parent().ok_or_else(|| io::Error::other("creation target has no parent"))?;
                // Resolve the first existing ancestor before creating directories through a symlink.
                let existing = parent.ancestors().find(|path| path.exists()).ok_or_else(|| io::Error::other("no existing target ancestor"))?;
                if !fs::canonicalize(existing)?.starts_with(&canonical_home) {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "creation target escapes writer home"));
                }
                fs::create_dir_all(parent)?;
                let canonical_parent = fs::canonicalize(parent)?;
                if !canonical_parent.starts_with(canonical_home.join(SESSIONS_SUBDIR)) {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "strict creation requires the session directory"));
                }
                let target = canonical_parent.join(task_plan.path.file_name().ok_or_else(|| io::Error::other("missing target filename"))?);
                let temporary = tempfile::NamedTempFile::new_in(&canonical_parent)?;
                let (unlocked, temporary_path) = temporary.into_parts();
                drop(unlocked);
                let mut file = fs::OpenOptions::new().read(true).append(true).open(&temporary_path)?;
                match file.try_lock() {
                    Ok(()) => {}
                    Err(std::fs::TryLockError::WouldBlock) => return Err(io::Error::other("initial file unexpectedly locked")),
                    Err(std::fs::TryLockError::Error(error)) => return Err(error),
                }
                let mut initial_line = serde_json::to_vec(&serde_json::json!({
                    "type":"panng_commit", "timestamp":task_plan.header.session_meta().meta.timestamp,
                    "ordinal":1, "payload":task_plan.initial,
                }))?;
                initial_line.push(b'\n');
                std::io::Write::write_all(&mut file, task_plan.header.as_bytes())?;
                std::io::Write::write_all(&mut file, &initial_line)?;
                std::io::Write::flush(&mut file)?;
                file.sync_all()?;
                let recovered = crate::read_panng_suffix(&mut file)?;
                let strict = super::strict::StrictState::fresh(&recovered, &task_plan.initial)?;
                match temporary_path.persist_noclobber(&target) {
                    Ok(()) => published = true,
                    Err(error) => {
                        // Publication may have linked the destination before reporting an error.
                        // AlreadyExists is the one definitive no-clobber rejection.
                        published = error.error.kind() != io::ErrorKind::AlreadyExists;
                        return Err(error.error);
                    }
                }
                super::synced::sync_ancestors(&target)?;
                Ok::<_, io::Error>((file, strict, target, recovered.cut.end_byte_offset))
            })();
            result.map_err(|failure| if published { SyncRecordError::Uncertain(failure) } else { SyncRecordError::DefinitelyRejected(failure) })
        }).await.map_err(|failure| error(SyncRecordError::Uncertain(io::Error::other(failure))))?
            .map_err(error)?;
        let (file, strict, path, end_byte_offset) = result;
        let state = RolloutWriterState {
            writer: Some(JsonlWriter {
                file: tokio::fs::File::from_std(file),
            }),
            deferred_creation: false,
            pending_items: Vec::new(),
            meta: None,
            cwd,
            rollout_path: path,
            ordinal_state: RolloutOrdinalState::Paginated { next: Some(2) },
            last_logged_error: None,
            synced: super::synced::SyncState::default(),
            strict: Some(strict),
        };
        Ok((
            Self::start_writer(state, Some(writer_lock), RecorderMode::Strict),
            SyncedRecord {
                ordinal: Some(1),
                end_byte_offset,
            },
        ))
    }
}

#[cfg(test)]
#[path = "strict_create_tests.rs"]
mod tests;
