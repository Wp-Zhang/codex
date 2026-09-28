use crate::Machine;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::io::Read;
use std::io::Write;

const MAX_FRAME: usize = 1024 * 1024;

/// A joint snapshot for fault-injection experiments, not a Codex rollout format.
/// The caller owns its writer lock, sync barrier and acknowledgement boundary.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    version: u32,
    machine: Machine,
}

impl Checkpoint {
    pub fn new(machine: Machine) -> Self {
        Self {
            version: 1,
            machine,
        }
    }
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    /// Publish only after the host-provided durable commit barrier succeeds.
    pub fn commit(
        live: &mut Machine,
        candidate: Machine,
        persist: impl FnOnce(&Self) -> io::Result<()>,
    ) -> io::Result<()> {
        let checkpoint = Self::new(candidate);
        persist(&checkpoint)?;
        *live = checkpoint.machine;
        Ok(())
    }

    /// Writes a bounded frame and commit trailer. Success is NOT an fsync acknowledgement.
    pub fn write(&self, writer: &mut impl Write) -> io::Result<()> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_FRAME {
            return Err(io::Error::other("checkpoint exceeds frame limit"));
        }
        let length = u64::try_from(bytes.len())
            .map_err(io::Error::other)?
            .to_le_bytes();
        writer.write_all(&length)?;
        writer.write_all(&bytes)?;
        writer.write_all(&length)
    }

    /// Replays complete frames; a torn final frame is ignored, complete invalid data is rejected.
    pub fn read_latest(reader: &mut impl Read) -> io::Result<Option<Self>> {
        let mut latest = None;
        loop {
            let mut header = [0; 8];
            match reader.read_exact(&mut header) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(latest),
                Err(err) => return Err(err),
            }
            let length = usize::try_from(u64::from_le_bytes(header)).map_err(io::Error::other)?;
            if length > MAX_FRAME {
                return Err(io::Error::other("invalid frame length"));
            }
            let mut body = vec![0; length];
            let mut trailer = [0; 8];
            match reader
                .read_exact(&mut body)
                .and_then(|()| reader.read_exact(&mut trailer))
            {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(latest),
                Err(err) => return Err(err),
            }
            if trailer != header {
                return Err(io::Error::other("invalid commit trailer"));
            }
            let checkpoint: Self = serde_json::from_slice(&body)?;
            if checkpoint.version != 1 {
                return Err(io::Error::other("unsupported checkpoint version"));
            }
            let machine = &checkpoint.machine;
            if machine
                .history
                .iter()
                .any(|entry| entry.sequence > machine.sequence)
                || machine
                    .history
                    .windows(2)
                    .any(|pair| pair[0].sequence >= pair[1].sequence)
            {
                return Err(io::Error::other("invalid history watermarks"));
            }
            latest = Some(checkpoint);
        }
    }
}
