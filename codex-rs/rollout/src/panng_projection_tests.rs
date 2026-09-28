use super::*;
use codex_history::CompactedItem;
use codex_history::CompactionResumeMetadata;
use codex_history::PanngCommitData;
use codex_history::PanngHostCheckpoint;
use codex_panng_runtime::CommitId;
use codex_panng_runtime::ExecutionEpoch;
use codex_panng_runtime::RuntimeMutation;
use codex_panng_runtime::RuntimeSnapshot;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::Cursor;

fn id(number: u64) -> String {
    format!("00000000-0000-4000-8000-{number:012x}")
}
fn branch() -> BranchId {
    BranchId::new(id(/*number*/ 1)).unwrap()
}
fn initial_runtime() -> RuntimeSnapshot {
    RuntimeSnapshot::new(ExecutionEpoch::new(id(/*number*/ 2)).unwrap(), Vec::new()).unwrap()
}
fn host_checkpoint() -> PanngHostCheckpoint {
    PanngHostCheckpoint {
        compacted: CompactedItem {
            message: String::new(), replacement_history: Some(Vec::new()), guardian_history: None,
            retained_context: None, mcp_resource_origins: None, window_number: Some(0),
            first_window_id: Some("00000000-0000-7000-8000-000000000004".into()), previous_window_id: None,
            window_id: Some("00000000-0000-7000-8000-000000000004".into()),
            compaction_response_id: None, latest_token_usage_record: None,
            resume_metadata: Some(CompactionResumeMetadata { multi_agent_version: None, last_started_turn_id: None, previous_turn_settings: None }),
        },
        companions: vec![
            RolloutItem::WorldState(codex_protocol::protocol::WorldStateItem::full(Default::default())),
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(serde_json::from_value(json!({
                "thread_id": id(/*number*/ 9), "thread_settings": {
                    "model": "test", "model_provider_id": "test", "service_tier": null,
                    "approval_policy": "never", "approvals_reviewer": "user",
                    "permission_profile": codex_protocol::models::PermissionProfile::read_only(),
                    "cwd": std::env::current_dir().unwrap(), "reasoning_effort": null,
                    "reasoning_summary": null, "personality": null,
                    "collaboration_mode": {"mode":"default", "settings": {
                        "model":"test", "reasoning_effort":null, "developer_instructions":null
                    }}, "disabled_plugin_ids": []
                }
            })).unwrap())),
        ], parent_cut: None,
    }
}
fn commit(sequence: u64, mutation: RuntimeMutation) -> PanngCommit {
    let checkpoint = mutation.snapshot_state().map(|_| host_checkpoint());
    PanngCommit::new(PanngCommitData {
        version: 1,
        branch: branch(),
        commit_id: CommitId::new(id(sequence + 100)).unwrap(),
        sequence,
        previous_sequence: sequence.checked_sub(/*rhs*/ 1),
        mutation,
        host_effects: Vec::new(),
        checkpoint,
    })
    .unwrap()
}
fn snapshot(sequence: u64, state: RuntimeSnapshot) -> PanngCommit {
    let reason = if sequence == 0 {
        SnapshotReason::Initial
    } else {
        SnapshotReason::ReplayBound
    };
    commit(sequence, RuntimeMutation::snapshot(state, reason).unwrap())
}
fn header() -> Vec<u8> {
    let thread_id = ThreadId::from_string(&id(/*number*/ 9)).unwrap();
    let meta = codex_protocol::protocol::SessionMetaLine {
        meta: codex_protocol::protocol::SessionMeta {
            id: thread_id,
            session_id: thread_id.into(),
            timestamp: "2026-09-28T00:00:00Z".into(),
            source: codex_protocol::protocol::SessionSource::Exec,
            history_mode: codex_protocol::protocol::ThreadHistoryMode::PanngV1,
            cwd: std::env::current_dir().unwrap(),
            ..Default::default()
        },
        git: None,
    };
    codex_history::PanngSessionHeader::new(meta, branch())
        .unwrap()
        .as_bytes()
        .to_vec()
}

fn line(commit: &PanngCommit, ordinal: u64) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&json!({
        "type":"panng_commit", "timestamp":"2026-09-28T00:00:00Z", "ordinal":ordinal, "payload":commit
    })).unwrap();
    bytes.push(b'\n');
    bytes
}

