use super::*;
use codex_core::CodexThread;
use codex_core::StartThreadOptions;
use codex_thread_store::ForkBoundary;
use codex_thread_store::ItemSortKey;
use codex_thread_store::ListItemsParams;
use codex_thread_store::ListItemsPosition;
use codex_thread_store::PrepareForkParams;
use codex_thread_store::SortDirection;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::mount_sse_once;
use pretty_assertions::assert_eq;

async fn say(
    harness: &TestCodexHarness,
    thread: &CodexThread,
    prompt: &str,
    reply: &str,
) -> anyhow::Result<ResponseMock> {
    let mock = mount_sse_once(
        harness.server(),
        sse(vec![
            ev_assistant_message(reply, reply),
            ev_completed(reply),
        ]),
    )
    .await;
    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), thread.next_event()).await??;
        match event.msg {
            EventMsg::TurnComplete(turn) => {
                assert!(turn.error.is_none(), "{turn:?}");
                break;
            }
            EventMsg::Error(error) => anyhow::bail!("{}", error.message),
            _ => {}
        }
    }
    Ok(mock)
}

async fn paged_items(
    test: &TestCodex,
    id: codex_protocol::ThreadId,
) -> anyhow::Result<Vec<codex_thread_store::StoredThreadItem>> {
    let mut items = Vec::new();
    let mut position = None;
    loop {
        let page = test
            .thread_store
            .list_items(ListItemsParams {
                thread_id: id,
                turn_id: None,
                include_archived: true,
                position,
                page_size: 1,
                sort_direction: SortDirection::Asc,
                sort_key: ItemSortKey::CreatedAtOrdinal,
                after_updated_at_ordinal: None,
            })
            .await?;
        items.extend(page.items);
        match page.next_cursor {
            Some(cursor) => position = Some(ListItemsPosition::Cursor(cursor)),
            None => return Ok(items),
        }
        assert!(
            items.len() < 20,
            "fork cursor looped across overlapping ordinal ranges"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_public_fork_freezes_parent_and_continues_after_cold_resume() -> anyhow::Result<()> {
    let harness = harness().await?;
    let test = harness.test();
    say(&harness, &test.codex, "parent before fork", "PARENT_BEFORE").await?;
    let parent_ui = paged_items(test, test.session_configured.thread_id).await?;
    let prepared = test
        .thread_store
        .prepare_fork(PrepareForkParams {
            thread_id: test.session_configured.thread_id,
            boundary: ForkBoundary::Latest,
        })
        .await?;
    let cut = prepared.history_base.unwrap();
    let child = test
        .thread_manager
        .fork_prepared_thread(StartThreadOptions::new(test.config.clone()), prepared)
        .await?;
    let child_id = child.thread_id;
    let child_path = child.thread.rollout_path().unwrap();
    let root = test.thread_store.experimental_panng_read(child_id).await?;
    assert_eq!(
        root.commits[0].mutation().snapshot_reason(),
        Some(codex_panng_runtime::SnapshotReason::Fork)
    );
    assert_eq!(root.commits[0].checkpoint().unwrap().parent_cut, Some(cut));
    say(
        &harness,
        &test.codex,
        "parent after fork",
        "PARENT_AFTER_CUT",
    )
    .await?;
    let request = say(
        &harness,
        &child.thread,
        "child continue",
        "CHILD_BEFORE_CLOSE",
    )
    .await?;
    let input = request.single_request().body_json().to_string();
    assert!(input.contains("PARENT_BEFORE"));
    assert!(!input.contains("PARENT_AFTER_CUT"));
    let child_ui = paged_items(test, child_id).await?;
    assert_eq!(&child_ui[..parent_ui.len()], parent_ui.as_slice());
    assert_eq!(child_ui.len(), parent_ui.len() + 2);
    child.thread.shutdown_and_wait().await?;
    assert_eq!(paged_items(test, child_id).await?, child_ui);
    let loaded = test
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: child_id,
            include_archived: true,
        })
        .await?;
    let resumed = test
        .thread_manager
        .resume_thread_with_history(
            test.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: child_id,
                history: Arc::new(loaded.items),
                rollout_path: Some(child_path),
            }),
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await?;
    let request = say(
        &harness,
        &resumed.thread,
        "child after resume",
        "CHILD_RESUMED",
    )
    .await?;
    let input = request.single_request().body_json().to_string();
    assert!(input.contains("PARENT_BEFORE") && input.contains("CHILD_BEFORE_CLOSE"));
    assert!(!input.contains("PARENT_AFTER_CUT"));
    assert_eq!(paged_items(test, child_id).await?.len(), child_ui.len() + 2);
    resumed.thread.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
