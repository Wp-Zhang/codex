//! Exclusive prepare → durable → publish transactions shared by bootstrap and the future host.
//! Dropping a lease fences this runtime; only a strict cold reopen can recover its authority.
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::bail;
use codex_history::PanngCommit;
use codex_history::PanngCommitData;
use codex_history::PanngHostCheckpoint;
use codex_history::RolloutItem;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::CommitId;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::RuntimeSnapshot;
use codex_panng_runtime::SnapshotReason;
use codex_thread_store::DurableRecordPosition;
use codex_thread_store::ExperimentalPanngReconcile;
use codex_thread_store::LiveThread;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct HostCommitCoordinator(Arc<Mutex<Slot>>, Arc<tokio::sync::Semaphore>);

enum Slot {
    Ready(CommittedState),
    Leased,
    Fenced { candidate: Option<Box<PanngCommit>> },
}
struct CommittedState {
    branch: BranchId,
    sequence: u64,
    runtime: RuntimeSnapshot,
    acknowledged: Option<DurableRecordPosition>,
    checkpoint_ack: Option<DurableRecordPosition>,
}
struct Prepared {
    commit: PanngCommit,
    next_runtime: RuntimeSnapshot,
}

/// Owns the entire preparation interval, including input-order reservations on a candidate.
/// No caller may mutate live history before `commit_and_publish` invokes its callback.
pub(crate) struct HostTransaction {
    owner: HostCommitCoordinator,
    state: Option<CommittedState>,
    commit_id: CommitId,
    prepared: Option<Prepared>,
    published: bool,
    failed: bool,
    _lease: tokio::sync::OwnedSemaphorePermit,
}

impl HostCommitCoordinator {
    pub(crate) fn created(
        initial: &PanngCommit,
        receipt: DurableRecordPosition,
    ) -> anyhow::Result<Self> {
        let runtime = initial
            .mutation()
            .snapshot_state()
            .ok_or_else(|| anyhow::anyhow!("Panng creation lacks initial runtime snapshot"))?
            .clone();
        Ok(Self(
            Arc::new(Mutex::new(Slot::Ready(CommittedState {
                branch: initial.branch().clone(),
                sequence: initial.sequence(),
                runtime,
                acknowledged: Some(receipt),
                checkpoint_ack: Some(receipt),
            }))),
            Arc::new(tokio::sync::Semaphore::new(/*permits*/ 1)),
        ))
    }

    pub(crate) fn recovered(
        recovered: &codex_rollout::RecoveredPanngSuffix,
    ) -> anyhow::Result<Self> {
        let sequence = recovered
            .commits
            .last()
            .ok_or_else(|| anyhow::anyhow!("Panng recovery lacks a checkpoint suffix"))?
            .sequence();
        Ok(Self(
            Arc::new(Mutex::new(Slot::Ready(CommittedState {
                branch: recovered.branch.clone(),
                sequence,
                runtime: recovered.runtime.clone(),
                // A read cut does not grant durable append authority.
                acknowledged: None,
                checkpoint_ack: None,
            }))),
            Arc::new(tokio::sync::Semaphore::new(/*permits*/ 1)),
        ))
    }

    /// Acquire before deriving a candidate so concurrent settings/history share one owner.
    pub(crate) async fn begin_transaction(&self) -> anyhow::Result<HostTransaction> {
        let lease = Arc::clone(&self.1).acquire_owned().await?;
        let commit_id = CommitId::new(Uuid::now_v7().to_string())?;
        let state = {
            let mut slot = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("Panng coordinator poisoned"))?;
            match &*slot {
                Slot::Leased => bail!("Panng host transaction is already active"),
                Slot::Fenced { candidate } => bail!(
                    "Panng coordinator fenced; strict reopen required (candidate {:?})",
                    candidate.as_ref().map(|commit| commit.commit_id().as_str())
                ),
                Slot::Ready(_) => {}
            }
            match std::mem::replace(&mut *slot, Slot::Leased) {
                Slot::Ready(state) => state,
                Slot::Leased | Slot::Fenced { .. } => {
                    bail!("Panng coordinator lost transaction ownership")
                }
            }
        };
        Ok(HostTransaction {
            owner: self.clone(),
            state: Some(state),
            commit_id,
            prepared: None,
            published: false,
            failed: false,
            _lease: lease,
        })
    }
}

impl HostTransaction {
    /// Reserved once, before a delivery mutation can name its enclosing commit.
    pub(crate) fn commit_id(&self) -> &CommitId {
        &self.commit_id
    }

    pub(crate) fn branch(&self) -> anyhow::Result<&BranchId> {
        self.state
            .as_ref()
            .map(|state| &state.branch)
            .ok_or_else(|| anyhow::anyhow!("Panng transaction has no state"))
    }

