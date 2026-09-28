//! Scope validation before the public input path changes settings or input order.
use super::*;
use codex_protocol::turn_input::TurnInput as SubmittedInput;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputRequest;

pub(super) async fn validate(
    session: &Session,
    request: &TurnInputRequest,
    mode: &TurnInputMode,
) -> CodexResult<()> {
    let text_input = matches!(&request.input,
        SubmittedInput::UserInput { content, .. }
        if !content.is_empty() && content.iter().all(|item| matches!(item, UserInput::Text { .. })));
    if !text_input
        || !matches!(
            mode,
            TurnInputMode::StartOrSteer | TurnInputMode::StartIfIdle | TurnInputMode::Steer { .. }
        )
        || request.start.parent_turn_id.is_some()
        || request.start.root_turn_id.is_some()
        || request.start.cyber_access_program.is_some()
    {
        return Err(CodexErr::InvalidRequest(
            "Panng accepts text input with user-owned start options".into(),
        ));
    }
    let updates = super::thread_settings::prepare_update(request.thread_settings.clone());
    let state = session.state.lock().await;
    session
        .apply_session_settings(&state.session_configuration, &updates)
        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
    if session.enabled(Feature::CodeMode) {
        return Err(CodexErr::InvalidRequest(
            "Panng cannot be combined with code mode".into(),
        ));
    }
    // Retain requested configuration even when a current catalog masks its effect.
    // Atomic idle admission precedes its durable installation in update_panng_settings_if.
    Ok(())
}

#[cfg(test)]
#[path = "public_admission_tests.rs"]
mod tests;
