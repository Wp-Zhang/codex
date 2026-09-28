use super::tests::make_session_and_context;
use super::*;
use codex_history::CompactionResumeMetadata;
use codex_history::PanngCommit;
use codex_history::PanngCommitData;
use codex_history::PanngHostCheckpoint;
use codex_history::ResponseItemEnvelope;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::CommitId;
use codex_panng_runtime::ExecutionEpoch;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::RuntimeSnapshot;
use codex_panng_runtime::SnapshotReason;
use pretty_assertions::assert_eq;
use serde_json::json;

fn delta(effects: Vec<RolloutItem>) -> PanngCommitData {
    PanngCommitData {
        version: 1,
        branch: BranchId::new("00000000-0000-4000-8000-000000000001".into()).unwrap(),
        commit_id: CommitId::new("00000000-0000-4000-8000-000000000002".into()).unwrap(),
        sequence: 1,
        previous_sequence: Some(0),
        mutation: RuntimeMutation::delta(Vec::new()).unwrap(),
        host_effects: effects,
        checkpoint: None,
    }
}

fn outer(data: PanngCommitData) -> RolloutItem {
    RolloutItem::PanngCommit(Box::new(PanngCommit::new(data).unwrap()))
}

#[tokio::test]
async fn panng_checkpoint_and_suffix_reconstruct_once_without_rewriting_outer_history() {
    let (session, turn) = make_session_and_context().await;
    let response = |text: &str| {
        ResponseItemEnvelope::new(
            serde_json::from_value::<ResponseItem>(json!({
                "type":"message", "role":"user", "content":[{"type":"input_text","text":text}]
            }))
            .unwrap(),
        )
    };
    let record = token_record(session.thread_id(), 17);
    let compacted = CompactedItem {
        message: "checkpoint".into(),
        replacement_history: Some(vec![response("before checkpoint")]),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: Some(0),
        first_window_id: Some("00000000-0000-7000-8000-000000000004".into()),
        previous_window_id: None,
        window_id: Some("00000000-0000-7000-8000-000000000004".into()),
        compaction_response_id: None,
        latest_token_usage_record: Some(record.clone()),
        resume_metadata: Some(CompactionResumeMetadata {
            multi_agent_version: None,
            last_started_turn_id: None,
            previous_turn_settings: None,
        }),
    };
    let context = RolloutItem::TurnContext(turn.to_turn_context_item());
    let settings = RolloutItem::EventMsg(thread_settings::applied_event(&session).await);
    let companions = vec![
        RolloutItem::WorldState(codex_protocol::protocol::WorldStateItem::full(
            Default::default(),
        )),
        context,
        settings,
    ];
    let mut snapshot = delta(Vec::new());
    snapshot.commit_id = CommitId::new("00000000-0000-4000-8000-000000000005".into()).unwrap();
    snapshot.sequence = 0;
    snapshot.previous_sequence = None;
    snapshot.mutation = RuntimeMutation::snapshot(
        RuntimeSnapshot::new(
            ExecutionEpoch::new("00000000-0000-4000-8000-000000000003".into()).unwrap(),
            Vec::new(),
        )
        .unwrap(),
        SnapshotReason::Initial,
    )
    .unwrap();
    snapshot.checkpoint = Some(PanngHostCheckpoint {
        compacted: compacted.clone(),
        companions: companions.clone(),
        parent_cut: None,
    });
    let suffix = RolloutItem::ResponseItem(response("after checkpoint"));
    let checkpoint = outer(snapshot);
    assert_eq!(
        Session::last_token_usage_record_from_rollout(std::slice::from_ref(&checkpoint)),
        Some(record.clone())
    );
    assert_eq!(
        Session::last_token_info_from_rollout(std::slice::from_ref(&checkpoint)),
        Some(TokenUsageInfo {
            total_token_usage: record.thread_token_usage.clone(),
            last_token_usage: record.usage.clone(),
            model_context_window: None,
        })
    );
    let newer_record = token_record(session.thread_id(), 99);
    let canonical = vec![
        checkpoint,
        outer(delta(vec![
            suffix.clone(),
            RolloutItem::TokenUsageRecord(newer_record.clone()),
        ])),
    ];
    assert_eq!(
        Session::last_token_usage_record_from_rollout(&canonical),
        Some(newer_record)
    );
    let original = serde_json::to_value(&canonical).unwrap();
    let mut native = vec![RolloutItem::Compacted(compacted)];
    native.extend(companions);
    native.push(suffix);
    let expected = session
        .reconstruct_history_from_rollout(&turn, &native)
        .await;
    assert_eq!(
        session
            .reconstruct_history_from_rollout(&turn, &canonical)
            .await,
        expected
    );
    assert_eq!(expected.history.len(), 2);
    assert_eq!(serde_json::to_value(&canonical).unwrap(), original);
    assert!(
        crate::thread_rollout_truncation::initial_history_has_prior_user_turns(
            &InitialHistory::Forked(canonical.clone())
        )
    );
    assert!(
        crate::thread_rollout_truncation::truncate_rollout_before_turn_id(
            canonical.clone(),
            "missing"
        )
        .is_err()
    );
    assert!(
        crate::thread_rollout_truncation::truncate_rollout_after_turn_id(canonical, "missing")
            .is_err()
    );
}

