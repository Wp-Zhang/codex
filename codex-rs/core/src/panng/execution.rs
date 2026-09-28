//! Durable cross-step execution for finite native tools and the host test probe.
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::stream_events_utils::InFlightFuture;
use crate::tools::router::ToolCall;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_panng_runtime::CallId;
use codex_panng_runtime::ExecutionOutcome;
use codex_panng_runtime::LogicalStepId;
use codex_panng_runtime::OperationChange;
use codex_panng_runtime::OperationId;
use codex_panng_runtime::OperationIdentity;
use codex_panng_runtime::OperationRecord;
use codex_panng_runtime::OperationState;
use codex_panng_runtime::ResultId;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::RuntimeSnapshot;
use codex_panng_runtime::StoredResult;
use codex_panng_runtime::TurnId;
use codex_panng_runtime::UnknownResultReason;
use codex_protocol::error::CodexErr;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
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

#[path = "execution_encoding.rs"]
mod encoding;
#[path = "execution_maintenance.rs"]
mod maintenance;
#[path = "result_reader.rs"]
pub(crate) mod result_reader;
#[path = "execution_ui.rs"]
mod ui;
use encoding::completion_envelope;
use encoding::dispatch_descriptor;
use encoding::persist_result;

type Completion = (OperationId, Result<ResponseItemEnvelope, CodexErr>);
/// Created by the sampling caller outside transport retries.
pub(crate) struct PanngLogicalStep {
    pub(crate) id: LogicalStepId,
    pub(crate) epoch: codex_panng_runtime::ExecutionEpoch,
}

struct State {
    pending: BTreeMap<OperationId, Arc<StepContext>>,
    observed: BTreeMap<OperationId, Result<ResponseItemEnvelope, CodexErr>>,
    futures: FuturesUnordered<BoxFuture<'static, Completion>>,
    active_calls: BTreeMap<String, OperationId>,
    tool_ui: BTreeMap<OperationId, ItemCompletedEvent>,
    capture_error: Option<String>,
    closed: bool,
}

pub(crate) struct DurablePool {
    state: Mutex<State>,
    advancement: Semaphore,
    pub(crate) cancellation: CancellationToken,
    pub(crate) execution_gate: Arc<RwLock<()>>,
}

