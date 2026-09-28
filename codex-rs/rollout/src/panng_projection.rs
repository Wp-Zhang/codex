//! Bounded full-history UI replay up to a caller-supplied acknowledged canonical cut.
//! This reader does not manufacture durability evidence or execute runtime transitions.
use crate::strict_jsonl::StrictJsonlScanner;
use crate::strict_jsonl::Termination;
use codex_history::PanngCommit;
use codex_history::PanngSessionHeader;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_panng_runtime::BranchId;
use codex_panng_runtime::SnapshotReason;
use codex_protocol::ThreadId;
use codex_protocol::protocol::HistoryPosition;
use serde::Deserialize;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Take;
use std::io::{self};

const HEADER_LIMIT: usize = 256 * 1024;
const RECORD_LIMIT: usize = 16 * 1024 * 1024 + 1024;
const BATCH_BYTES: usize = 32 * 1024 * 1024;
const BATCH_RECORDS: usize = 256;

/// Supplied by the owner of a durable writer ACK, never inferred from the file length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanngProjectionTarget {
    pub thread_id: ThreadId,
    pub branch: BranchId,
    pub end: HistoryPosition,
}

pub struct PanngProjectionRecord {
    pub line: RolloutLine,
    pub start_byte_offset: u64,
    pub end_byte_offset: u64,
}

/// Apply all records and this watermark in one SQLite transaction, after fallible UI projection.
pub struct PanngProjectionBatch {
    pub records: Vec<PanngProjectionRecord>,
    pub start: HistoryPosition,
    pub end: HistoryPosition,
    pub complete: bool,
}

/// Iterates canonical outer records, including history before the latest runtime checkpoint.
/// Peak encoded buffering is one 32 MiB batch plus one bounded lookahead record and I/O buffer.
pub struct PanngProjectionReader<R> {
    reader: BufReader<Take<R>>,
    target: PanngProjectionTarget,
    cursor: HistoryPosition,
    header: PanngSessionHeader,
    pending: Option<Vec<u8>>,
    failed: bool,
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

impl<R: Read + Seek> PanngProjectionReader<R> {
    /// `after` must be the watermark of a committed projection transaction in this generation.
    /// Its header identity, predecessor boundary and relation to the target are checked again.
    /// Currently only self-contained journals whose rollout identity equals thread ID are supported.
    pub fn new(
        mut reader: R,
        target: PanngProjectionTarget,
        after: Option<HistoryPosition>,
    ) -> io::Result<Self> {
        if target.end.thread_id != target.thread_id
            || target.end.end_ordinal_exclusive < 2
            || target.end.end_byte_offset > reader.seek(SeekFrom::End(0))?
        {
            return Err(invalid("invalid acknowledged Panng projection target"));
        }
        reader.seek(SeekFrom::Start(0))?;
        let (header, header_end) = {
            let mut prefix = BufReader::new(reader.by_ref().take(target.end.end_byte_offset));
            let bytes =
                read_record(&mut prefix, HEADER_LIMIT)?.ok_or_else(|| invalid("missing header"))?;
            let header = PanngSessionHeader::from_slice(&bytes).map_err(invalid)?;
            if header.session_meta().meta.id != target.thread_id
                || header.branch() != &target.branch
            {
                return Err(invalid("Panng projection header identity mismatch"));
            }
            let initial_bytes = read_record(&mut prefix, RECORD_LIMIT)?
                .ok_or_else(|| invalid("missing initial checkpoint"))?;
            let initial = decode_commit(&initial_bytes, &target)?;
            let RolloutItem::PanngCommit(commit) = &initial.item else {
                unreachable!()
            };
            if initial.ordinal != Some(1)
                || !commit.is_checkpoint()
                || !matches!(
                    commit.mutation().snapshot_reason(),
                    Some(SnapshotReason::Initial | SnapshotReason::Fork)
                )
            {
                return Err(invalid("projection requires an initial joint checkpoint"));
            }
            if commit
                .checkpoint()
                .and_then(|checkpoint| checkpoint.parent_cut)
                != header.session_meta().meta.history_base
            {
                return Err(invalid("projection fork root and header parent cut differ"));
            }
            (header, bytes.len() as u64)
        };
        validate_boundary(&mut reader, &target, target.end, header_end)?;
        let cursor = after.unwrap_or(HistoryPosition {
            thread_id: target.end.thread_id,
            end_ordinal_exclusive: 0,
            end_byte_offset: 0,
        });
        if cursor.thread_id != target.end.thread_id
            || cursor.end_ordinal_exclusive > target.end.end_ordinal_exclusive
            || cursor.end_byte_offset > target.end.end_byte_offset
        {
            return Err(invalid(
                "projection watermark exceeds or belongs outside target",
            ));
        }
        validate_boundary(&mut reader, &target, cursor, header_end)?;
        if (cursor.end_ordinal_exclusive == target.end.end_ordinal_exclusive)
            != (cursor.end_byte_offset == target.end.end_byte_offset)
        {
            return Err(invalid(
                "projection watermark ordinal and byte cut disagree",
            ));
        }
        reader.seek(SeekFrom::Start(cursor.end_byte_offset))?;
        let remaining = target.end.end_byte_offset - cursor.end_byte_offset;
        Ok(Self {
            reader: BufReader::new(reader.take(remaining)),
            target,
            cursor,
            header,
            pending: None,
            failed: false,
        })
    }

    pub fn header(&self) -> &PanngSessionHeader {
        &self.header
    }