#[tokio::test]
async fn panng_early_settings_clear_and_token_record_are_visible() {
    let (session, turn) = make_session_and_context().await;
    let EventMsg::ThreadSettingsApplied(mut saved) = thread_settings::applied_event(&session).await
    else {
        unreachable!()
    };
    let thread_id = saved.thread_id.unwrap();
    saved
        .thread_settings
        .collaboration_mode
        .settings
        .reasoning_effort = None;
    saved.thread_settings.disabled_plugin_ids.clear();
    let settings = saved.thread_settings.clone();
    let mut stale_context = turn.to_turn_context_item();
    stale_context.disabled_plugin_ids = Some(vec!["stale".into()]);
    let record = token_record(thread_id, 42);
    let canonical = vec![outer(delta(vec![
        RolloutItem::TurnContext(stale_context),
        RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(saved)),
        RolloutItem::TokenUsageRecord(record.clone()),
    ]))];
    assert_eq!(
        crate::panng::host_replay::collaboration_mode(&canonical, thread_id),
        Some(&settings.collaboration_mode)
    );
    assert_eq!(
        codex_history::latest_disabled_plugin_ids(&canonical, thread_id),
        Some(&[] as &[String])
    );
    assert_eq!(
        Session::last_token_usage_record_from_rollout(&canonical),
        Some(record.clone())
    );
    assert_eq!(
        Session::last_token_info_from_rollout(&canonical),
        Some(TokenUsageInfo {
            total_token_usage: record.thread_token_usage,
            last_token_usage: record.usage,
            model_context_window: None,
        })
    );
}

fn token_record(thread_id: codex_protocol::ThreadId, total_tokens: i64) -> TokenUsageRecord {
    TokenUsageRecord {
        thread_id,
        session_id: thread_id.into(),
        turn_id: "turn-1".into(),
        root_turn_id: "turn-1".into(),
        response_id: "response-1".into(),
        usage: TokenUsage {
            total_tokens: 17,
            ..Default::default()
        },
        turn_token_usage: TokenUsage::default(),
        thread_token_usage: TokenUsage {
            total_tokens,
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn panng_failed_bootstrap_does_not_publish_session_configured_and_retains_cleanup() {
    let startup = Arc::new(startup::SessionStartup::default());
    let (tx, rx) = async_channel::unbounded();
    let home = tempfile::tempdir().unwrap();
    let result = super::tests::make_session_with_setup_and_events(
        |config| {
            config.codex_home =
                codex_utils_absolute_path::AbsolutePathBuf::try_from(home.path()).unwrap();
            config.ephemeral = false;
        },
        |configuration| {
            // Metadata-only settings fit the Initial checkpoint's 16 MiB cap but cannot
            // become an effective-settings delta (256 KiB). No model context is injected.
            configuration.disabled_plugin_ids = vec!["disabled-plugin-".repeat(24_000)];
        },
        ThreadHistoryMode::PanngV1,
        Some(Arc::clone(&startup)),
        tx,
        Arc::new(codex_extension_api::ExtensionRegistryBuilder::new().build()),
    )
    .await;
    assert!(result.is_err());
    let error = result.err().unwrap().to_string();
    assert!(
        error.contains("Panng commit exceeds encoded size limit"),
        "unexpected initialization error: {error}"
    );
    let retained = startup
        .session
        .get()
        .expect("constructed session remains owned for cleanup");
    let live = retained.live_thread().unwrap();
    let path = live.local_rollout_path().await.unwrap().unwrap();
    let recorded = codex_rollout::read_panng_suffix(std::fs::File::open(&path).unwrap()).unwrap();
    assert_eq!(
        recorded.commits.len(),
        1,
        "only Initial checkpoint may be durable"
    );
    while let Ok(event) = rx.try_recv() {
        assert!(!matches!(event.msg, EventMsg::SessionConfigured(_)));
    }
    let store = retained
        .services
        .thread_store
        .as_any()
        .downcast_ref::<codex_thread_store::LocalThreadStore>()
        .unwrap();
    assert!(store.live_rollout_path(retained.thread_id()).await.is_ok());
    startup.cleanup().await;
    assert!(store.live_rollout_path(retained.thread_id()).await.is_err());
}
