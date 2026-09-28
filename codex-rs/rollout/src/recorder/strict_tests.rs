use super::*;
use codex_history::CompactedItem;
use codex_history::CompactionResumeMetadata;
use codex_history::PanngCommitData;
use codex_history::PanngHostCheckpoint;
use codex_history::RolloutItem;
use codex_panng_runtime::*;
use codex_protocol::protocol::EventMsg;
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
fn header() -> Vec<u8> {
    let thread_id = ThreadId::from_string(&id(9)).unwrap();
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

fn setup() -> (
    tempfile::TempDir,
    PathBuf,
    crate::config::RolloutConfig,
    Arc<crate::WriterLockGuard>,
) {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join(crate::SESSIONS_SUBDIR);
    std::fs::create_dir(&sessions).unwrap();
    let path = sessions.join("journal.jsonl");
    let mut bytes = header();
    bytes.extend(line(&snapshot(0, initial_runtime()), 1));
    std::fs::write(&path, bytes).unwrap();
    let config = crate::config::RolloutConfig {
        codex_home: directory.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        cwd: directory.path().to_path_buf(),
        model_provider_id: "test".into(),
        generate_memories: false,
    };
    let coordinator = Arc::new(crate::WriterLockCoordinator::new(directory.path()));
    let guard = Arc::new(
        coordinator
            .acquire(ThreadId::from_string(&id(9)).unwrap())
            .unwrap(),
    );
    (directory, path, config, guard)
}
fn fence(sequence: u64, prior: &RuntimeSnapshot, epoch: u64) -> PanngCommit {
    let operations = prior
        .operations()
        .iter()
        .map(|(revision, operation)| {
            if operation.state() == &OperationState::Prepared {
                (
                    *revision + 1,
                    operation
                        .ready(
                            ResultId::new(id(50)).unwrap(),
                            ExecutionOutcome::Indeterminate,
                            StoredResult::unknown(UnknownResultReason::ResumeFence),
                        )
                        .unwrap(),
                )
            } else {
                (*revision, operation.clone())
            }
        })
        .collect();
    commit(
        sequence,
        RuntimeMutation::snapshot(
            RuntimeSnapshot::new(ExecutionEpoch::new(id(epoch)).unwrap(), operations).unwrap(),
            SnapshotReason::ResumeFence,
        )
        .unwrap(),
    )
}
fn prepared_for(epoch: u64) -> OperationRecord {
    let mut value = serde_json::to_value(prepared()).unwrap();
    value["owner_epoch"] = json!(id(epoch));
    serde_json::from_value(value).unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn public_cold_fence_and_native_bypass_and_runtime_validation() {
    let (_directory, path, config, guard) = setup();
    let (recorder, recovered) =
        RolloutRecorder::resume_panng_with_writer_lock(&config, path.clone(), guard, branch())
            .await
            .unwrap();
    let before = std::fs::read(&path).unwrap();
    let delta = commit(1, RuntimeMutation::delta(Vec::new()).unwrap());
    assert!(matches!(
        recorder.append_panng(delta.clone()).await,
        Err(SyncRecordError::DefinitelyRejected(_))
    ));
    assert!(
        recorder
            .reconcile_panng(snapshot(0, initial_runtime()))
            .await
            .is_err()
    );
    assert!(recorder.record_canonical_items(&[]).await.is_err());
    assert!(
        recorder
            .append_synced(RolloutItem::EventMsg(EventMsg::ShutdownComplete))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let resume = fence(1, &recovered.runtime, 3);
    let receipt = recorder.append_panng(resume.clone()).await.unwrap();
    assert_eq!(
        recorder.append_panng(resume.clone()).await.unwrap(),
        receipt
    );
    assert_eq!(
        recorder.reconcile_panng(resume).await.unwrap(),
        ReconciledAppend::Durable(receipt)
    );
    let prepared = prepared_for(3);
    let ready = prepared
        .ready(
            ResultId::new(id(60)).unwrap(),
            ExecutionOutcome::Indeterminate,
            StoredResult::unknown(UnknownResultReason::ExecutorDisconnected),
        )
        .unwrap();
    let wrong = commit(
        2,
        RuntimeMutation::delta(vec![
            OperationChange::new(Some(0), 1, ready.clone()).unwrap(),
        ])
        .unwrap(),
    );
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        recorder.append_panng(wrong).await,
        Err(SyncRecordError::DefinitelyRejected(_))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    recorder
        .append_panng(commit(
            2,
            RuntimeMutation::delta(vec![OperationChange::new(None, 0, prepared).unwrap()]).unwrap(),
        ))
        .await
        .unwrap();
    recorder
        .append_panng(commit(
            3,
            RuntimeMutation::delta(vec![
                OperationChange::new(Some(0), 1, ready.clone()).unwrap(),
            ])
            .unwrap(),
        ))
        .await
        .unwrap();
    recorder.shutdown().await.unwrap();
    let restored = read_panng_suffix(std::fs::File::open(path).unwrap()).unwrap();
    assert_eq!(
        restored.runtime,
        RuntimeSnapshot::new(ExecutionEpoch::new(id(3)).unwrap(), vec![(1, ready)]).unwrap()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn exact_attempt_reconciliation_preserves_cold_fence_until_durable() {
    for stage in [
        WriteStage::BeforeWrite,
        WriteStage::PartialWrite,
        WriteStage::BeforeSync,
        WriteStage::AfterSync,
    ] {
        let (_directory, path, config, guard) = setup();
        let recovered = read_panng_suffix(std::fs::File::open(&path).unwrap()).unwrap();
        let mut strict = StrictState::recovered(&recovered).unwrap();
        strict.fault = Some(stage);
        let state = RolloutWriterState {
            writer: Some(JsonlWriter {
                file: tokio::fs::File::from_std(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .append(true)
                        .open(&path)
                        .unwrap(),
                ),
            }),
            deferred_creation: false,
            pending_items: Vec::new(),
            meta: None,
            cwd: config.cwd,
            rollout_path: path.clone(),
            ordinal_state: RolloutOrdinalState::Paginated { next: Some(2) },
            last_logged_error: None,
            synced: super::super::synced::SyncState::default(),
            strict: Some(strict),
        };
        let recorder = RolloutRecorder::start_writer(state, Some(guard), RecorderMode::Strict);
        let resume = fence(1, &recovered.runtime, 3);
        assert!(matches!(
            recorder.append_panng(resume.clone()).await,
            Err(SyncRecordError::Uncertain(_))
        ));
        assert!(recorder.flush().await.is_err());
        assert!(recorder.shutdown().await.is_err());
        let different = fence(1, &recovered.runtime, 4);
        assert!(recorder.reconcile_panng(different).await.is_err());
        assert!(
            recorder
                .append_panng(commit(2, RuntimeMutation::delta(Vec::new()).unwrap()))
                .await
                .is_err()
        );
        let outcome = recorder.reconcile_panng(resume.clone()).await.unwrap();
        if matches!(stage, WriteStage::BeforeWrite | WriteStage::PartialWrite) {
            assert_eq!(outcome, ReconciledAppend::Absent);
            assert_eq!(
                read_panng_suffix(std::fs::File::open(&path).unwrap())
                    .unwrap()
                    .runtime,
                recovered.runtime
            );
            assert!(
                recorder
                    .append_panng(commit(1, RuntimeMutation::delta(Vec::new()).unwrap()))
                    .await
                    .is_err()
            );
            recorder.append_panng(resume.clone()).await.unwrap();
        } else {
            assert!(matches!(outcome, ReconciledAppend::Durable(_)));
        }
        let bytes = std::fs::read(&path).unwrap();
        recorder.append_panng(resume).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        recorder.shutdown().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cold_repair_requires_matching_guard_home_and_branch() {
    let (directory, path, config, guard) = setup();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend(b"{\"torn");
    std::fs::write(&path, &bytes).unwrap();
    let coordinator = Arc::new(crate::WriterLockCoordinator::new(directory.path()));
    let wrong_thread = Arc::new(
        coordinator
            .acquire(ThreadId::from_string(&id(8)).unwrap())
            .unwrap(),
    );
    assert!(
        RolloutRecorder::resume_panng_with_writer_lock(
            &config,
            path.clone(),
            wrong_thread,
            branch()
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let wrong_home = tempfile::tempdir().unwrap();
    let other = Arc::new(crate::WriterLockCoordinator::new(wrong_home.path()));
    let wrong_home_guard = Arc::new(
        other
            .acquire(ThreadId::from_string(&id(9)).unwrap())
            .unwrap(),
    );
    let mut other_config = config.clone();
    other_config.codex_home = wrong_home.path().to_path_buf();
    assert!(
        RolloutRecorder::resume_panng_with_writer_lock(
            &other_config,
            path.clone(),
            wrong_home_guard.clone(),
            branch()
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(
        RolloutRecorder::resume_panng_with_writer_lock(
            &config,
            path.clone(),
            wrong_home_guard,
            branch()
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(
        RolloutRecorder::resume_panng_with_writer_lock(
            &config,
            path.clone(),
            guard.clone(),
            BranchId::new(id(8)).unwrap()
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let (recorder, recovered) =
        RolloutRecorder::resume_panng_with_writer_lock(&config, path.clone(), guard, branch())
            .await
            .unwrap();
    assert_eq!(recovered.torn_tail_bytes, 6);
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        recovered.cut.end_byte_offset
    );
    recorder.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn actual_file_lock_rejects_two_namespaces_for_the_same_inode() {
    let (_first_home, path, config, guard) = setup();
    let (recorder, _) = RolloutRecorder::resume_panng_with_writer_lock(
        &config,
        path.clone(),
        guard.clone(),
        branch(),
    )
    .await
    .unwrap();
    assert!(
        matches!(RolloutRecorder::resume_panng_with_writer_lock(&config, path.clone(), guard, branch()).await,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock)
    );
    let (_second_home, second_path, second_config, second_guard) = setup();
    std::fs::remove_file(&second_path).unwrap();
    std::fs::hard_link(&path, &second_path).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(
        matches!(RolloutRecorder::resume_panng_with_writer_lock(&second_config, second_path.clone(), second_guard.clone(), branch()).await,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let escape = second_path.with_file_name("escape.jsonl");
    std::os::unix::fs::symlink(&path, &escape).unwrap();
    assert!(
        matches!(RolloutRecorder::resume_panng_with_writer_lock(&second_config, escape, second_guard, branch()).await,
        Err(error) if error.kind() == io::ErrorKind::InvalidInput)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    recorder.shutdown().await.unwrap();
}

#[cfg(unix)]
#[path = "strict_fault_tests.rs"]
mod faults;

#[cfg(unix)]
#[path = "strict_native_boundary_tests.rs"]
mod native_boundaries;

#[cfg(unix)]
#[path = "strict_position_tests.rs"]
mod durable_position;
