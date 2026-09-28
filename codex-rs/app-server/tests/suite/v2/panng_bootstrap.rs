use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadClosedNotification;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeInitialTurnsPageParams;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::ThreadUnsubscribeStatus;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(/*secs*/ 20);

#[tokio::test]
async fn panng_public_api_bootstraps_unloads_and_resumes_without_executing_a_turn() -> Result<()> {
    let provider = create_mock_responses_server_repeating_assistant("must not be sampled").await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&provider.uri())
        .with_root_config("thread_unload_delay_secs = 1")
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build()
        .await?;
    app.initialize().await?;
    let start_id = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::PanngV1),
            ..Default::default()
        })
        .await?;
    let started: ThreadStartResponse = timeout(TIMEOUT, app.read_response(start_id)).await??;
    assert_eq!(
        (
            started.thread.history_mode,
            started.model.as_str(),
            started.model_provider.as_str(),
            started.thread.model.as_deref(),
            started.thread.model_provider.as_str()
        ),
        (
            ThreadHistoryMode::PanngV1,
            "mock-model",
            "mock_provider",
            Some("mock-model"),
            "mock_provider"
        )
    );
    let thread_id = started.thread.id.clone();
    started
        .thread
        .path
        .as_ref()
        .context("strict creation must materialize its journal")?;

    let unsubscribe_id = app
        .send_thread_unsubscribe_request(ThreadUnsubscribeParams {
            thread_id: thread_id.clone(),
        })
        .await?;
    let unsubscribed: ThreadUnsubscribeResponse =
        timeout(TIMEOUT, app.read_response(unsubscribe_id)).await??;
    assert_eq!(unsubscribed.status, ThreadUnsubscribeStatus::Unsubscribed);
    // An unsubscribe ACK alone is a warm-resume test. Wait for the actual idle unload.
    let closed: ThreadClosedNotification =
        timeout(TIMEOUT, app.read_notification("thread/closed")).await??;
    assert_eq!(
        closed,
        ThreadClosedNotification {
            thread_id: thread_id.clone()
        }
    );

    let resume_id = app
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            ..Default::default()
        })
        .await?;
    let resumed: ThreadResumeResponse = timeout(TIMEOUT, app.read_response(resume_id)).await??;
    assert_eq!(
        (
            &resumed.thread.id,
            resumed.thread.history_mode,
            &resumed.model,
            &resumed.model_provider,
            &resumed.cwd,
            &resumed.thread.path,
            &resumed.thread.model
        ),
        (
            &thread_id,
            ThreadHistoryMode::PanngV1,
            &started.model,
            &started.model_provider,
            &started.cwd,
            &started.thread.path,
            &started.thread.model
        )
    );
    assert!(resumed.thread.turns.is_empty());

    timeout(TIMEOUT, app.shutdown_gracefully()).await??;
    let sampled = provider
        .received_requests()
        .await
        .context("mock provider records requests")?
        .into_iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path().ends_with("/responses")
        })
        .map(|request| request.url.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        sampled,
        Vec::<String>::new(),
        "rejected turns must not sample or dispatch model tool calls"
    );
    Ok(())
}

