use super::*;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn native_writer_rejects_boxed_commit_without_modifying_its_log() {
    let (_directory, path, config, guard) = setup();
    let meta = SessionMetaLine {
        meta: SessionMeta {
            id: ThreadId::from_string(&id(9)).unwrap(),
            session_id: ThreadId::from_string(&id(9)).unwrap().into(),
            history_mode: ThreadHistoryMode::Paginated,
            cwd: config.cwd.clone(),
            ..SessionMeta::default()
        },
        git: None,
    };
    let mut bytes = serde_json::to_vec(&crate::RolloutLine {
        timestamp: "2026-09-28T00:00:00Z".into(),
        ordinal: Some(0),
        item: RolloutItem::SessionMeta(meta),
    })
    .unwrap();
    bytes.push(b'\n');
    std::fs::write(&path, &bytes).unwrap();
    let recorder = RolloutRecorder::new_with_writer_lock(
        &config,
        RolloutRecorderParams::Resume { path: path.clone() },
        guard,
    )
    .await
    .unwrap();
    assert_eq!(
        recorder.panng_durable_position().await.unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    let boxed = RolloutItem::PanngCommit(Box::new(snapshot(0, initial_runtime())));
    assert_eq!(
        recorder
            .record_canonical_items(std::slice::from_ref(&boxed))
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(matches!(
        recorder.append_synced(boxed).await,
        Err(SyncRecordError::DefinitelyRejected(_))
    ));
    recorder.flush().await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    recorder.shutdown().await.unwrap();
}

#[tokio::test]
async fn tolerant_history_loader_cannot_accept_a_recognized_panng_mode() {
    let (_directory, path, _config, _guard) = setup();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        RolloutRecorder::load_rollout_items(&path)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    // Even a missing header cannot turn atomic records into native history.
    std::fs::write(&path, line(&snapshot(0, initial_runtime()), 1)).unwrap();
    assert_eq!(
        RolloutRecorder::load_rollout_items(&path)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    std::fs::write(&path, &bytes).unwrap();
    assert!(read_panng_suffix(std::fs::File::open(&path).unwrap()).is_ok());
}
