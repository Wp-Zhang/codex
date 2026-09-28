use super::*;
use pretty_assertions::assert_eq;

async fn verify_cold_context(test: &TestCodex) -> anyhow::Result<()> {
    let id = test.session_configured.thread_id;
    let before = test.codex.conversation_history_snapshot().await;
    let items = before.items().cloned().collect::<Vec<_>>();
    let retained = before.retained_context().cloned();
    let runtime = test.thread_store.experimental_panng_read(id).await?.runtime;
    test.codex.shutdown_and_wait().await?;
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
            None,
            ClientMcpExtensions::default(),
        )
        .await?;
    let restored = resumed.thread.conversation_history_snapshot().await;
    assert_eq!(restored.items().cloned().collect::<Vec<_>>(), items);
    assert_eq!(restored.retained_context(), retained.as_ref());
    assert_eq!(
        test.thread_store
            .experimental_panng_read(id)
            .await?
            .runtime
            .operations(),
        runtime.operations()
    );
    resumed.thread.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_plan_question_uses_native_answer_and_retains_verified_context_on_resume()
-> anyhow::Result<()> {
    let harness = TestCodexHarness::with_auto_env_builder(builder().with_config(|config| {
        config.experimental_request_user_input_enabled = true;
        config.features.enable(Feature::GuardianApproval).unwrap();
    }))
    .await?;
    let requests = core_test_support::responses::mount_sse_sequence(harness.server(), vec![
        sse(vec![ev_function_call("question", "request_user_input", &json!({"questions":[{
            "id":"route", "header":"Route", "question":"Which route should the plan use?",
            "options":[{"label":"Safe","description":"Use the conservative route."},{"label":"Fast","description":"Use the faster route."}]
        }]}).to_string()), ev_completed("question")]),
        sse(vec![ev_assistant_message("plan", "<proposed_plan>Use the Safe route.</proposed_plan>"), ev_completed("plan")]),
    ]).await;
    let test = harness.test();
    let mut mode = test
        .codex
        .thread_settings_snapshot()
        .await
        .collaboration_mode;
    mode.mode = codex_protocol::config_types::ModeKind::Plan;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Ask me which route to take before producing the plan.".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    collaboration_mode: Some(mode),
                    ..Default::default()
                },
            ),
        )
        .await?;
    loop {
        let event =
            tokio::time::timeout(Duration::from_secs(20), test.codex.next_event()).await??;
        match event.msg {
            EventMsg::RequestUserInput(question) => {
                assert_eq!(question.call_id, "question");
                let durable = test
                    .thread_store
                    .experimental_panng_read(test.session_configured.thread_id)
                    .await
                    .expect("question Prepared prefix must be readable before answer");
                assert!(
                    durable.runtime.operations().iter().any(|(_, op)| op
                        .identity()
                        .call_id
                        .as_str()
                        == "question"
                        && matches!(op.state(), OperationState::Prepared))
                );
                test.codex
                    .submit(codex_protocol::protocol::Op::UserInputAnswer {
                        id: question.turn_id,
                        response: serde_json::from_value(
                            json!({"answers":{"route":{"answers":["Safe"]}}}),
                        )?,
                    })
                    .await?;
                break;
            }
            EventMsg::Error(error) => {
                panic!("Plan question failed before answer: {}", error.message)
            }
            _ => {}
        }
    }
    complete(test)
        .await
        .expect("Plan question failed after native answer");
    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].body_json()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "request_user_input")
    );
    assert!(requests[1].body_contains_text("Safe"));
    let history = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    assert_eq!(history.runtime.operations().len(), 1);
    assert!(
        history
            .runtime
            .operations()
            .iter()
            .all(|(_, op)| matches!(op.state(), OperationState::InContext { .. }))
    );
    assert!(history.commits.iter().flat_map(codex_history::PanngCommit::host_effects).any(|effect| matches!(effect,
        codex_history::RolloutItem::RetainedContext(codex_history::RetainedContextEvent::VerifiedAnswer { answer, .. })
            if answer.questions.len() == 1 && answer.questions[0].answer == "Safe")));
    verify_cold_context(test).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_default_update_plan_is_durable_before_progress_notification_and_resume()
-> anyhow::Result<()> {
    let harness = TestCodexHarness::with_auto_env_builder(builder().with_config(|config| {
        config.update_plan_enabled = true;
    }))
    .await?;
    let plan = json!({"explanation":"Track real task progress","plan":[{"step":"Inspect sources","status":"in_progress"},{"step":"Apply changes","status":"pending"}]});
    let requests = core_test_support::responses::mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_function_call("progress", "update_plan", &plan.to_string()),
                ev_completed("progress"),
            ]),
            sse(vec![
                ev_assistant_message("done", "Progress recorded."),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let test = harness.test();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Record the task progress with update_plan.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    loop {
        let event =
            tokio::time::timeout(Duration::from_secs(20), test.codex.next_event()).await??;
        match event.msg {
            EventMsg::PlanUpdate(update) => {
                assert_eq!(serde_json::to_value(&update)?, plan);
                let durable = test
                    .thread_store
                    .experimental_panng_read(test.session_configured.thread_id)
                    .await?;
                assert!(durable.commits.iter().flat_map(codex_history::PanngCommit::host_effects).any(|effect| matches!(effect,
                    codex_history::RolloutItem::EventMsg(EventMsg::PlanUpdate(recorded)) if serde_json::to_value(recorded).unwrap() == plan)));
                break;
            }
            EventMsg::Error(error) => anyhow::bail!("Plan progress failed: {}", error.message),
            _ => {}
        }
    }
    complete(test).await?;
    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].body_json()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "update_plan")
    );
    assert!(requests[1].body_contains_text("Plan updated"));
    verify_cold_context(test).await
}
