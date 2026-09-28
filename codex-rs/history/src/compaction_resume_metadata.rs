//! Defines resume metadata stored directly on a compaction.

use crate::RolloutItem;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::turn_input::CyberAccessProgram;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Resume metadata that is not represented by the companion records after a compaction.
///
/// Presence distinguishes compactions that explicitly persisted these values from older
/// compactions that did not. Each field is authoritative, including an intentionally absent value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CompactionResumeMetadata {
    /// Selected runtime, including when the companion turn context is absent.
    pub multi_agent_version: Option<MultiAgentVersion>,
    /// Turn identity used to admit continuations after cold resume.
    pub last_started_turn_id: Option<String>,
    pub previous_turn_settings: Option<PreviousTurnSettings>,
}

/// Previous user-turn settings used to reconstruct context changes after resume.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct PreviousTurnSettings {
    pub model: String,
    /// Historical program for this model; absence must not inherit the next turn's selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cyber_access_program: Option<CyberAccessProgram>,
    pub comp_hash: Option<String>,
    pub realtime_active: Option<bool>,
}

/// Returns the runtime version stored in a turn context or compaction resume metadata.
pub fn resume_multi_agent_version(item: &RolloutItem) -> Option<MultiAgentVersion> {
    if let Some(version) = panng_resume_multi_agent_version(item) {
        return version;
    }
    crate::host_replay_items(std::slice::from_ref(item))
        .rev()
        .find_map(resume_multi_agent_version_from_replay)
}

pub(crate) fn resume_multi_agent_version_from_replay(
    item: crate::HostReplayItem<'_>,
) -> Option<MultiAgentVersion> {
    match item {
        crate::HostReplayItem::Item(RolloutItem::TurnContext(context)) => {
            context.multi_agent_version
        }
        crate::HostReplayItem::Compacted(compacted) => compacted
            .resume_metadata
            .as_ref()
            .and_then(|metadata| metadata.multi_agent_version),
        crate::HostReplayItem::Item(_) => None,
    }
}

/// The outer Option distinguishes an authoritative clear from no Panng selection.
/// A checkpoint's frozen reference context may predate its selected runtime.
pub(crate) fn panng_resume_multi_agent_version(
    item: &RolloutItem,
) -> Option<Option<MultiAgentVersion>> {
    let RolloutItem::PanngCommit(commit) = item else {
        return None;
    };
    if let Some(checkpoint) = commit.checkpoint() {
        return checkpoint
            .compacted
            .resume_metadata
            .as_ref()
            .map(|metadata| metadata.multi_agent_version);
    }
    commit
        .host_effects()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            RolloutItem::TurnContext(context) => context.multi_agent_version.map(Some),
            _ => None,
        })
}