fn journal(count: u64) -> (Vec<u8>, PanngProjectionTarget) {
    let mut bytes = header();
    for sequence in 0..count {
        let item = if sequence == 0 || sequence == count / 2 {
            snapshot(sequence, initial_runtime())
        } else {
            commit(sequence, RuntimeMutation::delta(Vec::new()).unwrap())
        };
        bytes.extend(line(&item, sequence + 1));
    }
    let thread_id = ThreadId::from_string(&id(/*number*/ 9)).unwrap();
    let target = PanngProjectionTarget {
        thread_id,
        branch: branch(),
        end: HistoryPosition {
            thread_id,
            end_ordinal_exclusive: count + 1,
            end_byte_offset: bytes.len() as u64,
        },
    };
    (bytes, target)
}

#[test]
fn real_file_batches_preserve_history_before_checkpoints_and_resume_watermarks() {
    let (bytes, target) = journal(/*count*/ 260);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal.jsonl");
    std::fs::write(&path, &bytes).unwrap();
    let mut reader = PanngProjectionReader::new(
        std::fs::File::open(&path).unwrap(),
        target.clone(),
        /*after*/ None,
    )
    .unwrap();
    let first = reader.read_batch().unwrap().unwrap();
    assert_eq!(first.records.len(), 256);
    assert!(!first.complete);
    let last = reader.read_batch().unwrap().unwrap();
    assert_eq!(last.start, first.end);
    assert_eq!(last.end, target.end);
    assert!(last.complete);
    assert!(reader.read_batch().unwrap().is_none());
    assert_eq!(
        first
            .records
            .iter()
            .chain(&last.records)
            .map(|record| record.line.ordinal.unwrap())
            .collect::<Vec<_>>(),
        (0..261).collect::<Vec<_>>()
    );
    let mut resumed =
        PanngProjectionReader::new(std::fs::File::open(&path).unwrap(), target, Some(first.end))
            .unwrap();
    let resumed_last = resumed.read_batch().unwrap().unwrap();
    assert_eq!(resumed_last.end, last.end);
    assert_eq!(
        resumed_last
            .records
            .into_iter()
            .map(|record| serde_json::to_value(record.line).unwrap())
            .collect::<Vec<_>>(),
        last.records
            .into_iter()
            .map(|record| serde_json::to_value(record.line).unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn one_record_watermarks_resume_without_repeating_or_skipping_outer_records() {
    let (bytes, target) = journal(/*count*/ 4);
    let mut reader =
        PanngProjectionReader::new(Cursor::new(&bytes), target.clone(), /*after*/ None).unwrap();
    let batch = reader.read_batch().unwrap().unwrap();
    for (index, record) in batch.records.iter().enumerate() {
        let cut = HistoryPosition {
            thread_id: target.thread_id,
            end_ordinal_exclusive: index as u64 + 1,
            end_byte_offset: record.end_byte_offset,
        };
        let mut resumed =
            PanngProjectionReader::new(Cursor::new(&bytes), target.clone(), Some(cut)).unwrap();
        let remaining = resumed.read_batch().unwrap();
        assert_eq!(
            remaining
                .map(|batch| batch
                    .records
                    .into_iter()
                    .map(|record| record.line.ordinal.unwrap())
                    .collect::<Vec<_>>())
                .unwrap_or_default(),
            ((index as u64 + 1)..5).collect::<Vec<_>>()
        );
    }
}

struct AckBoundReader {
    cursor: Cursor<Vec<u8>>,
    ack: u64,
}
impl Read for AckBoundReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        assert!(
            self.cursor.position() + buffer.len() as u64 <= self.ack,
            "read would observe bytes outside durable cut"
        );
        self.cursor.read(buffer)
    }
}
impl Seek for AckBoundReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.cursor.seek(position)
    }
}

#[test]
fn unacknowledged_valid_and_corrupt_tail_is_not_even_read() {
    let (mut bytes, target) = journal(/*count*/ 3);
    bytes.extend(line(
        &commit(
            /*sequence*/ 3,
            RuntimeMutation::delta(Vec::new()).unwrap(),
        ),
        /*ordinal*/ 4,
    ));
    bytes.extend(b"complete corruption\npartial tail");
    let input = AckBoundReader {
        cursor: Cursor::new(bytes),
        ack: target.end.end_byte_offset,
    };
    let mut reader = PanngProjectionReader::new(input, target.clone(), /*after*/ None).unwrap();
    let batch = reader.read_batch().unwrap().unwrap();
    assert_eq!(batch.end, target.end);
    assert_eq!(batch.records.len(), 4);
    assert!(reader.read_batch().unwrap().is_none());
}

