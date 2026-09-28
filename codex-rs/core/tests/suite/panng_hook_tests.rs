use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_native_hooks_rewrite_block_and_commit_context_before_followup() -> anyhow::Result<()>
{
    let rewrite = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => {
            "echo HOOK_REWRITTEN > rewritten.txt; echo REWRITE_EXECUTED"
        }
        TestTargetOs::Windows => {
            "Set-Content rewritten.txt HOOK_REWRITTEN; Write-Output REWRITE_EXECUTED"
        }
    };
    let harness = TestCodexHarness::with_auto_env_builder(builder()
        .with_pre_build_hook(move |home| {
            let script = home.join("panng_hooks.py");
            let code = r#"import json, sys
p = json.load(sys.stdin)
command = p['tool_input'].get('command', '')
event = p['hook_event_name']
if event == 'PreToolUse':
    out = {'hookEventName': event, 'additionalContext': 'PRE_ALLOWED_CONTEXT'}
    if 'HOOK_ORIGINAL' in command:
        out['updatedInput'] = {'command': REWRITE}
        out['permissionDecision'] = 'allow'
    if 'MUST_NOT_RUN' in command:
        out.update(permissionDecision='deny', permissionDecisionReason='PRE_BLOCK_REASON', additionalContext='PRE_BLOCK_CONTEXT')
    print(json.dumps({'hookSpecificOutput':out}))
else:
    out = {'hookSpecificOutput':{'hookEventName':event, 'additionalContext':'POST_CONTEXT'}}
    if 'POST_EXECUTED' in command:
        out.update(decision='block', reason='POST_BLOCK_REASON')
    print(json.dumps(out))
"#.replace("REWRITE", &serde_json::to_string(rewrite).unwrap());
            std::fs::write(&script, code).unwrap();
            let group = json!({"matcher":"^Bash$","hooks":[{"type":"command","command":format!("python3 {}",script.display())}]});
            std::fs::write(home.join("hooks.json"), json!({"hooks":{"PreToolUse":[group],"PostToolUse":[group]}}).to_string()).unwrap();
        })
        .with_config(core_test_support::hooks::trust_discovered_hooks)).await?;
    let (original, denied, post) = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => (
            "echo HOOK_ORIGINAL > rewritten.txt",
            "echo MUST_NOT_RUN > denied.txt",
            "echo POST_EXECUTED > post.txt; echo POST_RESULT",
        ),
        TestTargetOs::Windows => (
            "Set-Content rewritten.txt HOOK_ORIGINAL",
            "Set-Content denied.txt MUST_NOT_RUN",
            "Set-Content post.txt POST_EXECUTED; Write-Output POST_RESULT",
        ),
    };
    let requests = core_test_support::responses::mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![exec("rewrite", original), ev_completed("rewrite")]),
            sse(vec![exec("blocked", denied), ev_completed("blocked")]),
            sse(vec![exec("post-block", post), ev_completed("post-block")]),
            sse(vec![
                ev_assistant_message("done", "hooks observed"),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let test = harness.test();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Run the three commands through the configured native hooks.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    complete(test).await?;
    assert_eq!(
        harness.read_file_text("rewritten.txt").await?.trim(),
        "HOOK_REWRITTEN"
    );
    assert!(
        harness.read_file_text("denied.txt").await.is_err(),
        "PreToolUse denial must precede execution"
    );
    assert_eq!(
        harness.read_file_text("post.txt").await?.trim(),
        "POST_EXECUTED",
        "PostToolUse blocks the result after actual execution"
    );
    let requests = requests.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[1].body_contains_text("PRE_ALLOWED_CONTEXT"));
    assert!(requests[1].body_contains_text("POST_CONTEXT"));
    assert!(requests[1].body_contains_text("REWRITE_EXECUTED"));
    assert!(requests[2].body_contains_text("PRE_BLOCK_CONTEXT"));
    assert!(requests[2].body_contains_text("PRE_BLOCK_REASON"));
    assert!(requests[3].body_contains_text("POST_BLOCK_REASON"));
    let id = test.session_configured.thread_id;
    let history = test.thread_store.experimental_panng_read(id).await?;
    let canonical = serde_json::to_string(&history.commits)?;
    for marker in [
        "panng_hook_decision",
        "HOOK_REWRITTEN",
        "PRE_ALLOWED_CONTEXT",
        "PRE_BLOCK_CONTEXT",
        "POST_CONTEXT",
        "POST_BLOCK_REASON",
    ] {
        assert!(
            canonical.contains(marker),
            "hook effect must have a canonical source: {marker}"
        );
    }
    let before = test
        .codex
        .conversation_history_snapshot()
        .await
        .items()
        .cloned()
        .collect::<Vec<_>>();
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
    assert_eq!(
        resumed
            .thread
            .conversation_history_snapshot()
            .await
            .items()
            .cloned()
            .collect::<Vec<_>>(),
        before
    );
    resumed.thread.shutdown_and_wait().await?;
    Ok(())
}
