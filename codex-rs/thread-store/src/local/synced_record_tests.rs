use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::security_risk::SecurityRiskScore;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use codex_rollout::SyncRecordError;
use pretty_assertions::assert_eq;

use super::LocalThreadStore;
use super::live_writer;
use super::test_support::test_config;
use crate::CreateThreadParams;
use crate::DurableRecordPosition;
use crate::InMemoryThreadStore;
use crate::LiveThread;
use crate::ThreadMetadataPatch;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use crate::ThreadStoreError;

fn create_params(thread_id: ThreadId, history_mode: ThreadHistoryMode) -> CreateThreadParams {
    CreateThreadParams {
        creator_user_id: None,
        creator_account_id: None,
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "sync-test".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode,
        history_base: None,
        subagent_history_start_ordinal: None,
        initial_window_id: uuid::Uuid::now_v7().to_string(),
        runtime_workspace_roots: None,
        metadata: ThreadPersistenceMetadata {
            cwd: Some(std::env::current_dir().expect("cwd")),
            model_provider: "test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn score_item() -> RolloutItem {
    RolloutItem::SecurityRiskScore(SecurityRiskScore {
        scores: [("test".to_string(), 0.25)].into(),
        call_id: Some("call-a".to_string()),
        action: None,
        sampled_at: None,
    })
}

#[tokio::test]
async fn unsupported_store_rejects_synced_append_through_live_thread() {
    let store = Arc::new(InMemoryThreadStore::default());
    let live = LiveThread::create(
        store,
        create_params(ThreadId::new(), ThreadHistoryMode::Legacy),
    )
    .await
    .expect("create");
    assert!(matches!(
        live.append_synced_record(score_item()).await,
        Err(ThreadStoreError::Unsupported {
            operation: "append_synced_record"
        })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn synced_local_record_returns_reopenable_cut_despite_projection_failure() {
    let home = tempfile::tempdir().expect("home");
    let config = test_config(home.path());
    let runtime = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state");
    let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
    let thread_id = ThreadId::new();
    let live = LiveThread::create(
        store.clone(),
        create_params(thread_id, ThreadHistoryMode::Paginated),
    )
    .await
    .expect("create deferred writer");
    let path = live_writer::rollout_path(&store, thread_id)
        .await
        .expect("path");
    assert!(
        !path.exists(),
        "create stays deferred until first synced record"
    );
    let item = score_item();
    let receipt = live
        .append_synced_record(item.clone())
        .await
        .expect("sync record");
    let text = std::fs::read_to_string(&path).expect("reopen canonical file");
    let lines: Vec<_> = text
        .lines()
        .map(|line| codex_rollout::parse_rollout_line(line).expect("line"))
        .collect();
    let last = lines.last().expect("synced record");
    assert_eq!(
        serde_json::to_value(&last.item).expect("actual"),
        serde_json::to_value(&item).expect("expected")
    );
    assert_eq!(
        receipt,
        DurableRecordPosition {
            end: codex_protocol::protocol::HistoryPosition {
                thread_id,
                end_ordinal_exclusive: last.ordinal.expect("paginated ordinal") + 1,
                end_byte_offset: text.len() as u64,
            },
        }
    );

    // A staged SQLite-only patch makes metadata failure explicit instead of allowing
    // the local store's ordinary best-effort metadata fallback to mask it.
    store
        .stage_pending_thread_metadata(
            thread_id,
            ThreadMetadataPatch {
                project_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .expect("stage metadata requiring SQLite");
    // Both rebuildable projections now fail, while the independent file writer remains usable.
    store
        .thread_history_db()
        .await
        .expect("history db")
        .close()
        .await;
    runtime.close().await;
    let message = RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: "turn-1".to_string(),
        item: TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
            text: "metadata projection must not erase the canonical ACK".to_string(),
            text_elements: Vec::new(),
        }])),
        started_at_ms: Some(0),
        completed_at_ms: 1,
    }));
    let second = live
        .append_synced_record(message.clone())
        .await
        .expect("canonical ACK survives projection errors");
    assert!(
        super::thread_history_materialization::materialize_to_sqlite(&store, thread_id, &path)
            .await
            .is_err()
    );
    let bytes = std::fs::read(&path).expect("reopen after projection failure");
    let appended =
        std::str::from_utf8(&bytes[receipt.end.end_byte_offset as usize..]).expect("utf8");
    let line =
        codex_rollout::parse_rollout_line(appended.trim_end()).expect("exactly one new record");
    assert_eq!(
        serde_json::to_value(line.item).expect("actual"),
        serde_json::to_value(message).expect("expected")
    );
    assert_eq!(second.end.end_byte_offset, bytes.len() as u64);
    assert_eq!(
        second.end.end_ordinal_exclusive,
        receipt.end.end_ordinal_exclusive + 1
    );
    let metadata_error = live
        .flush()
        .await
        .expect_err("failed metadata remains pending after canonical ACK");
    assert!(
        matches!(metadata_error, ThreadStoreError::Internal { message }
        if message.contains("failed to read thread metadata"))
    );
    live.shutdown()
        .await
        .expect_err("pending metadata still fails after the canonical ACK");
}

