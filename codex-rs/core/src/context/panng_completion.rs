use super::ContextualUserFragment;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ContentItemKind;

pub(crate) struct PanngCompletion(String);

impl PanngCompletion {
    pub(crate) fn new(id: String, output: String) -> Result<Self, CodexErr> {
        let mut body = serde_json::json!({"operation_id": id, "output": output}).to_string();
        if body.len() > 768 {
            let mut end = output.len().min(480);
            while !output.is_char_boundary(end) {
                end -= 1;
            }
            let mut preview = output[..end].to_owned();
            loop {
                body = serde_json::json!({"operation_id":id,"output":preview,"truncated":true,"read_tool":"read_operation_result"}).to_string();
                if body.len() <= 768 {
                    break;
                }
                if preview.pop().is_none() {
                    return Err(CodexErr::Fatal(
                        "completion identity exceeds notification limit".into(),
                    ));
                }
            }
        }
        Ok(Self(body))
    }
}

impl ContextualUserFragment for PanngCompletion {
    fn role(&self) -> &'static str {
        "user"
    }
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("panng.operation_completion".into())
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        ("<panng_completion>", "</panng_completion>")
    }
    fn body(&self) -> String {
        self.0.clone()
    }
}
