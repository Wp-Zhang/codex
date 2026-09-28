//! Opt-in experiment restricted to the host-provided panng_probe test tool.
//! This is not a durable executor and is intentionally absent from CLI/config registration.

pub(crate) mod execution;
pub(crate) mod host_replay;
pub(crate) mod stream;
pub(crate) mod turn;

use crate::context::ContextualUserFragment;
use crate::context::PanngCompletion;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::stream_events_utils::InFlightFuture;
use crate::stream_events_utils::mark_thread_memory_mode_polluted_if_external_context;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_history::ResponseItemEnvelope;
use codex_panng_runtime::Kind;
use codex_panng_runtime::Machine;
use codex_protocol::error::CodexErr;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "probes_tests.rs"]
mod probes_tests;

#[cfg(test)]
#[path = "admission_tests.rs"]
mod admission_tests;

#[cfg(test)]
#[path = "advance_tests.rs"]
mod advance_tests;

/// Host-only experimental opt-in. Install as a turn lifecycle contributor in a mock session.
/// Only the host-provided panng_probe test tool participates; checkpoints/recovery are tested separately in panng-runtime.
#[derive(Default)]
pub struct PanngPrototype;

impl TurnLifecycleContributor for PanngPrototype {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            input.turn_store.insert(Pool::new());
        })
    }
    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(pool) = input.turn_store.get::<Pool>()
                && let Err(error) = pool.stop().await
            {
                tracing::error!(%error, "prototype stop failed");
            }
        })
    }
    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(pool) = input.turn_store.get::<Pool>()
                && let Err(error) = pool.stop().await
            {
                tracing::error!(%error, "prototype abort cleanup failed");
            }
        })
    }
}

type Completion = (String, Result<ResponseItemEnvelope, CodexErr>);
struct Pending {
    step: Arc<StepContext>,
    acknowledged: bool,
}
struct State {
    machine: Machine,
    pending: BTreeMap<String, Pending>,
    futures: FuturesUnordered<BoxFuture<'static, Completion>>,
}

pub(crate) struct Pool {
    state: Mutex<State>,
    advancement: Semaphore,
    pub(crate) cancellation: CancellationToken,
    pub(crate) execution_gate: Arc<RwLock<()>>,
}

impl Pool {
    fn new() -> Self {
        Self {
            state: Mutex::new(State {
                machine: Machine::new(/*epoch*/ 1),
                pending: BTreeMap::new(),
                futures: FuturesUnordered::new(),
            }),
            advancement: Semaphore::new(/*permits*/ 1),
            cancellation: CancellationToken::new(),
            execution_gate: Arc::new(RwLock::new(())),
        }
    }

    /// Admits a side-effect-free probe before constructing its executor future.
    /// Construction may spawn work, so rejected calls must never invoke the factory.
    /// This is only in-memory admission; real tools still require a durable commit first.
    pub(crate) async fn submit(
        &self,
        id: String,
        step: Arc<StepContext>,
        cancellation: &CancellationToken,
        dispatch: impl FnOnce() -> InFlightFuture<'static>,
    ) -> Result<(), CodexErr> {
        let mut state = self.state.lock().await;
        if self.cancellation.is_cancelled() || cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        if self.advancement.available_permits() == 0 {
            return Err(CodexErr::Fatal(
                "prototype admission during advancement is unsupported".into(),
            ));
        }
        if state.pending.len() >= 64 || id.len() > 128 {
            return Err(CodexErr::Fatal("prototype call limit exceeded".into()));
        }
        state
            .machine
            .admit(&id, Kind::Job)
            .map_err(|err| CodexErr::Fatal(err.into()))?;
        state.pending.insert(
            id.clone(),
            Pending {
                step,
                acknowledged: false,
            },
        );
        // No await between admission and construction. Cancellation after admission
        // is handled by the executor's child token, rather than a rejected factory.
        let future = dispatch();
        state
            .futures
            .push(async move { (id, future.await) }.boxed());
        Ok(())
    }