#[cfg(unix)]
#[tokio::test]
async fn recorder_rejection_keeps_its_type_through_local_store_and_live_thread() {
    let home = tempfile::tempdir().expect("home");
    let store = Arc::new(LocalThreadStore::new(
        test_config(home.path()),
        /*state_db*/ None,
    ));
    let thread_id = ThreadId::new();
    let live = LiveThread::create(
        store.clone(),
        create_params(thread_id, ThreadHistoryMode::Paginated),
    )
    .await
    .expect("create");
    let (recorder, _, _) = live_writer::live_writer_parts(&store, thread_id)
        .await
        .expect("recorder");
    recorder
        .discard()
        .await
        .expect("close canonical writer without removing store handle");
    assert!(matches!(
        live.append_synced_record(score_item()).await,
        Err(ThreadStoreError::SyncedRecord {
            source: SyncRecordError::DefinitelyRejected(_)
        })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn accepted_writer_failure_remains_uncertain_through_live_thread() {
    let home = tempfile::tempdir().expect("home");
    let store = Arc::new(LocalThreadStore::new(
        test_config(home.path()),
        /*state_db*/ None,
    ));
    let thread_id = ThreadId::new();
    let live = LiveThread::create(
        store.clone(),
        create_params(thread_id, ThreadHistoryMode::Paginated),
    )
    .await
    .expect("create deferred writer");
    let path = live_writer::rollout_path(&store, thread_id)
        .await
        .expect("path");
    // A real filesystem failure after queue acceptance exercises conservative uncertainty
    // propagation without exporting recorder fault-injection hooks to another crate.
    std::fs::create_dir_all(&path).expect("occupy canonical file path with directory");
    assert!(matches!(
        live.append_synced_record(score_item()).await,
        Err(ThreadStoreError::SyncedRecord {
            source: SyncRecordError::Uncertain(_)
        })
    ));
    assert!(matches!(
        live.append_synced_record(score_item()).await,
        Err(ThreadStoreError::SyncedRecord {
            source: SyncRecordError::DefinitelyRejected(_)
        })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_synced_append_is_rejected_without_materializing_the_rollout() {
    let home = tempfile::tempdir().expect("home");
    let store = Arc::new(LocalThreadStore::new(
        test_config(home.path()),
        /*state_db*/ None,
    ));
    let thread_id = ThreadId::new();
    let live = LiveThread::create(
        store.clone(),
        create_params(thread_id, ThreadHistoryMode::Legacy),
    )
    .await
    .expect("create deferred legacy writer");
    let path = live_writer::rollout_path(&store, thread_id)
        .await
        .expect("path");
    assert!(!path.exists());
    assert!(matches!(
        live.append_synced_record(score_item()).await,
        Err(ThreadStoreError::Unsupported {
            operation: "append_synced_record_legacy"
        })
    ));
    assert!(
        !path.exists(),
        "unsupported append must not create canonical history"
    );
    live.shutdown().await.expect("shutdown deferred writer");
    assert!(
        !path.exists(),
        "shutdown must not publish a rejected record"
    );
}
