//! Rebuildable UI rows, bounded by a live writer's actual canonical ACK.
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::project_rollout_line;
use codex_protocol::ThreadId;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::PanngProjectionReader;
use codex_rollout::PanngProjectionTarget;

use super::LocalThreadStore;
use super::rollout_lineage::RolloutLineage;
use super::thread_history::ProjectedRolloutLine;
use super::thread_history::RolloutProjectionStep;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn is_panng(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<bool> {
    if let Some(entry) = store.live_recorders.lock().await.get(&thread_id) {
        return Ok(entry.history_mode == codex_protocol::protocol::ThreadHistoryMode::PanngV1);
    }
    if let Some(state) = store.state_db().await
        && let Some(metadata) = state.get_thread(thread_id).await.map_err(internal)?
    {
        return Ok(metadata.history_mode == codex_protocol::protocol::ThreadHistoryMode::PanngV1);
    }
    // Missing metadata never permits a tolerant parse or unbounded native materialization.
    Ok(matches!(header(store, thread_id).await, Ok(Some(_))))
}

/// Caller holds the per-thread coordination lock. This is a receipt cache, not runtime state.
/// Projection is best effort after ACK; failures must never report the canonical append rejected.
pub(super) async fn acknowledge_locked(store: &LocalThreadStore, target: PanngProjectionTarget) {
    let thread_id = target.thread_id;
    let path = {
        let mut entries = store.live_recorders.lock().await;
        let Some(entry) = entries.get_mut(&thread_id) else {
            tracing::error!(%thread_id, "acknowledged Panng writer disappeared under coordination lock");
            return;
        };
        entry.panng_projection_target = Some(target.clone());
        entry.recorder.rollout_path().to_path_buf()
    };
    if let Err(error) = project_locked(store, &path, target).await {
        tracing::warn!(%thread_id, %error, "Panng UI projection is behind canonical ACK; read will retry");
    }
}

/// Caller retains the coordination lock through subsequent SQL paging.
pub(super) async fn lineage_locked(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
) -> ThreadStoreResult<(RolloutLineage, Option<codex_rollout::RolloutRecorder>)> {
    let live = {
        let entries = store.live_recorders.lock().await;
        entries.get(&thread_id).map(|entry| {
            (
                entry.recorder.clone(),
                entry.panng_projection_target.clone(),
            )
        })
    };
    let (recorder, target, cold) = match live {
        Some((recorder, Some(target))) => (recorder, target, false),
        Some((recorder, None)) => {
            // A resumed writer already synchronized its validated prefix, but has no append ACK.
            // Ask that writer for actual durability evidence; a ReadCut is insufficient.
            let receipt = recorder.panng_durable_position().await.map_err(internal)?;
            let (_, header) = header(store, thread_id)
                .await?
                .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
            let target = target(thread_id, header.branch().clone(), receipt)?;
            (recorder, target, false)
        }
        None => {
            let (path, header) = header(store, thread_id)
                .await?
                .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
            if !include_archived
                && super::helpers::rollout_path_is_archived(&store.config.codex_home, &path)
            {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!("thread {thread_id} is archived"),
                });
            }
            let lock = store.acquire_writer_lock(thread_id)?;
            let config = codex_rollout::RolloutConfig {
                codex_home: store.config.codex_home.clone(),
                sqlite: store.config.sqlite.clone(),
                cwd: header.session_meta().meta.cwd.clone(),
                model_provider_id: header
                    .session_meta()
                    .meta
                    .model_provider
                    .clone()
                    .unwrap_or_else(|| store.config.default_model_provider_id.clone()),
                generate_memories: false,
            };
            let (recorder, recovered) =
                codex_rollout::RolloutRecorder::resume_panng_with_writer_lock(
                    &config,
                    path,
                    lock,
                    header.branch().clone(),
                )
                .await
                .map_err(internal)?;
            let result = recorder
                .panng_durable_position()
                .await
                .map_err(internal)
                .and_then(|receipt| target(thread_id, recovered.branch, receipt));
            match result {
                Ok(target) => (recorder, target, true),
                Err(error) => {
                    let _ = recorder.shutdown().await;
                    return Err(error);
                }
            }
        }
    };
    let path = recorder.rollout_path().to_path_buf();
    if let Err(error) = project_locked(store, &path, target.clone()).await {
        if cold {
            let _ = recorder.shutdown().await;
        }
        return Err(error);
    }
    let mut lineage = store.resolve_rollout_lineage(thread_id).await?;
    let tail = lineage
        .segments
        .last_mut()
        .ok_or_else(|| internal("Panng lineage is empty"))?;
    tail.end = Some(target.end);
    for segment in lineage
        .segments()
        .iter()
        .take(lineage.segments().len().saturating_sub(1))
    {
        let cut = segment
            .end
            .ok_or_else(|| internal("Panng ancestor has no frozen cut"))?;
        let (_, header) = header(store, segment.rollout_id())
            .await?
            .ok_or_else(|| internal("Panng ancestor is missing"))?;
        project_locked(
            store,
            &segment.rollout_path,
            PanngProjectionTarget {
                thread_id: segment.rollout_id(),
                branch: header.branch().clone(),
                end: cut,
            },
        )
        .await?;
    }
    // Retain actual file/writer ownership through SQL reads, including updated rows.
    Ok((lineage, cold.then_some(recorder)))
}

