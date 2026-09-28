//! Read-only recovery of a bounded suffix rooted at one self-contained Panng checkpoint.
//! Commit IDs are checked only across the initial checkpoint and inspected suffix. Older
//! prefix integrity and execution ownership are not established by this reader.

use std::collections::BTreeSet;
use std::io;
use std::io::BufRead;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

use codex_history::PanngCommit;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::OperationState;
use codex_panng_runtime::RuntimeSnapshot;
use codex_panng_runtime::SnapshotReason;
use codex_protocol::ThreadId;
use serde::Deserialize;

use crate::strict_jsonl::StrictJsonlScanner;
use crate::strict_jsonl::Termination;

const MAX_PANNG_HEADER_BYTES: usize = 256 * 1024;
const MAX_PANNG_LINE_BYTES: usize = 16 * 1024 * 1024 + 1024;

/// Read-only canonical position, not a durability ACK or authorization to execute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanngReadCut {
    pub ordinal: u64,
    pub end_byte_offset: u64,
}

/// Verified checkpoint and later commits; the original header remains available to host decoders.
#[derive(Debug)]
pub struct RecoveredPanngSuffix {
    pub header_bytes: Vec<u8>,
    pub thread_id: ThreadId,
    pub initial_commit_id: codex_panng_runtime::CommitId,
    pub branch: BranchId,
    pub runtime: RuntimeSnapshot,
    pub commits: Vec<PanngCommit>,
    pub checkpoint_offset: u64,
    pub cut: PanngReadCut,
    pub torn_tail_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitLine {
    #[serde(rename = "type")]
    kind: String,
    timestamp: String,
    ordinal: u64,
    payload: PanngCommit,
}

/// Inspect a complete file without changing bytes, restoring runtime from its newest checkpoint.
///
/// The suffix is limited to 32 MiB and 4096 records. The header (256 KiB) and initial
/// checkpoint (16 MiB plus envelope allowance) are independently read and validated.
/// An unterminated final fragment is reported, never repaired. A complete corrupt record is fatal.
/// Callers must fence old execution before using this state to begin a new session.
pub fn read_panng_suffix(reader: impl Read + Seek) -> io::Result<RecoveredPanngSuffix> {
    read_prefix(reader, None)
}

/// Reconstruct a previously frozen complete prefix; this validates a cut, not a new durability ACK.
pub fn read_panng_prefix(
    reader: impl Read + Seek,
    cut: codex_protocol::protocol::HistoryPosition,
) -> io::Result<RecoveredPanngSuffix> {
    let recovered = read_prefix(reader, Some(cut))?;
    if recovered.thread_id != cut.thread_id
        || recovered.torn_tail_bytes != 0
        || recovered.cut.ordinal.checked_add(1) != Some(cut.end_ordinal_exclusive)
        || recovered.cut.end_byte_offset != cut.end_byte_offset
    {
        return Err(invalid("Panng frozen prefix does not match its exact cut"));
    }
    Ok(recovered)
}

fn read_prefix(
    mut reader: impl Read + Seek,
    cut: Option<codex_protocol::protocol::HistoryPosition>,
) -> io::Result<RecoveredPanngSuffix> {
    reader.seek(SeekFrom::Start(0))?;
    let mut prefix = Vec::new();
    reader
        .by_ref()
        .take((MAX_PANNG_HEADER_BYTES + 1) as u64)
        .read_to_end(&mut prefix)?;
    let header_end = prefix
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .filter(|end| *end <= MAX_PANNG_HEADER_BYTES)
        .ok_or_else(|| invalid("missing, torn or oversized Panng header"))?;
    prefix.truncate(header_end);
    let header = codex_history::PanngSessionHeader::from_slice(&prefix).map_err(invalid)?;
    let branch = header.branch().clone();
    let thread_id = header.session_meta().meta.id;
    reader.seek(SeekFrom::Start(header_end as u64))?;
    let mut initial_bytes = Vec::new();
    io::BufReader::new(reader.by_ref())
        .take((MAX_PANNG_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut initial_bytes)?;
    if initial_bytes.len() > MAX_PANNG_LINE_BYTES || !initial_bytes.ends_with(b"\n") {
        return Err(invalid(
            "missing, torn or oversized initial Panng checkpoint",
        ));
    }
    let initial = decode_commit(&initial_bytes, &branch, thread_id)?;
    if initial.ordinal != 1
        || initial.payload.sequence() != 0
        || !initial.payload.is_checkpoint()
        || !matches!(
            initial.payload.mutation().snapshot_reason(),
            Some(SnapshotReason::Initial | SnapshotReason::Fork)
        )
    {
        return Err(invalid(
            "header must be followed by initial or fork checkpoint",
        ));
    }
    validate_prepared_ownership(
        &branch,
        initial
            .payload
            .mutation()
            .snapshot_state()
            .ok_or_else(|| invalid("missing initial runtime checkpoint"))?,
    )?;
    let parent_cut = initial
        .payload
        .checkpoint()
        .and_then(|checkpoint| checkpoint.parent_cut);
    if parent_cut != header.session_meta().meta.history_base {
        return Err(invalid(
            "Panng fork checkpoint and header parent cut differ",
        ));
    }
    if cut.is_some_and(|cut| cut.end_byte_offset < header_end as u64 + initial_bytes.len() as u64) {
        return Err(invalid("Panng fork cut must include the root checkpoint"));
    }
    let initial_commit_id = initial.payload.commit_id().clone();
    drop(initial_bytes);
    drop(initial);
    let file_end = reader.seek(SeekFrom::End(0))?;
    let end = cut.map_or(file_end, |cut| cut.end_byte_offset);
    if end > file_end {
        return Err(invalid("Panng frozen cut exceeds its source file"));
    }
    let mut scanner = StrictJsonlScanner::new_at(reader, end)?;
    let mut records = Vec::new();
    let mut ids = BTreeSet::new();
    let mut torn_tail_bytes = 0;
    let mut cut = None;
    let checkpoint_offset;
    loop {
        let raw = scanner
            .next_record()?
            .ok_or_else(|| invalid("Panng initialization lacks a complete checkpoint"))?;
        if raw.termination == Termination::TornTail {
            torn_tail_bytes = raw.end - raw.start;
            continue;
        }
        if raw.start < header_end as u64 {
            return Err(invalid("Panng initialization lacks a complete checkpoint"));
        }
        let line = decode_commit(&raw.bytes, &branch, thread_id)?;
        if (raw.start != header_end as u64 && line.payload.commit_id() == &initial_commit_id)
            || !ids.insert(line.payload.commit_id().clone())
        {
            return Err(invalid("duplicate Panng commit identity"));
        }
        if cut.is_none() {
            cut = Some(PanngReadCut {
                ordinal: line.ordinal,
                end_byte_offset: raw.end,
            });
        }
        let checkpoint = line.payload.is_checkpoint();
        records.push((line.ordinal, line.payload));
        if checkpoint {
            checkpoint_offset = raw.start;
            break;
        }
    }
    records.reverse();
    let (first_ordinal, first) = records
        .first()
        .ok_or_else(|| invalid("missing checkpoint"))?;
    if first.sequence().checked_add(1) != Some(*first_ordinal)
        || (*first_ordinal == 1 && checkpoint_offset != header_end as u64)
    {
        return Err(invalid(
            "initial Panng checkpoint must immediately follow header",
        ));
    }
    let mut runtime = first
        .mutation()
        .snapshot_state()
        .ok_or_else(|| invalid("missing runtime checkpoint"))?
        .clone();
    validate_prepared_ownership(&branch, &runtime)?;
    let mut previous_sequence = first.sequence();
    let mut previous_ordinal = *first_ordinal;
    for (ordinal, commit) in records.iter().skip(1) {
        if previous_sequence.checked_add(1) != Some(commit.sequence())
            || commit.previous_sequence() != Some(previous_sequence)
            || previous_ordinal.checked_add(1) != Some(*ordinal)
        {
            return Err(invalid("Panng sequence or ordinal gap"));
        }
        runtime = commit
            .mutation()
            .apply(&branch, Some((&branch, &runtime)))
            .map_err(invalid)?;
        validate_prepared_ownership(&branch, &runtime)?;
        previous_sequence = commit.sequence();
        previous_ordinal = *ordinal;
    }
    Ok(RecoveredPanngSuffix {
        header_bytes: prefix,
        thread_id,
        initial_commit_id,
        branch,
        runtime,
        commits: records.into_iter().map(|(_, commit)| commit).collect(),
        checkpoint_offset,
        cut: cut.ok_or_else(|| invalid("missing complete Panng record"))?,
        torn_tail_bytes,
    })
}

fn decode_commit(bytes: &[u8], branch: &BranchId, thread_id: ThreadId) -> io::Result<CommitLine> {
    let line: CommitLine = serde_json::from_slice(bytes).map_err(invalid)?;
    if line.kind != "panng_commit"
        || line.timestamp.is_empty()
        || line.timestamp.len() > 128
        || line.ordinal == 0
        || line.payload.branch() != branch
    {
        return Err(invalid("invalid Panng outer record or branch"));
    }
    validate_host_ownership(&line.payload, thread_id)?;
    Ok(line)
}

pub(crate) fn validate_host_ownership(commit: &PanngCommit, thread_id: ThreadId) -> io::Result<()> {
    let companions = commit
        .checkpoint()
        .into_iter()
        .flat_map(|checkpoint| &checkpoint.companions);
    for item in commit.host_effects().iter().chain(companions) {
        let owner = match item {
            codex_history::RolloutItem::EventMsg(
                codex_protocol::protocol::EventMsg::ThreadSettingsApplied(settings),
            ) => Some(settings.thread_id),
            codex_history::RolloutItem::EventMsg(
                codex_protocol::protocol::EventMsg::ItemCompleted(event),
            ) => Some(Some(event.thread_id)),
            _ => None,
        };
        if owner.is_some_and(|owner| owner != Some(thread_id)) {
            return Err(invalid(
                "Panng host effect does not belong to header thread",
            ));
        }
    }
    Ok(())
}

fn validate_prepared_ownership(branch: &BranchId, state: &RuntimeSnapshot) -> io::Result<()> {
    if state.operations().iter().any(|(_, operation)| {
        operation.state() == &OperationState::Prepared
            && (&operation.identity().origin_branch != branch
                || &operation.identity().owner_epoch != state.execution_epoch())
    }) {
        return Err(invalid(
            "prepared operation does not own current branch and epoch",
        ));
    }
    Ok(())
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
#[path = "panng_recovery_tests.rs"]
mod tests;
