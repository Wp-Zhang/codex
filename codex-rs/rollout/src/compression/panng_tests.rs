use super::*;
#[cfg(unix)]
use codex_history::CompactedItem;
#[cfg(unix)]
use codex_history::CompactionResumeMetadata;
#[cfg(unix)]
use codex_history::PanngCommit;
#[cfg(unix)]
use codex_history::PanngCommitData;
#[cfg(unix)]
use codex_history::PanngHostCheckpoint;
#[cfg(unix)]
use codex_panng_runtime::*;
#[cfg(unix)]
use codex_protocol::models::BaseInstructions;
#[cfg(unix)]
use codex_protocol::protocol::ThreadHistoryMode;
use pretty_assertions::assert_eq;
use serde_json::json;
#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
fn id(number: u64) -> String {
    format!("00000000-0000-4000-8000-{number:012x}")
}
#[cfg(unix)]
fn branch() -> BranchId {
    BranchId::new(id(1)).unwrap()
}
#[cfg(unix)]
fn initial_runtime() -> RuntimeSnapshot {
    RuntimeSnapshot::new(ExecutionEpoch::new(id(2)).unwrap(), Vec::new()).unwrap()
}
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
async fn cold_worker_preserves_strict_journals_in_both_roots_and_still_compresses_native()
-> anyhow::Result<()> {
    for archived in [false, true] {
        let home = TempDir::new()?;
        let config = RolloutConfig {
            codex_home: home.path().to_path_buf(),
            sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
            cwd: home.path().to_path_buf(),
            model_provider_id: "test".into(),
            generate_memories: false,
        };
        let thread_id = ThreadId::from_string(&id(9))?;
        let params = RolloutRecorderParams::new(
            thread_id,
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "compression-test".into(),
            BaseInstructions::default(),
            Vec::new(),
        )
        .with_history_mode(ThreadHistoryMode::PanngV1)
        .with_initial_window_id("00000000-0000-7000-8000-000000000004".into());
        let initial = snapshot(0, initial_runtime());
        let plan = RolloutRecorder::prepare_panng_create(&config, params, initial.clone()).await?;
        let mut strict_path = plan.path().to_path_buf();
        {
            let locks = Arc::new(crate::WriterLockCoordinator::new(home.path()));
            let guard = Arc::new(locks.acquire(thread_id)?);
            let (recorder, _) =
                RolloutRecorder::create_panng_with_writer_lock(&config, plan, guard).await?;
            recorder.shutdown().await?;
        }
        if archived {
            let destination =
                archived_rollout_path(home.path(), "2025-01-03T12-00-00", Uuid::parse_str(&id(9))?);
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::rename(&strict_path, &destination)?;
            strict_path = destination;
        }
        let native_id = Uuid::from_u128(501);
        let native_path = rollout_path(home.path(), "2025-01-03T12-00-00", native_id);
        write_rollout(
            &native_path,
            ThreadId::from_string(&native_id.to_string())?,
            "native still compresses",
        )?;
        set_old_mtime(&native_path)?;
        set_old_mtime(&strict_path)?;
        let before = fs::read(&strict_path)?;
        worker::run(home.path().to_path_buf(), Startup).await?;
        assert_eq!(
            (
                native_path.exists(),
                compressed_rollout_path(&native_path).exists()
            ),
            (false, true)
        );
        assert_eq!(
            (
                strict_path.exists(),
                compressed_rollout_path(&strict_path).exists()
            ),
            (true, false)
        );
        assert_eq!(fs::read(&strict_path)?, before);
        let locks = Arc::new(crate::WriterLockCoordinator::new(home.path()));
        let guard = Arc::new(locks.acquire(thread_id)?);
        let (recorder, recovered) =
            RolloutRecorder::resume_panng_with_writer_lock(&config, strict_path, guard, branch())
                .await?;
        assert_eq!(recovered.runtime, initial_runtime());
        assert_eq!(
            recovered.commits[0].canonical_bytes()?,
            initial.canonical_bytes()?
        );
        recorder.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn cold_worker_does_not_treat_ambiguous_format_as_native() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let mut protected = Vec::new();
    for (index, mode_fields) in [
        r#""history_mode":"panng_v1","history_mode":"legacy""#,
        r#""history_mode":"legacy","history_mode":"panng_v1""#,
        r#""history_mode":"future_journal""#,
    ]
    .into_iter()
    .enumerate()
    {
        let uuid = Uuid::from_u128(600 + index as u128);
        let path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
        write_rollout(
            &path,
            ThreadId::from_string(&uuid.to_string())?,
            "format must not default",
        )?;
        let contents = fs::read_to_string(&path)?;
        let protected_contents = contents.replacen(r#""history_mode":"legacy""#, mode_fields, 1);
        assert_ne!(
            contents, protected_contents,
            "fixture must replace the format discriminator"
        );
        fs::write(&path, protected_contents.as_bytes())?;
        set_old_mtime(&path)?;
        protected.push((path, protected_contents.into_bytes()));
    }
    worker::run(home.path().to_path_buf(), Startup).await?;
    for (path, bytes) in protected {
        assert_eq!(fs::read(&path)?, bytes);
        assert!(!compressed_rollout_path(&path).exists());
    }
    Ok(())
}

#[tokio::test]
async fn cold_worker_streams_large_native_header_before_classifying_its_format()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(700);
    let path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(
        &path,
        ThreadId::from_string(&uuid.to_string())?,
        "large native header",
    )?;
    let contents = fs::read_to_string(&path)?;
    let (header, tail) = contents.split_once('\n').unwrap();
    let mut header: serde_json::Value = serde_json::from_str(header)?;
    header["payload"]["base_instructions"] = json!({"text":"x".repeat(300 * 1024)});
    header["payload"]["history_mode"] = json!("paginated");
    header["ordinal"] = json!(0);
    let mut tail: serde_json::Value = serde_json::from_str(tail.trim_end())?;
    tail["ordinal"] = json!(1);
    fs::write(&path, format!("{header}\n{tail}\n"))?;
    set_old_mtime(&path)?;
    worker::run(home.path().to_path_buf(), Startup).await?;
    assert_eq!(
        (path.exists(), compressed_rollout_path(&path).exists()),
        (false, true)
    );
    Ok(())
}
