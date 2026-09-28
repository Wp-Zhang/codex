use super::*;
use codex_history::CompactedItem;
use codex_history::CompactionResumeMetadata;
use codex_history::PanngCommitData;
use codex_history::PanngHostCheckpoint;
use codex_history::RolloutItem;
use codex_panng_runtime::*;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::json;

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

#[cfg(unix)]
#[tokio::test]
async fn publishes_complete_initial_checkpoint_and_fresh_writer_without_clobbering() {
    let home = tempfile::tempdir().unwrap();
    let config = crate::RolloutConfig {
        codex_home: home.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        cwd: home.path().to_path_buf(),
        model_provider_id: "test".into(),
        generate_memories: false,
    };
    let thread_id = ThreadId::from_string(&id(9)).unwrap();
    let params = RolloutRecorderParams::new(
        thread_id,
        /*forked_from_id*/ None,
        /*parent_thread_id*/ None,
        SessionSource::Exec,
        /*thread_source*/ None,
        "test".into(),
        BaseInstructions::default(),
        Vec::new(),
    )
    .with_history_mode(ThreadHistoryMode::PanngV1)
    .with_initial_window_id("00000000-0000-7000-8000-000000000004".into());
    let initial = snapshot(0, initial_runtime());
    let plan = RolloutRecorder::prepare_panng_create(&config, params, initial.clone())
        .await
        .unwrap();
    assert!(!plan.path().exists());
    let locks = Arc::new(crate::WriterLockCoordinator::new(home.path()));
    let lock = Arc::new(locks.acquire(thread_id).unwrap());
    let (recorder, receipt) =
        RolloutRecorder::create_panng_with_writer_lock(&config, plan.clone(), lock.clone())
            .await
            .unwrap();
    let recovered = crate::read_panng_suffix(File::open(plan.path()).unwrap()).unwrap();
    assert_eq!(
        receipt,
        SyncedRecord {
            ordinal: Some(1),
            end_byte_offset: recovered.cut.end_byte_offset
        }
    );
    assert_eq!(recovered.runtime, initial_runtime());
    recorder
        .append_panng(commit(1, RuntimeMutation::delta(Vec::new()).unwrap()))
        .await
        .unwrap();
    let before = fs::read(plan.path()).unwrap();
    let error =
        match RolloutRecorder::create_panng_with_writer_lock(&config, plan.clone(), lock).await {
            Ok(_) => panic!("existing target must not be replaced"),
            Err(error) => error,
        };
    assert_eq!(
        (&error.path, &error.branch, &error.commit_id),
        (&plan.path, initial.branch(), initial.commit_id())
    );
    assert!(matches!(
        error.source,
        SyncRecordError::DefinitelyRejected(_)
    ));
    assert_eq!(fs::read(plan.path()).unwrap(), before);
    recorder.shutdown().await.unwrap();
}
