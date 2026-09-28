use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn metadata() -> SessionMetaLine {
    serde_json::from_value(json!({
        "session_id":"00000000-0000-4000-8000-000000000001",
        "id":"00000000-0000-4000-8000-000000000001",
        "timestamp":"2026-09-28T00:00:00Z",
        "cwd":std::env::current_dir().unwrap(),
        "originator":"panng-test", "cli_version":"0.1", "source":"exec",
        "model_provider":"test", "base_instructions":{"text":"instructions"},
        "history_mode":"panng_v1", "git":{"branch":"main"}
    }))
    .unwrap()
}
fn branch() -> BranchId {
    BranchId::new("00000000-0000-4000-8000-000000000002".into()).unwrap()
}
fn bytes(value: &Value) -> Vec<u8> {
    let mut result = serde_json::to_vec(value).unwrap();
    result.push(b'\n');
    result
}
fn value() -> Value {
    serde_json::from_slice(
        PanngSessionHeader::new(metadata(), branch())
            .unwrap()
            .as_bytes(),
    )
    .unwrap()
}

#[test]
fn panng_header_roundtrip_retains_full_native_metadata_and_exact_future_bytes() {
    let meta = metadata();
    let header = PanngSessionHeader::new(meta.clone(), branch()).unwrap();
    let decoded = PanngSessionHeader::from_slice(header.as_bytes()).unwrap();
    assert_eq!(decoded.as_bytes(), header.as_bytes());
    assert_eq!(
        serde_json::to_value(decoded.session_meta()).unwrap(),
        serde_json::to_value(meta).unwrap()
    );
    assert_eq!(decoded.branch(), &branch());

    let mut future = value();
    future["future_root"] = json!({"list":[true,null,0.125]});
    future["payload"]["future_native"] = json!({"nested":{"keep":"verbatim"}});
    future["payload"]["panng"]["future_engine"] = json!([1, 2, 3]);
    let raw = format!(" {} \r\n", serde_json::to_string(&future).unwrap()).into_bytes();
    let decoded = PanngSessionHeader::from_slice(&raw).unwrap();
    assert_eq!(decoded.as_bytes(), raw);
    assert_eq!(decoded.as_bytes(), raw);
    assert!(
        serde_json::to_value(decoded.session_meta())
            .unwrap()
            .get("future_native")
            .is_none()
    );
}

#[test]
fn panng_header_rejects_missing_identity_or_invalid_engine_without_legacy_defaults() {
    for field in [
        "session_id",
        "id",
        "cwd",
        "originator",
        "cli_version",
        "timestamp",
        "source",
        "panng",
    ] {
        let mut invalid = value();
        invalid["payload"].as_object_mut().unwrap().remove(field);
        assert!(
            PanngSessionHeader::from_slice(&bytes(&invalid)).is_err(),
            "missing {field}"
        );
    }
    for (pointer, replacement) in [
        ("/type", json!("event_msg")),
        ("/ordinal", json!(1)),
        ("/payload/history_mode", json!("paginated")),
        ("/payload/panng/version", json!(2)),
        ("/payload/panng/branch", json!("not-a-uuid")),
        (
            "/payload/panng/branch",
            json!("00000000000040008000000000000002"),
        ),
    ] {
        let mut invalid = value();
        *invalid.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            PanngSessionHeader::from_slice(&bytes(&invalid)).is_err(),
            "invalid {pointer}"
        );
    }
    let mut wrong_mode = metadata();
    wrong_mode.meta.history_mode = ThreadHistoryMode::Legacy;
    assert!(PanngSessionHeader::new(wrong_mode, branch()).is_err());
}

#[test]
fn panng_header_rejects_duplicate_fields_before_lossy_native_deserialization() {
    let valid = serde_json::to_string(&value()).unwrap();
    let raw = value();
    let payload = raw["payload"].as_object().unwrap();
    let engine = payload["panng"].as_object().unwrap();
    for (key, field_value) in [
        ("type", raw["type"].clone()),
        ("ordinal", raw["ordinal"].clone()),
        ("id", payload["id"].clone()),
        ("history_mode", payload["history_mode"].clone()),
        ("panng", payload["panng"].clone()),
        ("version", engine["version"].clone()),
        ("branch", engine["branch"].clone()),
    ] {
        let member = format!(
            "{}:{}",
            serde_json::to_string(key).unwrap(),
            serde_json::to_string(&field_value).unwrap()
        );
        let duplicate = format!(
            "{}\n",
            valid.replacen(&member, &format!("{member},{member}"), 1)
        );
        assert_ne!(duplicate.trim_end(), valid, "fixture duplicated {key}");
        assert!(
            PanngSessionHeader::from_slice(duplicate.as_bytes()).is_err(),
            "duplicate {key}"
        );
    }
    let escaped_duplicate = format!(
        "{}\n",
        valid.replacen("\"ordinal\":0", "\"ordinal\":0,\"\\u006frdinal\":0", 1)
    );
    assert!(PanngSessionHeader::from_slice(escaped_duplicate.as_bytes()).is_err());
    let unknown_duplicate = format!("{{\"future\":{{\"x\":1,\"x\":2}},{}\n", &valid[1..]);
    assert!(PanngSessionHeader::from_slice(unknown_duplicate.as_bytes()).is_err());
}

#[test]
fn panng_header_enforces_bounded_single_complete_line() {
    let valid = PanngSessionHeader::new(metadata(), branch()).unwrap();
    let raw = valid.as_bytes();
    assert!(PanngSessionHeader::from_slice(&raw[..raw.len() - 1]).is_err());
    assert!(PanngSessionHeader::from_slice(&[raw, raw].concat()).is_err());
    assert!(PanngSessionHeader::from_slice(&[raw, b"\n"].concat()).is_err());
    let pretty = format!("{}\n", serde_json::to_string_pretty(&value()).unwrap());
    assert!(PanngSessionHeader::from_slice(pretty.as_bytes()).is_err());
    let mut large = value();
    large["future"] = json!("x".repeat(MAX_HEADER_BYTES));
    assert!(PanngSessionHeader::from_slice(&bytes(&large)).is_err());
}
