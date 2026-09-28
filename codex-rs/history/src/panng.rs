//! Versioned atomic host/runtime records. Decoding does not grant execution authority.

use crate::CompactedItem;
use crate::RolloutItem;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::CommitId;
use codex_panng_runtime::OperationState;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::SnapshotReason;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde::de::Error as _;
use serde::ser::Error as _;

const MAX_DELTA_BYTES: usize = 256 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const MAX_EFFECTS: usize = 64;

/// Construction input; only `PanngCommit::new` yields a validated record.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PanngCommitData {
    pub version: u32,
    pub branch: BranchId,
    pub commit_id: CommitId,
    pub sequence: u64,
    pub previous_sequence: Option<u64>,
    pub mutation: RuntimeMutation,
    pub host_effects: Vec<RolloutItem>,
    pub checkpoint: Option<PanngHostCheckpoint>,
}

/// All host recovery constituents share the outer commit's atomic cut.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PanngHostCheckpoint {
    pub compacted: CompactedItem,
    pub companions: Vec<RolloutItem>,
    pub parent_cut: Option<HistoryPosition>,
}

/// Structurally validated storage record. The writer must additionally validate the
/// transition against prior state and the host projector's item/turn cardinality.
#[derive(Clone, Debug, JsonSchema)]
pub struct PanngCommit(PanngCommitData);

