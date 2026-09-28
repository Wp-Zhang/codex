use super::*;
use crate::ItemSortKey;
use crate::ListItemsParams;
use crate::ListItemsPosition;
use crate::ListTurnsParams;
use crate::SortDirection;
use crate::StoredTurnItemsView;
use pretty_assertions::assert_eq;

fn numbered(sequence: u64, effects: Vec<RolloutItem>) -> PanngCommit {
    let mut value = serde_json::to_value(delta(effects)).unwrap();
    value["commit_id"] = json!(uuid::Uuid::new_v4().to_string());
    value["sequence"] = json!(sequence);
    value["previous_sequence"] = json!(sequence - 1);
    PanngCommit::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap()
}
fn checkpoint(first: &PanngCommit, sequence: u64, reason: &str) -> PanngCommit {
    let mut value = serde_json::to_value(first).unwrap();
    value["commit_id"] = json!(uuid::Uuid::new_v4().to_string());
    value["sequence"] = json!(sequence);
    value["previous_sequence"] = json!(sequence - 1);
    value["mutation"]["Snapshot"]["reason"] = json!(reason);
    if reason == "ResumeFence" {
        value["mutation"]["Snapshot"]["state"]["execution_epoch"] =
            json!(uuid::Uuid::new_v4().to_string());
    }
    PanngCommit::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap()
}
fn item(thread_id: ThreadId, turn: &str, name: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: turn.into(),
        item: TurnItem::UserMessage(UserMessageItem {
            id: name.into(),
            client_id: None,
            content: Vec::new(),
        }),
        started_at_ms: Some(100),
        completed_at_ms: 200,
    }))
}
fn start(turn: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(
        codex_protocol::protocol::TurnStartedEvent {
            turn_id: turn.into(),
            root_turn_id: None,
            trace_id: None,
            started_at: Some(10),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        },
    ))
}
fn item_params(thread_id: ThreadId, direction: SortDirection) -> ListItemsParams {
    ListItemsParams {
        thread_id,
        turn_id: None,
        include_archived: false,
        position: None,
        page_size: 1,
        sort_direction: direction,
        sort_key: ItemSortKey::CreatedAtOrdinal,
        after_updated_at_ordinal: None,
    }
}
async fn item_ids(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    direction: SortDirection,
) -> Vec<String> {
    let mut params = item_params(thread_id, direction);
    let mut ids = Vec::new();
    loop {
        let page = store.list_items(params.clone()).await.unwrap();
        ids.extend(page.items.into_iter().map(|item| item.item_id));
        match page.next_cursor {
            Some(cursor) => params.position = Some(ListItemsPosition::Cursor(cursor)),
            None => return ids,
        }
    }
}

