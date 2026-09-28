use super::ContextualUserFragment;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ContentItemKind;

pub(crate) struct PanngCompletion(String);

impl PanngCompletion {
    pub(crate) fn new(id: String, output: String) -> Result<Self, CodexErr> {
        let body = serde_json::json!({"operation_id": id, "output": output}).to_string();
        // Conservative byte bound includes wrapper headroom; test tools only, no truncation claims.
        if body.len() > 768 {
            return Err(CodexErr::Fatal(
                "prototype completion exceeds byte limit".into(),
            ));
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
