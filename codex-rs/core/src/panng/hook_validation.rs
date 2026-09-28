//! Bounded native hook context prepared for the acknowledged Panng history path.
use super::*;
use codex_history::ResponseItemEnvelope;

/// Prepare native hook fragments without installing them; the input owner may include these
/// with its user input in one acknowledged transaction. Oversized context is rejected intact.
pub(crate) fn panng_hook_context_items(
    contexts: Vec<String>,
) -> CodexResult<Vec<ResponseItemEnvelope>> {
    if contexts.len() > 16 {
        return Err(CodexErr::InvalidRequest(
            "Panng hooks exceed 16 context fragments".into(),
        ));
    }
    additional_context_messages(contexts)
        .into_iter()
        .map(|item| {
            if serde_json::to_vec(&item)
                .map_err(|error| CodexErr::Fatal(error.to_string()))?
                .len()
                > 4096
            {
                return Err(CodexErr::InvalidRequest(
                    "Panng hook context exceeds its 4 KiB serialized fragment limit".into(),
                ));
            }
            Ok(ResponseItemEnvelope::new(item))
        })
        .collect()
}

/// Hook execution remains native. The driver owns the atomic input/context commit afterward.
pub(crate) async fn inspect_pending_input_panng(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    input: &TurnInput,
) -> CodexResult<HookRuntimeOutcome> {
    let outcome = inspect_pending_input(session, turn, input).await;
    panng_hook_context_items(outcome.additional_contexts.clone())?;
    Ok(outcome)
}

/// Panng callers must propagate this error before executing a rewritten tool or publishing a result.
pub(crate) async fn record_additional_contexts_checked(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    additional_contexts: Vec<String>,
) -> CodexResult<()> {
    if turn_context.history_mode != codex_protocol::protocol::ThreadHistoryMode::PanngV1 {
        record_additional_contexts(sess, turn_context, additional_contexts).await;
        return Ok(());
    }
    let items = panng_hook_context_items(additional_contexts)?;
    if items.is_empty() {
        return Ok(());
    }
    sess.begin_panng_history(turn_context, turn_context.model_info(), items)
        .await?
        .commit(
            codex_panng_runtime::RuntimeMutation::delta(Vec::new())
                .map_err(|error| CodexErr::Fatal(error.to_string()))?,
        )
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "hook_context_tests.rs"]
mod tests;
