//! Experimental decision and checkpoint contracts. No executor or Codex history ownership.

mod checkpoint;
mod machine;

pub use checkpoint::Checkpoint;
pub use machine::Cut;
pub use machine::Decision;
pub use machine::Entry;
pub use machine::Kind;
pub use machine::Machine;
pub use machine::Phase;
pub use machine::Status;

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod checkpoint_tests;
#[cfg(test)]
#[path = "machine_tests.rs"]
mod machine_tests;

mod journal_ids;
mod journal_types;
pub use journal_ids::BoundedJson;
pub use journal_ids::BranchId;
pub use journal_ids::CallId;
pub use journal_ids::CommitId;
pub use journal_ids::Digest;
pub use journal_ids::ExecutionEpoch;
pub use journal_ids::JournalError;
pub use journal_ids::LogicalStepId;
pub use journal_ids::OperationId;
pub use journal_ids::ProviderResponseId;
pub use journal_ids::ResultId;
pub use journal_ids::ToolName;
pub use journal_ids::TurnId;
pub use journal_types::DispatchDescriptor;
pub use journal_types::ExecutionOutcome;
pub use journal_types::OperationChange;
pub use journal_types::OperationIdentity;
pub use journal_types::OperationRecord;
pub use journal_types::OperationState;
pub use journal_types::ResultEncoding;
pub use journal_types::StoredResult;
pub use journal_types::UnknownResultReason;
