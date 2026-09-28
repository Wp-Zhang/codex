//! Experimental local journal bridge; runtime and sequence remain owned by RolloutRecorder.
use super::*;
use crate::DurableRecordPosition;
use crate::ExperimentalPanngReconcile;
use codex_app_server_protocol::project_rollout_line;
use codex_rollout::PanngCommit;
use codex_rollout::ReconciledAppend;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use codex_rollout::SyncedRecord;

pub(super) async fn create(
    store: &LocalThreadStore,
    params: CreateThreadParams,
    initial: PanngCommit,
) -> ThreadStoreResult<DurableRecordPosition> {
    if !cfg!(unix) {
        return Err(ThreadStoreError::Unsupported {
            operation: "experimental_panng_create",
        });
    }
    let thread_id = params.thread_id;
    let branch = initial.branch().clone();
    if params.history_mode != ThreadHistoryMode::PanngV1 {
        return Err(ThreadStoreError::InvalidRequest {
            message: "Panng creation requires explicit panng_v1 mode".into(),
        });
    }
    validate_projection(&initial)?;
    let _guard = store.live_writer_locks.lock(thread_id).await;
    store.ensure_live_recorder_absent(thread_id).await?;
    let writer_lock = store.acquire_writer_lock(thread_id)?;
    if codex_rollout::find_rollout_path_by_rollout_id(&store.config.codex_home, thread_id)
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(ThreadStoreError::Conflict {
            message: "thread already has canonical history; strict reopen is required".into(),
        });
    }
    let (config, recorder_params) = super::create_thread::prepare(store, params)?;
    let plan = RolloutRecorder::prepare_panng_create(&config, recorder_params, initial)
        .await
        .map_err(internal)?;
    let (recorder, receipt) =
        RolloutRecorder::create_panng_with_writer_lock(&config, plan, writer_lock.clone())
            .await
            .map_err(|source| ThreadStoreError::PanngCreate { source })?;
    store
        .insert_live_recorder(
            thread_id,
            recorder,
            thread_id,
            ThreadHistoryMode::PanngV1,
            writer_lock,
        )
        .await?;
    let receipt = position(thread_id, receipt)?;
    super::panng_projection::acknowledge_locked(
        store,
        codex_rollout::PanngProjectionTarget {
            thread_id,
            branch,
            end: receipt.end,
        },
    )
    .await;
    Ok(receipt)
}

pub(super) async fn resume(
    store: &LocalThreadStore,
    params: crate::ResumeThreadParams,
    branch: codex_rollout::BranchId,
) -> ThreadStoreResult<codex_rollout::RecoveredPanngSuffix> {
    let thread_id = params.thread_id;
    let _guard = store.live_writer_locks.lock(thread_id).await;
    store.ensure_live_recorder_absent(thread_id).await?;
    let writer_lock = store.acquire_writer_lock(thread_id)?;
    let path = match params.rollout_path {
        Some(path) => path,
        None => codex_rollout::find_rollout_path_by_rollout_id(&store.config.codex_home, thread_id)
            .await
            .map_err(internal)?
            .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?,
    };
    // Apply archive policy to the same physical path the strict constructor will open.
    // A sessions/ alias must not conceal an archived journal before tail repair.
    let path = tokio::fs::canonicalize(path).await.map_err(internal)?;
    if !params.include_archived
        && super::helpers::rollout_path_is_archived(&store.config.codex_home, &path)
    {
        return Err(ThreadStoreError::InvalidRequest {
            message: "Panng thread is archived".into(),
        });
    }
    let config = codex_rollout::RolloutConfig {
        codex_home: store.config.codex_home.clone(),
        sqlite: store.config.sqlite.clone(),
        cwd: params
            .metadata
            .cwd
            .ok_or_else(|| internal("strict resume requires cwd"))?,
        model_provider_id: params.metadata.model_provider,
        generate_memories: matches!(
            params.metadata.memory_mode,
            codex_protocol::protocol::ThreadMemoryMode::Enabled
        ),
    };
    let (recorder, recovered) =
        RolloutRecorder::resume_panng_with_writer_lock(&config, path, writer_lock.clone(), branch)
            .await
            .map_err(internal)?;
    for commit in &recovered.commits {
        validate_projection(commit)?;
    }
    store
        .insert_live_recorder(
            thread_id,
            recorder,
            thread_id,
            ThreadHistoryMode::PanngV1,
            writer_lock,
        )
        .await?;
    Ok(recovered)
}

