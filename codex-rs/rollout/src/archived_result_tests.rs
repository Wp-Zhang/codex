use super::*;
use codex_history::CompactedItem;
use codex_history::CompactionResumeMetadata;
use codex_history::PanngCommit;
use codex_history::PanngCommitData;
use codex_history::PanngHostCheckpoint;
use codex_panng_runtime::*;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use pretty_assertions::assert_eq;
use serde_json::json;

fn id(number: u64) -> String {
    format!("00000000-0000-4000-8000-{number:012x}")
}
fn branch() -> BranchId {
    BranchId::new(id(/*number*/ 1)).unwrap()
}
fn initial_runtime() -> RuntimeSnapshot {
    RuntimeSnapshot::new(ExecutionEpoch::new(id(/*number*/ 2)).unwrap(), Vec::new()).unwrap()
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
                "thread_id": id(/*number*/ 9), "thread_settings": {
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
        previous_sequence: sequence.checked_sub(/*rhs*/ 1),
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
    let thread_id = ThreadId::from_string(&id(/*number*/ 9)).unwrap();
    let meta = codex_protocol::protocol::SessionMetaLine {
        meta: codex_protocol::protocol::SessionMeta {
            id: thread_id,
            session_id: thread_id.into(),
            timestamp: "2026-09-28T00:00:00Z".into(),
            source: codex_protocol::protocol::SessionSource::Exec,
            history_mode: codex_protocol::protocol::ThreadHistoryMode::PanngV1,
            cwd: std::env::current_dir().unwrap(),
            ..Default::default()
        },
        git: None,
    };
    codex_history::PanngSessionHeader::new(meta, branch())
        .unwrap()
        .as_bytes()
        .to_vec()
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

fn delivered_journal() -> (Vec<u8>, OperationRecord) {
    let mut bytes = header();
    bytes.extend(line(&snapshot(0, initial_runtime()), 1));
    let prepared = prepared();
    let ready = prepared
        .ready(
            ResultId::new(id(88)).unwrap(),
            ExecutionOutcome::Succeeded,
            StoredResult::inline(
                ResultEncoding::CodexResponseEnvelopeV1,
                "{\"output\":\"historical-result\"}".into(),
            )
            .unwrap(),
        )
        .unwrap();
    let delivered = ready.in_context(CommitId::new(id(103)).unwrap()).unwrap();
    for (sequence, expected, revision, record) in [
        (1, None, 0, prepared),
        (2, Some(0), 1, ready),
        (3, Some(1), 2, delivered.clone()),
    ] {
        bytes.extend(line(
            &commit(
                sequence,
                RuntimeMutation::delta(vec![
                    OperationChange::new(expected, revision, record).unwrap(),
                ])
                .unwrap(),
            ),
            sequence + 1,
        ));
    }
    (bytes, delivered)
}
fn target(bytes: &[u8], last_ordinal: u64) -> PanngProjectionTarget {
    let thread_id = ThreadId::from_string(&id(9)).unwrap();
    PanngProjectionTarget {
        thread_id,
        branch: branch(),
        end: HistoryPosition {
            thread_id,
            end_ordinal_exclusive: last_ordinal + 1,
            end_byte_offset: bytes.len() as u64,
        },
    }
}
fn retirement(sequence: u64) -> PanngCommit {
    commit(
        sequence,
        RuntimeMutation::snapshot(
            RuntimeSnapshot::new(ExecutionEpoch::new(id(900)).unwrap(), Vec::new()).unwrap(),
            SnapshotReason::RetireDelivered,
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn real_file_preserves_delivered_membership_after_retirement_without_reading_past_ack() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("history.jsonl");
    let (mut bytes, delivered) = delivered_journal();
    bytes.extend(line(&retirement(4), 5));
    let cut = target(&bytes, 5);
    bytes.extend(b"complete corrupt unacknowledged line\n");
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        find_panng_operation(
            path.parent().unwrap(),
            &path,
            cut.clone(),
            &delivered.identity().id
        )
        .await
        .unwrap(),
        Some(delivered)
    );
    assert_eq!(
        find_panng_operation(
            path.parent().unwrap(),
            &path,
            cut,
            &OperationId::new(id(999)).unwrap()
        )
        .await
        .unwrap(),
        None
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn full_prefix_lookup_rejects_invalid_transition_even_when_later_anchor_is_valid() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("history.jsonl");
    let mut bytes = header();
    bytes.extend(line(&snapshot(0, initial_runtime()), 1));
    bytes.extend(line(
        &commit(
            1,
            RuntimeMutation::delta(vec![OperationChange::new(None, 0, prepared()).unwrap()])
                .unwrap(),
        ),
        2,
    ));
    // Structurally empty anchor is valid in isolation, but cannot retire this Prepared operation.
    bytes.extend(line(&retirement(2), 3));
    std::fs::write(&path, &bytes).unwrap();
    assert!(
        find_panng_operation(
            path.parent().unwrap(),
            &path,
            target(&bytes, 3),
            &prepared().identity().id
        )
        .await
        .is_err()
    );
    let (mut bytes, _) = delivered_journal();
    bytes.extend(b"{invalid complete record}\n");
    bytes.extend(line(&retirement(5), 6));
    std::fs::write(&path, &bytes).unwrap();
    assert!(
        find_panng_operation(
            path.parent().unwrap(),
            &path,
            target(&bytes, 6),
            &prepared().identity().id
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn lookup_rejects_reused_retired_identity_and_nonboundary_target() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("history.jsonl");
    let (mut bytes, delivered) = delivered_journal();
    bytes.extend(line(&retirement(4), 5));
    let mut identity = prepared().identity().clone();
    identity.owner_epoch = ExecutionEpoch::new(id(900)).unwrap();
    let reused = OperationRecord::prepared(identity, prepared().descriptor().clone());
    bytes.extend(line(
        &commit(
            5,
            RuntimeMutation::delta(vec![OperationChange::new(None, 0, reused).unwrap()]).unwrap(),
        ),
        6,
    ));
    std::fs::write(&path, &bytes).unwrap();
    assert!(
        find_panng_operation(
            path.parent().unwrap(),
            &path,
            target(&bytes, 6),
            &delivered.identity().id
        )
        .await
        .is_err()
    );
    let mut partial = target(&bytes, 6);
    partial.end.end_byte_offset -= 1;
    assert!(
        find_panng_operation(
            path.parent().unwrap(),
            &path,
            partial,
            &delivered.identity().id
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn fork_frozen_ancestor_preserves_retired_results_without_later_parent_bytes() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let parent_path = sessions.join(format!("rollout-2026-09-28T00-00-00-{}.jsonl", id(9)));
    let (mut parent_bytes, delivered) = delivered_journal();
    parent_bytes.extend(line(&retirement(4), 5));
    let parent_cut = target(&parent_bytes, 5).end;
    parent_bytes.extend(b"complete corrupt bytes outside frozen parent cut\n");
    std::fs::write(&parent_path, &parent_bytes).unwrap();
    let child_id = ThreadId::from_string(&id(70)).unwrap();
    let child_branch = BranchId::new(id(71)).unwrap();
    let mut meta = PanngSessionHeader::from_slice(&header())
        .unwrap()
        .session_meta()
        .clone();
    meta.meta.id = child_id;
    meta.meta.session_id = child_id.into();
    meta.meta.forked_from_id = Some(parent_cut.thread_id);
    meta.meta.history_base = Some(parent_cut);
    let mut checkpoint = host_checkpoint();
    checkpoint.parent_cut = Some(parent_cut);
    for item in &mut checkpoint.companions {
        if let RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(settings)) = item {
            settings.thread_id = Some(child_id);
        }
    }
    let root = PanngCommit::new(PanngCommitData {
        version: 1,
        branch: child_branch.clone(),
        commit_id: CommitId::new(id(72)).unwrap(),
        sequence: 0,
        previous_sequence: None,
        host_effects: Vec::new(),
        checkpoint: Some(checkpoint),
        mutation: RuntimeMutation::snapshot(
            RuntimeSnapshot::new(ExecutionEpoch::new(id(73)).unwrap(), Vec::new()).unwrap(),
            SnapshotReason::Fork,
        )
        .unwrap(),
    })
    .unwrap();
    let mut child_bytes = PanngSessionHeader::new(meta, child_branch.clone())
        .unwrap()
        .as_bytes()
        .to_vec();
    child_bytes.extend(line(&root, 1));
    let child_path = sessions.join(format!("rollout-2026-09-28T00-00-00-{child_id}.jsonl"));
    std::fs::write(&child_path, &child_bytes).unwrap();
    let target = PanngProjectionTarget {
        thread_id: child_id,
        branch: child_branch,
        end: HistoryPosition {
            thread_id: child_id,
            end_ordinal_exclusive: 2,
            end_byte_offset: child_bytes.len() as u64,
        },
    };
    assert_eq!(
        find_panng_operation(home.path(), &child_path, target, &delivered.identity().id)
            .await
            .unwrap(),
        Some(delivered)
    );
    assert_eq!(std::fs::read(parent_path).unwrap(), parent_bytes);
}
