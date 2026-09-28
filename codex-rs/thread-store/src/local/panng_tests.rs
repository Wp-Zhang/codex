use super::LocalThreadStore;
use super::live_writer;
use super::test_support::test_config;
use crate::CreateThreadParams;
use crate::LiveThread;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStoreError;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::user_input::UserInput;
use codex_rollout::PanngCommit;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;

fn create_params(thread_id: ThreadId, history_mode: ThreadHistoryMode) -> CreateThreadParams {
    CreateThreadParams {
        creator_user_id: None,
        creator_account_id: None,
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "sync-test".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode,
        history_base: None,
        subagent_history_start_ordinal: None,
        initial_window_id: uuid::Uuid::now_v7().to_string(),
        runtime_workspace_roots: None,
        metadata: ThreadPersistenceMetadata {
            cwd: Some(std::env::current_dir().expect("cwd")),
            model_provider: "test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn initial(params: &CreateThreadParams) -> PanngCommit {
    let compacted = codex_rollout::CompactedItem {
        message: String::new(),
        replacement_history: Some(Vec::new()),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: Some(0),
        first_window_id: Some(params.initial_window_id.clone()),
        previous_window_id: None,
        window_id: Some(params.initial_window_id.clone()),
        compaction_response_id: None,
        latest_token_usage_record: None,
        resume_metadata: Some(codex_rollout::CompactionResumeMetadata {
            multi_agent_version: None,
            last_started_turn_id: None,
            previous_turn_settings: None,
        }),
    };
    let settings = RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(serde_json::from_value(json!({
        "thread_id":params.thread_id, "thread_settings": {
            "model":"test", "model_provider_id":"test-provider", "service_tier":null,
            "approval_policy":"never", "approvals_reviewer":"user",
            "permission_profile":codex_protocol::models::PermissionProfile::read_only(),
            "cwd":params.metadata.cwd, "reasoning_effort":null, "reasoning_summary":null,"personality":null,
            "collaboration_mode":{"mode":"default","settings":{"model":"test","reasoning_effort":null,"developer_instructions":null}},
            "disabled_plugin_ids":[]
        }
    })).unwrap()));
    PanngCommit::from_slice(&serde_json::to_vec(&json!({
        "version":1,"branch":"00000000-0000-4000-8000-000000000001",
        "commit_id":"00000000-0000-4000-8000-000000000002","sequence":0,"previous_sequence":null,
        "mutation":{"Snapshot":{"state":{"execution_epoch":"00000000-0000-4000-8000-000000000003","operations":[]},"reason":"Initial"}},
        "host_effects":[],"checkpoint":{"compacted":compacted,
            "companions":[RolloutItem::WorldState(codex_protocol::protocol::WorldStateItem::full(Default::default())),settings],"parent_cut":null}
    })).unwrap()).unwrap()
}
fn delta(effects: Vec<RolloutItem>) -> PanngCommit {
    PanngCommit::from_slice(
        &serde_json::to_vec(&json!({
            "version":1,"branch":"00000000-0000-4000-8000-000000000001",
            "commit_id":"00000000-0000-4000-8000-000000000004","sequence":1,"previous_sequence":0,
            "mutation":{"Delta":{"changes":[]}},"host_effects":effects,"checkpoint":null
        }))
        .unwrap(),
    )
    .unwrap()
}
fn visible(thread_id: ThreadId) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: "turn-1".into(),
        item: TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
            text: "persisted through strict writer".into(),
            text_elements: Vec::new(),
        }])),
        started_at_ms: Some(100),
        completed_at_ms: 200,
    }))
}