pub(super) async fn append(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    commit: PanngCommit,
) -> ThreadStoreResult<DurableRecordPosition> {
    validate_projection(&commit)?;
    let _guard = store.live_writer_locks.lock(thread_id).await;
    let (recorder, rollout_id, mode) = live_writer::live_writer_parts(store, thread_id).await?;
    if mode != ThreadHistoryMode::PanngV1 {
        return Err(ThreadStoreError::Unsupported {
            operation: "experimental_panng_append_native",
        });
    }
    let branch = commit.branch().clone();
    let receipt = recorder
        .append_panng(commit)
        .await
        .map_err(|source| ThreadStoreError::SyncedRecord { source })?;
    let receipt = position(rollout_id, receipt)?;
    super::panng_projection::acknowledge_locked(
        store,
        codex_rollout::PanngProjectionTarget {
            thread_id,
            branch,
            end: receipt.end,
        },
    )
    .await;
    Ok(receipt)
}

pub(super) async fn reconcile(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    commit: PanngCommit,
) -> ThreadStoreResult<ExperimentalPanngReconcile> {
    validate_projection(&commit)?;
    let _guard = store.live_writer_locks.lock(thread_id).await;
    let (recorder, rollout_id, mode) = live_writer::live_writer_parts(store, thread_id).await?;
    if mode != ThreadHistoryMode::PanngV1 {
        return Err(ThreadStoreError::Unsupported {
            operation: "experimental_panng_reconcile_native",
        });
    }
    let branch = commit.branch().clone();
    match recorder
        .reconcile_panng(commit)
        .await
        .map_err(|source| ThreadStoreError::SyncedRecord { source })?
    {
        ReconciledAppend::Durable(receipt) => {
            let receipt = position(rollout_id, receipt)?;
            super::panng_projection::acknowledge_locked(
                store,
                codex_rollout::PanngProjectionTarget {
                    thread_id,
                    branch,
                    end: receipt.end,
                },
            )
            .await;
            Ok(ExperimentalPanngReconcile::Durable(receipt))
        }
        ReconciledAppend::Absent => Ok(ExperimentalPanngReconcile::Absent),
    }
}

pub(super) async fn read(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<codex_rollout::RecoveredPanngSuffix> {
    let _guard = store.live_writer_locks.lock(thread_id).await;
    let (recorder, _, mode) = live_writer::live_writer_parts(store, thread_id).await?;
    if mode != ThreadHistoryMode::PanngV1 {
        return Err(ThreadStoreError::Unsupported {
            operation: "experimental_panng_read_native",
        });
    }
    // The barrier refuses pending uncertainty; a readable complete line is not itself an ACK.
    recorder.flush().await.map_err(internal)?;
    let path = recorder.rollout_path().to_path_buf();
    let recovered = tokio::task::spawn_blocking(move || {
        codex_rollout::read_panng_suffix(std::fs::File::open(path)?)
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    if recovered.thread_id != thread_id {
        return Err(internal("strict journal thread mismatch"));
    }
    Ok(recovered)
}

fn validate_projection(commit: &PanngCommit) -> ThreadStoreResult<()> {
    project_rollout_line(&RolloutLine {
        timestamp: "1970-01-01T00:00:00Z".into(),
        ordinal: None,
        item: RolloutItem::PanngCommit(Box::new(commit.clone())),
    })
    .map_err(|error| ThreadStoreError::InvalidRequest {
        message: format!("invalid Panng UI projection before durable commit: {error}"),
    })?;
    Ok(())
}
fn position(
    rollout_id: ThreadId,
    receipt: SyncedRecord,
) -> ThreadStoreResult<DurableRecordPosition> {
    let exclusive = receipt
        .ordinal
        .and_then(|ordinal| ordinal.checked_add(1))
        .ok_or_else(|| ThreadStoreError::SyncedRecord {
            source: codex_rollout::SyncRecordError::Uncertain(std::io::Error::other(
                "strict ACK lacks a valid exclusive ordinal",
            )),
        })?;
    Ok(DurableRecordPosition {
        end: codex_protocol::protocol::HistoryPosition {
            thread_id: rollout_id,
            end_ordinal_exclusive: exclusive,
            end_byte_offset: receipt.end_byte_offset,
        },
    })
}
fn internal(error: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: error.to_string(),
    }
}
