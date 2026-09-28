use super::*;
use crate::CompactionResumeMetadata;
use crate::ResponseItemEnvelope;
use codex_panng_runtime::ExecutionEpoch;
use codex_panng_runtime::RuntimeSnapshot;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use serde_json::json;

fn data(mutation: RuntimeMutation) -> PanngCommitData {
    PanngCommitData {
        version: 1,
        branch: BranchId::new("00000000-0000-4000-8000-000000000001".into()).unwrap(),
        commit_id: CommitId::new("00000000-0000-4000-8000-000000000002".into()).unwrap(),
        sequence: 1,
        previous_sequence: Some(0),
        mutation,
        host_effects: Vec::new(),
        checkpoint: None,
    }
}

fn output(number: serde_json::Number) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope::new(
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call-a".into()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text("actual result".into()),
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    turn_id: Some("turn-a".into()),
                    create_time: Some(number),
                    ..Default::default()
                },
            ),
        },
    ))
}

#[test]
fn atomic_delta_preserves_float_metadata_and_canonical_identity() {
    for literal in ["0.12345678912345678", "1e-300", "1790571625.873609"] {
        let mut data = data(RuntimeMutation::delta(Vec::new()).unwrap());
        let effect = output(literal.parse().unwrap());
        let expected = serde_json::to_value(&effect).unwrap();
        data.host_effects.push(effect);
        let commit = PanngCommit::new(data).unwrap();
        let bytes = commit.canonical_bytes().unwrap();
        let restored = PanngCommit::from_slice(&bytes).unwrap();
        assert_eq!(restored.canonical_bytes().unwrap(), bytes);
        assert_eq!(
            serde_json::to_value(&restored.host_effects()[0]).unwrap(),
            expected
        );
        assert_eq!(serde_json::to_vec(&commit).unwrap(), bytes);
    }
}

#[test]
fn non_restorable_warehouse_fields_cannot_silently_change_identity() {
    let mut data = data(RuntimeMutation::delta(Vec::new()).unwrap());
    let mut effect = output("0.25".parse().unwrap());
    if let RolloutItem::ResponseItem(ResponseItemEnvelope {
        item:
            ResponseItem::FunctionCallOutput {
                internal_chat_message_metadata_passthrough: Some(metadata),
                ..
            },
        ..
    }) = &mut effect
    {
        metadata.cell_id = Some("non-restorable-cell".into());
    }
    data.host_effects.push(effect);
    assert!(
        PanngCommit::new(data)
            .unwrap_err()
            .contains("non-restorable")
    );

    let mut valid = data_for_raw();
    valid["host_effects"][0]["payload"]["internal_chat_message_metadata_passthrough"]["cell_id"] =
        json!("forged");
    assert!(PanngCommit::from_slice(&serde_json::to_vec(&valid).unwrap()).is_err());
}

fn data_for_raw() -> serde_json::Value {
    let mut data = data(RuntimeMutation::delta(Vec::new()).unwrap());
    data.host_effects.push(output("0.5".parse().unwrap()));
    serde_json::to_value(PanngCommit::new(data).unwrap()).unwrap()
}

