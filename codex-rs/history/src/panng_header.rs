//! Strict outer header identity with an immutable canonical byte representation.
//! SessionMetaLine is a read-only host view: unknown future fields survive in the owned bytes.

use std::collections::BTreeSet;
use std::sync::Arc;

use codex_panng_runtime::BranchId;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use serde::Deserialize;
use serde::de::Error as _;
use serde_json::Value;

const MAX_HEADER_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct PanngSessionHeader {
    bytes: Arc<[u8]>,
    session_meta: SessionMetaLine,
    branch: BranchId,
}

impl PanngSessionHeader {
    /// Builds the only new-header representation; parsed headers must retain their original bytes.
    pub fn new(meta: SessionMetaLine, branch: BranchId) -> Result<Self, String> {
        if meta.meta.history_mode != ThreadHistoryMode::PanngV1 {
            return Err("Panng header requires panng_v1 history mode".into());
        }
        let mut payload = serde_json::to_value(&meta).map_err(|error| error.to_string())?;
        payload
            .as_object_mut()
            .ok_or("session metadata must be an object")?
            .insert(
                "panng".into(),
                serde_json::json!({"version":1,"branch":branch}),
            );
        let value = serde_json::json!({
            "type":"session_meta", "timestamp":meta.meta.timestamp, "ordinal":0, "payload":payload,
        });
        let mut bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        Self::from_slice(&bytes)
    }

    /// Accepts exactly one complete bounded JSONL header, rejecting duplicate keys before Value.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_HEADER_BYTES {
            return Err("Panng header exceeds 256 KiB".into());
        }
        let body = bytes
            .strip_suffix(b"\n")
            .ok_or("Panng header requires a terminating LF")?;
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        if body.iter().any(|byte| matches!(*byte, b'\n' | b'\r')) {
            return Err("Panng header must occupy one JSONL line".into());
        }
        serde_json::from_slice::<UniqueFields>(body).map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_slice(body).map_err(|error| error.to_string())?;
        if value.get("type").and_then(Value::as_str) != Some("session_meta")
            || value.get("ordinal").and_then(Value::as_u64) != Some(0)
            || value
                .get("timestamp")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err("Panng header requires session_meta, ordinal zero and a timestamp".into());
        }
        let payload = value
            .get("payload")
            .and_then(Value::as_object)
            .ok_or("Panng header requires object session metadata")?;
        // Strict headers never depend on the native legacy fallback that synthesizes session_id.
        if !payload.contains_key("session_id") || !payload.contains_key("source") {
            return Err("Panng header requires explicit session_id and source".into());
        }
        let engine = payload
            .get("panng")
            .and_then(Value::as_object)
            .ok_or("Panng header requires engine metadata")?;
        if engine.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("unsupported Panng header version".into());
        }
        let raw_branch = engine
            .get("branch")
            .and_then(Value::as_str)
            .ok_or("Panng header requires a branch identity")?;
        let branch = BranchId::new(raw_branch.to_owned()).map_err(|error| error.to_string())?;
        if branch.as_str() != raw_branch {
            return Err("Panng branch must be a canonical UUID".into());
        }
        let session_meta: SessionMetaLine = serde_json::from_value(Value::Object(payload.clone()))
            .map_err(|error| error.to_string())?;
        if session_meta.meta.history_mode != ThreadHistoryMode::PanngV1 {
            return Err("Panng header requires panng_v1 history mode".into());
        }
        Ok(Self {
            bytes: Arc::from(bytes),
            session_meta,
            branch,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn session_meta(&self) -> &SessionMetaLine {
        &self.session_meta
    }
    pub fn branch(&self) -> &BranchId {
        &self.branch
    }
}

/// A bounded first pass detects duplicate keys even in fields the native metadata decoder ignores.
struct UniqueFields;
impl<'de> Deserialize<'de> for UniqueFields {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueFields;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON without duplicate object fields")
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(UniqueFields)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(UniqueFields)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(UniqueFields)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(UniqueFields)
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(UniqueFields)
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueFields)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                while sequence.next_element::<UniqueFields>()?.is_some() {}
                Ok(UniqueFields)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut keys = BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(A::Error::custom("duplicate Panng header field"));
                    }
                    map.next_value::<UniqueFields>()?;
                }
                Ok(UniqueFields)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
#[path = "panng_header_tests.rs"]
mod tests;