    /// A failed batch cannot be resumed in-place or skipped; recreate from the last committed cut.
    pub fn read_batch(&mut self) -> io::Result<Option<PanngProjectionBatch>> {
        if self.failed {
            return Err(invalid("projection reader is failed"));
        }
        let result = self.read_batch_inner();
        self.failed = result.is_err();
        result
    }

    fn read_batch_inner(&mut self) -> io::Result<Option<PanngProjectionBatch>> {
        if self.cursor == self.target.end {
            return Ok(None);
        }
        let start = self.cursor;
        let mut records = Vec::new();
        let mut total = 0;
        while records.len() < BATCH_RECORDS
            && self.cursor.end_byte_offset < self.target.end.end_byte_offset
        {
            let bytes = match self.pending.take() {
                Some(bytes) => bytes,
                None => read_record(
                    &mut self.reader,
                    if self.cursor.end_ordinal_exclusive == 0 {
                        HEADER_LIMIT
                    } else {
                        RECORD_LIMIT
                    },
                )?
                .ok_or_else(|| invalid("acknowledged prefix ended early"))?,
            };
            if !records.is_empty() && total + bytes.len() > BATCH_BYTES {
                self.pending = Some(bytes);
                break;
            }
            let line = if self.cursor.end_ordinal_exclusive == 0 {
                // Header validation retains unknown engine fields without re-persisting its host view.
                let header = PanngSessionHeader::from_slice(&bytes).map_err(invalid)?;
                if header.as_bytes() != self.header.as_bytes() {
                    return Err(invalid("header changed during projection"));
                }
                let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(invalid)?;
                RolloutLine {
                    timestamp: value["timestamp"]
                        .as_str()
                        .ok_or_else(|| invalid("header timestamp missing"))?
                        .into(),
                    ordinal: Some(0),
                    item: RolloutItem::SessionMeta(header.session_meta().clone()),
                }
            } else {
                decode_commit(&bytes, &self.target)?
            };
            if line.ordinal != Some(self.cursor.end_ordinal_exclusive) {
                return Err(invalid("Panng projection ordinal gap"));
            }
            let end = self
                .cursor
                .end_byte_offset
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| invalid("projection offset overflow"))?;
            total += bytes.len();
            records.push(PanngProjectionRecord {
                line,
                start_byte_offset: self.cursor.end_byte_offset,
                end_byte_offset: end,
            });
            self.cursor.end_byte_offset = end;
            self.cursor.end_ordinal_exclusive = self
                .cursor
                .end_ordinal_exclusive
                .checked_add(/*rhs*/ 1)
                .ok_or_else(|| invalid("projection ordinal overflow"))?;
            if self.cursor.end_ordinal_exclusive > self.target.end.end_ordinal_exclusive {
                return Err(invalid("projection passed acknowledged ordinal"));
            }
        }
        if self.cursor.end_byte_offset == self.target.end.end_byte_offset
            && self.cursor != self.target.end
        {
            return Err(invalid(
                "acknowledged projection ordinal does not match byte cut",
            ));
        }
        Ok(Some(PanngProjectionBatch {
            records,
            start,
            end: self.cursor,
            complete: self.cursor == self.target.end,
        }))
    }
}

fn decode_commit(bytes: &[u8], target: &PanngProjectionTarget) -> io::Result<RolloutLine> {
    let line: CommitLine = serde_json::from_slice(bytes).map_err(invalid)?;
    if line.kind != "panng_commit"
        || line.timestamp.is_empty()
        || line.timestamp.len() > 128
        || line.payload.branch() != &target.branch
        || line.payload.sequence().checked_add(/*rhs*/ 1) != Some(line.ordinal)
    {
        return Err(invalid(
            "invalid Panng projection envelope, branch or sequence",
        ));
    }
    chrono::DateTime::parse_from_rfc3339(&line.timestamp).map_err(invalid)?;
    crate::panng_recovery::validate_host_ownership(&line.payload, target.thread_id)?;
    Ok(RolloutLine {
        timestamp: line.timestamp,
        ordinal: Some(line.ordinal),
        item: RolloutItem::PanngCommit(Box::new(line.payload)),
    })
}

fn validate_boundary(
    reader: &mut (impl Read + Seek),
    target: &PanngProjectionTarget,
    cut: HistoryPosition,
    header_end: u64,
) -> io::Result<()> {
    match cut.end_ordinal_exclusive {
        0 if cut.end_byte_offset == 0 => return Ok(()),
        1 if cut.end_byte_offset == header_end => return Ok(()),
        0 | 1 => return Err(invalid("invalid header projection watermark")),
        _ => {}
    }
    let mut scanner = StrictJsonlScanner::new_at(reader, cut.end_byte_offset)?;
    let record = scanner
        .next_record()?
        .ok_or_else(|| invalid("missing projection cut record"))?;
    if record.termination != Termination::Complete || record.start < header_end {
        return Err(invalid(
            "projection cut is not a complete outer record boundary",
        ));
    }
    let line = decode_commit(&record.bytes, target)?;
    if line
        .ordinal
        .and_then(|ordinal| ordinal.checked_add(/*rhs*/ 1))
        != Some(cut.end_ordinal_exclusive)
    {
        return Err(invalid("projection cut byte and ordinal mismatch"));
    }
    Ok(())
}

fn read_record(reader: &mut impl BufRead, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(invalid("incomplete record inside acknowledged cut"))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        if count > limit.saturating_sub(bytes.len()) {
            return Err(invalid("Panng projection record exceeds byte limit"));
        }
        bytes.reserve_exact(count);
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(Some(bytes));
        }
    }
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
#[path = "panng_projection_tests.rs"]
mod tests;
