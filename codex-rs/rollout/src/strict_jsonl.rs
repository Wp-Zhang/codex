//! Bounded, read-only framing for future strict journal replay.
//! This module does not validate Panng commit semantics or authorize recovery.

use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
struct Limits {
    suffix_bytes: u64,
    record_bytes: usize,
    records: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            suffix_bytes: 32 * 1024 * 1024,
            record_bytes: 16 * 1024 * 1024 + 1024,
            records: 4096,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Termination {
    Complete,
    TornTail,
}

/// Original byte positions include the newline; payload bytes exclude it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RawRecord {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) bytes: Vec<u8>,
    pub(crate) termination: Termination,
}

/// Reads newest-to-oldest without skipping oversized, blank or malformed complete records.
/// Callers must still validate schema, sequence and checkpoint semantics before repairing a tail.
pub(crate) struct StrictJsonlScanner<R> {
    reader: R,
    cursor: u64,
    chunk_start: u64,
    chunk: Vec<u8>,
    read_bytes: u64,
    records: usize,
    first: bool,
    failed: bool,
    limits: Limits,
}

impl<R: Read + Seek> StrictJsonlScanner<R> {
    pub(crate) fn new_at(mut reader: R, end: u64) -> io::Result<Self> {
        if end > reader.seek(SeekFrom::End(0))? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "strict cut exceeds file length",
            ));
        }
        Ok(Self {
            reader,
            cursor: end,
            chunk_start: end,
            chunk: Vec::new(),
            read_bytes: 0,
            records: 0,
            first: true,
            failed: false,
            limits: Limits::default(),
        })
    }

    pub(crate) fn next_record(&mut self) -> io::Result<Option<RawRecord>> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "strict scanner is failed",
            ));
        }
        let result = self.next_record_inner();
        self.failed = result.is_err();
        result
    }

    fn next_record_inner(&mut self) -> io::Result<Option<RawRecord>> {
        if self.cursor == 0 {
            return Ok(None);
        }
        if self.records >= self.limits.records {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "strict record count limit exceeded",
            ));
        }
        let end = self.cursor;
        let termination = if self.peek()? == b'\n' {
            self.cursor -= 1;
            Termination::Complete
        } else if self.first {
            Termination::TornTail
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing record delimiter",
            ));
        };
        self.first = false;
        let mut bytes = Vec::new();
        while self.cursor > 0 {
            let byte = self.peek()?;
            if byte == b'\n' {
                break;
            }
            if bytes.len() == self.limits.record_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "strict record byte limit exceeded",
                ));
            }
            if bytes.len() == bytes.capacity() {
                bytes.reserve_exact((self.limits.record_bytes - bytes.len()).min(CHUNK_BYTES));
            }
            bytes.push(byte);
            self.cursor -= 1;
        }
        bytes.reverse();
        if termination == Termination::Complete {
            if bytes.iter().all(u8::is_ascii_whitespace) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "blank complete journal record",
                ));
            }
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }
        self.records += 1;
        Ok(Some(RawRecord {
            start: self.cursor,
            end,
            bytes,
            termination,
        }))
    }

    fn peek(&mut self) -> io::Result<u8> {
        if self.cursor <= self.chunk_start {
            let remaining = self
                .limits
                .suffix_bytes
                .checked_sub(self.read_bytes)
                .filter(|remaining| *remaining > 0)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "strict suffix byte limit exceeded",
                    )
                })?;
            let count = self.cursor.min(CHUNK_BYTES as u64).min(remaining) as usize;
            self.chunk_start = self.cursor - count as u64;
            // Enforce the cumulative read limit before allocating or reading the next chunk.
            self.chunk.resize(count, 0);
            self.reader.seek(SeekFrom::Start(self.chunk_start))?;
            self.reader.read_exact(&mut self.chunk)?;
            self.read_bytes += count as u64;
        }
        Ok(self.chunk[(self.cursor - self.chunk_start - 1) as usize])
    }
}

