//! A cancelled admitted command can stop before native execution emits its first UI item.
use super::*;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::ExecCommandSource;

pub(super) const UNCONFIRMED: &str = "Execution was interrupted before a terminal result was confirmed. The exit code and any side effects are unknown.";

/// Describes the requested command, without claiming an executable, process or successful exit.
/// Native completed items always take precedence; this fills only a missing Indeterminate UI.
pub(super) async fn unconfirmed_command(
    session: &Session,
    step: &StepContext,
    operation: &OperationRecord,
) -> Result<ItemCompletedEvent, CodexErr> {
    let arguments: serde_json::Value =
        serde_json::from_str(operation.descriptor().arguments().as_str()).map_err(fatal)?;
    let command = codex_utils_output_truncation::formatted_truncate_text(
        arguments
            .get("cmd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("[command input unavailable]"),
        codex_utils_output_truncation::TruncationPolicy::Bytes(2048),
    );
    let selected_id = arguments
        .get("environment_id")
        .and_then(serde_json::Value::as_str);
    let environment = match selected_id {
        Some(id) => step
            .environments
            .turn_environments()
            .find(|environment| environment.selection.environment_id == id),
        None => step.environments.primary(),
    }
    .ok_or_else(|| fatal("interrupted command has no captured environment"))?;
    let cwd = arguments
        .get("workdir")
        .and_then(serde_json::Value::as_str)
        .filter(|workdir| !workdir.is_empty())
        .map_or_else(
            || Ok(environment.cwd().clone()),
            |workdir| environment.cwd().join(workdir),
        )
        .map_err(fatal)?;
    let call_id = operation.identity().call_id.as_str();
    let event = ItemCompletedEvent {
        thread_id: session.thread_id(),
        turn_id: step.turn.sub_id.clone(),
        started_at_ms: step.turn.turn_timing_state.take_item_started(call_id).await,
        completed_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
        item: TurnItem::CommandExecution(CommandExecutionItem {
            id: call_id.into(),
            command: vec![command.clone()],
            parsed_cmd: vec![ParsedCommand::Unknown { cmd: command }],
            cwd,
            source: ExecCommandSource::Agent,
            status: CommandExecutionStatus::Failed,
            sandbox_type: None,
            model_context: Some(step.model_context()),
            plugin_id: None,
            script_path: None,
            process_id: None,
            interaction_input: None,
            stdout: None,
            stderr: None,
            aggregated_output: Some(UNCONFIRMED.into()),
            formatted_output: Some(UNCONFIRMED.into()),
            exit_code: None,
            duration: None,
        }),
    };
    crate::session::panng_events::bound_tool_completed(event)
}
