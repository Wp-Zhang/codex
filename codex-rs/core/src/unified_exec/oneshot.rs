//! One-shot execution over the shared unified-exec lifecycle.
//!
//! The caller never receives a resumable process. Timeout and cancellation
//! terminate the exact process handle published by the normal startup path.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use tokio::time::Duration;
use tokio::time::Instant;
use tracing::Instrument;

use super::ExecCommandRequest;
use super::UnifiedExecContext;
use super::UnifiedExecError;
use super::UnifiedExecProcess;
use super::UnifiedExecProcessManager;
use super::trace_id;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ExecCommandToolOutput;

pub(super) struct Completion<'a> {
    pub timeout: Duration,
    pub timed_out: bool,
    pub process: &'a OnceLock<Arc<UnifiedExecProcess>>,
    pub exit_watcher: Option<tokio::task::JoinHandle<()>>,
}

// Only joins existing one-shot tasks after their calling handler is aborted. No process
// ownership or execution state lives here; interactive tasks never register.
#[derive(Default)]
struct OneShotCleanup(Mutex<HashMap<String, tokio::sync::oneshot::Receiver<()>>>);

impl UnifiedExecProcessManager {
    /// Waits for the already-started one-shot task, including its terminal UI watcher.
    /// Call after joining an aborted tool handler, before publishing its aborted response.
    pub(crate) async fn join_one_shot_cleanup(
        turn: &TurnContext,
        call_id: &str,
    ) -> Result<(), UnifiedExecError> {
        let Some(cleanup) = turn.extension_data.get::<OneShotCleanup>() else {
            return Ok(());
        };
        let receiver = cleanup
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(call_id);
        if let Some(receiver) = receiver {
            receiver.await.map_err(|_| {
                UnifiedExecError::process_failed(
                    "one-shot cleanup task ended without confirming completion".into(),
                )
            })?;
        }
        Ok(())
    }

    #[tracing::instrument(
        name = "unified_exec.exec_command",
        level = "info",
        skip_all,
        fields(
            conversation.id = %context.session.thread_id,
            turn_id = trace_id(&context.step_context.turn.sub_id),
            call_id = trace_id(&context.call_id),
            unified_exec_process_id = request.process_id,
            mode = "oneshot",
            outcome = tracing::field::Empty,
        )
    )]
    pub(crate) async fn exec_command_to_completion(
        request: ExecCommandRequest,
        context: &UnifiedExecContext,
        timeout: Duration,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let context = UnifiedExecContext::new(
            Arc::clone(&context.session),
            Arc::clone(&context.step_context),
            context.cancellation_token.child_token(),
            context.call_id.clone(),
        );
        let _cancel_on_drop = context.cancellation_token.clone().drop_guard();
        let cleanup = context
            .step_context
            .turn
            .extension_data
            .get_or_init(OneShotCleanup::default);
        let call_id = context.call_id.clone();
        let (completed, completion_rx) = tokio::sync::oneshot::channel();
        let registered = {
            let mut entries = cleanup
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let std::collections::hash_map::Entry::Vacant(entry) = entries.entry(call_id.clone())
            {
                entry.insert(completion_rx);
                true
            } else {
                false
            }
        };
        if !registered {
            context
                .session
                .services
                .unified_exec_manager
                .release_process_id(request.process_id)
                .await;
            return Err(UnifiedExecError::process_failed(
                "one-shot call already has cleanup pending".into(),
            ));
        }
        let task = async move {
            let manager = &context.session.services.unified_exec_manager;
            let process_id = request.process_id;
            if Instant::now().checked_add(timeout).is_none() {
                manager.release_process_id(process_id).await;
                return (
                    Err(UnifiedExecError::process_failed(
                        "timeout_ms is too large".into(),
                    )),
                    "failed",
                );
            }

            let process = OnceLock::new();
            let mut completion = Completion {
                timeout,
                timed_out: false,
                process: &process,
                exit_watcher: None,
            };
            let mut cancelled = false;
            let mut result = {
                let mut execution =
                    Box::pin(manager.exec_command_inner(request, &context, Some(&mut completion)));
                tokio::select! {
                    biased;
                    _ = context.cancellation_token.cancelled() => {
                        if let Some(process) = process.get() {
                            if !process.has_exited()
                                && let Err(err) = process.terminate_confirmed().await
                            {
                                process.fail_and_terminate(err.to_string());
                            }
                            let _ = execution.await;
                        } else {
                            drop(execution);
                            manager.release_process_id(process_id).await;
                        }
                        cancelled = true;
                        Err(UnifiedExecError::process_failed("command cancelled".into()))
                    }
                    result = &mut execution => result,
                }
            };

            // Every result path (including cancellation and sandbox errors) waits for
            // the native terminal item before the owning tool can be considered settled.
            if let Some(watcher) = completion.exit_watcher.take()
                && let Err(error) = watcher.await
            {
                result = Err(UnifiedExecError::process_failed(format!(
                    "one-shot terminal watcher failed: {error}"
                )));
            }
            let outcome = if cancelled {
                "cancelled"
            } else if completion.timed_out {
                "timed_out"
            } else if result.is_ok() {
                "exited"
            } else {
                "failed"
            };
            let result = result.map(|mut output| {
                if completion.timed_out {
                    output.process_id = None;
                }
                output
            });
            (result, outcome)
        };
        let joined = tokio::spawn(
            async move {
                let result = task.await;
                let _ = completed.send(());
                result
            }
            .in_current_span(),
        )
        .await;
        cleanup
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&call_id);
        let (result, outcome) = match joined {
            Ok(result) => result,
            Err(err) => (
                Err(UnifiedExecError::process_failed(err.to_string())),
                "failed",
            ),
        };
        tracing::Span::current().record("outcome", outcome);
        result
    }
}

#[cfg(test)]
#[path = "oneshot_tests.rs"]
mod tests;
