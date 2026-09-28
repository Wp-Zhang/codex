use super::*;
use crate::CompactionResumeMetadata;
use crate::InitialHistory;
use crate::PanngCommit;
use crate::PanngCommitData;
use crate::PanngHostCheckpoint;
use crate::ResponseItemEnvelope;
use crate::latest_disabled_plugin_ids;
use crate::resume_multi_agent_version;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::CommitId;
use codex_panng_runtime::ExecutionEpoch;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::RuntimeSnapshot;
use codex_panng_runtime::SnapshotReason;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::WorldStateItem;
use pretty_assertions::assert_eq;
use serde_json::json;

fn settings(thread_id: ThreadId, disabled: Vec<String>) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(serde_json::from_value(json!({
        "thread_id": thread_id,
        "thread_settings": {
            "model":"test", "model_provider_id":"test", "approval_policy":"never",
            "approvals_reviewer":"user", "permission_profile":codex_protocol::models::PermissionProfile::read_only(),
            "cwd":std::env::current_dir().unwrap(),
            "collaboration_mode":{"mode":"default","settings":{"model":"test","reasoning_effort":null,"developer_instructions":null}},
            "disabled_plugin_ids":disabled
        }
    })).unwrap()))
}

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

fn snapshot(thread_id: ThreadId, disabled: Vec<String>) -> PanngCommitData {
    let mut data = delta(Vec::new());
    data.sequence = 0;
    data.previous_sequence = None;
    data.mutation = RuntimeMutation::snapshot(
        RuntimeSnapshot::new(
            ExecutionEpoch::new("00000000-0000-4000-8000-000000000003".into()).unwrap(),
            Vec::new(),
        )
        .unwrap(),
        SnapshotReason::Initial,
    )
    .unwrap();
    let mut context: TurnContextItem = serde_json::from_value(json!({
        "cwd":std::env::current_dir().unwrap(), "approval_policy":"never",
        "sandbox_policy":{"type":"danger-full-access"}, "model":"test", "summary":"auto"
    }))
    .unwrap();
    context.disabled_plugin_ids = Some(vec!["stale-turn-plugin".into()]);
    let replacement: ResponseItem = serde_json::from_value(json!({
        "type":"message","role":"user","content":[{"type":"input_text","text":"old checkpoint text"}]
    })).unwrap();
    data.checkpoint = Some(PanngHostCheckpoint {
        compacted: CompactedItem {
            message: "checkpoint".into(),
            replacement_history: Some(vec![ResponseItemEnvelope::new(replacement)]),
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
                multi_agent_version: Some(MultiAgentVersion::V2),
                last_started_turn_id: None,
                previous_turn_settings: None,
            }),
        },
        companions: vec![
            RolloutItem::WorldState(WorldStateItem::full(Default::default())),
            RolloutItem::TurnContext(context),
            settings(thread_id, disabled),
        ],
        parent_cut: None,
    });
    data
}

#[test]
fn host_replay_borrows_forward_reverse_and_never_replays_checkpoint_messages() {
    let commit = PanngCommit::new(snapshot(ThreadId::new(), Vec::new())).unwrap();
    let native_compacted = commit.checkpoint().unwrap().compacted.clone();
    let response: ResponseItem = serde_json::from_value(
        json!({"type":"function_call_output","call_id":"a","output":"new result"}),
    )
    .unwrap();
    let delta = PanngCommit::new(delta(vec![RolloutItem::ResponseItem(
        ResponseItemEnvelope::new(response),
    )]))
    .unwrap();
    let items = vec![
        RolloutItem::PanngCommit(Box::new(delta)),
        RolloutItem::PanngCommit(Box::new(commit)),
        RolloutItem::Compacted(native_compacted),
    ];
    let before = serde_json::to_vec(&items).unwrap();
    let view = host_replay_items(&items);
    let forward = view.clone().collect::<Vec<_>>();
    let backward = view.rev().collect::<Vec<_>>();
    assert_eq!(forward.len(), 6);
    for (left, right) in forward.iter().zip(backward.iter().rev()) {
        assert!(match (left, right) {
            (HostReplayItem::Item(left), HostReplayItem::Item(right)) =>
                std::ptr::eq(*left, *right),
            (HostReplayItem::Compacted(left), HostReplayItem::Compacted(right)) =>
                std::ptr::eq(*left, *right),
            _ => false,
        });
    }
    let RolloutItem::PanngCommit(outer) = &items[1] else {
        unreachable!()
    };
    assert!(
        matches!(forward[1], HostReplayItem::Compacted(value) if std::ptr::eq(value, &outer.checkpoint().unwrap().compacted))
    );
    assert!(
        matches!(forward[2], HostReplayItem::Item(value) if std::ptr::eq(value, &outer.checkpoint().unwrap().companions[0]))
    );
    assert_eq!(
        forward
            .iter()
            .filter(|item| matches!(item, HostReplayItem::Item(RolloutItem::ResponseItem(_))))
            .count(),
        1
    );
    assert_eq!(serde_json::to_vec(&items).unwrap(), before);
    let restored: Vec<RolloutItem> = serde_json::from_slice(&before).unwrap();
    assert_eq!(serde_json::to_vec(&restored).unwrap(), before);
}