#[test]
fn complete_bad_lines_fail_atomically_and_fence_the_reader() {
    let (bytes, target) = journal(/*count*/ 4);
    let originals = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    for (pointer, value) in [
        ("/type", json!("event_msg")),
        ("/ordinal", json!(99)),
        ("/payload/branch", json!(id(/*number*/ 77))),
        ("/payload/version", json!(99)),
        ("/payload/previous_sequence", json!(99)),
    ] {
        let mut records = originals.clone();
        let mut bad: serde_json::Value = serde_json::from_slice(&records[2]).unwrap();
        *bad.pointer_mut(pointer).unwrap() = value;
        records[2] = serde_json::to_vec(&bad).unwrap();
        records[2].push(b'\n');
        let corrupted = records.concat();
        let mut target = target.clone();
        target.end.end_byte_offset = corrupted.len() as u64;
        let mut reader =
            PanngProjectionReader::new(Cursor::new(corrupted), target, /*after*/ None).unwrap();
        assert!(reader.read_batch().is_err(), "accepted {pointer}");
        assert!(
            reader.read_batch().is_err(),
            "failed reader allowed skipping {pointer}"
        );
    }
}

#[test]
fn partial_ack_and_mismatched_watermarks_are_rejected() {
    let (bytes, target) = journal(/*count*/ 3);
    let mut partial = target.clone();
    partial.end.end_byte_offset -= 1;
    assert!(PanngProjectionReader::new(Cursor::new(&bytes), partial, /*after*/ None).is_err());
    let wrong_byte = HistoryPosition {
        end_byte_offset: header().len() as u64 + 1,
        end_ordinal_exclusive: 1,
        ..target.end
    };
    let wrong_ordinal = HistoryPosition {
        end_ordinal_exclusive: target.end.end_ordinal_exclusive - 1,
        ..target.end
    };
    let foreign = HistoryPosition {
        thread_id: ThreadId::from_string(&id(/*number*/ 88)).unwrap(),
        ..target.end
    };
    for watermark in [wrong_byte, wrong_ordinal, foreign] {
        assert!(
            PanngProjectionReader::new(Cursor::new(&bytes), target.clone(), Some(watermark))
                .is_err()
        );
    }
}

#[test]
fn large_snapshots_cross_batch_byte_limit_only_at_record_boundaries() {
    let (mut bytes, mut target) = journal(/*count*/ 1);
    for sequence in 1..=3 {
        let mut checkpoint = host_checkpoint();
        checkpoint.compacted.message = "x".repeat(11 * 1024 * 1024);
        let commit = PanngCommit::new(PanngCommitData {
            version: 1,
            branch: branch(),
            commit_id: CommitId::new(id(sequence + 100)).unwrap(),
            sequence,
            previous_sequence: Some(sequence - 1),
            mutation: RuntimeMutation::snapshot(initial_runtime(), SnapshotReason::ReplayBound)
                .unwrap(),
            host_effects: Vec::new(),
            checkpoint: Some(checkpoint),
        })
        .unwrap();
        bytes.extend(line(&commit, sequence + 1));
    }
    target.end.end_byte_offset = bytes.len() as u64;
    target.end.end_ordinal_exclusive = 5;
    let mut reader =
        PanngProjectionReader::new(Cursor::new(bytes), target.clone(), /*after*/ None).unwrap();
    let first = reader.read_batch().unwrap().unwrap();
    assert_eq!(
        first
            .records
            .iter()
            .map(|record| record.line.ordinal.unwrap())
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert!(!first.complete);
    let second = reader.read_batch().unwrap().unwrap();
    assert_eq!(second.start, first.end);
    assert_eq!(second.end, target.end);
    assert_eq!(
        second
            .records
            .iter()
            .map(|record| record.line.ordinal.unwrap())
            .collect::<Vec<_>>(),
        vec![4]
    );
    assert!(second.complete);
}

#[test]
fn record_limit_is_checked_before_buffer_growth() {
    let mut reader = BufReader::new(Cursor::new(b"123456789\n"));
    assert!(read_record(&mut reader, /*limit*/ 8).is_err());
    let mut reader = BufReader::new(Cursor::new(b"1234567\n"));
    assert_eq!(
        read_record(&mut reader, /*limit*/ 8).unwrap(),
        Some(b"1234567\n".to_vec())
    );
    let mut reader = BufReader::new(Cursor::new(b"1234567"));
    assert!(read_record(&mut reader, /*limit*/ 8).is_err());
}
