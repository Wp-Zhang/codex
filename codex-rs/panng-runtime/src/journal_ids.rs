use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Invalid persisted runtime data or a transition outside the journal contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalError(pub(crate) &'static str);

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for JournalError {}

macro_rules! bounded_id {
    ($name:ident, $limit:expr, $validate:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
        #[serde(into = "String")]
        pub struct $name(String);
        impl $name {
            pub fn new(value: String) -> Result<Self, JournalError> {
                ($validate)(value).map(Self)
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl serde::de::Visitor<'_> for Visitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str("a bounded runtime identity")
                    }
                    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<$name, E> {
                        if value.len() > $limit {
                            return Err(E::custom("runtime string exceeds its byte limit"));
                        }
                        $name::new(value.to_owned()).map_err(E::custom)
                    }
                }
                deserializer.deserialize_str(Visitor)
            }
        }
        impl TryFrom<String> for $name {
            type Error = JournalError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

fn uuid(value: String) -> Result<String, JournalError> {
    if value.len() > 36 || !value.is_ascii() {
        return Err(JournalError("invalid UUID identity"));
    }
    uuid::Uuid::parse_str(&value)
        .map(|id| id.to_string())
        .map_err(|_| JournalError("invalid UUID identity"))
}
fn text(value: String, limit: usize) -> Result<String, JournalError> {
    if value.is_empty() || value.len() > limit {
        return Err(JournalError("identity length exceeds bounds"));
    }
    Ok(value)
}
bounded_id!(BranchId, 36, uuid);
bounded_id!(ExecutionEpoch, 36, uuid);
bounded_id!(CommitId, 36, uuid);
bounded_id!(OperationId, 36, uuid);
bounded_id!(ResultId, 36, uuid);
bounded_id!(LogicalStepId, 36, uuid);
bounded_id!(TurnId, 128, |value| text(value, 128));
bounded_id!(ProviderResponseId, 256, |value| text(value, 256));
bounded_id!(CallId, 256, |value| text(value, 256));
bounded_id!(ToolName, 256, |value| text(value, 256));
bounded_id!(Digest, 64, |value: String| {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(JournalError("digest must be lowercase SHA-256 hex"));
    }
    Ok(value)
});
bounded_id!(BoundedJson, 64 * 1024, |value: String| {
    if value.len() > 64 * 1024 {
        return Err(JournalError("arguments exceed 64 KiB"));
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&value).map_err(|_| JournalError("arguments must be JSON"))?;
    if !parsed.is_object() {
        return Err(JournalError("arguments must be an object"));
    }
    Ok(value)
});