    /// Duplicate admission may release a lease only before a canonical candidate is prepared.
    pub(crate) fn cancel_unprepared(mut self) -> anyhow::Result<()> {
        if self.prepared.is_some() || self.failed {
            bail!("Panng prepared or failed transaction cannot be cancelled as a no-op");
        }
        let state = self
            .state
            .take()
            .ok_or_else(|| anyhow::anyhow!("Panng transaction has no state"))?;
        {
            let mut slot = self
                .owner
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("Panng coordinator poisoned"))?;
            if !matches!(*slot, Slot::Leased) {
                bail!("Panng transaction lost no-op ownership");
            }
            *slot = Slot::Ready(state);
        }
        self.published = true;
        Ok(())
    }

    pub(crate) fn runtime(&self) -> anyhow::Result<&RuntimeSnapshot> {
        self.state
            .as_ref()
            .map(|state| &state.runtime)
            .ok_or_else(|| anyhow::anyhow!("Panng transaction has no state"))
    }

    pub(crate) fn acknowledged(&self) -> anyhow::Result<DurableRecordPosition> {
        self.state
            .as_ref()
            .and_then(|state| state.acknowledged)
            .ok_or_else(|| anyhow::anyhow!("Panng transaction has no durable ACK"))
    }

    pub(crate) fn checkpoint_due(&self) -> anyhow::Result<bool> {
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Panng transaction has no state"))?;
        let (Some(ack), Some(checkpoint)) = (state.acknowledged, state.checkpoint_ack) else {
            return Ok(false);
        };
        Ok(ack
            .end
            .end_ordinal_exclusive
            .saturating_sub(checkpoint.end.end_ordinal_exclusive)
            >= 64
            || ack
                .end
                .end_byte_offset
                .saturating_sub(checkpoint.end.end_byte_offset)
                >= 2 * 1024 * 1024)
    }

    pub(crate) fn needs_resume_fence(&self) -> anyhow::Result<bool> {
        self.state
            .as_ref()
            .map(|state| state.acknowledged.is_none())
            .ok_or_else(|| anyhow::anyhow!("Panng transaction has no state"))
    }

    /// Freezes exactly one candidate. The store retains its own exact-byte encoding for reconcile.
    pub(crate) fn prepare(
        &mut self,
        mutation: RuntimeMutation,
        checkpoint: Option<PanngHostCheckpoint>,
        host_effects: Vec<RolloutItem>,
    ) -> anyhow::Result<()> {
        let result = (|| {
            if self.failed {
                bail!("Panng transaction preparation is fenced");
            }
            if self.prepared.is_some() {
                bail!("Panng transaction candidate is already prepared");
            }
            let state = self
                .state
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Panng transaction has no state"))?;
            if state.acknowledged.is_none()
                && mutation.snapshot_reason() != Some(SnapshotReason::ResumeFence)
            {
                bail!("cold Panng transaction requires ResumeFence before host effects");
            }
            let next_runtime =
                mutation.apply(&state.branch, Some((&state.branch, &state.runtime)))?;
            let commit = PanngCommit::new(PanngCommitData {
                version: 1,
                branch: state.branch.clone(),
                commit_id: self.commit_id().clone(),
                sequence: state
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Panng sequence overflow"))?,
                previous_sequence: Some(state.sequence),
                mutation,
                host_effects,
                checkpoint,
            })
            .map_err(anyhow::Error::msg)?;
            self.prepared = Some(Prepared {
                commit,
                next_runtime,
            });
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// The publication callback must install its prepared live candidate and queue notifications.
    /// Errors, cancellation, or a dropped callback permanently fence this coordinator; a durable
    /// candidate may already exist, so callers must never retry it with a new identity.
    pub(crate) async fn commit_and_publish<F, Fut>(
        mut self,
        live: &LiveThread,
        publish: F,
    ) -> anyhow::Result<DurableRecordPosition>
    where
        F: FnOnce(DurableRecordPosition) -> Fut,
        Fut: Future<Output = anyhow::Result<()>>,
    {
        if self.failed {
            bail!("Panng transaction preparation failed");
        }
        let prepared = self
            .prepared
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Panng transaction was not prepared"))?;
        let receipt = match live
            .experimental_panng_append(prepared.commit.clone())
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => match live
                .experimental_panng_reconcile(prepared.commit.clone())
                .await?
            {
                ExperimentalPanngReconcile::Durable(receipt) => receipt,
                ExperimentalPanngReconcile::Absent => return Err(error.into()),
            },
        };
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Panng transaction has no state"))?;
        state.sequence = prepared.commit.sequence();
        state.runtime = prepared.next_runtime.clone();
        state.acknowledged = Some(receipt);
        if prepared.commit.checkpoint().is_some() {
            state.checkpoint_ack = Some(receipt);
        }
        publish(receipt).await?;
        let state = self
            .state
            .take()
            .ok_or_else(|| anyhow::anyhow!("Panng transaction lost state before publication"))?;
        {
            let mut slot = self
                .owner
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("Panng coordinator poisoned"))?;
            if !matches!(*slot, Slot::Leased) {
                bail!("Panng transaction lost publication ownership");
            }
            *slot = Slot::Ready(state);
        }
        self.published = true;
        Ok(receipt)
    }
}

impl Drop for HostTransaction {
    fn drop(&mut self) {
        if !self.published
            && let Ok(mut slot) = self.owner.0.lock()
        {
            *slot = Slot::Fenced {
                candidate: self
                    .prepared
                    .take()
                    .map(|prepared| Box::new(prepared.commit)),
            };
        }
    }
}

#[cfg(test)]
#[path = "host_commit_tests.rs"]
mod tests;