#[tokio::test]
async fn real_paging_retains_ui_before_snapshot_and_rebuilds_after_database_loss() {
    let home = tempfile::tempdir().unwrap();
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let store = Arc::new(LocalThreadStore::new(config.clone(), Some(state.clone())));
    let thread_id = ThreadId::new();
    let params = create_params(thread_id, ThreadHistoryMode::PanngV1);
    let first = initial(&params);
    let (live, _) =
        LiveThread::experimental_panng_create(store.clone(), params.clone(), first.clone())
            .await
            .unwrap();
    live.experimental_panng_append(numbered(
        1,
        vec![start("turn-a"), item(thread_id, "turn-a", "a")],
    ))
    .await
    .unwrap();
    live.experimental_panng_append(checkpoint(&first, 2, "ReplayBound"))
        .await
        .unwrap();
    live.experimental_panng_append(numbered(
        3,
        vec![start("turn-b"), item(thread_id, "turn-b", "b")],
    ))
    .await
    .unwrap();
    assert_eq!(
        item_ids(&store, thread_id, SortDirection::Asc).await,
        vec!["a", "b"]
    );
    assert_eq!(
        item_ids(&store, thread_id, SortDirection::Desc).await,
        vec!["b", "a"]
    );
    for (direction, expected) in [
        (SortDirection::Asc, vec!["turn-a", "turn-b"]),
        (SortDirection::Desc, vec!["turn-b", "turn-a"]),
    ] {
        let mut params = ListTurnsParams {
            thread_id,
            include_archived: false,
            cursor: None,
            page_size: 1,
            sort_direction: direction,
            items_view: StoredTurnItemsView::Summary,
        };
        let mut turns = Vec::new();
        loop {
            let page = store.list_turns(params.clone()).await.unwrap();
            turns.extend(page.turns.into_iter().map(|turn| turn.turn_id));
            if page.next_cursor.is_none() {
                break;
            }
            params.cursor = page.next_cursor;
        }
        assert_eq!(turns, expected);
    }
    let path = live_writer::rollout_path(&store, thread_id).await.unwrap();
    live.shutdown().await.unwrap();
    let closed_bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        item_ids(&store, thread_id, SortDirection::Asc).await,
        vec!["a", "b"]
    );
    assert_eq!(
        item_ids(&store, thread_id, SortDirection::Desc).await,
        vec!["b", "a"]
    );
    assert_eq!(std::fs::read(&path).unwrap(), closed_bytes);
    assert!(store.live_recorders.lock().await.get(&thread_id).is_none());
    drop(live);
    store.thread_history_db().await.unwrap().close().await;
    state.close().await;
    drop(store);
    drop(state);
    std::fs::remove_file(config.sqlite.thread_history_db_path()).unwrap();
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let store = Arc::new(LocalThreadStore::new(config, Some(state.clone())));
    assert_eq!(
        item_ids(&store, thread_id, SortDirection::Asc).await,
        vec!["a", "b"]
    );
    let (live, _) = LiveThread::experimental_panng_resume(
        store.clone(),
        crate::ResumeThreadParams {
            thread_id,
            rollout_path: Some(path),
            history: None,
            include_archived: false,
            metadata: params.metadata,
        },
        first.branch().clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        item_ids(&store, thread_id, SortDirection::Asc).await,
        vec!["a", "b"]
    );
    live.experimental_panng_append(checkpoint(&first, 4, "ResumeFence"))
        .await
        .unwrap();
    assert_eq!(
        item_ids(&store, thread_id, SortDirection::Asc).await,
        vec!["a", "b"]
    );
    assert_eq!(
        item_ids(&store, thread_id, SortDirection::Desc).await,
        vec!["b", "a"]
    );
    live.shutdown().await.unwrap();
    store.thread_history_db().await.unwrap().close().await;
    state.close().await;
}

#[tokio::test]
async fn sqlite_failure_preserves_ack_then_read_catches_up_without_rewriting_journal() {
    let home = tempfile::tempdir().unwrap();
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let store = Arc::new(LocalThreadStore::new(config, Some(state.clone())));
    let thread_id = ThreadId::new();
    let params = create_params(thread_id, ThreadHistoryMode::PanngV1);
    let first = initial(&params);
    let (live, creation) = LiveThread::experimental_panng_create(store.clone(), params, first)
        .await
        .unwrap();
    let pool = store.thread_history_db().await.unwrap();
    sqlx::query("CREATE TRIGGER fail_panng_projection BEFORE INSERT ON thread_items BEGIN SELECT RAISE(FAIL, 'injected projection failure'); END").execute(pool).await.unwrap();
    let receipt = live
        .experimental_panng_append(numbered(
            1,
            vec![start("turn-a"), item(thread_id, "turn-a", "a")],
        ))
        .await
        .unwrap();
    let watermark = super::super::thread_history::projection_state(&store, thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (watermark.next_ordinal, watermark.next_byte_offset),
        (
            creation.end.end_ordinal_exclusive,
            creation.end.end_byte_offset
        )
    );
    assert!(
        store
            .list_items(item_params(thread_id, SortDirection::Asc))
            .await
            .is_err()
    );
    let path = live_writer::rollout_path(&store, thread_id).await.unwrap();
    let canonical = std::fs::read(&path).unwrap();
    assert_eq!(canonical.len() as u64, receipt.end.end_byte_offset);
    sqlx::query("DROP TRIGGER fail_panng_projection")
        .execute(pool)
        .await
        .unwrap();
    state.delete_thread(thread_id).await.unwrap();
    assert!(state.get_thread(thread_id).await.unwrap().is_none());
    assert_eq!(
        item_ids(&store, thread_id, SortDirection::Asc).await,
        vec!["a"]
    );
    let watermark = super::super::thread_history::projection_state(&store, thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (watermark.next_ordinal, watermark.next_byte_offset),
        (
            receipt.end.end_ordinal_exclusive,
            receipt.end.end_byte_offset
        )
    );
    assert_eq!(std::fs::read(path).unwrap(), canonical);
    live.shutdown().await.unwrap();
    pool.close().await;
    state.close().await;
}
