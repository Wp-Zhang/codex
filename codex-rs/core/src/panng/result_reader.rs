//! Read-only result paging handled by the host, never scheduled as another operation.
use super::encoding::load_result;
use super::fatal;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::router::ToolCall;
use codex_history::ResponseItemEnvelope;
use codex_panng_runtime::RuntimeMutation;
use codex_protocol::error::CodexErr;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::sync::Arc;

pub(crate) const NAME: &str = "read_operation_result";
pub(crate) struct ResultReader;

impl ToolExecutor<ToolInvocation> for ResultReader {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(NAME)
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(codex_tools::ResponsesApiTool {
            name: NAME.into(),
            description: "Read a completed operation's retained text result. offset and limit are UTF-8 byte offsets; follow next_offset until null. The returned JSON is capped at 8192 bytes. Results retain the original tool's truncation markers.".into(),
            strict: false,
            parameters: codex_tools::JsonSchema::object(
                std::collections::BTreeMap::from([
                    ("operation_id".into(), codex_tools::JsonSchema::string(/*description*/ None)),
                    ("offset".into(), codex_tools::JsonSchema::integer(/*description*/ None)),
                    ("limit".into(), codex_tools::JsonSchema::integer(/*description*/ None)),
                ]),
                Some(vec!["operation_id".into()]), Some(false.into()),
            ),
            output_schema: None, defer_loading: None,
        })
    }
    fn handle<'a>(&'a self, _: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async {
            Err(codex_tools::FunctionCallError::RespondToModel(
                "Result reads require the Panng host boundary".into(),
            ))
        })
    }
}
impl CoreToolRuntime for ResultReader {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    operation_id: String,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

pub(crate) async fn read(
    session: &Arc<Session>,
    step: &StepContext,
    call: ToolCall,
    original: ResponseItem,
) -> Result<(), CodexErr> {
    codex_panng_runtime::CallId::new(call.call_id.clone()).map_err(fatal)?;
    let page = async {
    let ToolPayload::Function { arguments } = &call.payload else {
        return Err(fatal("result reader requires function arguments"));
    };
    if call.encrypted_function_args.is_some() {
        return Err(fatal("result reader requires plaintext arguments"));
    }
    let args: Arguments = serde_json::from_str(arguments)
        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
    let operation_id = codex_panng_runtime::OperationId::new(args.operation_id.clone())
        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
    let limit = args.limit.unwrap_or(4096);
    if !(1..=8192).contains(&limit) {
        return Err(CodexErr::InvalidRequest("result page limit must be 1..8192".into()));
    }
    let inspection = session
        .begin_panng_history(&step.turn, &step.settings.model_info, Vec::new())
        .await?;
    let operation = inspection
        .runtime()?
        .operations()
        .iter()
        .find(|(_, operation)| operation.identity().id.as_str() == args.operation_id)
        .map(|(_, operation)| operation.clone());
    let target = inspection.projection_target()?;
    inspection.cancel_unprepared()?;
    let operation = match operation {
        Some(operation) => Some(operation),
        None => {
            let path = session
                .live_thread()
                .ok_or_else(|| fatal("result lookup requires a live Panng journal"))?
                .local_rollout_path()
                .await
                .map_err(fatal)?
                .ok_or_else(|| fatal("result lookup requires a local Panng journal"))?;
            codex_rollout::find_panng_operation(step.turn.config.codex_home.as_path(), &path, target, &operation_id)
                .await
                .map_err(fatal)?
        }
    };
    let operation =
        operation.ok_or_else(|| CodexErr::InvalidRequest("operation result does not belong to this branch".into()))?;
    if !matches!(operation.state(), codex_panng_runtime::OperationState::Ready { result, .. }
        | codex_panng_runtime::OperationState::InContext { result, .. } if result.unknown_reason().is_none()) {
        return Err(CodexErr::InvalidRequest("operation has no confirmed completed result yet".into()));
    }
    let envelope = load_result(step.turn.config.codex_home.as_path(), &operation).await?;
    let output = match &envelope.item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => return Err(fatal("result has no text output")),
    };
    let text = output
        .text_content()
        .ok_or_else(|| fatal("result is not text"))?;
    if args.offset > text.len() || !text.is_char_boundary(args.offset) {
        return Err(CodexErr::InvalidRequest("result offset is outside a UTF-8 boundary".into()));
    }
    let mut end = args.offset.saturating_add(limit).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == args.offset && end < text.len() {
        return Err(CodexErr::InvalidRequest("page limit is smaller than the next UTF-8 character".into()));
    }
    let history_budget = codex_utils_output_truncation::with_serialization_allowance(
        step.settings.model_info.truncation_policy.into(),
    )
    .byte_budget()
    .min(8192);
    let page = loop {
        let page = serde_json::json!({"operation_id":args.operation_id,"offset":args.offset,
            "next_offset":(end < text.len()).then_some(end),"total_bytes":text.len(),"output":&text[args.offset..end]}).to_string();
        if page.len() <= history_budget {
            break page;
        }
        if end == args.offset {
            return Err(fatal("tool output budget cannot fit result page metadata"));
        }
        end -= 1;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == args.offset && end < text.len() {
            return Err(fatal(
                "tool output budget cannot fit the next result character",
            ));
        }
    };
    Ok::<String, CodexErr>(page)
    }.await;
    let (text, success) = match page {
        Ok(page) => (page, true),
        Err(error)
            if matches!(
                error.details(),
                codex_protocol::error::CodexErrorDetails::InvalidRequest(_)
            ) =>
        {
            (format!("Result read rejected: {error}"), false)
        }
        Err(error) => return Err(error),
    };
    let mut output = FunctionCallOutputPayload::from_text(text);
    output.success = Some(success);
    session
        .begin_panng_history(
            &step.turn,
            &step.settings.model_info,
            vec![
                ResponseItemEnvelope::new(original),
                ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some(call.call_id),
                    name: None,
                    namespace: None,
                    output,
                    internal_chat_message_metadata_passthrough: None,
                }),
            ],
        )
        .await?
        .commit(RuntimeMutation::delta(Vec::new()).map_err(fatal)?)
        .await?;
    Ok(())
}
