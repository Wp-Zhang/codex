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
