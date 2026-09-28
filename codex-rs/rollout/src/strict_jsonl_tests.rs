use super::*;
use pretty_assertions::assert_eq;
use std::io::Cursor;

#[test]
fn reverse_records_preserve_offsets_and_torn_tail() {
    let bytes = b"{\"a\":1}\n{}\n{\"t";
    let mut scanner = StrictJsonlScanner::new_at(Cursor::new(bytes), bytes.len() as u64).unwrap();
    let actual = std::iter::from_fn(|| scanner.next_record().unwrap()).collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            RawRecord {
                start: 11,
                end: 14,
                bytes: b"{\"t".to_vec(),
                termination: Termination::TornTail
            },
            RawRecord {
                start: 8,
                end: 11,
                bytes: b"{}".to_vec(),
                termination: Termination::Complete
            },
            RawRecord {
                start: 0,
                end: 8,
                bytes: b"{\"a\":1}".to_vec(),
                termination: Termination::Complete
            },
        ]
    );
}

#[test]
fn every_tail_cut_is_explicitly_framed_and_never_skipped() {
    let bytes = b"{}\n{\"value\":12}\n";
    for end in 3..=bytes.len() {
        let mut scanner = StrictJsonlScanner::new_at(Cursor::new(bytes), end as u64).unwrap();
        let record = scanner.next_record().unwrap().unwrap();
        if end == 3 {
            assert_eq!(
                record,
                RawRecord {
                    start: 0,
                    end: 3,
                    bytes: b"{}".to_vec(),
                    termination: Termination::Complete
                }
            );
        } else {
            let complete = end == bytes.len();
            assert_eq!(
                record,
                RawRecord {
                    start: 3,
                    end: end as u64,
                    bytes: bytes[3..end - usize::from(complete)].to_vec(),
                    termination: if complete {
                        Termination::Complete
                    } else {
                        Termination::TornTail
                    },
                }
            );
            assert_eq!(scanner.next_record().unwrap().unwrap().start, 0);
        }
        assert_eq!(scanner.next_record().unwrap(), None);
    }
}

#[test]
fn corrupt_blank_and_oversized_records_fail_without_skipping() {
    for bytes in [
        b"{}\nbad\n".as_slice(),
        b"{}\n\n".as_slice(),
        b"{}\n \n".as_slice(),
    ] {
        let mut scanner =
            StrictJsonlScanner::new_at(Cursor::new(bytes), bytes.len() as u64).unwrap();
        assert_eq!(
            scanner.next_record().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(scanner.next_record().is_err());
    }
    let mut scanner = StrictJsonlScanner::new_at(Cursor::new(b"{}\n"), 3).unwrap();
    scanner.limits.record_bytes = 1;
    assert!(scanner.next_record().is_err());
}

#[test]
fn budgets_fail_before_extra_reads_or_records() {
    let bytes = b"{}\n{}\n";
    let mut scanner = StrictJsonlScanner::new_at(Cursor::new(bytes), 6).unwrap();
    scanner.limits.suffix_bytes = 2;
    assert!(scanner.next_record().is_err());
    assert_eq!(scanner.read_bytes, 2);
    assert!(scanner.chunk.len() <= 2);

    let mut scanner = StrictJsonlScanner::new_at(Cursor::new(bytes), 6).unwrap();
    scanner.limits.records = 1;
    assert!(scanner.next_record().unwrap().is_some());
    assert!(scanner.next_record().is_err());

    let mut scanner = StrictJsonlScanner::new_at(Cursor::new(b"{}\n"), 3).unwrap();
    scanner.limits = Limits {
        suffix_bytes: 3,
        record_bytes: 2,
        records: 1,
    };
    assert!(scanner.next_record().unwrap().is_some());
    assert_eq!(scanner.next_record().unwrap(), None);
    assert!(StrictJsonlScanner::new_at(Cursor::new(bytes), 7).is_err());
}

#[tokio::test]
async fn native_constructor_refuses_foreign_headers_without_repairing_tail() {
    use crate::RolloutRecorder;
    use crate::RolloutRecorderParams;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionMetaLine;
    use codex_utils_absolute_path::test_support::PathExt;

    let directory = tempfile::tempdir().unwrap();
    let config = crate::config::RolloutConfig {
        codex_home: directory.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        cwd: directory.path().to_path_buf(),
        model_provider_id: "test".to_string(),
        generate_memories: false,
    };
    let thread_id = ThreadId::new();
    let header = crate::RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta {
            id: thread_id,
            session_id: thread_id.into(),
            cwd: directory.path().to_path_buf(),
            ..SessionMeta::default()
        },
        git: None,
    });
    let mut value = serde_json::to_value(crate::RolloutLine {
        timestamp: "2026-09-28T00:00:00Z".to_string(),
        ordinal: None,
        item: header,
    })
    .unwrap();
    // history_mode sorts after base_instructions: the preflight must stream through this value,
    // not reject a valid native header or give up before discovering a foreign mode.
    value["payload"]["base_instructions"] = serde_json::json!({ "text": "x".repeat(300 * 1024) });
    for mode in ["panng_v1", "future_mode"] {
        value["payload"]["history_mode"] = serde_json::Value::String(mode.to_string());
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.extend_from_slice(b"\n{\"partial\":");
        let path = directory.path().join(format!("{mode}.jsonl"));
        std::fs::write(&path, &bytes).unwrap();
        let result = RolloutRecorder::new(
            &config,
            RolloutRecorderParams::Resume { path: path.clone() },
        )
        .await;
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::Unsupported));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let compressed_path = directory
            .path()
            .join(format!("{mode}-compressed.jsonl.zst"));
        let compressed = zstd::stream::encode_all(Cursor::new(&bytes), /*level*/ 0).unwrap();
        std::fs::write(&compressed_path, &compressed).unwrap();
        let result = RolloutRecorder::new(
            &config,
            RolloutRecorderParams::Resume {
                path: compressed_path.clone(),
            },
        )
        .await;
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::Unsupported));
        assert_eq!(std::fs::read(&compressed_path).unwrap(), compressed);
        assert!(!compressed_path.with_extension("").exists());
    }
    value["payload"]["history_mode"] = serde_json::Value::String("legacy".to_string());
    let bytes = serde_json::to_vec(&value).unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, &bytes).unwrap();
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::Resume { path: path.clone() },
    )
    .await
    .unwrap();
    recorder.shutdown().await.unwrap();
    let mut repaired = bytes.clone();
    repaired.push(b'\n');
    assert_eq!(std::fs::read(path).unwrap(), repaired);

    let compressed_path = directory.path().join("native-compressed.jsonl.zst");
    std::fs::write(
        &compressed_path,
        zstd::stream::encode_all(Cursor::new(&bytes), /*level*/ 0).unwrap(),
    )
    .unwrap();
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::Resume {
            path: compressed_path.clone(),
        },
    )
    .await
    .unwrap();
    recorder.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read(compressed_path.with_extension("")).unwrap(),
        repaired
    );
}
