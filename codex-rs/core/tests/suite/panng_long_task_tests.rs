use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_eighty_real_commands_retire_windows_and_keep_old_results_readable()
-> anyhow::Result<()> {
    let harness = harness().await?;
    let progress = Arc::new(Mutex::new((0usize, None::<String>)));
    let observer = progress.clone();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let body = request_body(request);
            let mut progress = observer.lock().unwrap();
            let index = progress.0;
            if index == 1 {
                progress.1 = Some(
                    completions(&body).last().unwrap()["operation_id"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
            let events = if index < 80 {
                let command = match test_target_os() {
                    TestTargetOs::Linux | TestTargetOs::MacOs => {
                        format!("echo iteration-{index} >> executed.txt; echo iteration-{index}")
                    }
                    TestTargetOs::Windows => format!(
                        "Add-Content executed.txt iteration-{index}; Write-Output iteration-{index}"
                    ),
                };
                vec![
                    exec(&format!("long-{index}"), &command),
                    ev_completed(&format!("long-{index}")),
                ]
            } else if index == 80 {
                vec![
                    ev_function_call(
                        "read-retired",
                        "read_operation_result",
                        &json!({
                            "operation_id":progress.1.as_ref().unwrap(), "offset":0, "limit":1024,
                        })
                        .to_string(),
                    ),
                    ev_completed("old-result"),
                ]
            } else if index == 81 {
                let output = body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| {
                        item["type"] == "function_call_output" && item["call_id"] == "read-retired"
                    })
                    .unwrap();
                let page: Value = serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
                assert!(page["output"].as_str().unwrap().contains("iteration-0"));
                vec![
                    ev_assistant_message("done", "Eighty commands completed."),
                    ev_completed("done"),
                ]
            } else if index == 82 {
                vec![
                    ev_function_call(
                        "cold-read-retired",
                        "read_operation_result",
                        &json!({
                            "operation_id":progress.1.as_ref().unwrap(), "offset":0, "limit":1024,
                        })
                        .to_string(),
                    ),
                    ev_completed("cold-old-result"),
                ]
            } else if index == 83 {
                let output = body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| {
                        item["type"] == "function_call_output"
                            && item["call_id"] == "cold-read-retired"
                    })
                    .unwrap();
                let page: Value = serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
                assert!(page["output"].as_str().unwrap().contains("iteration-0"));
                let command = match test_target_os() {
                    TestTargetOs::Linux | TestTargetOs::MacOs => {
                        "echo resumed-command >> executed.txt; echo resumed-command"
                    }
                    TestTargetOs::Windows => {
                        "Add-Content executed.txt resumed-command; Write-Output resumed-command"
                    }
                };
                vec![
                    exec("resumed-command", command),
                    ev_completed("resumed-command"),
                ]
            } else {
                assert_eq!(index, 84);
                assert!(
                    completions(&body)
                        .iter()
                        .any(|notice| notice.to_string().contains("resumed-command"))
                );
                vec![
                    ev_assistant_message(
                        "cold-done",
                        "Recovered old result and executed one new command.",
                    ),
                    ev_completed("cold-done"),
                ]
            };
            progress.0 += 1;
            response(events)
        })
        .mount(harness.server())
        .await;
    let test = harness.test();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Run eighty finite commands, then read the first retained result.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    complete(test).await?;
    assert_eq!(progress.lock().unwrap().0, 82);
    let recovered = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    assert!(recovered.runtime.operations().len() < 64);
    assert!(
        recovered
            .runtime
            .operations()
            .iter()
            .all(|(_, op)| matches!(op.state(), OperationState::InContext { .. }))
    );
    // Only the current bounded window is in runtime; historical lookup did not resurrect the first.
    let first = progress.lock().unwrap().1.clone().unwrap();
    assert!(
        recovered
            .runtime
            .operations()
            .iter()
            .all(|(_, op)| op.identity().id.as_str() != first)
    );
    let expected = (0..80)
        .map(|index| format!("iteration-{index}"))
        .collect::<Vec<_>>();
    assert_eq!(
        harness
            .read_file_text("executed.txt")
            .await?
            .lines()
            .collect::<Vec<_>>(),
        expected
    );
    let id = test.session_configured.thread_id;
    let params = codex_thread_store::ListItemsParams {
        thread_id: id,
        turn_id: None,
        include_archived: true,
        position: None,
        page_size: 200,
        sort_direction: codex_thread_store::SortDirection::Asc,
        sort_key: codex_thread_store::ItemSortKey::CreatedAtOrdinal,
        after_updated_at_ordinal: None,
    };
    let live_ui = test.thread_store.list_items(params.clone()).await?;
    assert!(live_ui.next_cursor.is_none());
    test.codex.shutdown_and_wait().await?;
    let cold_ui = test.thread_store.list_items(params).await?;
    assert_eq!(cold_ui.items, live_ui.items);
    let loaded = test
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: id,
            include_archived: true,
        })
        .await?;
    let resumed = test
        .thread_manager
        .resume_thread_with_history(
            test.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: id,
                history: Arc::new(loaded.items),
                rollout_path: test.session_configured.rollout_path.clone(),
            }),
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await?;
    assert_eq!(
        test.thread_store
            .experimental_panng_read(id)
            .await?
            .runtime
            .operations(),
        recovered.runtime.operations()
    );
    resumed
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Read the first retired result again, then execute exactly one new command."
                .into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    loop {
        let event = tokio::time::timeout(
            Duration::from_secs(/*secs*/ 30),
            resumed.thread.next_event(),
        )
        .await??;
        match event.msg {
            EventMsg::TurnComplete(turn) => {
                assert!(turn.error.is_none());
                break;
            }
            EventMsg::Error(error) => {
                anyhow::bail!("cold long-task continuation failed: {}", error.message)
            }
            EventMsg::TurnAborted(_) => anyhow::bail!("cold long-task continuation aborted"),
            _ => {}
        }
    }
    assert_eq!(progress.lock().unwrap().0, 85);
    let after = test.thread_store.experimental_panng_read(id).await?;
    assert!(after.runtime.operations().len() < 64);
    assert!(
        after
            .runtime
            .operations()
            .iter()
            .all(|(_, op)| op.identity().id.as_str() != first
                && op.identity().call_id.as_str() != "cold-read-retired")
    );
    assert!(
        after
            .runtime
            .operations()
            .iter()
            .any(
                |(_, op)| op.identity().call_id.as_str() == "resumed-command"
                    && matches!(op.state(), OperationState::InContext { .. })
            )
    );
    let mut expected = expected;
    expected.push("resumed-command".into());
    assert_eq!(
        harness
            .read_file_text("executed.txt")
            .await?
            .lines()
            .collect::<Vec<_>>(),
        expected,
        "retired commands must not rerun on recovery or result retrieval"
    );
    resumed.thread.shutdown_and_wait().await?;
    Ok(())
}