    /// Wait for useful work, consume currently ready results, acknowledge remaining calls once.
    /// Returning true means results were delivered and sampling must continue even without new calls.
    pub(crate) async fn advance(
        &self,
        session: &Arc<Session>,
        cancellation: &CancellationToken,
    ) -> Result<bool, CodexErr> {
        // One sampling coordinator owns advancement. The permit detects misuse without
        // keeping the state lock held while waiting for tools or committing history.
        let _advancement = self.advancement.try_acquire().map_err(|_| {
            CodexErr::Fatal("concurrent prototype advancement is unsupported".into())
        })?;
        let mut futures = {
            let mut state = self.state.lock().await;
            if self.cancellation.is_cancelled() || cancellation.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            if state.futures.is_empty() {
                return Ok(false);
            }
            std::mem::take(&mut state.futures)
        };
        let first = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
            _ = self.cancellation.cancelled() => return Err(CodexErr::TurnAborted),
            item = futures.next() => item,
        };
        let mut ready = Vec::new();
        if let Some(item) = first {
            ready.push(item);
        }
        while let Some(Some(item)) = futures.next().now_or_never() {
            ready.push(item);
        }
        {
            let mut state = self.state.lock().await;
            if self.cancellation.is_cancelled() || cancellation.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            state.futures = futures;
        }
        // Publish each successful result before inspecting the next one. A later
        // fatal result must not discard an earlier result already marked delivered.
        for (id, result) in ready {
            let mut envelope = result?;
            let pending = {
                let mut state = self.state.lock().await;
                if self.cancellation.is_cancelled() || cancellation.is_cancelled() {
                    return Err(CodexErr::TurnAborted);
                }
                let pending = state
                    .pending
                    .remove(&id)
                    .ok_or_else(|| CodexErr::Fatal("missing prototype call".into()))?;
                state
                    .machine
                    .complete(&id, /*epoch*/ 1, "completed".into())
                    .map_err(|err| CodexErr::Fatal(err.into()))?;
                state
                    .machine
                    .deliver(&id)
                    .map_err(|err| CodexErr::Fatal(err.into()))?;
                pending
            };
            mark_thread_memory_mode_polluted_if_external_context(
                session,
                &pending.step.turn,
                &envelope.item,
            )
            .await;
            if pending.acknowledged {
                // The host probe returns a tiny text result. Real/multimodal tools are not admitted.
                let output = serde_json::to_string(&envelope.item)
                    .map_err(|err| CodexErr::Fatal(err.to_string()))?;
                envelope.item = ContextualUserFragment::into(PanngCompletion::new(id, output)?);
            }
            session
                .record_annotated_conversation_items(
                    &pending.step.turn,
                    &pending.step.settings.model_info,
                    vec![envelope],
                )
                .await;
        }
        let acknowledgements = {
            let mut state = self.state.lock().await;
            if self.cancellation.is_cancelled() || cancellation.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            let mut acknowledgements = Vec::new();
            for (id, pending) in &mut state.pending {
                if pending.acknowledged {
                    continue;
                }
                let item = ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some(id.clone()),
                    name: None,
                    namespace: None,
                    output: FunctionCallOutputPayload::from_text(format!(
                        "Operation {id} is still running; its completion will arrive automatically."
                    )),
                    internal_chat_message_metadata_passthrough: None,
                };
                pending.acknowledged = true;
                acknowledgements.push((Arc::clone(&pending.step), item));
            }
            acknowledgements
        };
        for (step, item) in acknowledgements {
            if self.cancellation.is_cancelled() || cancellation.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            session
                .record_conversation_items(&step.turn, &step.settings.model_info, &[item])
                .await;
        }
        Ok(true)
    }

    pub(crate) async fn stop(&self) -> Result<(), CodexErr> {
        self.cancellation.cancel();
        // Wake a waiting coordinator, then let any in-progress history publication
        // finish before returning. Do not hold state while awaiting this fence.
        let _advancement =
            self.advancement.acquire().await.map_err(|error| {
                CodexErr::Fatal(format!("prototype stop fence failed: {error}"))
            })?;
        let mut state = self.state.lock().await;
        state.machine.begin_close();
        state.futures.clear();
        state.pending.clear();
        state
            .machine
            .settle()
            .map_err(|error| CodexErr::Fatal(error.into()))
    }
}
