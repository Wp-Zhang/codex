use super::*;
use crate::ThreadMetadataBuilder;
use codex_history::CompactedItem;
use codex_history::CompactionResumeMetadata;
use codex_history::PanngCommit;
use codex_history::PanngHostCheckpoint;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::WorldStateItem;
use pretty_assertions::assert_eq;
use serde_json::json;

fn metadata() -> ThreadMetadata {
    ThreadMetadataBuilder::new(
        ThreadId::default(),
        std::path::PathBuf::new(),
        chrono::Utc::now(),
        SessionSource::Cli,
    )
    .build("original-provider")
}

fn settings(model: &str, reasoning_effort: Option<ReasoningEffort>) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(serde_json::from_value(json!({
        "thread_id": ThreadId::default(), "thread_settings": {
            "model": model, "model_provider_id": "new-provider", "service_tier": null,
            "approval_policy": "never", "approvals_reviewer": "user",
            "permission_profile": codex_protocol::models::PermissionProfile::read_only(),
            "cwd": std::env::current_dir().unwrap(), "reasoning_effort": reasoning_effort,
            "reasoning_summary": null, "personality": null,
            "collaboration_mode": {"mode":"default", "settings": {
                "model": model, "reasoning_effort":reasoning_effort, "developer_instructions":null
            }}, "disabled_plugin_ids": []
        }
    })).unwrap()))
}

fn delta(effects: Vec<RolloutItem>) -> RolloutItem {
    let value = json!({
        "version": 1, "branch": "00000000-0000-4000-8000-000000000001",
        "commit_id": "00000000-0000-4000-8000-000000000002",
        "sequence": 1, "previous_sequence": 0, "mutation": {"Delta": {"changes": []}},
        "host_effects": effects, "checkpoint": null,
    });
    RolloutItem::PanngCommit(Box::new(
        PanngCommit::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap(),
    ))
}

fn snapshot(latest_settings: RolloutItem) -> RolloutItem {
    let checkpoint = PanngHostCheckpoint {
        compacted: CompactedItem {
            message: "retained context".into(),
            replacement_history: Some(Vec::new()),
            guardian_history: None,
            retained_context: None,
            mcp_resource_origins: None,
            window_number: Some(0),
            first_window_id: Some("00000000-0000-7000-8000-000000000004".into()),
            previous_window_id: None,
            window_id: Some("00000000-0000-7000-8000-000000000004".into()),
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: Some(CompactionResumeMetadata {
                multi_agent_version: None,
                last_started_turn_id: None,
                previous_turn_settings: None,
            }),
        },
        companions: vec![
            RolloutItem::WorldState(WorldStateItem::full(Default::default())),
            latest_settings,
        ],
        parent_cut: None,
    };
    let value = json!({
        "version":1, "branch":"00000000-0000-4000-8000-000000000001",
        "commit_id":"00000000-0000-4000-8000-000000000002", "sequence":0, "previous_sequence":null,
        "mutation":{"Snapshot":{"state":{"execution_epoch":"00000000-0000-4000-8000-000000000003","operations":[]},"reason":"Initial"}},
        "host_effects":[], "checkpoint": checkpoint,
    });
    RolloutItem::PanngCommit(Box::new(
        PanngCommit::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap(),
    ))
}

#[test]
fn panng_snapshot_applies_latest_settings_and_clears_stale_reasoning_effort() {
    let mut actual = metadata();
    apply_rollout_item(
        &mut actual,
        &settings("old-model", Some(ReasoningEffort::High)),
        "fallback",
    );
    let latest = settings("checkpoint-model", /*reasoning_effort*/ None);
    let mut expected = actual.clone();
    apply_rollout_item(&mut expected, &latest, "fallback");
    let checkpoint = snapshot(latest);
    assert!(rollout_item_affects_thread_metadata(&checkpoint));
    apply_rollout_item(&mut actual, &checkpoint, "fallback");
    assert_eq!(actual, expected);
    assert_eq!(actual.reasoning_effort, None);
}

#[test]
fn panng_delta_preserves_settings_order_and_explicit_clears() {
    let mut actual = metadata();
    let mut expected = actual.clone();
    let effects = vec![
        settings("first-model", Some(ReasoningEffort::High)),
        settings("last-model", /*reasoning_effort*/ None),
    ];
    for effect in &effects {
        apply_rollout_item(&mut expected, effect, "fallback");
    }
    let commit = delta(effects);
    assert!(rollout_item_affects_thread_metadata(&commit));
    apply_rollout_item(&mut actual, &commit, "fallback");
    assert_eq!(actual, expected);
    assert_eq!(actual.model.as_deref(), Some("last-model"));
    assert_eq!(actual.reasoning_effort, None);
}

#[test]
fn runtime_only_and_world_state_deltas_do_not_change_thread_metadata() {
    for commit in [
        delta(Vec::new()),
        delta(vec![RolloutItem::WorldState(WorldStateItem::full(
            Default::default(),
        ))]),
    ] {
        let mut actual = metadata();
        let expected = actual.clone();
        assert!(!rollout_item_affects_thread_metadata(&commit));
        apply_rollout_item(&mut actual, &commit, "fallback");
        assert_eq!(actual, expected);
    }
}
