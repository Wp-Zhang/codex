//! Freeze a strict source at an acknowledged outer boundary and reuse the public fork carrier.
use super::*;
use std::sync::Arc;

pub(super) async fn prepare(
    store: &LocalThreadStore,
    params: crate::PrepareForkParams,
) -> ThreadStoreResult<crate::PreparedFork> {
    let thread_id = params.thread_id;
    let latest = matches!(&params.boundary, crate::ForkBoundary::Latest);
    let reservation = store.live_writer_locks.reserve_lifecycle(thread_id).await;
    let guard = store.live_writer_locks.lock(thread_id).await;
    let (lineage, cold) = super::panng_projection::lineage_locked(store, thread_id, true).await?;
    let result = async {
        let cut = super::paginated_fork::history_base_at_boundary(
            store,
            thread_id,
            params.boundary,
            &lineage,
        )
        .await?
        .ok_or_else(|| internal("Panng fork requires its root checkpoint"))?;
        let path = lineage
            .segments()
            .iter()
            .find(|segment| segment.rollout_id() == cut.thread_id)
            .ok_or_else(|| internal("fork cut is outside its source lineage"))?
            .rollout_path
            .clone();
        let recovered = tokio::task::spawn_blocking(move || {
            codex_rollout::read_panng_prefix(
                codex_rollout::open_rollout_seekable_reader(&path)?,
                cut,
            )
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;
        let header = codex_rollout::PanngSessionHeader::from_slice(&recovered.header_bytes)
            .map_err(internal)?;
        let mut items = vec![codex_rollout::RolloutItem::SessionMeta(
            header.session_meta().clone(),
        )];
        items.extend(
            recovered
                .commits
                .into_iter()
                .map(|commit| codex_rollout::RolloutItem::PanngCommit(Box::new(commit))),
        );
        if latest {
            restore_active_start(store, &lineage, &mut items).await?;
        }
        Ok(crate::PreparedFork::new(
            thread_id,
            Some(cut),
            Arc::new(items),
            reservation,
        ))
    }
    .await;
    if let Some(writer) = cold {
        writer.shutdown().await.map_err(internal)?;
    }
    drop(guard);
    result
}

fn internal(error: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("Panng fork preparation failed: {error}"),
    }
}

// A bounded recovery checkpoint can precede the current turn's original start. Recover only
// that canonical boundary so the existing core Interrupted fork logic can close the child view.
async fn restore_active_start(
    store: &LocalThreadStore,
    lineage: &super::rollout_lineage::RolloutLineage,
    items: &mut Vec<codex_rollout::RolloutItem>,
) -> ThreadStoreResult<()> {
    use codex_protocol::protocol::EventMsg;
    use codex_rollout::RolloutItem;
    let mut last = None;
    let mut visible_start = None;
    for item in items.iter() {
        if let RolloutItem::PanngCommit(commit) = item {
            if let Some(checkpoint) = commit.checkpoint() {
                last = checkpoint
                    .compacted
                    .resume_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.last_started_turn_id.clone());
            }
            for effect in commit.host_effects() {
                if let RolloutItem::EventMsg(EventMsg::TurnStarted(start)) = effect {
                    last = Some(start.turn_id.clone());
                    visible_start = last.clone();
                }
            }
        }
    }
    let Some(turn_id) = last else {
        return Ok(());
    };
    if visible_start.as_ref() == Some(&turn_id) {
        return Ok(());
    }
    let row = super::thread_history::find_visible_turn(
        store.thread_history_db().await?,
        lineage,
        &turn_id,
    )
    .await?;
    if row.status != "inProgress" {
        return Ok(());
    }
    let segment = lineage
        .segments()
        .iter()
        .find(|segment| segment.rollout_id() == row.rollout_id)
        .ok_or_else(|| internal("active fork turn has no source segment"))?;
    let after = codex_protocol::protocol::HistoryPosition {
        thread_id: row.rollout_id,
        end_ordinal_exclusive: u64::try_from(row.rollout_ordinal).map_err(internal)?,
        end_byte_offset: u64::try_from(
            row.rollout_byte_offset
                .ok_or_else(|| internal("active turn has no canonical position"))?,
        )
        .map_err(internal)?,
    };
    let (_, header) = super::panng_projection::header(store, row.rollout_id)
        .await?
        .ok_or_else(|| internal("active turn source missing"))?;
    let target = codex_rollout::PanngProjectionTarget {
        thread_id: row.rollout_id,
        branch: header.branch().clone(),
        end: segment
            .end
            .ok_or_else(|| internal("active fork source is not frozen"))?,
    };
    let path = segment.rollout_path.clone();
    let start = tokio::task::spawn_blocking(move || {
        let mut reader = codex_rollout::PanngProjectionReader::new(codex_rollout::open_rollout_seekable_reader(&path)?, target, Some(after))?;
        let batch = reader.read_batch()?.ok_or_else(|| std::io::Error::other("active fork start missing"))?;
        let record = batch.records.first().ok_or_else(|| std::io::Error::other("active fork start missing"))?;
        let RolloutItem::PanngCommit(commit) = &record.line.item else { return Err(std::io::Error::other("active fork start is not canonical")); };
        commit.host_effects().iter().find(|effect| matches!(effect, RolloutItem::EventMsg(EventMsg::TurnStarted(start)) if start.turn_id == turn_id))
            .cloned().ok_or_else(|| std::io::Error::other("active fork start does not match its projection"))
    }).await.map_err(internal)?.map_err(internal)?;
    items.insert(1, start);
    Ok(())
}
