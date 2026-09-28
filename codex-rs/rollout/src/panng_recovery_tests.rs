use super::*;
use codex_history::CompactedItem;
use codex_history::CompactionResumeMetadata;
use codex_history::PanngCommitData;
use codex_history::PanngHostCheckpoint;
use codex_history::RolloutItem;
use codex_panng_runtime::*;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::Cursor;

fn id(number: u64) -> String {
    format!("00000000-0000-4000-8000-{number:012x}")
}
fn branch() -> BranchId {
    BranchId::new(id(1)).unwrap()
}
fn initial_runtime() -> RuntimeSnapshot {
    RuntimeSnapshot::new(ExecutionEpoch::new(id(2)).unwrap(), Vec::new()).unwrap()
}
fn host_checkpoint() -> PanngHostCheckpoint {
    PanngHostCheckpoint {
        compacted: CompactedItem {
            message: String::new(), replacement_history: Some(Vec::new()), guardian_history: None,
            retained_context: None, mcp_resource_origins: None, window_number: Some(0),
            first_window_id: Some("00000000-0000-7000-8000-000000000004".into()), previous_window_id: None,
            window_id: Some("00000000-0000-7000-8000-000000000004".into()),
            compaction_response_id: None, latest_token_usage_record: None,
            resume_metadata: Some(CompactionResumeMetadata { multi_agent_version: None, last_started_turn_id: None, previous_turn_settings: None }),
        },
        companions: vec![
            RolloutItem::WorldState(codex_protocol::protocol::WorldStateItem::full(Default::default())),
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(serde_json::from_value(json!({
                "thread_id": id(9), "thread_settings": {
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
    }
}
fn commit(sequence: u64, mutation: RuntimeMutation) -> PanngCommit {
    let checkpoint = mutation.snapshot_state().map(|_| host_checkpoint());
    PanngCommit::new(PanngCommitData {
        version: 1,
        branch: branch(),
        commit_id: CommitId::new(id(sequence + 100)).unwrap(),
        sequence,
        previous_sequence: sequence.checked_sub(1),
        mutation,
        host_effects: Vec::new(),
        checkpoint,
    })
    .unwrap()
}
fn snapshot(sequence: u64, state: RuntimeSnapshot) -> PanngCommit {
    let reason = if sequence == 0 {
        SnapshotReason::Initial
    } else {
        SnapshotReason::ReplayBound
    };
    commit(sequence, RuntimeMutation::snapshot(state, reason).unwrap())
}
fn header() -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&json!({
        "type":"session_meta", "timestamp":"2026-09-28T00:00:00Z", "ordinal":0,
        "payload": { "id":id(9), "history_mode":"panng_v1", "panng":{"version":1,"branch":id(1)} }
    }))
    .unwrap();
    bytes.push(b'\n');
    bytes
}
fn line(commit: &PanngCommit, ordinal: u64) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&json!({
        "type":"panng_commit", "timestamp":"2026-09-28T00:00:00Z", "ordinal":ordinal, "payload":commit
    })).unwrap();
    bytes.push(b'\n');
    bytes
}
fn prepared() -> OperationRecord {
    OperationRecord::prepared(
        OperationIdentity {
            id: OperationId::new(id(3)).unwrap(),
            origin_branch: branch(),
            owner_epoch: ExecutionEpoch::new(id(2)).unwrap(),
            turn_id: TurnId::new("turn".into()).unwrap(),
            logical_step_id: LogicalStepId::new(id(4)).unwrap(),
            call_id: CallId::new("call".into()).unwrap(),
            provider_response_id: None,
        },
        DispatchDescriptor::new(
            ToolName::new("probe".into()).unwrap(),
            BoundedJson::new("{}".into()).unwrap(),
            Digest::new("a".repeat(64)).unwrap(),
            Digest::new("b".repeat(64)).unwrap(),
            Digest::new("c".repeat(64)).unwrap(),
        ),
    )
}

#[test]
fn real_file_restores_semantic_delta_at_each_tail_cut_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal.jsonl");
    let initial = snapshot(0, initial_runtime());
    let mutation = RuntimeMutation::delta(vec![
        OperationChange::new(
            /*expected_revision*/ None,
            /*revision*/ 0,
            prepared(),
        )
        .unwrap(),
    ])
    .unwrap();
    let expected = mutation
        .apply(&branch(), Some((&branch(), &initial_runtime())))
        .unwrap();
    let delta = commit(1, mutation);
    let mut prefix = header();
    prefix.extend(line(&initial, 1));
    let last = line(&delta, 2);
    for offset in 0..=last.len() {
        let mut bytes = prefix.clone();
        bytes.extend(&last[..offset]);
        std::fs::write(&path, &bytes).unwrap();
        let restored = read_panng_suffix(std::fs::File::open(&path).unwrap()).unwrap();
        let complete = offset == last.len();
        assert_eq!(
            restored.runtime,
            if complete {
                expected.clone()
            } else {
                initial_runtime()
            }
        );
        assert_eq!(
            restored.cut,
            PanngReadCut {
                ordinal: if complete { 2 } else { 1 },
                end_byte_offset: if complete {
                    bytes.len() as u64
                } else {
                    prefix.len() as u64
                },
            }
        );
        assert_eq!(
            restored.torn_tail_bytes,
            if complete { 0 } else { offset as u64 }
        );
        assert_eq!(restored.thread_id.to_string(), id(9));
        assert_eq!(restored.header_bytes, header());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn rejects_complete_corruption_unknown_versions_branches_ids_and_gaps() {
    let initial = snapshot(0, initial_runtime());
    let delta = commit(1, RuntimeMutation::delta(Vec::new()).unwrap());
    let mut prefix = header();
    prefix.extend(line(&initial, 1));
    let valid: serde_json::Value = serde_json::from_slice(&line(&delta, 2)).unwrap();
    let mut invalid = Vec::new();
    for (pointer, replacement) in [
        ("/payload/version", json!(2)),
        ("/payload/branch", json!(id(8))),
        ("/payload/commit_id", json!(id(100))),
        ("/payload/sequence", json!(2)),
        ("/ordinal", json!(3)),
        ("/type", json!("response_item")),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(pointer).unwrap() = replacement;
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        invalid.push(bytes);
    }
    invalid.extend([b"broken\n".to_vec(), b"\n".to_vec()]);
    for suffix in invalid {
        let mut bytes = prefix.clone();
        bytes.extend(suffix);
        assert!(read_panng_suffix(Cursor::new(bytes)).is_err());
    }
    let mut bytes = prefix;
    bytes.extend(line(
        &commit(2, RuntimeMutation::delta(Vec::new()).unwrap()),
        2,
    ));
    assert!(read_panng_suffix(Cursor::new(bytes)).is_err());
}

#[test]
fn validates_initial_commit_separately_but_does_not_claim_entire_prefix_integrity() {
    let initial = snapshot(0, initial_runtime());
    let latest = snapshot(2, initial_runtime());
    let mut bytes = header();
    bytes.extend(line(&initial, 1));
    bytes.extend(b"unverified-old-prefix\n");
    let checkpoint_offset = bytes.len() as u64;
    bytes.extend(line(&latest, 3));
    let recovered = read_panng_suffix(Cursor::new(bytes)).unwrap();
    assert_eq!(recovered.checkpoint_offset, checkpoint_offset);
    assert_eq!(recovered.commits.len(), 1);
    assert_eq!(recovered.runtime, initial_runtime());

    let mut missing_initial = header();
    missing_initial.extend(line(&latest, 3));
    assert!(read_panng_suffix(Cursor::new(missing_initial)).is_err());
    assert!(read_panng_suffix(Cursor::new(header())).is_err());
    let mut torn_initial = header();
    torn_initial.extend(b"{\"type\":");
    assert!(read_panng_suffix(Cursor::new(torn_initial)).is_err());
}

#[test]
fn snapshot_cannot_give_foreign_prepared_operation_execution_ownership() {
    let initial = snapshot(0, initial_runtime());
    let state =
        RuntimeSnapshot::new(ExecutionEpoch::new(id(2)).unwrap(), vec![(0, prepared())]).unwrap();
    let latest = snapshot(1, state);
    let mut value: serde_json::Value = serde_json::from_slice(&line(&latest, 2)).unwrap();
    value["payload"]["mutation"]["Snapshot"]["state"]["operations"][0][1]["origin_branch"] =
        json!(id(8));
    let mut bytes = header();
    bytes.extend(line(&initial, 1));
    bytes.extend(serde_json::to_vec(&value).unwrap());
    bytes.push(b'\n');
    assert!(read_panng_suffix(Cursor::new(bytes)).is_err());
}

#[test]
fn suffix_record_cap_is_enforced_without_exposing_a_limit_override() {
    let mut bytes = header();
    bytes.extend(line(&snapshot(0, initial_runtime()), 1));
    for sequence in 1..4096 {
        bytes.extend(line(
            &commit(sequence, RuntimeMutation::delta(Vec::new()).unwrap()),
            sequence + 1,
        ));
    }
    assert_eq!(
        read_panng_suffix(Cursor::new(bytes.clone()))
            .unwrap()
            .commits
            .len(),
        4096
    );
    bytes.extend(line(
        &commit(4096, RuntimeMutation::delta(Vec::new()).unwrap()),
        4097,
    ));
    assert!(read_panng_suffix(Cursor::new(bytes)).is_err());
}

#[test]
fn panng_header_and_torn_record_caps_reject_without_weakening_native_headers() {
    let mut oversized_header = vec![b' '; MAX_PANNG_HEADER_BYTES];
    oversized_header.extend(header());
    assert!(read_panng_suffix(Cursor::new(oversized_header)).is_err());

    let mut bytes = header();
    bytes.extend(line(&snapshot(0, initial_runtime()), 1));
    bytes.extend(std::iter::repeat_n(b'x', MAX_PANNG_LINE_BYTES + 1));
    assert!(read_panng_suffix(Cursor::new(bytes)).is_err());
}

#[test]
fn checkpoint_and_delta_settings_require_header_thread_ownership() {
    let initial = snapshot(0, initial_runtime());
    for owner in [None, Some(ThreadId::from_string(&id(8)).unwrap())] {
        let mut settings = host_checkpoint().companions.remove(1);
        if let RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) = &mut settings {
            event.thread_id = owner;
        } else {
            panic!("fixture must provide settings");
        }
        let effect = serde_json::to_value(settings).unwrap();
        let mut wrong_checkpoint: serde_json::Value =
            serde_json::from_slice(&line(&initial, 1)).unwrap();
        wrong_checkpoint["payload"]["checkpoint"]["companions"][1] = effect.clone();
        let mut bytes = header();
        bytes.extend(serde_json::to_vec(&wrong_checkpoint).unwrap());
        bytes.push(b'\n');
        assert!(read_panng_suffix(Cursor::new(bytes)).is_err());

        let mut wrong_delta: serde_json::Value = serde_json::from_slice(&line(
            &commit(1, RuntimeMutation::delta(Vec::new()).unwrap()),
            2,
        ))
        .unwrap();
        wrong_delta["payload"]["host_effects"] = json!([effect]);
        let mut bytes = header();
        bytes.extend(line(&initial, 1));
        bytes.extend(serde_json::to_vec(&wrong_delta).unwrap());
        bytes.push(b'\n');
        assert!(read_panng_suffix(Cursor::new(bytes)).is_err());
    }
}

#[test]
fn later_checkpoint_cannot_reuse_initial_commit_identity() {
    let initial = snapshot(0, initial_runtime());
    let latest = snapshot(2, initial_runtime());
    let mut value: serde_json::Value = serde_json::from_slice(&line(&latest, 3)).unwrap();
    value["payload"]["commit_id"] = json!(initial.commit_id());
    let mut bytes = header();
    bytes.extend(line(&initial, 1));
    bytes.extend(line(
        &commit(1, RuntimeMutation::delta(Vec::new()).unwrap()),
        2,
    ));
    bytes.extend(serde_json::to_vec(&value).unwrap());
    bytes.push(b'\n');
    assert!(read_panng_suffix(Cursor::new(bytes)).is_err());
}