impl PanngCommit {
    pub fn new(data: PanngCommitData) -> Result<Self, String> {
        if data.version != 1 {
            return Err(format!("unsupported Panng commit version {}", data.version));
        }
        if data.sequence.checked_sub(1) != data.previous_sequence {
            return Err("Panng predecessor is not the immediately prior sequence".into());
        }
        data.mutation
            .validate()
            .map_err(|error| error.to_string())?;
        // Only a delta introduces a delivery. Snapshot records retain the original
        // delivery identity across compaction, fork, and resume fencing.
        if let RuntimeMutation::Delta { changes } = &data.mutation {
            for change in changes {
                if let OperationState::InContext {
                    delivery_commit, ..
                } = change.operation().state()
                    && delivery_commit != &data.commit_id
                {
                    return Err("Panng delivery must reference its enclosing commit".into());
                }
            }
        }
        let initial = matches!(
            data.mutation.snapshot_reason(),
            Some(SnapshotReason::Initial | SnapshotReason::Fork)
        );
        if initial != (data.sequence == 0) {
            return Err("Panng initial/fork snapshot must begin sequence zero".into());
        }
        if data.host_effects.len() > MAX_EFFECTS {
            return Err("too many Panng host effects".into());
        }
        match (&data.checkpoint, data.mutation.snapshot_state()) {
            (Some(checkpoint), Some(_)) => {
                if !data.host_effects.is_empty()
                    || checkpoint.compacted.replacement_history.is_none()
                    || checkpoint.compacted.resume_metadata.is_none()
                    || checkpoint.compacted.window_number.is_none()
                {
                    return Err(
                        "Panng snapshot requires complete host context and no append effects"
                            .into(),
                    );
                }
                if data.mutation.snapshot_reason() == Some(SnapshotReason::Fork)
                    && checkpoint.parent_cut.is_none()
                {
                    return Err("Panng fork snapshot requires its frozen parent cut".into());
                }
                if data.mutation.snapshot_reason() != Some(SnapshotReason::Fork)
                    && checkpoint.parent_cut.is_some()
                {
                    return Err("only a fork snapshot may introduce a parent cut".into());
                }
                if checkpoint.compacted.first_window_id.is_none()
                    || checkpoint.compacted.window_id.is_none()
                {
                    return Err("Panng snapshot requires explicit context window identities".into());
                }
                for id in [
                    &checkpoint.compacted.first_window_id,
                    &checkpoint.compacted.window_id,
                    &checkpoint.compacted.previous_window_id,
                ]
                .into_iter()
                .flatten()
                {
                    let parsed = uuid::Uuid::parse_str(id)
                        .map_err(|_| "invalid Panng context window UUID")?;
                    if parsed.get_version_num() != 7 || parsed.to_string() != *id {
                        return Err(
                            "Panng context window identity must be a canonical UUIDv7".into()
                        );
                    }
                }
                let mut seen = [false; 3];
                let mut previous_slot = None;
                for item in &checkpoint.companions {
                    let slot = match item {
                        RolloutItem::WorldState(world) if world.full => 0,
                        RolloutItem::TurnContext(_) => 1,
                        RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(_)) => 2,
                        _ => return Err("invalid Panng checkpoint companion".into()),
                    };
                    if previous_slot.is_some_and(|previous| previous >= slot) {
                        return Err("Panng checkpoint companions must follow WorldState, TurnContext, settings order".into());
                    }
                    previous_slot = Some(slot);
                    if std::mem::replace(&mut seen[slot], true) {
                        return Err("duplicate Panng checkpoint companion".into());
                    }
                }
                if !seen[2] {
                    return Err("Panng snapshot requires current thread settings".into());
                }
            }
            (None, None) => {
                for item in &data.host_effects {
                    let allowed = match item {
                        RolloutItem::ResponseItem(_)
                        | RolloutItem::WorldState(_)
                        | RolloutItem::TurnContext(_)
                        | RolloutItem::TokenUsageRecord(_)
                        | RolloutItem::RetainedContext(_)
                        | RolloutItem::SecurityRiskScore(_) => true,
                        RolloutItem::EventMsg(event) => matches!(
                            event,
                            EventMsg::ItemCompleted(_)
                                | EventMsg::TurnStarted(_)
                                | EventMsg::TurnComplete(_)
                                | EventMsg::TurnAborted(_)
                                | EventMsg::ThreadSettingsApplied(_)
                                | EventMsg::ContextCompacted(_)
                                | EventMsg::PlanUpdate(_)
                        ),
                        _ => false,
                    };
                    if !allowed {
                        return Err("invalid Panng append effect".into());
                    }
                }
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err("Panng runtime and host snapshots must share one commit".into());
            }
        }
        let limit = if data.checkpoint.is_some() {
            MAX_SNAPSHOT_BYTES
        } else {
            MAX_DELTA_BYTES
        };
        let commit = Self(data);
        let bytes = commit
            .canonical_bytes()
            .map_err(|error| error.to_string())?;
        if bytes.len() > limit {
            return Err("Panng commit exceeds encoded size limit".into());
        }
        let restored: PanngCommitData =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if Self(restored)
            .canonical_bytes()
            .map_err(|error| error.to_string())?
            != bytes
        {
            return Err("Panng effects contain non-restorable host metadata".into());
        }
        Ok(commit)
    }

    /// Apply raw byte limits before allocating a decoded JSON graph.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err("Panng input exceeds record limit".into());
        }
        serde_json::from_slice(bytes).map_err(|error| error.to_string())
    }

    /// Deterministic payload bytes, excluding the outer ordinal and timestamp.
    pub fn canonical_bytes(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(&self.canonical_value()?)
    }

    pub fn commit_id(&self) -> &CommitId {
        &self.0.commit_id
    }
    pub fn branch(&self) -> &BranchId {
        &self.0.branch
    }
    pub fn sequence(&self) -> u64 {
        self.0.sequence
    }
    pub fn previous_sequence(&self) -> Option<u64> {
        self.0.previous_sequence
    }
    pub fn mutation(&self) -> &RuntimeMutation {
        &self.0.mutation
    }
    pub fn is_checkpoint(&self) -> bool {
        self.0.checkpoint.is_some()
    }
    pub fn checkpoint(&self) -> Option<&PanngHostCheckpoint> {
        self.0.checkpoint.as_ref()
    }
    pub fn host_effects(&self) -> &[RolloutItem] {
        &self.0.host_effects
    }

    fn canonical_value(&self) -> serde_json::Result<serde_json::Value> {
        Ok(sorted_json(serde_json::to_value(&self.0)?))
    }
}

fn sorted_json(mut value: serde_json::Value) -> serde_json::Value {
    // Sort explicitly even when another workspace consumer enables preserve_order.
    let mut pending = vec![&mut value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::Object(map) => {
                map.sort_keys();
                pending.extend(map.values_mut());
            }
            serde_json::Value::Array(items) => pending.extend(items.iter_mut()),
            _ => {}
        }
    }
    value
}

impl Serialize for PanngCommit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.canonical_value()
            .map_err(S::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PanngCommit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = sorted_json(serde_json::Value::deserialize(deserializer)?);
        let data = serde_json::from_value(raw.clone()).map_err(D::Error::custom)?;
        let commit = Self::new(data).map_err(D::Error::custom)?;
        if commit.canonical_value().map_err(D::Error::custom)? != raw {
            return Err(D::Error::custom(
                "Panng record would lose or reinterpret host fields",
            ));
        }
        Ok(commit)
    }
}

#[cfg(test)]
#[path = "panng_tests.rs"]
mod tests;