impl DurablePool {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                pending: BTreeMap::new(),
                observed: BTreeMap::new(),
                futures: FuturesUnordered::new(),
                active_calls: BTreeMap::new(),
                tool_ui: BTreeMap::new(),
                capture_error: None,
                closed: false,
            }),
            advancement: Semaphore::new(/*permits*/ 1),
            cancellation: CancellationToken::new(),
            execution_gate: Arc::new(RwLock::new(())),
        }
    }

    /// Called by native event producers; never acquires a journal lease or publishes.
    pub(crate) async fn capture_tool_completed(
        &self,
        event: ItemCompletedEvent,
    ) -> Result<(), CodexErr> {
        let event = crate::session::panng_events::bound_tool_completed(event);
        let mut state = self.state.lock().await;
        let result = (|| {
            if state.closed {
                return Err(fatal("tool completion arrived after its pool closed"));
            }
            let event = event?;
            let id = state
                .active_calls
                .get(&event.item.id())
                .cloned()
                .ok_or_else(|| fatal("tool completion has no admitted Panng call"))?;
            let step = state
                .pending
                .get(&id)
                .ok_or_else(|| fatal("tool completion owner has retired"))?;
            if event.turn_id != step.turn.sub_id {
                return Err(fatal("tool completion turn differs from its owner"));
            }
            if let Some(previous) = state.tool_ui.get(&id) {
                if serde_json::to_value(previous).map_err(fatal)?
                    != serde_json::to_value(&event).map_err(fatal)?
                {
                    return Err(fatal("conflicting native tool completion"));
                }
            } else {
                state.tool_ui.insert(id, event);
            }
            Ok(())
        })();
        if let Err(error) = &result {
            state.capture_error.get_or_insert_with(|| error.to_string());
            self.cancellation.cancel();
        }
        result
    }

    /// The factory may spawn synchronously, so even its construction follows the durable intent.
    pub(crate) async fn submit(
        &self,
        session: &Arc<Session>,
        step: Arc<StepContext>,
        logical_step: LogicalStepId,
        call: ToolCall,
        original: ResponseItemEnvelope,
        cancellation: &CancellationToken,
        dispatch: impl FnOnce(CancellationToken) -> InFlightFuture<'static>,
    ) -> Result<(), CodexErr> {
        let _advance = self
            .advancement
            .try_acquire()
            .map_err(|_| fatal("concurrent durable admission/advancement"))?;
        if self.cancellation.is_cancelled() || cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        let descriptor = dispatch_descriptor(&step, &call)?;
        let matches_original = match (&original.item, &call.payload) {
            (
                ResponseItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    encrypted_function_args: None,
                    ..
                },
                crate::tools::context::ToolPayload::Function {
                    arguments: dispatched,
                },
            ) => {
                call_id == &call.call_id && name == &call.tool_name.name && arguments == dispatched
            }
            (
                ResponseItem::CustomToolCall {
                    call_id,
                    name,
                    input,
                    ..
                },
                crate::tools::context::ToolPayload::Custom { input: dispatched },
            ) => call_id == &call.call_id && name == &call.tool_name.name && input == dispatched,
            _ => false,
        };
        if !matches_original {
            return Err(fatal("durable original call differs from dispatch"));
        }
        let id = OperationId::new(uuid::Uuid::new_v4().to_string()).map_err(fatal)?;
        let output = FunctionCallOutputPayload::from_text(format!(
            "Operation {} is running; its completion will arrive automatically.",
            id.as_str()
        ));
        let receipt = ResponseItemEnvelope::new(
            if matches!(
                call.payload,
                crate::tools::context::ToolPayload::Custom { .. }
            ) {
                ResponseItem::CustomToolCallOutput {
                    id: None,
                    call_id: call.call_id.clone(),
                    name: None,
                    output,
                    internal_chat_message_metadata_passthrough: None,
                }
            } else {
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some(call.call_id.clone()),
                    name: None,
                    namespace: None,
                    output,
                    internal_chat_message_metadata_passthrough: None,
                }
            },
        );
        let transaction = session
            .begin_panng_history(
                &step.turn,
                &step.settings.model_info,
                vec![original, receipt],
            )
            .await?;
        let runtime = transaction.runtime()?;
        let branch = transaction.branch()?;
        if let Some(captured) = step.turn.extension_data.get::<PanngLogicalStep>()
            && (captured.id != logical_step || &captured.epoch != runtime.execution_epoch())
        {
            transaction.cancel_unprepared()?;
            return Err(fatal(
                "stale Panng sampling step cannot admit work in a new execution epoch",
            ));
        }
        if let Some((_, existing)) = runtime.operations().iter().find(|(_, operation)| {
            let origin = operation.identity();
            &origin.origin_branch == branch
                && &origin.owner_epoch == runtime.execution_epoch()
                && origin.logical_step_id == logical_step
                && origin.call_id.as_str() == call.call_id
        }) {
            let same = existing.descriptor() == &descriptor;
            transaction.cancel_unprepared()?;
            return if same {
                Ok(())
            } else {
                Err(fatal(
                    "duplicate logical call has conflicting dispatch parameters",
                ))
            };
        }
        if self
            .state
            .lock()
            .await
            .active_calls
            .contains_key(&call.call_id)
        {
            transaction.cancel_unprepared()?;
            return Err(fatal("another active operation owns this tool call_id"));
        }
        if runtime.operations().len() >= 64 {
            transaction.cancel_unprepared()?;
            return Err(fatal("durable probe runtime operation limit reached"));
        }
        let call_id = call.call_id.clone();
        let operation = OperationRecord::prepared(
            OperationIdentity {
                id: id.clone(),
                origin_branch: branch.clone(),
                owner_epoch: runtime.execution_epoch().clone(),
                turn_id: TurnId::new(step.turn.sub_id.clone()).map_err(fatal)?,
                logical_step_id: logical_step,
                call_id: CallId::new(call.call_id).map_err(fatal)?,
                provider_response_id: None,
            },
            descriptor,
        );
        transaction
            .commit(
                RuntimeMutation::delta(vec![
                    OperationChange::new(
                        /*expected_revision*/ None, /*revision*/ 0, operation,
                    )
                    .map_err(fatal)?,
                ])
                .map_err(fatal)?,
            )
            .await?;
        let mut state = self.state.lock().await;
        state.active_calls.insert(call_id, id.clone());
        state.pending.insert(id.clone(), step);
        if self.cancellation.is_cancelled() || cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        let future = dispatch(self.cancellation.child_token());
        state
            .futures
            .push(async move { (id, future.await) }.boxed());
        Ok(())
    }

    /// Cancel execution and archive remaining observations before returning; no runtime shadow state.
    pub(crate) async fn stop(&self, session: &Arc<Session>) -> Result<(), CodexErr> {
        self.cancellation.cancel();
        let _advance = self.advancement.acquire().await.map_err(fatal)?;
        let mut futures = {
            let mut state = self.state.lock().await;
            std::mem::take(&mut state.futures)
        };
        // ToolCallRuntime's cancellation future aborts and joins its dispatch task. Poll it to
        // completion; dropping an AbortOnDrop handle alone does not await executor cleanup.
        while let Some((id, result)) = futures.next().await {
            self.state.lock().await.observed.entry(id).or_insert(result);
        }
        let ids = self
            .state
            .lock()
            .await
            .pending
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut first_error = None;
        for id in ids {
            if let Err(error) = self.deliver(session, &id).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        // Canonical Ready records survive errors. Release captured StepContexts on every exit
        // after joining, so turn -> pool -> step -> turn cannot keep the session alive.
        let mut state = self.state.lock().await;
        state.pending.clear();
        state.observed.clear();
        state.active_calls.clear();
        state.tool_ui.clear();
        state.closed = true;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn deliver(&self, session: &Arc<Session>, id: &OperationId) -> Result<bool, CodexErr> {
        let (step, result, failed, tool_ui) = {
            let state = self.state.lock().await;
            if let Some(error) = &state.capture_error {
                return Err(fatal(error));
            }
            let step = state
                .pending
                .get(id)
                .cloned()
                .ok_or_else(|| fatal("missing captured probe step"))?;
            let observed = state.observed.get(id);
            let failed = observed.is_some_and(Result::is_err);
            let result = match observed {
                Some(Ok(envelope)) => Some(envelope.clone()),
                Some(Err(_)) | None => None,
            };
            (step, result, failed, state.tool_ui.get(id).cloned())
        };
        let result = match result {
            Some(envelope) => {
                Some(persist_result(step.turn.config.codex_home.as_path(), &envelope).await?)
            }
            None => None,
        };
        let transaction = session
            .begin_panng_history(&step.turn, &step.settings.model_info, Vec::new())
            .await?;
        let (revision, operation) = find_operation(transaction.runtime()?, id)?;
        match operation.state() {
            OperationState::Prepared => {
                let (outcome, stored) = result.unwrap_or((
                    ExecutionOutcome::Indeterminate,
                    StoredResult::unknown(UnknownResultReason::UnconfirmedTermination),
                ));
                let tool_ui = match tool_ui {
                    Some(event) => Some(event),
                    None if outcome == ExecutionOutcome::Indeterminate
                        && operation.descriptor().tool().as_str() == "exec_command" =>
                    {
                        Some(ui::unconfirmed_command(session, &step, &operation).await?)
                    }
                    None => None,
                };
                let ready = operation
                    .ready(
                        ResultId::new(uuid::Uuid::new_v4().to_string()).map_err(fatal)?,
                        outcome,
                        stored,
                    )
                    .map_err(fatal)?;
                transaction
                    .commit_with_effects(
                        RuntimeMutation::delta(vec![
                            OperationChange::new(Some(revision), revision + 1, ready)
                                .map_err(fatal)?,
                        ])
                        .map_err(fatal)?,
                        tool_ui
                            .into_iter()
                            .map(|event| RolloutItem::EventMsg(EventMsg::ItemCompleted(event)))
                            .collect(),
                    )
                    .await?;
                self.state.lock().await.tool_ui.remove(id);
            }
            OperationState::Ready { .. } => transaction.cancel_unprepared()?,
            OperationState::InContext { .. } => {
                transaction.cancel_unprepared()?;
                let mut state = self.state.lock().await;
                state.pending.remove(id);
                state.observed.remove(id);
                state.tool_ui.remove(id);
                state.active_calls.retain(|_, owner| owner != id);
                return Ok(failed);
            }
            OperationState::InheritedUnknown => {
                transaction.cancel_unprepared()?;
                return Err(fatal("current executor cannot own inherited work"));
            }
        }
        // Re-read the authoritative Ready record. A previous delivery failure never rewrites it.
        let ready = session
            .begin_panng_history(&step.turn, &step.settings.model_info, Vec::new())
            .await?;
        let (_, operation) = find_operation(ready.runtime()?, id)?;
        let completion =
            completion_envelope(step.turn.config.codex_home.as_path(), &operation).await?;
        ready.cancel_unprepared()?;
        let transaction = session
            .begin_panng_history(&step.turn, &step.settings.model_info, vec![completion])
            .await?;
        let (revision, operation) = find_operation(transaction.runtime()?, id)?;
        let delivered = operation
            .in_context(transaction.commit_id().clone())
            .map_err(fatal)?;
        transaction
            .commit(
                RuntimeMutation::delta(vec![
                    OperationChange::new(Some(revision), revision + 1, delivered).map_err(fatal)?,
                ])
                .map_err(fatal)?,
            )
            .await?;
        let mut state = self.state.lock().await;
        state.pending.remove(id);
        state.observed.remove(id);
        state.tool_ui.remove(id);
        state.active_calls.retain(|_, owner| owner != id);
        Ok(failed)
    }
}

fn find_operation(
    runtime: &RuntimeSnapshot,
    id: &OperationId,
) -> Result<(u64, OperationRecord), CodexErr> {
    runtime
        .operations()
        .iter()
        .find(|(_, operation)| &operation.identity().id == id)
        .cloned()
        .ok_or_else(|| fatal("durable probe operation is missing"))
}

fn fatal(error: impl std::fmt::Display) -> CodexErr {
    CodexErr::Fatal(error.to_string())
}

#[cfg(all(test, unix))]
#[path = "execution_tests.rs"]
mod tests;
