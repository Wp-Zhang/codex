use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Job,
    Resource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Pending,
    Ready(String),
    Delivered,
    Indeterminate,
    Inherited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Open,
    Closing,
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Deliver(Vec<String>),
    Wait,
    Idle,
    Settle,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub sequence: u64,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cut {
    epoch: u64,
    sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Operation {
    kind: Kind,
    status: Status,
}

/// Pure prototype state. The host must durably commit a candidate before executing effects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub(crate) epoch: u64,
    pub(crate) history_epoch: u64,
    pub(crate) sequence: u64,
    pub(crate) phase: Phase,
    pub(crate) history: Vec<Entry>,
    pub(crate) operations: BTreeMap<String, Operation>,
}

impl Machine {
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch,
            history_epoch: 0,
            sequence: 0,
            phase: Phase::Open,
            history: Vec::new(),
            operations: BTreeMap::new(),
        }
    }

    pub fn admit(&mut self, id: &str, kind: Kind) -> Result<(), &'static str> {
        if self.phase != Phase::Open || id.is_empty() || self.operations.contains_key(id) {
            return Err("closed admission or duplicate operation");
        }
        self.operations.insert(
            id.to_owned(),
            Operation {
                kind,
                status: Status::Pending,
            },
        );
        Ok(())
    }

    pub fn complete(&mut self, id: &str, epoch: u64, output: String) -> Result<(), &'static str> {
        if epoch != self.epoch {
            return Err("stale execution epoch");
        }
        let operation = self.operations.get_mut(id).ok_or("unknown operation")?;
        match &operation.status {
            Status::Pending if self.phase != Phase::Closed => {
                operation.status = Status::Ready(output)
            }
            Status::Ready(existing) if existing == &output => {}
            Status::Pending
            | Status::Ready(_)
            | Status::Delivered
            | Status::Indeterminate
            | Status::Inherited => {
                return Err("late or conflicting completion; host must archive separately");
            }
        }
        Ok(())
    }

    pub fn decide(&self) -> Decision {
        match self.phase {
            Phase::Closed => return Decision::Closed,
            Phase::Closing => return Decision::Settle,
            Phase::Open => {}
        }
        let ready: Vec<_> = self
            .operations
            .iter()
            .filter(|&(_id, op)| matches!(op.status, Status::Ready(_)))
            .map(|(id, _op)| id.clone())
            .collect();
        if !ready.is_empty() {
            Decision::Deliver(ready)
        } else if self
            .operations
            .values()
            .any(|op| op.kind == Kind::Job && op.status == Status::Pending)
        {
            Decision::Wait
        } else {
            Decision::Idle
        }
    }

    pub fn deliver(&mut self, id: &str) -> Result<String, &'static str> {
        if self.phase != Phase::Open {
            return Err("turn is closing");
        }
        let op = self.operations.get_mut(id).ok_or("unknown operation")?;
        let Status::Ready(output) = &op.status else {
            return Err("result is not ready");
        };
        let output = output.clone();
        op.status = Status::Delivered;
        Ok(output)
    }

    pub fn append(&mut self, text: String) -> Result<(), &'static str> {
        if self.phase == Phase::Closed {
            return Err("closed history");
        }
        self.sequence = self.sequence.checked_add(1).ok_or("sequence overflow")?;
        self.history.push(Entry {
            sequence: self.sequence,
            text,
        });
        Ok(())
    }

    pub fn cut(&self) -> Cut {
        Cut {
            epoch: self.history_epoch,
            sequence: self.sequence,
        }
    }

    pub fn compact(&mut self, cut: Cut, summary: String) -> Result<(), &'static str> {
        if cut.epoch != self.history_epoch
            || cut.sequence > self.sequence
            || self.phase != Phase::Open
        {
            return Err("stale compaction");
        }
        self.history_epoch = self.history_epoch.checked_add(1).ok_or("epoch overflow")?;
        let mut history = vec![Entry {
            sequence: cut.sequence,
            text: summary,
        }];
        history.extend(
            self.history
                .iter()
                .filter(|entry| entry.sequence > cut.sequence)
                .cloned(),
        );
        self.history = history;
        Ok(())
    }

    pub fn begin_close(&mut self) {
        if self.phase == Phase::Open {
            self.phase = Phase::Closing;
        }
    }

    /// Called only after the host's bounded cancellation grace. It does not cancel processes.
    pub fn settle(&mut self) -> Result<(), &'static str> {
        if self.phase == Phase::Open {
            return Err("admission is still open");
        }
        for operation in self.operations.values_mut() {
            if operation.kind == Kind::Job && operation.status == Status::Pending {
                operation.status = Status::Indeterminate;
            }
        }
        self.phase = Phase::Closed;
        Ok(())
    }

    /// Fork an already reconstructed historical checkpoint without inheriting execution rights.
    pub fn fork(&self, epoch: u64) -> Result<Self, &'static str> {
        if epoch == self.epoch {
            return Err("fork needs a fresh execution epoch");
        }
        let mut child = self.clone();
        child.epoch = epoch;
        child.history_epoch = child.history_epoch.checked_add(1).ok_or("epoch overflow")?;
        child.phase = Phase::Open;
        for operation in child.operations.values_mut() {
            if operation.status == Status::Pending {
                operation.status = Status::Inherited;
            }
        }
        Ok(child)
    }

    pub fn status(&self, id: &str) -> Option<&Status> {
        self.operations.get(id).map(|op| &op.status)
    }
    pub fn history(&self) -> &[Entry] {
        &self.history
    }
}
