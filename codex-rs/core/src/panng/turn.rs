//! Thin Panng turn driver sharing the native sampling and tool runtime.
use super::execution::DurablePool;
use crate::client::ModelClientSession;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn::run_sampling_request;
use crate::session::turn_context::TurnContext;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
#[cfg(test)]
use codex_protocol::user_input::UserInput;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// RegularTask owns completion/abort and settles the retained pool outside this cancellable task.
pub(crate) async fn run_panng_turn(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    input: Vec<TurnInput>,
    cancellation: CancellationToken,
) -> CodexResult<Option<String>> {
    crate::session::panng_input::PendingInputs::retain(&turn, input);
    let input = Vec::new();
    let mut client = session.services.model_client.new_session();
    run_loop(session, turn, input, &mut client, cancellation).await
}

async fn run_loop(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    input: Vec<TurnInput>,
    client: &mut ModelClientSession,
    cancellation: CancellationToken,
) -> CodexResult<Option<String>> {
    if turn.config.features.enabled(Feature::CodeMode) {
        return Err(CodexErr::InvalidRequest(
            "Panng cannot be combined with code mode".into(),
        ));
    }
    if turn.extension_data.get::<DurablePool>().is_some() {
        return Err(CodexErr::InvalidRequest(
            "Panng turn already has an execution owner".into(),
        ));
    }
    if cancellation.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if turn
        .turn_timing_state
        .started_at_unix_secs()
        .await
        .is_none()
    {
        turn.turn_timing_state
            .mark_turn_started(std::time::Instant::now())
            .await;
    }
    let first_step = session
        .capture_step_context(Arc::clone(&turn), &cancellation)
        .await?;
    session
        .commit_panng_step(
            &first_step,
            Vec::new(),
            vec![RolloutItem::EventMsg(EventMsg::TurnStarted(
                TurnStartedEvent {
                    turn_id: turn.sub_id.clone(),
                    root_turn_id: Some(
                        turn.turn_metadata_state
                            .root_turn_id()
                            .unwrap_or_else(|| turn.sub_id.clone()),
                    ),
                    trace_id: turn.trace_id.clone(),
                    started_at: turn.turn_timing_state.started_at_unix_secs().await,
                    model_context_window: turn.model_context_window(),
                    collaboration_mode_kind: turn.mode(),
                },
            ))],
        )
        .await?;
    if crate::hook_runtime::run_pending_session_start_hooks_checked(&session, &turn).await? {
        return Ok(None);
    }
    if session
        .commit_panng_inputs(&first_step, input, Vec::new())
        .await?
    {
        return Ok(None);
    }
    turn.extension_data.insert(DurablePool::new());
    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
    let pool = turn
        .extension_data
        .get::<DurablePool>()
        .ok_or_else(|| CodexErr::Fatal("missing Panng execution owner".into()))?;
    let mut step = first_step;
    let mut retried_context_limit = false;
    loop {
        if cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        pool.maintain_epoch(&session, &step, &cancellation).await?;
        if crate::session::context_window::context_window_token_status(&session, &turn)
            .await
            .token_limit_reached
        {
            compact_context(&session, &step, pool.as_ref(), client, &cancellation).await?;
        }
        let input = session
            .clone_history()
            .await
            .for_prompt(&step.settings.model_info.input_modalities);
        let sampled = run_sampling_request(
            Arc::clone(&session),
            Arc::clone(&step),
            Arc::clone(&turn.extension_data),
            Arc::clone(&tracker),
            client,
            input,
            cancellation.child_token(),
        )
        .await;
        let (response, _) = match sampled {
            Err(error)
                if matches!(
                    error.details(),
                    codex_protocol::error::CodexErrorDetails::ContextWindowExceeded
                ) && !retried_context_limit =>
            {
                retried_context_limit = true;
                compact_context(&session, &step, pool.as_ref(), client, &cancellation).await?;
                continue;
            }
            result => result?,
        };
        retried_context_limit = false;
        crate::hook_runtime::drain_async_hook_results_checked(
            &session, &turn, /*before_user_prompt*/ false,
        )
        .await?;
        let pending = if let Some(state) = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await
        {
            session
                .input_queue
                .take_pending_input_for_turn_state(state.as_ref())
                .await
        } else {
            Vec::new()
        };
        let pending_is_empty = pending.is_empty();
        crate::session::panng_input::PendingInputs::retain(&turn, pending);
        if !response.needs_follow_up && pending_is_empty {
            return Ok(response.last_agent_message);
        }
        step = session
            .capture_step_context(Arc::clone(&turn), &cancellation)
            .await?;
        if session
            .commit_panng_inputs(&step, Vec::new(), Vec::new())
            .await?
        {
            return Ok(response.last_agent_message);
        }
    }
}

async fn compact_context(
    session: &Arc<Session>,
    step: &Arc<crate::session::step_context::StepContext>,
    pool: &DurablePool,
    client: &mut ModelClientSession,
    cancellation: &CancellationToken,
) -> CodexResult<()> {
    use codex_async_utils::OrCancelExt;
    pool.drain(session, cancellation).await?;
    let world_state = Arc::new(session.build_world_state_for_step(step).await?);
    crate::session::turn::run_auto_compact(
        session,
        Arc::clone(step),
        /*fallback_step_context*/ None,
        client,
        crate::compact::InitialContextInjection::BeforeLastUserMessage {
            world_state,
            step_context: Arc::clone(step),
        },
        codex_analytics::CompactionReason::ContextLimit,
        codex_analytics::CompactionPhase::MidTurn,
    )
    .or_cancel(cancellation)
    .await??;
    Ok(())
}

/// Internal host integration adapter. Public task execution uses its existing terminal owner.
#[cfg(test)]
pub(crate) async fn run_probe_turn(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    input: String,
    client: &mut ModelClientSession,
    cancellation: CancellationToken,
) -> CodexResult<Option<String>> {
    let result = run_loop(
        Arc::clone(&session),
        Arc::clone(&turn),
        vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: input,
                text_elements: Vec::new(),
            }],
            client_id: None,
            metadata: crate::session::UserInputMetadata {
                acceptance_order: None,
                origin: codex_history::UserInputOrigin::User,
            },
        }],
        client,
        cancellation,
    )
    .await;
    let last_agent_message = match result {
        Ok(message) => message,
        Err(error) => {
            if let Some(pool) = turn.extension_data.get::<DurablePool>() {
                pool.stop(&session).await?;
            }
            return Err(error);
        }
    };
    let started_at = turn.turn_timing_state.started_at_unix_secs().await;
    let (completed_at, duration_ms, _) = turn
        .turn_timing_state
        .complete_profile_and_duration_ms()
        .await;
    session
        .finish_panng_terminal(
            &turn,
            EventMsg::TurnComplete(codex_protocol::protocol::TurnCompleteEvent {
                turn_id: turn.sub_id.clone(),
                last_agent_message: last_agent_message.clone(),
                error: None,
                started_at,
                completed_at,
                duration_ms,
                time_to_first_token_ms: turn.turn_timing_state.time_to_first_token_ms().await,
            }),
        )
        .await;
    if let Some(error) = turn.terminal_error.lock().await.as_ref() {
        return Err(CodexErr::Fatal(error.message.clone()));
    }
    Ok(last_agent_message)
}
