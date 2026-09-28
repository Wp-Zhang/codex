//! Durable completed-item handling for the explicit Panng probe loop.
use std::sync::Arc;
use std::sync::atomic::Ordering;

use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_panng_runtime::RuntimeMutation;
use codex_protocol::config_types::ModeKind;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;

use super::execution::DurablePool;
use super::execution::PanngLogicalStep;
use crate::stream_events_utils::HandleOutputCtx;
use crate::stream_events_utils::OutputItemResult;
use crate::stream_events_utils::TurnItemContributorPolicy;
use crate::stream_events_utils::finalize_non_tool_response_item;
use crate::tools::context::ToolCallState;
use crate::tools::router::ToolRouter;
use crate::turn_timing::now_unix_timestamp_ms;

pub(crate) async fn handle_completed(
    ctx: &HandleOutputCtx,
    pool: &DurablePool,
    item: ResponseItem,
) -> Result<OutputItemResult> {
    let call = ToolRouter::build_tool_call(item.clone())
        .map_err(|error| CodexErr::Fatal(error.to_string()))?;
    if let Some(call) = call {
        if call.tool_name.is_default_namespace()
            && call.tool_name.name == super::execution::result_reader::NAME
        {
            super::execution::result_reader::read(&ctx.sess, &ctx.step_context, call, item).await?;
            return Ok(OutputItemResult {
                needs_follow_up: true,
                ..Default::default()
            });
        }
        let logical_step = ctx
            .turn_store
            .get::<PanngLogicalStep>()
            .ok_or_else(|| CodexErr::Fatal("Panng sampling step is missing".into()))?;
        let runtime = ctx.tool_runtime.clone();
        let dispatched = call.clone();
        pool.submit(
            &ctx.sess,
            Arc::clone(&ctx.step_context),
            logical_step.id.clone(),
            call,
            ResponseItemEnvelope::new(item),
            &ctx.cancellation_token,
            move |cancellation| {
                let state = Arc::new(ToolCallState::default());
                let future = runtime.handle_tool_call_with_state(
                    dispatched,
                    cancellation.clone(),
                    Arc::clone(&state),
                );
                Box::pin(async move {
                    let result = future.await?;
                    if cancellation.is_cancelled()
                        && !state.terminal_outcome_reached.load(Ordering::Acquire)
                    {
                        // Native abort output is a presentation response, not a confirmed tool result.
                        return Err(CodexErr::TurnAborted);
                    }
                    Ok(result)
                })
            },
        )
        .await?;
        return Ok(OutputItemResult {
            needs_follow_up: true,
            ..Default::default()
        });
    }
    if !matches!(
        item,
        ResponseItem::Message { .. } | ResponseItem::Reasoning { .. }
    ) {
        return Err(CodexErr::InvalidRequest(
            "Unsupported item in Panng probe execution".into(),
        ));
    }
    let finalized = finalize_non_tool_response_item(
        ctx.sess.as_ref(),
        TurnItemContributorPolicy::Skip,
        &item,
        ctx.step_context.turn.mode() == ModeKind::Plan,
    )
    .await;
    let last_agent_message = finalized
        .as_ref()
        .and_then(|value| value.facts.last_agent_message.clone());
    let mut effects: Vec<RolloutItem> = finalized
        .into_iter()
        .filter(|value| !matches!(&value.turn_item, codex_protocol::items::TurnItem::AgentMessage(message) if message.content.iter().all(|part| matches!(part, codex_protocol::items::AgentMessageContent::Text { text } if text.trim().is_empty()))))
        .map(|value| {
            let completed_at_ms = now_unix_timestamp_ms();
            RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: ctx.sess.thread_id(),
                turn_id: ctx.step_context.turn.sub_id.clone(),
                item: value.turn_item,
                started_at_ms: Some(completed_at_ms),
                completed_at_ms,
            }))
        })
        .collect();
    if ctx.step_context.turn.mode() == ModeKind::Plan
        && let Some(text) = crate::stream_events_utils::raw_assistant_output_text_from_item(&item)
        && let Some(plan) = codex_utils_stream_parser::extract_proposed_plan_text(&text)
    {
        let (plan, _) = codex_utils_stream_parser::strip_citations(&plan);
        let completed_at_ms = now_unix_timestamp_ms();
        effects.insert(
            0,
            RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: ctx.sess.thread_id(),
                turn_id: ctx.step_context.turn.sub_id.clone(),
                item: codex_protocol::items::TurnItem::Plan(codex_protocol::items::PlanItem {
                    id: format!("{}-plan", ctx.step_context.turn.sub_id),
                    text: plan,
                }),
                started_at_ms: Some(completed_at_ms),
                completed_at_ms,
            })),
        );
    }
    // A source can produce a plan plus ordinary assistant prose. Each UI item needs
    // its own ordinal, while the source enters model context exactly once.
    let mut source = Some(ResponseItemEnvelope::new(item));
    let mut effects = effects.into_iter();
    loop {
        let effect = effects.next();
        if source.is_none() && effect.is_none() {
            break;
        }
        ctx.sess
            .begin_panng_history(
                &ctx.step_context.turn,
                &ctx.step_context.settings.model_info,
                source.take().into_iter().collect(),
            )
            .await?
            .commit_with_effects(
                RuntimeMutation::delta(Vec::new())
                    .map_err(|error| CodexErr::Fatal(error.to_string()))?,
                effect.into_iter().collect(),
            )
            .await?;
    }
    Ok(OutputItemResult {
        last_agent_message,
        ..Default::default()
    })
}