#[test]
fn invalid_version_predecessor_and_effects_are_rejected_at_decode() {
    let valid = data_for_raw();
    for (field, replacement) in [
        ("version", json!(2)),
        ("previous_sequence", json!(7)),
        (
            "host_effects",
            json!([{"type":"panng_commit","payload":{}}]),
        ),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = replacement;
        assert!(
            PanngCommit::from_slice(&serde_json::to_vec(&invalid).unwrap()).is_err(),
            "{field}"
        );
    }
    let mut invalid = valid;
    invalid["host_effects"] =
        json!([{"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}]);
    assert!(PanngCommit::from_slice(&serde_json::to_vec(&invalid).unwrap()).is_err());
}

#[test]
fn checkpoint_requires_joint_context_and_round_trips_at_one_cut() {
    let state = RuntimeSnapshot::new(
        ExecutionEpoch::new("00000000-0000-4000-8000-000000000003".into()).unwrap(),
        Vec::new(),
    )
    .unwrap();
    let mut data = data(RuntimeMutation::snapshot(state, SnapshotReason::Initial).unwrap());
    data.sequence = 0;
    data.previous_sequence = None;
    assert!(PanngCommit::new(data.clone()).is_err());
    data.checkpoint = Some(PanngHostCheckpoint {
        compacted: CompactedItem {
            message: String::new(),
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
        companions: vec![RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
            serde_json::from_value(json!({
                "thread_id": null,
                "thread_settings": {
                    "model": "test", "model_provider_id": "test", "service_tier": null,
                    "approval_policy": "never", "approvals_reviewer": "user",
                    "permission_profile": codex_protocol::models::PermissionProfile::read_only(),
                    "cwd": std::env::current_dir().unwrap(),
                    "reasoning_effort": null, "reasoning_summary": null, "personality": null,
                    "collaboration_mode": {"mode":"default", "settings": {
                        "model":"test", "reasoning_effort":null, "developer_instructions":null
                    }}, "disabled_plugin_ids": []
                }
            }))
            .unwrap(),
        ))],
        parent_cut: None,
    });
    let mut invalid_window = data.clone();
    invalid_window
        .checkpoint
        .as_mut()
        .unwrap()
        .compacted
        .window_id = Some("window-0".into());
    assert!(PanngCommit::new(invalid_window).is_err());
    let mut missing_settings = data.clone();
    missing_settings
        .checkpoint
        .as_mut()
        .unwrap()
        .companions
        .clear();
    assert!(PanngCommit::new(missing_settings).is_err());
    let mut patch_baseline = data.clone();
    patch_baseline
        .checkpoint
        .as_mut()
        .unwrap()
        .companions
        .push(RolloutItem::WorldState(crate::WorldStateItem::patch(
            Default::default(),
        )));
    assert!(PanngCommit::new(patch_baseline).is_err());
    let commit = PanngCommit::new(data.clone()).unwrap();
    let bytes = commit.canonical_bytes().unwrap();
    assert_eq!(
        PanngCommit::from_slice(&bytes)
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        bytes
    );
    // Fenced snapshots retain historical delivery references rather than claiming
    // that already delivered results were newly delivered by the snapshot commit.
    for reason in [SnapshotReason::Fork, SnapshotReason::ResumeFence] {
        let historical_commit =
            CommitId::new("00000000-0000-4000-8000-000000000077".into()).unwrap();
        let record = delivered_operation(historical_commit);
        let state = RuntimeSnapshot::new(
            ExecutionEpoch::new("00000000-0000-4000-8000-000000000088".into()).unwrap(),
            vec![(2, record.clone())],
        )
        .unwrap();
        let mut snapshot = data.clone();
        snapshot.mutation = RuntimeMutation::snapshot(state, reason).unwrap();
        match reason {
            SnapshotReason::Fork => {
                snapshot.checkpoint.as_mut().unwrap().parent_cut = Some(HistoryPosition {
                    thread_id: codex_protocol::ThreadId::new(),
                    end_ordinal_exclusive: 3,
                    end_byte_offset: 100,
                });
            }
            SnapshotReason::ResumeFence => {
                snapshot.sequence = 1;
                snapshot.previous_sequence = Some(0);
            }
            SnapshotReason::Initial
            | SnapshotReason::Compaction
            | SnapshotReason::ReplayBound
            | SnapshotReason::RetireDelivered => {
                unreachable!()
            }
        }
        let snapshot = PanngCommit::new(snapshot).unwrap();
        let restored = PanngCommit::from_slice(&snapshot.canonical_bytes().unwrap()).unwrap();
        assert_eq!(
            restored.mutation().snapshot_state().unwrap().operations(),
            &[(2, record)]
        );
    }
    data.host_effects.push(output("0.1".parse().unwrap()));
    assert!(PanngCommit::new(data).is_err());
}

#[test]
fn oversized_effects_and_encoded_delta_are_rejected() {
    let mut data = data(RuntimeMutation::delta(Vec::new()).unwrap());
    data.host_effects = (0..65).map(|_| output("0.1".parse().unwrap())).collect();
    assert!(PanngCommit::new(data.clone()).is_err());
    data.host_effects = vec![RolloutItem::ResponseItem(ResponseItemEnvelope::new(
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("large".into()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text("x".repeat(MAX_DELTA_BYTES)),
            internal_chat_message_metadata_passthrough: None,
        },
    ))];
    assert!(PanngCommit::new(data).is_err());
}

fn delivered_operation(delivery_commit: CommitId) -> codex_panng_runtime::OperationRecord {
    serde_json::from_value(json!({
        "id":"00000000-0000-4000-8000-000000000010",
        "origin_branch":"00000000-0000-4000-8000-000000000001",
        "owner_epoch":"00000000-0000-4000-8000-000000000003",
        "turn_id":"turn-a",
        "logical_step_id":"00000000-0000-4000-8000-000000000011",
        "call_id":"call-a", "provider_response_id":null,
        "descriptor": {"tool":"test", "arguments":"{}",
            "environment_digest":"a".repeat(64), "authorization_digest":"b".repeat(64), "catalog_digest":"c".repeat(64)},
        "state":{"InContext":{
            "result_id":"00000000-0000-4000-8000-000000000012", "outcome":"Succeeded",
            "result":{"Inline":{"encoding":"CodexResponseEnvelopeV1","payload":"{}"}},
            "delivery_commit":delivery_commit,
        }}
    })).unwrap()
}

#[test]
fn delta_delivery_must_reference_the_enclosing_commit() {
    let outer = CommitId::new("00000000-0000-4000-8000-000000000002".into()).unwrap();
    let record = delivered_operation(outer);
    let mutation = RuntimeMutation::delta(vec![
        codex_panng_runtime::OperationChange::new(Some(1), 2, record).unwrap(),
    ])
    .unwrap();
    let valid = data(mutation);
    let commit = PanngCommit::new(valid.clone()).unwrap();
    assert_eq!(
        PanngCommit::from_slice(&commit.canonical_bytes().unwrap())
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        commit.canonical_bytes().unwrap()
    );
    let mut invalid = valid;
    invalid.commit_id = CommitId::new("00000000-0000-4000-8000-000000000099".into()).unwrap();
    assert!(PanngCommit::new(invalid.clone()).is_err());
    assert!(PanngCommit::from_slice(&serde_json::to_vec(&invalid).unwrap()).is_err());
}

#[test]
fn host_generated_fractional_create_time_keeps_exact_f64_bits_across_canonical_reload() {
    // Same construction as Session::response_item_create_time, not Number::from_str:
    // parsing the fixture first can already round away the bit this regression protects.
    // 1790601806.0000021 previously parsed as 1790601806.000002 without float_roundtrip.
    let timestamp = f64::from_bits(0x41da_ae9a_9380_0009);
    let number = serde_json::Number::from_f64(timestamp).unwrap();
    let mut data = data(RuntimeMutation::delta(Vec::new()).unwrap());
    let effect = output(number);
    let expected = serde_json::to_value(&effect).unwrap();
    data.host_effects.push(effect);
    let commit =
        PanngCommit::new(data).expect("valid host timestamp must not be rejected as lost metadata");
    let bytes = commit.canonical_bytes().unwrap();
    let recovered = PanngCommit::from_slice(&bytes).unwrap();
    assert_eq!(recovered.canonical_bytes().unwrap(), bytes);
    assert_eq!(
        serde_json::to_value(&recovered.host_effects()[0]).unwrap(),
        expected
    );
    let RolloutItem::ResponseItem(envelope) = &recovered.host_effects()[0] else {
        panic!("response envelope")
    };
    let ResponseItem::FunctionCallOutput {
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = &envelope.item
    else {
        panic!("timestamp metadata")
    };
    assert_eq!(
        metadata
            .create_time
            .as_ref()
            .unwrap()
            .as_f64()
            .unwrap()
            .to_bits(),
        timestamp.to_bits()
    );
}

#[test]
fn provider_reasoning_encryption_and_message_phase_survive_canonical_reload() {
    let mut data = data(RuntimeMutation::delta(Vec::new()).unwrap());
    for raw in [
        json!({"type":"reasoning","id":"reasoning-fixture", "summary":[{"type":"summary_text","text":"provider summary"}],
            "content":[{"type":"reasoning_text","text":"synthetic fixture"}], "encrypted_content":"opaque-provider-envelope"}),
        json!({"type":"message","id":"message-fixture","role":"assistant","phase":"final_answer",
            "content":[{"type":"output_text","text":"visible answer"}]}),
    ] {
        let mut item: ResponseItem = serde_json::from_value(raw).unwrap();
        item.set_create_time_if_missing(
            serde_json::Number::from_f64(f64::from_bits(0x41da_ae9a_9380_0009)).unwrap(),
        );
        data.host_effects
            .push(RolloutItem::ResponseItem(ResponseItemEnvelope::new(item)));
    }
    let expected = serde_json::to_value(&data.host_effects).unwrap();
    let commit = PanngCommit::new(data).unwrap();
    let recovered = PanngCommit::from_slice(&commit.canonical_bytes().unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(recovered.host_effects()).unwrap(),
        expected
    );
}