#[tokio::test]
async fn panng_public_api_completes_an_idle_text_turn() -> Result<()> {
    let provider = create_mock_responses_server_repeating_assistant("done").await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&provider.uri())
        .with_root_config("thread_unload_delay_secs = 1")
        .disable_feature(codex_features::Feature::CodeMode)
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build()
        .await?;
    app.initialize().await?;
    let started_id = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::PanngV1),
            ..Default::default()
        })
        .await?;
    let started: ThreadStartResponse = timeout(TIMEOUT, app.read_response(started_id)).await??;
    let request_id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: started.thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Say done.".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let response: codex_app_server_protocol::TurnStartResponse =
        timeout(TIMEOUT, app.read_response(request_id)).await??;
    let completed: codex_app_server_protocol::TurnCompletedNotification =
        timeout(TIMEOUT, app.read_notification("turn/completed")).await??;
    assert_eq!(completed.turn.id, response.turn.id);
    assert_eq!(
        completed.turn.status,
        codex_app_server_protocol::TurnStatus::Completed
    );
    let path = started.thread.path.as_ref().context("Panng journal path")?;
    let recovered = codex_rollout::read_panng_suffix(std::fs::File::open(path)?)?;
    let terminal_ids = codex_rollout::host_replay_items(
        &recovered
            .commits
            .into_iter()
            .map(|commit| codex_rollout::RolloutItem::PanngCommit(Box::new(commit)))
            .collect::<Vec<_>>(),
    )
    .filter_map(|item| match item {
        codex_rollout::HostReplayItem::Item(codex_rollout::RolloutItem::EventMsg(
            codex_protocol::protocol::EventMsg::TurnComplete(event),
        )) => Some(event.turn_id.clone()),
        _ => None,
    })
    .collect::<Vec<_>>();
    assert_eq!(terminal_ids, vec![response.turn.id]);
    let second_id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: started.thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Say done again.".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let second: codex_app_server_protocol::TurnStartResponse =
        timeout(TIMEOUT, app.read_response(second_id)).await??;
    let second_completed: codex_app_server_protocol::TurnCompletedNotification =
        timeout(TIMEOUT, app.read_notification("turn/completed")).await??;
    assert_eq!(second_completed.turn.id, second.turn.id);
    let read_id = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: started.thread.id.clone(),
            include_turns: true,
        })
        .await?;
    let full: ThreadReadResponse = timeout(TIMEOUT, app.read_response(read_id)).await??;
    assert_eq!(full.thread.turns.len(), 2);
    for turn in &full.thread.turns {
        assert!(
            turn.items
                .iter()
                .any(|item| matches!(item, ThreadItem::UserMessage { .. }))
        );
        assert!(
            turn.items.iter().any(
                |item| matches!(item, ThreadItem::AgentMessage { text, .. } if text == "done")
            )
        );
    }
    // Repeat cold recovery: after the first ResumeFence the model suffix no longer
    // contains historical turn boundaries, but full UI history must still retain both.
    for _ in 0..2 {
        let unsubscribe_id = app
            .send_thread_unsubscribe_request(ThreadUnsubscribeParams {
                thread_id: started.thread.id.clone(),
            })
            .await?;
        let unsubscribed: ThreadUnsubscribeResponse =
            timeout(TIMEOUT, app.read_response(unsubscribe_id)).await??;
        assert_eq!(unsubscribed.status, ThreadUnsubscribeStatus::Unsubscribed);
        let closed: ThreadClosedNotification =
            timeout(TIMEOUT, app.read_notification("thread/closed")).await??;
        assert_eq!(closed.thread_id, started.thread.id);
        let before_read = std::fs::read(path)?;
        let read_id = app
            .send_thread_read_request(ThreadReadParams {
                thread_id: started.thread.id.clone(),
                include_turns: true,
            })
            .await?;
        let cold: ThreadReadResponse = timeout(TIMEOUT, app.read_response(read_id)).await??;
        assert_eq!(cold.thread.turns, full.thread.turns);
        assert_eq!(
            std::fs::read(path)?,
            before_read,
            "cold UI read must not write a resume fence"
        );
        let resume_id = app
            .send_thread_resume_request(ThreadResumeParams {
                thread_id: started.thread.id.clone(),
                ..Default::default()
            })
            .await?;
        let resumed: ThreadResumeResponse =
            timeout(TIMEOUT, app.read_response(resume_id)).await??;
        assert_eq!(resumed.thread.turns, full.thread.turns);
    }
    let resume_id = app
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: started.thread.id.clone(),
            exclude_turns: true,
            initial_turns_page: Some(ThreadResumeInitialTurnsPageParams {
                limit: Some(1),
                sort_direction: Some(SortDirection::Asc),
                items_view: Some(TurnItemsView::Full),
            }),
            ..Default::default()
        })
        .await?;
    let paged: ThreadResumeResponse = timeout(TIMEOUT, app.read_response(resume_id)).await??;
    assert!(paged.thread.turns.is_empty());
    let page = paged.initial_turns_page.context("initial history page")?;
    assert_eq!(page.data, full.thread.turns[..1]);
    let list_id = app
        .send_thread_turns_list_request(ThreadTurnsListParams {
            thread_id: started.thread.id.clone(),
            cursor: page.next_cursor,
            limit: Some(1),
            sort_direction: Some(SortDirection::Asc),
            items_view: Some(TurnItemsView::Full),
        })
        .await?;
    let next: ThreadTurnsListResponse = timeout(TIMEOUT, app.read_response(list_id)).await??;
    assert_eq!(next.data, full.thread.turns[1..]);
    assert_eq!(next.next_cursor, None);
    timeout(TIMEOUT, app.shutdown_gracefully()).await??;
    Ok(())
}