#[cfg(unix)]
#[tokio::test]
async fn real_local_create_append_reconcile_read_and_cold_resume_share_canonical_cut() {
    let home = tempfile::tempdir().unwrap();
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let store = Arc::new(LocalThreadStore::new(config, Some(state)));
    let thread_id = ThreadId::new();
    let params = create_params(thread_id, ThreadHistoryMode::PanngV1);
    let first = initial(&params);
    let (live, creation) =
        LiveThread::experimental_panng_create(store.clone(), params.clone(), first.clone())
            .await
            .unwrap();
    assert_eq!(creation.end.end_ordinal_exclusive, 2);
    let recovered = live.experimental_panng_read().await.unwrap();
    assert_eq!(
        recovered.commits[0].canonical_bytes().unwrap(),
        first.canonical_bytes().unwrap()
    );
    assert_eq!(recovered.cut.end_byte_offset, creation.end.end_byte_offset);
    let second = delta(vec![visible(thread_id)]);
    let receipt = live
        .experimental_panng_append(second.clone())
        .await
        .unwrap();
    assert_eq!(receipt.end.end_ordinal_exclusive, 3);
    assert_eq!(
        live.experimental_panng_reconcile(second.clone())
            .await
            .unwrap(),
        crate::ExperimentalPanngReconcile::Durable(receipt)
    );
    let restored = live.experimental_panng_read().await.unwrap();
    assert_eq!(
        restored
            .commits
            .iter()
            .map(PanngCommit::canonical_bytes)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        vec![
            first.canonical_bytes().unwrap(),
            second.canonical_bytes().unwrap()
        ]
    );
    let path = live_writer::rollout_path(&store, thread_id).await.unwrap();
    live.shutdown().await.unwrap();
    let (resumed, cold) = LiveThread::experimental_panng_resume(
        store.clone(),
        crate::ResumeThreadParams {
            thread_id,
            rollout_path: Some(path),
            history: None,
            include_archived: false,
            metadata: params.metadata,
        },
        first.branch().clone(),
    )
    .await
    .unwrap();
    assert_eq!(cold.cut.end_byte_offset, receipt.end.end_byte_offset);
    assert!(resumed.experimental_panng_append(second).await.is_err());
    resumed.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn ui_cardinality_is_rejected_before_any_canonical_bytes_change() {
    let home = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalThreadStore::new(test_config(home.path()), None));
    let thread_id = ThreadId::new();
    let params = create_params(thread_id, ThreadHistoryMode::PanngV1);
    let first = initial(&params);
    let (live, _) = LiveThread::experimental_panng_create(store.clone(), params, first)
        .await
        .unwrap();
    let path = live_writer::rollout_path(&store, thread_id).await.unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let invalid = delta(vec![visible(thread_id), visible(thread_id)]);
    assert!(matches!(
        live.experimental_panng_append(invalid).await,
        Err(ThreadStoreError::InvalidRequest { .. })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    live.experimental_panng_append(delta(vec![visible(thread_id)]))
        .await
        .unwrap();
    live.shutdown().await.unwrap();
}

#[tokio::test]
async fn custom_store_does_not_implicitly_support_panng_creation() {
    let params = create_params(ThreadId::new(), ThreadHistoryMode::PanngV1);
    let first = initial(&params);
    let result = LiveThread::experimental_panng_create(
        Arc::new(crate::InMemoryThreadStore::default()),
        params,
        first,
    )
    .await;
    assert!(matches!(
        result,
        Err(ThreadStoreError::Unsupported {
            operation: "experimental_panng_create"
        })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn wrong_thread_item_is_rejected_before_canonical_write() {
    let home = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalThreadStore::new(
        test_config(home.path()),
        /*state_db*/ None,
    ));
    let thread_id = ThreadId::new();
    let params = create_params(thread_id, ThreadHistoryMode::PanngV1);
    let first = initial(&params);
    let (live, initial_cut) = LiveThread::experimental_panng_create(store.clone(), params, first)
        .await
        .unwrap();
    let path = live_writer::rollout_path(&store, thread_id).await.unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let wrong_thread = delta(vec![visible(ThreadId::new())]);
    assert!(matches!(
        live.experimental_panng_append(wrong_thread).await,
        Err(ThreadStoreError::SyncedRecord {
            source: codex_rollout::SyncRecordError::DefinitelyRejected(_)
        })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        live.experimental_panng_read()
            .await
            .unwrap()
            .cut
            .end_byte_offset,
        initial_cut.end.end_byte_offset
    );
    let valid = delta(vec![visible(thread_id)]);
    let receipt = live.experimental_panng_append(valid).await.unwrap();
    assert_eq!(
        receipt.end.end_ordinal_exclusive,
        initial_cut.end.end_ordinal_exclusive + 1
    );
    live.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn archived_symlink_is_rejected_before_strict_tail_repair() {
    use std::io::Write;
    let home = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalThreadStore::new(
        test_config(home.path()),
        /*state_db*/ None,
    ));
    let thread_id = ThreadId::new();
    let params = create_params(thread_id, ThreadHistoryMode::PanngV1);
    let first = initial(&params);
    let branch = first.branch().clone();
    let (live, receipt) =
        LiveThread::experimental_panng_create(store.clone(), params.clone(), first)
            .await
            .unwrap();
    let alias = live_writer::rollout_path(&store, thread_id).await.unwrap();
    live.shutdown().await.unwrap();
    let archive_dir = home.path().join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR);
    std::fs::create_dir_all(&archive_dir).unwrap();
    let archived = archive_dir.join(alias.file_name().unwrap());
    std::fs::rename(&alias, &archived).unwrap();
    std::os::unix::fs::symlink(&archived, &alias).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&archived)
        .unwrap()
        .write_all(b"{\"torn")
        .unwrap();
    let bytes = std::fs::read(&archived).unwrap();
    let denied = crate::ResumeThreadParams {
        thread_id,
        rollout_path: Some(alias.clone()),
        history: None,
        include_archived: false,
        metadata: params.metadata.clone(),
    };
    assert!(matches!(
        LiveThread::experimental_panng_resume(store.clone(), denied, branch.clone()).await,
        Err(ThreadStoreError::InvalidRequest { .. })
    ));
    assert_eq!(
        std::fs::read(&archived).unwrap(),
        bytes,
        "archive policy precedes tail repair"
    );
    let allowed = crate::ResumeThreadParams {
        thread_id,
        rollout_path: Some(alias),
        history: None,
        include_archived: true,
        metadata: params.metadata,
    };
    let (resumed, recovered) = LiveThread::experimental_panng_resume(store, allowed, branch)
        .await
        .unwrap();
    assert_eq!(recovered.cut.end_byte_offset, receipt.end.end_byte_offset);
    assert_eq!(
        std::fs::metadata(&archived).unwrap().len(),
        receipt.end.end_byte_offset
    );
    resumed.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_git_metadata_clear_changes_only_the_sqlite_projection() {
    let home = tempfile::tempdir().unwrap();
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let store = Arc::new(LocalThreadStore::new(config, Some(state)));
    let thread_id = ThreadId::new();
    let params = create_params(thread_id, ThreadHistoryMode::PanngV1);
    let first = initial(&params);
    let (live, receipt) = LiveThread::experimental_panng_create(store.clone(), params, first)
        .await
        .unwrap();
    let path = live_writer::rollout_path(&store, thread_id).await.unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let updated = live
        .update_metadata(
            crate::ThreadMetadataPatch {
                git_info: Some(crate::GitInfoPatch {
                    sha: Some(Some("test-sha".into())),
                    branch: Some(Some("test-branch".into())),
                    origin_url: Some(None),
                }),
                ..Default::default()
            },
            /*include_archived*/ false,
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(updated.git_info).unwrap(),
        json!({
            "commit_hash":"test-sha", "branch":"test-branch",
        })
    );
    let cleared = live
        .update_metadata(
            crate::ThreadMetadataPatch {
                git_info: Some(crate::GitInfoPatch {
                    sha: Some(None),
                    branch: Some(None),
                    origin_url: Some(None),
                }),
                ..Default::default()
            },
            /*include_archived*/ false,
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(cleared.git_info).unwrap(),
        serde_json::Value::Null
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let recovered = live.experimental_panng_read().await.unwrap();
    assert_eq!(
        (recovered.cut.ordinal + 1, recovered.cut.end_byte_offset),
        (
            receipt.end.end_ordinal_exclusive,
            receipt.end.end_byte_offset
        )
    );
    live.shutdown().await.unwrap();
}

#[cfg(unix)]
#[path = "panng_projection_tests.rs"]
mod projection_tests;
