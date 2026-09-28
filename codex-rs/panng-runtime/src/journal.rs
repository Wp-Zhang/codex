use crate::journal_ids::*;
use crate::journal_types::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum SnapshotReason {
    Initial,
    Compaction,
    ReplayBound,
    Fork,
    ResumeFence,
    /// Close a fully delivered execution window; old readers reject this capability.
    RetireDelivered,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "SnapshotWire", into = "SnapshotWire")]
pub struct RuntimeSnapshot(SnapshotWire);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SnapshotWire {
    execution_epoch: ExecutionEpoch,
    #[serde(deserialize_with = "operations")]
    operations: Vec<(u64, OperationRecord)>,
}
impl RuntimeSnapshot {
    pub fn new(
        execution_epoch: ExecutionEpoch,
        operations: Vec<(u64, OperationRecord)>,
    ) -> Result<Self, JournalError> {
        Self::try_from(SnapshotWire {
            execution_epoch,
            operations,
        })
    }
    /// Builds a fork candidate; apply it with SnapshotReason::Fork and parent provenance.
    pub fn fork(&self, execution_epoch: ExecutionEpoch) -> Result<Self, JournalError> {
        if &execution_epoch == self.execution_epoch() {
            return Err(JournalError("fork requires a fresh epoch"));
        }
        let operations = self
            .operations()
            .iter()
            .map(|(revision, operation)| {
                let revision = if operation.state() == &OperationState::Prepared {
                    revision
                        .checked_add(1)
                        .ok_or(JournalError("revision overflow"))?
                } else {
                    *revision
                };
                Ok((revision, operation.inherited()))
            })
            .collect::<Result<Vec<_>, JournalError>>()?;
        Self::new(execution_epoch, operations)
    }
    pub fn execution_epoch(&self) -> &ExecutionEpoch {
        &self.0.execution_epoch
    }
    pub fn operations(&self) -> &[(u64, OperationRecord)] {
        &self.0.operations
    }
}
impl TryFrom<SnapshotWire> for RuntimeSnapshot {
    type Error = JournalError;
    fn try_from(value: SnapshotWire) -> Result<Self, JournalError> {
        if value.operations.len() > 64 {
            return Err(JournalError("snapshot exceeds 64 operations"));
        }
        let mut previous = None;
        let mut calls = BTreeSet::new();
        let mut results = BTreeSet::new();
        for (revision, operation) in &value.operations {
            let reachable_revision = match operation.state() {
                OperationState::Prepared => 0,
                OperationState::Ready { .. } | OperationState::InheritedUnknown => 1,
                OperationState::InContext { .. } => 2,
            };
            if *revision != reachable_revision {
                return Err(JournalError(
                    "operation revision is unreachable for its state",
                ));
            }
            let identity = operation.identity();
            if previous.is_some_and(|id| id >= &identity.id) {
                return Err(JournalError("operations must be sorted and unique"));
            }
            previous = Some(&identity.id);
            if operation.state() == &OperationState::Prepared
                && identity.owner_epoch != value.execution_epoch
            {
                return Err(JournalError(
                    "prepared operation belongs to a stale execution epoch",
                ));
            }
            if !calls.insert((
                &identity.origin_branch,
                &identity.owner_epoch,
                &identity.logical_step_id,
                &identity.call_id,
            )) {
                return Err(JournalError("duplicate logical call identity"));
            }
            if operation
                .state()
                .result_id()
                .is_some_and(|id| !results.insert(id))
            {
                return Err(JournalError(
                    "result identity reused by different operations",
                ));
            }
        }
        if serde_json::to_vec(&value)
            .map_err(|_| JournalError("snapshot serialization failed"))?
            .len()
            > 16 * 1024 * 1024
        {
            return Err(JournalError("snapshot exceeds 16 MiB"));
        }
        Ok(Self(value))
    }
}
impl From<RuntimeSnapshot> for SnapshotWire {
    fn from(value: RuntimeSnapshot) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub enum RuntimeMutation {
    Delta {
        changes: Vec<OperationChange>,
    },
    Snapshot {
        state: RuntimeSnapshot,
        reason: SnapshotReason,
    },
}
#[derive(Deserialize)]
#[serde(remote = "RuntimeMutation", deny_unknown_fields)]
enum MutationWire {
    Delta {
        #[serde(deserialize_with = "changes")]
        changes: Vec<OperationChange>,
    },
    Snapshot {
        state: RuntimeSnapshot,
        reason: SnapshotReason,
    },
}
impl<'de> Deserialize<'de> for RuntimeMutation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mutation = MutationWire::deserialize(deserializer)?;
        mutation.validate().map_err(serde::de::Error::custom)?;
        Ok(mutation)
    }
}
impl RuntimeMutation {
    pub fn delta(changes: Vec<OperationChange>) -> Result<Self, JournalError> {
        let mutation = Self::Delta { changes };
        mutation.validate()?;
        Ok(mutation)
    }
    pub fn snapshot(state: RuntimeSnapshot, reason: SnapshotReason) -> Result<Self, JournalError> {
        let mutation = Self::Snapshot { state, reason };
        mutation.validate()?;
        Ok(mutation)
    }
    pub fn snapshot_state(&self) -> Option<&RuntimeSnapshot> {
        match self {
            Self::Snapshot { state, .. } => Some(state),
            Self::Delta { .. } => None,
        }
    }
    pub fn snapshot_reason(&self) -> Option<SnapshotReason> {
        match self {
            Self::Snapshot { reason, .. } => Some(*reason),
            Self::Delta { .. } => None,
        }
    }
    pub fn validate(&self) -> Result<(), JournalError> {
        match self {
            Self::Delta { changes } => {
                if changes.len() > 32 {
                    return Err(JournalError("delta exceeds 32 changes"));
                }
                let mut ids = BTreeSet::new();
                if changes
                    .iter()
                    .any(|change| !ids.insert(&change.operation().identity().id))
                {
                    return Err(JournalError("delta changes an operation more than once"));
                }
                if serde_json::to_vec(self)
                    .map_err(|_| JournalError("delta serialization failed"))?
                    .len()
                    > 256 * 1024
                {
                    return Err(JournalError("delta exceeds 256 KiB"));
                }
            }
            Self::Snapshot { state, reason } => {
                if *reason == SnapshotReason::RetireDelivered && !state.operations().is_empty() {
                    return Err(JournalError(
                        "retirement checkpoint must have no active operations",
                    ));
                }
                if *reason == SnapshotReason::Initial && !state.operations().is_empty() {
                    return Err(JournalError("initial checkpoint must be empty"));
                }
                // A recovery anchor may be validated without replaying its preceding
                // checkpoint. These reasons must independently prove no executable intent
                // survived the fork or resume fence, even if its epoch looks current.
                if matches!(reason, SnapshotReason::Fork | SnapshotReason::ResumeFence)
                    && state
                        .operations()
                        .iter()
                        .any(|(_, operation)| operation.state() == &OperationState::Prepared)
                {
                    return Err(JournalError(
                        "fork and resume checkpoints cannot retain prepared operations",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Validates against the prior branch checkpoint and returns a candidate without mutating it.
    /// The host must commit that candidate before publication or execution. Only an explicit
    /// RetireDelivered checkpoint may remove operations, after all delivery is complete.
    /// Hosts must fence admission by the captured epoch and preserve historical result lookup.
    pub fn apply(
        &self,
        branch: &BranchId,
        prior: Option<(&BranchId, &RuntimeSnapshot)>,
    ) -> Result<RuntimeSnapshot, JournalError> {
        self.validate()?;
        match self {
            Self::Delta { changes } => {
                let (old_branch, old) = prior.ok_or(JournalError("delta requires a checkpoint"))?;
                if old_branch != branch {
                    return Err(JournalError("delta cannot change branch"));
                }
                let mut operations: BTreeMap<_, _> = old
                    .operations()
                    .iter()
                    .map(|(revision, record)| {
                        (record.identity().id.clone(), (*revision, record.clone()))
                    })
                    .collect();
                for change in changes {
                    let operation = change.operation();
                    let identity = operation.identity();
                    match (operations.get(&identity.id), change.expected_revision()) {
                        (None, None) => {
                            if &identity.origin_branch != branch
                                || &identity.owner_epoch != old.execution_epoch()
                            {
                                return Err(JournalError(
                                    "new intent must belong to current branch and epoch",
                                ));
                            }
                        }
                        (Some((revision, previous)), Some(expected)) => {
                            if *revision != expected {
                                return Err(JournalError("operation revision conflict"));
                            }
                            if !operation.follows(previous) {
                                return Err(JournalError(
                                    "conflicting provenance, descriptor, or state transition",
                                ));
                            }
                            if previous.state() == &OperationState::Prepared
                                && (&identity.origin_branch != branch
                                    || &identity.owner_epoch != old.execution_epoch())
                            {
                                return Err(JournalError("completion does not own execution"));
                            }
                        }
                        (None, Some(_)) | (Some(_), None) => {
                            return Err(JournalError("operation revision conflict"));
                        }
                    }
                    operations.insert(identity.id.clone(), (change.revision(), operation.clone()));
                }
                RuntimeSnapshot::new(
                    old.execution_epoch().clone(),
                    operations.into_values().collect(),
                )
            }
            Self::Snapshot { state, reason } => {
                let Some((old_branch, old)) = prior else {
                    if *reason != SnapshotReason::Initial || !state.operations().is_empty() {
                        return Err(JournalError("initial checkpoint must be empty"));
                    }
                    return Ok(state.clone());
                };
                match reason {
                    SnapshotReason::Initial => {
                        return Err(JournalError(
                            "initial checkpoint cannot replace prior state",
                        ));
                    }
                    SnapshotReason::Compaction | SnapshotReason::ReplayBound => {
                        if branch != old_branch || state != old {
                            return Err(JournalError(
                                "checkpoint cannot rewrite or prune runtime state",
                            ));
                        }
                    }
                    SnapshotReason::RetireDelivered => {
                        if branch != old_branch
                            || state.execution_epoch() == old.execution_epoch()
                            || !state.operations().is_empty()
                            || old.operations().iter().any(|(_, operation)| {
                                !matches!(operation.state(), OperationState::InContext { .. })
                                    || &operation.identity().owner_epoch == state.execution_epoch()
                            })
                        {
                            return Err(JournalError(
                                "retirement requires a fresh same-branch epoch after all delivery",
                            ));
                        }
                    }
                    SnapshotReason::Fork | SnapshotReason::ResumeFence => {
                        let fork = *reason == SnapshotReason::Fork;
                        if (branch != old_branch) != fork
                            || state.execution_epoch() == old.execution_epoch()
                            || old.operations().iter().any(|(_, operation)| {
                                &operation.identity().owner_epoch == state.execution_epoch()
                            })
                            || (fork
                                && old.operations().iter().any(|(_, operation)| {
                                    &operation.identity().origin_branch == branch
                                }))
                            || state.operations().len() != old.operations().len()
                        {
                            return Err(JournalError("invalid branch or execution fence"));
                        }
                        for ((revision, next), (previous_revision, previous)) in
                            state.operations().iter().zip(old.operations())
                        {
                            if previous.state() == &OperationState::Prepared {
                                if Some(*revision) != previous_revision.checked_add(1) {
                                    return Err(JournalError(
                                        "fenced operation revision must advance once",
                                    ));
                                }
                                if fork {
                                    if next != &previous.inherited() {
                                        return Err(JournalError(
                                            "fork must strip execution ownership",
                                        ));
                                    }
                                } else if !next.follows(previous)
                                    || !matches!(next.state(), OperationState::Ready {
                                    outcome: ExecutionOutcome::Indeterminate, result, ..
                                } if result.unknown_reason() == Some(UnknownResultReason::ResumeFence))
                                {
                                    return Err(JournalError(
                                        "resume must preserve provenance and record typed unknown outcome",
                                    ));
                                }
                            } else if revision != previous_revision || next != previous {
                                return Err(JournalError("fence cannot change observed results"));
                            }
                        }
                    }
                }
                Ok(state.clone())
            }
        }
    }
}

fn bounded_vec<'de, D, T, const LIMIT: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T, const LIMIT: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> serde::de::Visitor<'de> for Visitor<T, LIMIT> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {LIMIT} records")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element()? {
                if values.len() == LIMIT {
                    return Err(serde::de::Error::custom("too many runtime records"));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Visitor::<T, LIMIT>(std::marker::PhantomData))
}
fn operations<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<(u64, OperationRecord)>, D::Error> {
    bounded_vec::<D, _, 64>(deserializer)
}
fn changes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<OperationChange>, D::Error> {
    bounded_vec::<D, _, 32>(deserializer)
}

#[cfg(test)]
#[path = "retire_delivered_tests.rs"]
mod retire_delivered_tests;