/// Bounded canonical header discovery also works when the disposable metadata row is absent.
pub(super) async fn header(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<Option<(PathBuf, codex_rollout::PanngSessionHeader)>> {
    let Some(path) =
        codex_rollout::find_rollout_path_by_rollout_id(&store.config.codex_home, thread_id)
            .await
            .map_err(internal)?
    else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || {
        let path = std::fs::canonicalize(path).map_err(internal)?;
        let file = std::fs::File::open(&path).map_err(internal)?;
        let mut bytes = Vec::new();
        BufReader::new(file.take(256 * 1024 + 1))
            .read_until(b'\n', &mut bytes)
            .map_err(internal)?;
        let header = codex_rollout::PanngSessionHeader::from_slice(&bytes).map_err(internal)?;
        if header.session_meta().meta.id != thread_id {
            return Err(internal("Panng header thread mismatch"));
        }
        Ok(Some((path, header)))
    })
    .await
    .map_err(internal)?
}

fn target(
    thread_id: ThreadId,
    branch: codex_rollout::BranchId,
    receipt: codex_rollout::SyncedRecord,
) -> ThreadStoreResult<PanngProjectionTarget> {
    Ok(PanngProjectionTarget {
        thread_id,
        branch,
        end: HistoryPosition {
            thread_id,
            end_ordinal_exclusive: receipt
                .ordinal
                .and_then(|ordinal| ordinal.checked_add(1))
                .ok_or_else(|| internal("strict ACK lacks ordinal"))?,
            end_byte_offset: receipt.end_byte_offset,
        },
    })
}

async fn project_locked(
    store: &LocalThreadStore,
    path: &Path,
    target: PanngProjectionTarget,
) -> ThreadStoreResult<()> {
    if store.state_db.is_none() {
        return Ok(());
    }
    let thread_id = target.thread_id;
    let after = super::thread_history::projection_state(store, thread_id)
        .await?
        .map(|state| HistoryPosition {
            thread_id,
            end_ordinal_exclusive: state.next_ordinal,
            end_byte_offset: state.next_byte_offset,
        });
    let already_projected = after.is_some_and(|after| {
        after.end_ordinal_exclusive >= target.end.end_ordinal_exclusive
            && after.end_byte_offset >= target.end.end_byte_offset
    });
    let after = if already_projected { None } else { after };
    let path = path.to_path_buf();
    let mut reader = tokio::task::spawn_blocking(move || {
        PanngProjectionReader::new(std::fs::File::open(path)?, target, after)
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    if already_projected {
        return Ok(());
    }
    loop {
        // Parsing and file I/O remain off the async worker; retain only one decoded batch.
        let (returned_reader, batch) = tokio::task::spawn_blocking(move || {
            let batch = reader.read_batch();
            (reader, batch)
        })
        .await
        .map_err(internal)?;
        reader = returned_reader;
        let Some(batch) = batch.map_err(internal)? else {
            return Ok(());
        };
        let mut projections = Vec::with_capacity(batch.records.len());
        for record in batch.records {
            let changes = project_rollout_line(&record.line).map_err(internal)?;
            let fallback_created_at_ms = if changes
                .changed_items
                .iter()
                .any(|item| item.started_at_ms.is_none())
            {
                Some(
                    chrono::DateTime::parse_from_rfc3339(&record.line.timestamp)
                        .map_err(internal)?
                        .timestamp_millis(),
                )
            } else {
                None
            };
            projections.push(RolloutProjectionStep::Line(Box::new(
                ProjectedRolloutLine {
                    ordinal: record
                        .line
                        .ordinal
                        .ok_or_else(|| internal("strict projection lacks ordinal"))?,
                    start_byte_offset: record.start_byte_offset,
                    end_byte_offset: record.end_byte_offset,
                    fallback_created_at_ms,
                    changes,
                    realtime_item: None,
                },
            )));
        }
        super::thread_history::apply_projection(
            store,
            thread_id,
            batch.start.end_byte_offset,
            batch.end.end_byte_offset,
            /*initial_ordinal*/ 0,
            projections,
        )
        .await?;
    }
}

fn internal(error: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("strict Panng UI projection failed: {error}"),
    }
}

pub(super) async fn paging_lineage_locked(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
) -> ThreadStoreResult<(RolloutLineage, Option<codex_rollout::RolloutRecorder>)> {
    let live_panng = store
        .live_recorders
        .lock()
        .await
        .get(&thread_id)
        .is_some_and(|entry| entry.history_mode == ThreadHistoryMode::PanngV1);
    if live_panng {
        return lineage_locked(store, thread_id, include_archived).await;
    }
    let state = store
        .state_db()
        .await
        .ok_or(ThreadStoreError::Unsupported {
            operation: "list_history",
        })?;
    let metadata = state.get_thread(thread_id).await.map_err(internal)?;
    if metadata.is_none_or(|metadata| metadata.history_mode == ThreadHistoryMode::PanngV1) {
        lineage_locked(store, thread_id, include_archived).await
    } else {
        Ok((store.resolve_rollout_lineage(thread_id).await?, None))
    }
}
