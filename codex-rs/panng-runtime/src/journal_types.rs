use crate::journal_ids::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DispatchDescriptor {
    tool: ToolName,
    arguments: BoundedJson,
    environment_digest: Digest,
    authorization_digest: Digest,
    catalog_digest: Digest,
}
impl DispatchDescriptor {
    pub fn new(
        tool: ToolName,
        arguments: BoundedJson,
        environment_digest: Digest,
        authorization_digest: Digest,
        catalog_digest: Digest,
    ) -> Self {
        Self {
            tool,
            arguments,
            environment_digest,
            authorization_digest,
            catalog_digest,
        }
    }
    pub fn tool(&self) -> &ToolName {
        &self.tool
    }
    pub fn arguments(&self) -> &BoundedJson {
        &self.arguments
    }
    pub fn environment_digest(&self) -> &Digest {
        &self.environment_digest
    }
    pub fn authorization_digest(&self) -> &Digest {
        &self.authorization_digest
    }
    pub fn catalog_digest(&self) -> &Digest {
        &self.catalog_digest
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ExecutionOutcome {
    Succeeded,
    Failed,
    Canceled,
    Indeterminate,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ResultEncoding {
    CodexResponseEnvelopeV1,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum UnknownResultReason {
    ResumeFence,
    ExecutorDisconnected,
    UnconfirmedTermination,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "StoredResultWire", into = "StoredResultWire")]
pub struct StoredResult(StoredResultWire);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
enum StoredResultWire {
    Inline {
        encoding: ResultEncoding,
        #[serde(deserialize_with = "inline_payload")]
        payload: String,
    },
    Artifact {
        encoding: ResultEncoding,
        sha256: Digest,
        bytes: u64,
    },
    Unknown {
        reason: UnknownResultReason,
    },
}
impl StoredResult {
    pub fn unknown(reason: UnknownResultReason) -> Self {
        Self(StoredResultWire::Unknown { reason })
    }
    pub fn unknown_reason(&self) -> Option<UnknownResultReason> {
        match &self.0 {
            StoredResultWire::Unknown { reason } => Some(*reason),
            StoredResultWire::Inline { .. } | StoredResultWire::Artifact { .. } => None,
        }
    }
    pub fn inline_payload(&self) -> Option<(ResultEncoding, &str)> {
        match &self.0 {
            StoredResultWire::Inline { encoding, payload } => Some((*encoding, payload)),
            StoredResultWire::Artifact { .. } | StoredResultWire::Unknown { .. } => None,
        }
    }
    pub fn artifact_reference(&self) -> Option<(ResultEncoding, &Digest, u64)> {
        match &self.0 {
            StoredResultWire::Artifact {
                encoding,
                sha256,
                bytes,
            } => Some((*encoding, sha256, *bytes)),
            StoredResultWire::Inline { .. } | StoredResultWire::Unknown { .. } => None,
        }
    }
    pub fn inline(encoding: ResultEncoding, payload: String) -> Result<Self, JournalError> {
        Self::try_from(StoredResultWire::Inline { encoding, payload })
    }
    pub fn artifact(
        encoding: ResultEncoding,
        sha256: Digest,
        bytes: u64,
    ) -> Result<Self, JournalError> {
        Self::try_from(StoredResultWire::Artifact {
            encoding,
            sha256,
            bytes,
        })
    }
}
impl TryFrom<StoredResultWire> for StoredResult {
    type Error = JournalError;
    fn try_from(value: StoredResultWire) -> Result<Self, JournalError> {
        match &value {
            StoredResultWire::Inline { payload, .. } => {
                if payload.len() > 4096 {
                    return Err(JournalError("inline result exceeds 4 KiB"));
                }
                serde_json::from_str::<serde_json::Value>(payload)
                    .map_err(|_| JournalError("inline result must be JSON"))?;
            }
            StoredResultWire::Artifact { bytes, .. } => {
                if *bytes > 16 * 1024 * 1024 {
                    return Err(JournalError("artifact exceeds 16 MiB"));
                }
            }
            StoredResultWire::Unknown { .. } => {}
        }
        Ok(Self(value))
    }
}
impl From<StoredResult> for StoredResultWire {
    fn from(value: StoredResult) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub enum OperationState {
    Prepared,
    Ready {
        result_id: ResultId,
        outcome: ExecutionOutcome,
        result: StoredResult,
    },
    InContext {
        result_id: ResultId,
        outcome: ExecutionOutcome,
        result: StoredResult,
        delivery_commit: CommitId,
    },
    InheritedUnknown,
}

#[derive(Deserialize)]
#[serde(remote = "OperationState", deny_unknown_fields)]
enum OperationStateWire {
    Prepared,
    Ready {
        result_id: ResultId,
        outcome: ExecutionOutcome,
        result: StoredResult,
    },
    InContext {
        result_id: ResultId,
        outcome: ExecutionOutcome,
        result: StoredResult,
        delivery_commit: CommitId,
    },
    InheritedUnknown,
}
impl OperationState {
    fn validate(&self) -> Result<(), JournalError> {
        match self {
            Self::Ready {
                outcome, result, ..
            }
            | Self::InContext {
                outcome, result, ..
            } => {
                if (*outcome == ExecutionOutcome::Indeterminate)
                    != result.unknown_reason().is_some()
                {
                    return Err(JournalError(
                        "indeterminate outcomes require a typed unknown result",
                    ));
                }
            }
            Self::Prepared | Self::InheritedUnknown => {}
        }
        Ok(())
    }
    pub(crate) fn result_id(&self) -> Option<&ResultId> {
        match self {
            Self::Ready { result_id, .. } | Self::InContext { result_id, .. } => Some(result_id),
            Self::Prepared | Self::InheritedUnknown => None,
        }
    }
}
impl<'de> Deserialize<'de> for OperationState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let state = OperationStateWire::deserialize(deserializer)?;
        state.validate().map_err(serde::de::Error::custom)?;
        Ok(state)
    }
}

/// Immutable call provenance. Digests are identities, never permission grants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OperationIdentity {
    pub id: OperationId,
    pub origin_branch: BranchId,
    pub owner_epoch: ExecutionEpoch,
    pub turn_id: TurnId,
    pub logical_step_id: LogicalStepId,
    pub call_id: CallId,
    pub provider_response_id: Option<ProviderResponseId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OperationRecord {
    #[serde(flatten)]
    identity: OperationIdentity,
    descriptor: DispatchDescriptor,
    state: OperationState,
}
impl OperationRecord {
    pub fn prepared(identity: OperationIdentity, descriptor: DispatchDescriptor) -> Self {
        Self {
            identity,
            descriptor,
            state: OperationState::Prepared,
        }
    }
    pub fn identity(&self) -> &OperationIdentity {
        &self.identity
    }
    pub fn descriptor(&self) -> &DispatchDescriptor {
        &self.descriptor
    }
    pub fn state(&self) -> &OperationState {
        &self.state
    }
    pub fn ready(
        &self,
        result_id: ResultId,
        outcome: ExecutionOutcome,
        result: StoredResult,
    ) -> Result<Self, JournalError> {
        if self.state != OperationState::Prepared {
            return Err(JournalError("only prepared operations can complete"));
        }
        let mut next = self.clone();
        next.state = OperationState::Ready {
            result_id,
            outcome,
            result,
        };
        next.state.validate()?;
        Ok(next)
    }
    pub fn in_context(&self, delivery_commit: CommitId) -> Result<Self, JournalError> {
        let OperationState::Ready {
            result_id,
            outcome,
            result,
        } = &self.state
        else {
            return Err(JournalError("only ready operations can enter context"));
        };
        let mut next = self.clone();
        next.state = OperationState::InContext {
            result_id: result_id.clone(),
            outcome: *outcome,
            result: result.clone(),
            delivery_commit,
        };
        Ok(next)
    }
    /// Resolve inherited work only as an unknown notification; this never authorizes execution.
    pub fn inherited_in_context(
        &self,
        result_id: ResultId,
        delivery_commit: CommitId,
    ) -> Result<Self, JournalError> {
        if self.state != OperationState::InheritedUnknown {
            return Err(JournalError(
                "only inherited unknown work may use fork delivery",
            ));
        }
        let mut next = self.clone();
        next.state = OperationState::InContext {
            result_id,
            delivery_commit,
            outcome: ExecutionOutcome::Indeterminate,
            result: StoredResult::unknown(UnknownResultReason::ExecutorDisconnected),
        };
        Ok(next)
    }

    pub(crate) fn inherited(&self) -> Self {
        let mut next = self.clone();
        if next.state == OperationState::Prepared {
            next.state = OperationState::InheritedUnknown;
        }
        next
    }
    pub(crate) fn follows(&self, previous: &Self) -> bool {
        if self.identity != previous.identity || self.descriptor != previous.descriptor {
            return false;
        }
        match (&previous.state, &self.state) {
            (OperationState::Prepared, OperationState::Ready { .. }) => true,
            (
                OperationState::InheritedUnknown,
                OperationState::InContext {
                    outcome: ExecutionOutcome::Indeterminate,
                    result,
                    ..
                },
            ) if result.unknown_reason() == Some(UnknownResultReason::ExecutorDisconnected) => true,
            (
                OperationState::Ready {
                    result_id: a,
                    outcome: b,
                    result: c,
                },
                OperationState::InContext {
                    result_id: x,
                    outcome: y,
                    result: z,
                    ..
                },
            ) => a == x && b == y && c == z,
            (
                OperationState::Prepared,
                OperationState::Prepared
                | OperationState::InContext { .. }
                | OperationState::InheritedUnknown,
            )
            | (
                OperationState::Ready { .. },
                OperationState::Prepared
                | OperationState::Ready { .. }
                | OperationState::InheritedUnknown,
            )
            | (
                OperationState::InContext { .. } | OperationState::InheritedUnknown,
                OperationState::Prepared
                | OperationState::Ready { .. }
                | OperationState::InContext { .. }
                | OperationState::InheritedUnknown,
            ) => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "OperationChangeWire", into = "OperationChangeWire")]
pub struct OperationChange(OperationChangeWire);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OperationChangeWire {
    expected_revision: Option<u64>,
    revision: u64,
    operation: OperationRecord,
}
impl OperationChange {
    pub fn new(
        expected_revision: Option<u64>,
        revision: u64,
        operation: OperationRecord,
    ) -> Result<Self, JournalError> {
        Self::try_from(OperationChangeWire {
            expected_revision,
            revision,
            operation,
        })
    }
    pub fn expected_revision(&self) -> Option<u64> {
        self.0.expected_revision
    }
    pub fn revision(&self) -> u64 {
        self.0.revision
    }
    pub fn operation(&self) -> &OperationRecord {
        &self.0.operation
    }
}
impl TryFrom<OperationChangeWire> for OperationChange {
    type Error = JournalError;
    fn try_from(value: OperationChangeWire) -> Result<Self, JournalError> {
        let expected = match value.expected_revision {
            None => 0,
            Some(revision) => revision
                .checked_add(1)
                .ok_or(JournalError("revision overflow"))?,
        };
        if expected != value.revision {
            return Err(JournalError("noncontiguous operation revision"));
        }
        if value.expected_revision.is_none() && value.operation.state != OperationState::Prepared {
            return Err(JournalError("new operation must be prepared"));
        }
        Ok(Self(value))
    }
}
impl From<OperationChange> for OperationChangeWire {
    fn from(value: OperationChange) -> Self {
        value.0
    }
}

fn inline_payload<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    struct Visitor;
    impl serde::de::Visitor<'_> for Visitor {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most 4 KiB of JSON")
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<String, E> {
            if value.len() > 4096 {
                return Err(E::custom("inline result exceeds 4 KiB"));
            }
            serde_json::from_str::<serde_json::Value>(value).map_err(E::custom)?;
            Ok(value.to_owned())
        }
    }
    deserializer.deserialize_str(Visitor)
}
