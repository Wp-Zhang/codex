//! A fork owns one new joint root and references the frozen parent's immutable history.
use super::*;
use codex_history::PanngCommit;
use codex_history::PanngCommitData;
use codex_history::PanngHostCheckpoint;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::CommitId;
use codex_panng_runtime::ExecutionEpoch;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::RuntimeSnapshot;
use codex_panng_runtime::SnapshotReason;
use uuid::Uuid;

pub(super) fn initial_fork_checkpoint(
    thread_id: ThreadId,
    window_id: String,
    configuration: &SessionConfiguration,
    source: &[RolloutItem],
    parent_cut: HistoryPosition,
    truncation_policy: codex_protocol::openai_models::TruncationPolicyConfig,
    multi_agent_version: Option<MultiAgentVersion>,
) -> anyhow::Result<PanngCommit> {
    let mut runtime: Option<RuntimeSnapshot> = None;
    let mut parent_branch = None;
    for item in source {
        if let RolloutItem::PanngCommit(commit) = item {
            runtime = Some(match &runtime {
                Some(previous) => commit.mutation().apply(
                    commit.branch(),
                    Some((
                        parent_branch
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("missing fork branch"))?,
                        previous,
                    )),
                )?,
                None => commit
                    .mutation()
                    .snapshot_state()
                    .ok_or_else(|| anyhow::anyhow!("fork source lacks checkpoint"))?
                    .clone(),
            });
            parent_branch = Some(commit.branch().clone());
        }
    }
    let runtime = runtime.ok_or_else(|| anyhow::anyhow!("fork source lacks runtime"))?;
    let inherited = runtime.fork(ExecutionEpoch::new(Uuid::now_v7().to_string())?)?;
    let branch = BranchId::new(Uuid::now_v7().to_string())?;
    let mutation = RuntimeMutation::snapshot(inherited, SnapshotReason::Fork)?;
    mutation.apply(
        &branch,
        Some((
            parent_branch
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("fork source lacks branch"))?,
            &runtime,
        )),
    )?;
    let replay = Session::reconstruct_rollout_history(
        &configuration.session_source,
        ThreadHistoryMode::PanngV1,
        truncation_policy.into(),
        source,
    );
    let mut companions = Vec::new();
    if let Some(world) = replay.world_state_baseline {
        companions.push(RolloutItem::WorldState(WorldStateItem::full(
            world.into_object(),
        )));
    }
    if let Some(context) = replay.reference_context_item {
        companions.push(RolloutItem::TurnContext(context));
    }
    companions.push(RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
        codex_protocol::protocol::ThreadSettingsAppliedEvent {
            thread_id: Some(thread_id),
            thread_settings: configuration.thread_settings_snapshot(&configuration.environments),
        },
    )));
    let mcp_resource_origins = source.iter().rev().find_map(|item| match item {
        RolloutItem::PanngCommit(commit) => commit
            .checkpoint()
            .and_then(|checkpoint| checkpoint.compacted.mcp_resource_origins.clone()),
        _ => None,
    });
    PanngCommit::new(PanngCommitData {
        version: 1,
        branch,
        commit_id: CommitId::new(Uuid::now_v7().to_string())?,
        sequence: 0,
        previous_sequence: None,
        mutation,
        host_effects: Vec::new(),
        checkpoint: Some(PanngHostCheckpoint {
            compacted: CompactedItem {
                message: String::new(),
                replacement_history: Some(replay.history),
                retained_context: Some(replay.retained_context),
                guardian_history: replay.guardian_history,
                mcp_resource_origins,
                window_number: Some(0),
                first_window_id: Some(window_id.clone()),
                previous_window_id: None,
                window_id: Some(window_id),
                compaction_response_id: None,
                latest_token_usage_record: Session::last_token_usage_record_from_rollout(source),
                resume_metadata: Some(CompactionResumeMetadata {
                    multi_agent_version,
                    last_started_turn_id: replay.last_started_turn_id,
                    previous_turn_settings: replay.previous_turn_settings,
                }),
            },
            companions,
            parent_cut: Some(parent_cut),
        }),
    })
    .map_err(anyhow::Error::msg)
}
