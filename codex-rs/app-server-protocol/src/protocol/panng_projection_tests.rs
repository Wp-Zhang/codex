use super::*;
use codex_history::PanngCommit;
use codex_history::PanngHostCheckpoint;
use codex_history::ResponseItemEnvelope;
use codex_protocol::items::FunctionCallOutputItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use serde_json::json;

fn panng(effects: Vec<RolloutItem>) -> RolloutItem {
    let data = json!({
        "version": 1, "branch": "00000000-0000-4000-8000-000000000001",
        "commit_id": "00000000-0000-4000-8000-000000000002",
        "sequence": 1, "previous_sequence": 0,
        "mutation": {"Delta": {"changes": []}}, "host_effects": effects, "checkpoint": null,
    });
    RolloutItem::PanngCommit(Box::new(
        PanngCommit::from_slice(&serde_json::to_vec(&data).unwrap()).unwrap(),
    ))
}
fn output() -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope::new(
        ResponseItem::FunctionCallOutput {
            id: Some(codex_protocol::ResponseItemId::from_server(
                "result-1".into(),
            )),
            call_id: Some("call-1".into()),
            name: Some("probe".into()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text("done".into()),
            internal_chat_message_metadata_passthrough: None,
        },
    ))
}
fn completed() -> RolloutItem {
    item_completed(
        ThreadId::default(),
        "turn-1",
        TurnItem::FunctionCallOutput(FunctionCallOutputItem {
            id: "call-1".into(),
            name: "probe".into(),
            namespace: None,
            output: FunctionCallOutputBody::Text("done".into()),
        }),
    )
}
fn started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.into(),
        root_turn_id: None,
        trace_id: None,
        started_at: Some(10),
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }))
}
fn fallible(item: RolloutItem) -> Result<ThreadHistoryChangeSet, RolloutProjectionError> {
    project_rollout_line(&RolloutLine {
        timestamp: "2026-09-28T00:00:00Z".into(),
        ordinal: Some(7),
        item,
    })
}

#[test]
fn model_output_and_single_ui_event_preserve_one_native_projection() {
    let event = completed();
    assert_eq!(
        fallible(panng(vec![output(), event.clone()])).unwrap(),
        project(event)
    );
    assert_eq!(
        fallible(panng(vec![output()])).unwrap(),
        ThreadHistoryChangeSet::default()
    );
    let expected = ThreadHistoryChangeSet {
        changed_items: project(completed()).changed_items,
        changed_turns: project(started("turn-1")).changed_turns,
        ..Default::default()
    };
    assert_eq!(
        fallible(panng(vec![started("turn-1"), output(), completed()])).unwrap(),
        expected
    );
}

#[test]
fn rejects_multiple_ui_events_even_when_the_item_identity_is_unchanged() {
    assert_eq!(
        fallible(panng(vec![completed(), completed()])),
        Err(RolloutProjectionError::MultipleCompletedItems)
    );
    let mut different = completed();
    if let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = &mut different
        && let TurnItem::FunctionCallOutput(item) = &mut event.item
    {
        item.id = "call-2".into();
    }
    assert_eq!(
        fallible(panng(vec![completed(), different])),
        Err(RolloutProjectionError::MultipleCompletedItems)
    );
}

#[test]
fn rejects_multiple_boundaries_including_same_turn_and_unidentified_abort() {
    assert_eq!(
        fallible(panng(vec![started("turn-1"), started("turn-1")])),
        Err(RolloutProjectionError::MultipleTurnBoundaries)
    );
    let abort = RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: None,
        reason: TurnAbortReason::Interrupted,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }));
    assert_eq!(
        fallible(panng(vec![abort, started("turn-1")])),
        Err(RolloutProjectionError::MultipleTurnBoundaries)
    );
    assert_eq!(
        fallible(panng(vec![completed(), started("turn-2")])),
        Err(RolloutProjectionError::ConflictingTurns)
    );
}

#[test]
fn checkpoint_history_and_companions_do_not_recreate_old_ui_events() {
    let checkpoint = PanngHostCheckpoint {
        compacted: CompactedItem {
            message: "old context".into(), replacement_history: Some(vec![match output() {
                RolloutItem::ResponseItem(envelope) => envelope,
                _ => unreachable!(),
            }]), guardian_history: None, retained_context: None, mcp_resource_origins: None,
            window_number: Some(0), first_window_id: Some("00000000-0000-7000-8000-000000000004".into()),
            previous_window_id: None, window_id: Some("00000000-0000-7000-8000-000000000004".into()),
            compaction_response_id: None, latest_token_usage_record: None,
            resume_metadata: Some(codex_history::CompactionResumeMetadata {
                multi_agent_version: None, last_started_turn_id: None, previous_turn_settings: None,
            }),
        },
        companions: vec![
            RolloutItem::WorldState(codex_protocol::protocol::WorldStateItem::full(Default::default())),
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(serde_json::from_value(json!({
                "thread_id": null, "thread_settings": {
                    "model": "test", "model_provider_id": "test", "service_tier": null,
                    "approval_policy": "never", "approvals_reviewer": "user",
                    "permission_profile": codex_protocol::models::PermissionProfile::read_only(),
                    "cwd": std::env::current_dir().unwrap(), "reasoning_effort": null,
                    "reasoning_summary": null, "personality": null,
                    "collaboration_mode": {"mode":"default", "settings": {
                        "model":"test", "reasoning_effort":null, "developer_instructions":null
                    }}, "disabled_plugin_ids": []
                }
            })).unwrap())),
        ], parent_cut: None,
    };
    let data = json!({
        "version":1, "branch":"00000000-0000-4000-8000-000000000001",
        "commit_id":"00000000-0000-4000-8000-000000000002", "sequence":0, "previous_sequence":null,
        "mutation":{"Snapshot":{"state":{"execution_epoch":"00000000-0000-4000-8000-000000000003","operations":[]},"reason":"Initial"}},
        "host_effects":[], "checkpoint": checkpoint,
    });
    let item = RolloutItem::PanngCommit(Box::new(
        PanngCommit::from_slice(&serde_json::to_vec(&data).unwrap()).unwrap(),
    ));
    assert_eq!(fallible(item).unwrap(), ThreadHistoryChangeSet::default());
}
