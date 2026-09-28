//! Historical result membership comes from validated frozen journal prefixes, never blob names.
use std::collections::HashSet;
use std::io;
use std::path::Path;

use crate::PanngProjectionReader;
use crate::PanngProjectionTarget;
use crate::PanngSessionHeader;
use codex_history::RolloutItem;
use codex_panng_runtime::OperationId;
use codex_panng_runtime::OperationRecord;
use codex_panng_runtime::OperationState;
use codex_panng_runtime::RuntimeSnapshot;
use codex_panng_runtime::SnapshotReason;

/// Find a delivered operation through the acknowledged child prefix and immutable parent cuts.
/// The caller retains writer ownership; ancestor reads do not grant execution authority.
pub async fn find_panng_operation(
    codex_home: &Path,
    path: &Path,
    target: PanngProjectionTarget,
    id: &OperationId,
) -> io::Result<Option<OperationRecord>> {
    let mut path = path.to_path_buf();
    let mut target = target;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(target.thread_id) {
            return Err(invalid("Panng result ancestry is cyclic"));
        }
        let input_path = path.clone();
        let cut = target.end;
        let recovered = tokio::task::spawn_blocking(move || {
            crate::read_panng_prefix(crate::open_rollout_seekable_reader(&input_path)?, cut)
        })
        .await
        .map_err(io::Error::other)??;
        if recovered.branch != target.branch {
            return Err(invalid("result lookup branch differs from target"));
        }
        let header = PanngSessionHeader::from_slice(&recovered.header_bytes).map_err(invalid)?;
        let parent = if let Some(cut) = header.session_meta().meta.history_base {
            let parent_path = crate::find_rollout_path_by_rollout_id(codex_home, cut.thread_id)
                .await?
                .ok_or_else(|| invalid("fork result ancestor is missing"))?;
            let input_path = parent_path.clone();
            let parent = tokio::task::spawn_blocking(move || {
                crate::read_panng_prefix(crate::open_rollout_seekable_reader(&input_path)?, cut)
            })
            .await
            .map_err(io::Error::other)??;
            Some((
                parent_path,
                PanngProjectionTarget {
                    thread_id: parent.thread_id,
                    branch: parent.branch.clone(),
                    end: cut,
                },
                parent.runtime,
            ))
        } else {
            None
        };
        let prior = parent
            .as_ref()
            .map(|(_, target, runtime)| (target.branch.clone(), runtime.clone()));
        let input_target = target.clone();
        let input_id = id.clone();
        let delivered = tokio::task::spawn_blocking(move || {
            let mut reader = PanngProjectionReader::new(
                crate::open_rollout_seekable_reader(&path)?,
                input_target.clone(),
                None,
            )?;
            let mut runtime: Option<RuntimeSnapshot> = None;
            let mut delivered: Option<OperationRecord> = None;
            while let Some(batch) = reader.read_batch()? {
                for record in batch.records {
                    let RolloutItem::PanngCommit(commit) = record.line.item else {
                        continue;
                    };
                    let previous = match &runtime {
                        Some(state) => Some((&input_target.branch, state)),
                        None if commit.mutation().snapshot_reason()
                            == Some(SnapshotReason::Fork) =>
                        {
                            prior.as_ref().map(|(branch, runtime)| (branch, runtime))
                        }
                        None => None,
                    };
                    let next = commit
                        .mutation()
                        .apply(&input_target.branch, previous)
                        .map_err(invalid)?;
                    if let Some((_, operation)) = next
                        .operations()
                        .iter()
                        .find(|(_, operation)| operation.identity().id == input_id)
                    {
                        if delivered
                            .as_ref()
                            .is_some_and(|previous| previous != operation)
                        {
                            return Err(invalid(
                                "retired operation identity was reused or its result changed",
                            ));
                        }
                        if matches!(operation.state(), OperationState::InContext { .. }) {
                            delivered = Some(operation.clone());
                        }
                    }
                    runtime = Some(next);
                }
            }
            Ok::<_, io::Error>(delivered)
        })
        .await
        .map_err(io::Error::other)??;
        if delivered.is_some() {
            return Ok(delivered);
        }
        match parent {
            Some((parent_path, parent_target, _)) => {
                path = parent_path;
                target = parent_target;
            }
            None => return Ok(None),
        }
    }
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
#[path = "archived_result_tests.rs"]
mod tests;
