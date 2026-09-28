use super::fatal;
use crate::context::ContextualUserFragment;
use crate::context::PanngCompletion;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolPayload;
use crate::tools::router::ToolCall;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_panng_runtime::BoundedJson;
use codex_panng_runtime::Digest;
use codex_panng_runtime::DispatchDescriptor;
use codex_panng_runtime::ExecutionOutcome;
use codex_panng_runtime::OperationRecord;
use codex_panng_runtime::OperationState;
use codex_panng_runtime::ResultEncoding;
use codex_panng_runtime::StoredResult;
use codex_panng_runtime::ToolName;
use codex_protocol::error::CodexErr;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use sha2::Digest as _;
use sha2::Sha256;

fn encode_payload(envelope: &ResponseItemEnvelope) -> Result<(ExecutionOutcome, String), CodexErr> {
    let output = match &envelope.item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => {
            return Err(fatal(
                "finite tool output must be a function or custom result",
            ));
        }
    };
    if !matches!(output.body, FunctionCallOutputBody::Text(_)) {
        return Err(fatal("finite tool output must be text"));
    }
    let outcome = if output.success == Some(false) {
        ExecutionOutcome::Failed
    } else {
        ExecutionOutcome::Succeeded
    };
    let mut stored = envelope.clone();
    if let ResponseItem::FunctionCallOutput {
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    }
    | ResponseItem::CustomToolCallOutput {
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = &mut stored.item
    {
        // Upstream deliberately never restores these host/warehouse-only claims from disk.
        // Keep all reloadable message and harness provenance; do not use a lossy serde roundtrip.
        metadata.cell_id = None;
        metadata.executed_tool_calls = None;
        metadata.tool_calls_complete = None;
    }
    let payload = serde_json::to_string(&RolloutItem::ResponseItem(stored)).map_err(fatal)?;
    Ok((outcome, payload))
}

pub(super) async fn persist_result(
    home: &std::path::Path,
    envelope: &ResponseItemEnvelope,
) -> Result<(ExecutionOutcome, StoredResult), CodexErr> {
    let (outcome, payload) = encode_payload(envelope)?;
    let stored = if payload.len() <= 4096 {
        StoredResult::inline(ResultEncoding::CodexResponseEnvelopeV1, payload).map_err(fatal)?
    } else {
        codex_rollout::store_panng_result_blob(
            home,
            ResultEncoding::CodexResponseEnvelopeV1,
            payload.into_bytes(),
        )
        .await
        .map_err(fatal)?
    };
    Ok((outcome, stored))
}

pub(super) async fn load_result(
    home: &std::path::Path,
    operation: &OperationRecord,
) -> Result<ResponseItemEnvelope, CodexErr> {
    let result = match operation.state() {
        OperationState::Ready { result, .. } | OperationState::InContext { result, .. } => result,
        _ => return Err(fatal("operation has no completed result")),
    };
    let payload =
        if let Some((ResultEncoding::CodexResponseEnvelopeV1, payload)) = result.inline_payload() {
            payload.as_bytes().to_vec()
        } else if result.artifact_reference().is_some() {
            codex_rollout::load_panng_result_blob(home, result)
                .await
                .map_err(fatal)?
        } else {
            return Err(fatal("operation has no confirmed result bytes"));
        };
    let RolloutItem::ResponseItem(envelope) = serde_json::from_slice(&payload).map_err(fatal)?
    else {
        return Err(fatal("invalid stored result envelope"));
    };
    let expected_custom = operation.descriptor().tool().as_str() == "apply_patch";
    let matches_call = match &envelope.item {
        ResponseItem::FunctionCallOutput {
            call_id: Some(id), ..
        } => !expected_custom && id == operation.identity().call_id.as_str(),
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            expected_custom && call_id == operation.identity().call_id.as_str()
        }
        _ => false,
    };
    if !matches_call {
        return Err(fatal(
            "stored result family/identity differs from its operation",
        ));
    }
    Ok(envelope)
}

pub(super) async fn completion_envelope(
    home: &std::path::Path,
    operation: &OperationRecord,
) -> Result<ResponseItemEnvelope, CodexErr> {
    let OperationState::Ready {
        result, outcome, ..
    } = operation.state()
    else {
        return Err(fatal("notification requires Ready"));
    };
    let mut envelope = if result.unknown_reason().is_none() {
        load_result(home, operation).await?
    } else {
        let output = FunctionCallOutputPayload::from_text(format!(
            "Operation outcome is {outcome:?}; no confirmed result is available."
        ));
        ResponseItemEnvelope::new(if operation.descriptor().tool().as_str() == "apply_patch" {
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: operation.identity().call_id.as_str().into(),
                name: None,
                output,
                internal_chat_message_metadata_passthrough: None,
            }
        } else {
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some(operation.identity().call_id.as_str().into()),
                name: None,
                namespace: None,
                output,
                internal_chat_message_metadata_passthrough: None,
            }
        })
    };
    let text = serde_json::to_string(&envelope.item).map_err(fatal)?;
    envelope.item = ContextualUserFragment::into(PanngCompletion::new(
        operation.identity().id.as_str().into(),
        text,
    )?);
    Ok(envelope)
}

pub(super) fn dispatch_descriptor(
    step: &StepContext,
    call: &ToolCall,
) -> Result<DispatchDescriptor, CodexErr> {
    if !call.tool_name.is_default_namespace()
        || !matches!(
            call.tool_name.name.as_str(),
            "panng_probe" | "exec_command" | "apply_patch" | "request_user_input" | "update_plan"
        )
        || call.encrypted_function_args.is_some()
    {
        return Err(fatal(
            "only unencrypted finite execution and supported control tools support durable execution",
        ));
    }
    let arguments = match (&call.payload, call.tool_name.name.as_str()) {
        (
            ToolPayload::Function { arguments },
            "panng_probe" | "exec_command" | "request_user_input" | "update_plan",
        ) => BoundedJson::new(arguments.clone()).map_err(fatal)?,
        (ToolPayload::Custom { input }, "apply_patch") => {
            BoundedJson::new(serde_json::json!({"kind":"custom", "input":input}).to_string())
                .map_err(fatal)?
        }
        _ => return Err(fatal("unsupported finite tool payload family")),
    };
    let environments = step.environments.all_selections().iter().map(|environment| serde_json::json!({
        "environment_id":environment.environment_id, "cwd":environment.cwd.to_string(),
        "workspace_roots":environment.workspace_roots.iter().map(ToString::to_string).collect::<Vec<_>>(),
    })).collect::<Vec<_>>();
    let authorization = serde_json::json!({
        "approval_policy":step.settings.approval_policy(), "reviewer":step.settings.approvals_reviewer(),
        "permission_profile":step.turn.permission_profile_for_environments(&step.environments),
        "environment_permissions":step.environments.turn_environments().map(crate::session::turn_context::TurnEnvironment::permission_profile).collect::<Vec<_>>(),
    });
    Ok(DispatchDescriptor::new(
        ToolName::new(call.tool_name.name.clone()).map_err(fatal)?,
        arguments,
        digest(&environments)?,
        digest(&authorization)?,
        digest(step.tool_router.model_visible_specs().as_ref())?,
    ))
}
fn digest(value: &(impl serde::Serialize + ?Sized)) -> Result<Digest, CodexErr> {
    Digest::new(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(fatal)?)
    ))
    .map_err(fatal)
}