#[test]
fn early_history_accessors_observe_embedded_settings_clears_and_resume_metadata() {
    let thread_id = ThreadId::new();
    let selected = RolloutItem::PanngCommit(Box::new(
        PanngCommit::new(snapshot(thread_id, vec!["plugin-a".into()])).unwrap(),
    ));
    assert_eq!(
        resume_multi_agent_version(&selected),
        Some(MultiAgentVersion::V2)
    );
    let cleared = RolloutItem::PanngCommit(Box::new(
        PanngCommit::new(delta(vec![settings(thread_id, Vec::new())])).unwrap(),
    ));
    let history = InitialHistory::Forked(vec![selected, cleared]);
    assert_eq!(
        latest_disabled_plugin_ids(history.get_rollout_items(), thread_id),
        Some(&[] as &[String])
    );
    assert_eq!(
        history.get_multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    assert_eq!(history.get_event_msgs().unwrap().len(), 2);
    assert_eq!(
        history.session_cwd(),
        None,
        "embedded turn context does not redefine native session cwd semantics"
    );
    let checkpoint_clear = vec![RolloutItem::PanngCommit(Box::new(
        PanngCommit::new(snapshot(thread_id, Vec::new())).unwrap(),
    ))];
    assert_eq!(
        latest_disabled_plugin_ids(&checkpoint_clear, thread_id),
        Some(&[] as &[String])
    );
}

#[test]
fn checkpoint_order_and_nested_outer_records_are_rejected() {
    let mut wrong_order = snapshot(ThreadId::new(), Vec::new());
    wrong_order
        .checkpoint
        .as_mut()
        .unwrap()
        .companions
        .swap(0, 1);
    assert!(PanngCommit::new(wrong_order.clone()).is_err());
    assert!(PanngCommit::from_slice(&serde_json::to_vec(&wrong_order).unwrap()).is_err());
    let nested = RolloutItem::PanngCommit(Box::new(PanngCommit::new(delta(Vec::new())).unwrap()));
    let append = delta(vec![nested.clone()]);
    assert!(PanngCommit::new(append.clone()).is_err());
    assert!(PanngCommit::from_slice(&serde_json::to_vec(&append).unwrap()).is_err());
    let mut checkpoint = snapshot(ThreadId::new(), Vec::new());
    checkpoint
        .checkpoint
        .as_mut()
        .unwrap()
        .companions
        .push(nested);
    assert!(PanngCommit::new(checkpoint.clone()).is_err());
    assert!(PanngCommit::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).is_err());
}

#[test]
fn panng_checkpoint_runtime_wins_over_frozen_context_and_header_across_resume() {
    let thread_id = ThreadId::new();
    let header = RolloutItem::SessionMeta(codex_protocol::protocol::SessionMetaLine {
        meta: codex_protocol::protocol::SessionMeta {
            id: thread_id,
            session_id: thread_id.into(),
            multi_agent_version: Some(MultiAgentVersion::V1),
            ..Default::default()
        },
        git: None,
    });
    let mut first = snapshot(thread_id, Vec::new());
    first.sequence = 2;
    first.previous_sequence = Some(1);
    first.mutation = RuntimeMutation::snapshot(
        RuntimeSnapshot::new(
            ExecutionEpoch::new("00000000-0000-4000-8000-000000000005".into()).unwrap(),
            Vec::new(),
        )
        .unwrap(),
        SnapshotReason::ResumeFence,
    )
    .unwrap();
    let RolloutItem::TurnContext(context) = &mut first.checkpoint.as_mut().unwrap().companions[1]
    else {
        unreachable!()
    };
    context.multi_agent_version = Some(MultiAgentVersion::V1);
    let mut second = first.clone();
    second.sequence = 4;
    second.previous_sequence = Some(3);
    second.commit_id = CommitId::new("00000000-0000-4000-8000-000000000009".into()).unwrap();
    second.mutation = RuntimeMutation::snapshot(
        RuntimeSnapshot::new(
            ExecutionEpoch::new("00000000-0000-4000-8000-000000000008".into()).unwrap(),
            Vec::new(),
        )
        .unwrap(),
        SnapshotReason::ResumeFence,
    )
    .unwrap();
    for data in [first, second] {
        let record = RolloutItem::PanngCommit(Box::new(PanngCommit::new(data).unwrap()));
        assert_eq!(
            resume_multi_agent_version(&record),
            Some(MultiAgentVersion::V2)
        );
        let history = InitialHistory::Resumed(crate::ResumedHistory {
            conversation_id: thread_id,
            history: std::sync::Arc::new(vec![header.clone(), record]),
            rollout_path: None,
        });
        assert_eq!(
            history.get_multi_agent_version(),
            Some(MultiAgentVersion::V2)
        );
    }
}

#[test]
fn panng_checkpoint_clear_stops_fallback_but_later_delta_can_select_runtime() {
    let thread_id = ThreadId::new();
    let header = RolloutItem::SessionMeta(codex_protocol::protocol::SessionMetaLine {
        meta: codex_protocol::protocol::SessionMeta {
            id: thread_id,
            session_id: thread_id.into(),
            multi_agent_version: Some(MultiAgentVersion::V1),
            ..Default::default()
        },
        git: None,
    });
    let mut data = snapshot(thread_id, Vec::new());
    let checkpoint = data.checkpoint.as_mut().unwrap();
    checkpoint
        .compacted
        .resume_metadata
        .as_mut()
        .unwrap()
        .multi_agent_version = None;
    let RolloutItem::TurnContext(context) = &mut checkpoint.companions[1] else {
        unreachable!()
    };
    context.multi_agent_version = Some(MultiAgentVersion::V1);
    let mut later_context = context.clone();
    later_context.multi_agent_version = Some(MultiAgentVersion::V2);
    let record = RolloutItem::PanngCommit(Box::new(PanngCommit::new(data).unwrap()));
    assert_eq!(resume_multi_agent_version(&record), None);
    let mut items = vec![header, record];
    assert_eq!(
        InitialHistory::Forked(items.clone()).get_multi_agent_version(),
        None
    );
    items.push(RolloutItem::PanngCommit(Box::new(
        PanngCommit::new(delta(vec![RolloutItem::TurnContext(later_context)])).unwrap(),
    )));
    assert_eq!(
        InitialHistory::Forked(items).get_multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
}
