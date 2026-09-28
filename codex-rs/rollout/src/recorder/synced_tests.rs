use super::*;
use crate::ordinal::RolloutOrdinalState;
use codex_protocol::security_risk::SecurityRiskScore;
use pretty_assertions::assert_eq;

fn state(path: std::path::PathBuf) -> RolloutWriterState {
    RolloutWriterState {
        writer: None,
        deferred_creation: true,
        pending_items: Vec::new(),
        meta: None,
        cwd: path.parent().unwrap().to_path_buf(),
        rollout_path: path,
        ordinal_state: RolloutOrdinalState::Paginated { next: Some(7) },
        last_logged_error: None,
        synced: SyncState::default(),
    }
}

fn item() -> RolloutItem {
    RolloutItem::SecurityRiskScore(SecurityRiskScore {
        scores: Default::default(),
        call_id: None,
        action: None,
        sampled_at: None,
    })
}

#[cfg(unix)]
#[tokio::test]
async fn receipt_follows_file_and_ancestor_sync_and_orders_pending_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("new/nested/rollout.jsonl");
    let mut state = state(path.clone());
    state.pending_items.push(item());
    let receipt = state.append_synced(item()).await.unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let lines = String::from_utf8(bytes.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        receipt,
        SyncedRecord {
            ordinal: Some(8),
            end_byte_offset: bytes.len() as u64
        }
    );
    assert_eq!(
        lines
            .iter()
            .map(|line| line["ordinal"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![7, 8]
    );
    assert_eq!(
        state.synced.observed,
        vec![
            Stage::BeforeFileSync,
            Stage::FileSynced,
            Stage::BeforeDirectorySync,
            Stage::DirectoriesSynced
        ]
    );
    assert_eq!(state.ordinal_state.current().unwrap(), Some(9));
    assert!(state.pending_items.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn uncertain_write_and_sync_failures_fence_both_write_paths_without_retry() {
    for stage in [
        Stage::PartialWrite,
        Stage::BeforeFileSync,
        Stage::FileSynced,
        Stage::BeforeDirectorySync,
        Stage::DirectoriesSynced,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let mut state = state(path.clone());
        state.synced.fault = Some(stage);
        assert!(matches!(
            state.append_synced(item()).await,
            Err(SyncRecordError::Uncertain(_))
        ));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(state.ordinal_state.current().unwrap(), Some(7));
        assert!(matches!(
            state.append_synced(item()).await,
            Err(SyncRecordError::DefinitelyRejected(_))
        ));
        state.pending_items.push(item());
        assert!(state.flush().await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(state.synced.observed.last(), Some(&stage));
        assert_eq!(bytes.ends_with(b"\n"), stage != Stage::PartialWrite);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn receipts_use_append_end_even_after_file_cursor_is_moved() {
    use tokio::io::AsyncSeekExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    let mut state = state(path.clone());
    let first = state.append_synced(item()).await.unwrap();
    state
        .writer
        .as_mut()
        .unwrap()
        .file
        .seek(std::io::SeekFrom::Start(0))
        .await
        .unwrap();
    let second = state.append_synced(item()).await.unwrap();
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(
        second,
        SyncedRecord {
            ordinal: Some(8),
            end_byte_offset: bytes.len() as u64
        }
    );
    assert!(second.end_byte_offset > first.end_byte_offset);
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 2);
    assert_eq!(
        state.synced.observed,
        vec![
            Stage::BeforeFileSync,
            Stage::FileSynced,
            Stage::BeforeDirectorySync,
            Stage::DirectoriesSynced,
            Stage::BeforeFileSync,
            Stage::FileSynced
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn lost_receipt_does_not_reexecute_and_closed_channel_rejects() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    let state = state(path.clone());
    let (tx, rx) = tokio::sync::mpsc::channel(/*buffer*/ 4);
    let (ack, receipt) = oneshot::channel();
    drop(receipt);
    tx.send(RolloutCmd::AppendSynced {
        item: Box::new(item()),
        ack,
    })
    .await
    .unwrap();
    drop(tx);
    super::super::rollout_writer(state, rx).await.unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 1);

    let (tx, rx) = tokio::sync::mpsc::channel(/*buffer*/ 1);
    drop(rx);
    let coordinator = std::sync::Arc::new(crate::WriterLockCoordinator::new(directory.path()));
    let guard = std::sync::Arc::new(
        coordinator
            .acquire(codex_protocol::ThreadId::new())
            .unwrap(),
    );
    let recorder = RolloutRecorder {
        tx,
        writer_task: std::sync::Arc::new(super::super::RolloutWriterTask::new(Some(guard))),
        rollout_path: directory.path().join("unused.jsonl"),
    };
    let Err(SyncRecordError::DefinitelyRejected(error)) = recorder.append_synced(item()).await
    else {
        panic!("closed command channel must reject before accepting the record");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
}

#[cfg(not(unix))]
#[tokio::test]
async fn unsupported_platform_does_not_touch_storage() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    let mut state = state(path.clone());
    assert!(matches!(
        state.append_synced(item()).await,
        Err(SyncRecordError::DefinitelyRejected(_))
    ));
    assert!(!path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn excluded_item_is_rejected_without_materializing_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    let mut state = state(path.clone());
    let excluded = RolloutItem::ResponseItem(codex_history::ResponseItemEnvelope::new(
        codex_protocol::models::ResponseItem::Other,
    ));
    assert!(matches!(
        state.append_synced(excluded).await,
        Err(SyncRecordError::DefinitelyRejected(_))
    ));
    assert!(!path.exists());
    assert_eq!(state.ordinal_state.current().unwrap(), Some(7));
    assert!(state.synced.observed.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn creation_failure_fences_empty_deferred_flush_and_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let blocked_parent = directory.path().join("regular_file");
    std::fs::write(&blocked_parent, b"not a directory").unwrap();
    let mut state = state(blocked_parent.join("rollout.jsonl"));
    assert!(matches!(
        state.append_synced(item()).await,
        Err(SyncRecordError::Uncertain(_))
    ));
    assert!(state.is_deferred());
    assert!(state.pending_items.is_empty());
    assert!(state.flush().await.is_err());
    assert!(state.shutdown().await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn public_api_requires_owned_writer_and_returns_synced_position() {
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionMetaLine;
    use codex_protocol::protocol::ThreadHistoryMode;
    use codex_utils_absolute_path::test_support::PathExt;
    use std::sync::Arc;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    let thread_id = ThreadId::new();
    let metadata = RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta {
            id: thread_id,
            session_id: thread_id.into(),
            cwd: directory.path().to_path_buf(),
            history_mode: ThreadHistoryMode::Paginated,
            ..SessionMeta::default()
        },
        git: None,
    });
    let mut line = serde_json::to_vec(&RolloutLineRef {
        timestamp: "2026-09-28T00:00:00Z".to_string(),
        ordinal: Some(0),
        item: &metadata,
    })
    .unwrap();
    line.push(b'\n');
    std::fs::write(&path, &line).unwrap();
    let config = crate::config::RolloutConfig {
        codex_home: directory.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        cwd: directory.path().to_path_buf(),
        model_provider_id: "test".to_string(),
        generate_memories: false,
    };
    let params = super::super::RolloutRecorderParams::Resume { path: path.clone() };
    let unowned = RolloutRecorder::new(&config, params.clone()).await.unwrap();
    assert!(matches!(
        unowned.append_synced(item()).await,
        Err(SyncRecordError::DefinitelyRejected(_))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), line);
    unowned.shutdown().await.unwrap();

    let coordinator = Arc::new(crate::WriterLockCoordinator::new(directory.path()));
    let guard = Arc::new(coordinator.acquire(thread_id).unwrap());
    let owned = RolloutRecorder::new_with_writer_lock(&config, params, guard)
        .await
        .unwrap();
    let receipt = owned.append_synced(item()).await.unwrap();
    assert_eq!(
        receipt,
        SyncedRecord {
            ordinal: Some(1),
            end_byte_offset: std::fs::metadata(&path).unwrap().len()
        }
    );
    assert!(coordinator.acquire(thread_id).is_err());
    owned.shutdown().await.unwrap();
}