/// Refuse foreign history formats before native append can materialize or repair their bytes.
pub(crate) fn preflight_native_append(path: &Path) -> io::Result<()> {
    use serde::de::DeserializeSeed;
    use std::io::BufRead;
    let mut reader = io::BufReader::new(crate::seekable_reader::open_rollout_stream_reader(path)?);
    // Preserve empty native files, including whitespace, without retaining their entire prefix.
    loop {
        let buffer = reader.fill_buf()?;
        let whitespace = buffer
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        if buffer.is_empty() {
            return Ok(());
        }
        if whitespace < buffer.len() {
            reader.consume(whitespace);
            break;
        }
        reader.consume(whitespace);
    }
    let budget = std::rc::Rc::new(std::cell::Cell::new(None));
    let reader = TokenBudgetReader {
        reader,
        budget: budget.clone(),
    };
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let native = HeaderSeed {
        budget,
        payload: false,
    }
    .deserialize(&mut deserializer)
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if native {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native append does not support this rollout history mode",
        ))
    }
}

// Only field names and the small discriminator values have a token budget. Large unrelated
// values use IgnoredAny, whose JSON implementation skips strings without allocating them.
type TokenBudget = std::rc::Rc<std::cell::Cell<Option<usize>>>;
struct TokenBudgetReader<R> {
    reader: R,
    budget: TokenBudget,
}
impl<R: Read> Read for TokenBudgetReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let capacity = match self.budget.get() {
            Some(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "header discriminator token too large",
                ));
            }
            Some(remaining) => bytes.len().min(remaining),
            None => bytes.len(),
        };
        let read = self.reader.read(&mut bytes[..capacity])?;
        if let Some(remaining) = self.budget.get() {
            self.budget.set(Some(remaining - read));
        }
        Ok(read)
    }
}

struct BoundedSeed<T> {
    budget: TokenBudget,
    marker: std::marker::PhantomData<T>,
}
impl<'de, T: serde::Deserialize<'de>> serde::de::DeserializeSeed<'de> for BoundedSeed<T> {
    type Value = T;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<T, D::Error> {
        self.budget.set(Some(1024));
        let result = T::deserialize(deserializer);
        self.budget.set(None);
        result
    }
}

struct HeaderSeed {
    budget: TokenBudget,
    payload: bool,
}
impl<'de> serde::de::DeserializeSeed<'de> for HeaderSeed {
    type Value = bool;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<bool, D::Error> {
        deserializer.deserialize_map(self)
    }
}
impl<'de> serde::de::Visitor<'de> for HeaderSeed {
    type Value = bool;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a rollout header object")
    }
    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<bool, M::Error> {
        let mut kind = None;
        let mut native = None;
        while let Some(key) = map.next_key_seed(BoundedSeed::<String> {
            budget: self.budget.clone(),
            marker: std::marker::PhantomData,
        })? {
            match (self.payload, key.as_str()) {
                (false, "type") => {
                    if kind.is_some() {
                        return Err(serde::de::Error::duplicate_field("type"));
                    }
                    kind = Some(map.next_value_seed(BoundedSeed::<String> {
                        budget: self.budget.clone(),
                        marker: std::marker::PhantomData,
                    })?);
                }
                (false, "payload") => {
                    if native.is_some() {
                        return Err(serde::de::Error::duplicate_field("payload"));
                    }
                    native = Some(map.next_value_seed(HeaderSeed {
                        budget: self.budget.clone(),
                        payload: true,
                    })?);
                }
                (true, "history_mode") => {
                    if native.is_some() {
                        return Err(serde::de::Error::duplicate_field("history_mode"));
                    }
                    let value = map.next_value_seed(BoundedSeed::<serde_json::Value> {
                        budget: self.budget.clone(),
                        marker: std::marker::PhantomData,
                    })?;
                    native = Some(matches!(value.as_str(), Some("legacy" | "paginated")));
                }
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        if self.payload {
            return Ok(native.unwrap_or(true));
        }
        if kind.as_deref() != Some("session_meta") {
            return Err(serde::de::Error::custom(
                "rollout must begin with session metadata",
            ));
        }
        native.ok_or_else(|| serde::de::Error::missing_field("payload"))
    }
}

#[cfg(test)]
#[path = "strict_jsonl_tests.rs"]
mod tests;
