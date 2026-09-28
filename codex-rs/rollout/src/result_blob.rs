//! Immutable, home-shared bytes behind a runtime StoredResult::Artifact reference.
//! Publication is idempotent by digest. An error may leave an orphan blob; callers must
//! retry the same content, never publish Ready before this function acknowledges it.

use std::fs::File;
use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::path::Path;

use codex_panng_runtime::Digest;
use codex_panng_runtime::ResultEncoding;
use codex_panng_runtime::StoredResult;
use sha2::Digest as _;
use sha2::Sha256;

const MAX_BLOB_BYTES: usize = 16 * 1024 * 1024;
const NAMESPACE: &str = "panng-results";

/// Store exact encoded result bytes and acknowledge a synchronized immutable artifact.
/// Encoding/operation ownership belongs to the caller; this layer does not parse envelopes.
/// Writes currently require Unix file/directory synchronization, like the strict journal.
/// The returned reference is safe to place in a subsequent Ready commit. Publication
/// failure never permits that commit; retrying identical content validates and syncs it.
pub async fn store_panng_result_blob(
    codex_home: &Path,
    encoding: ResultEncoding,
    bytes: Vec<u8>,
) -> io::Result<StoredResult> {
    if !cfg!(unix) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable Panng result writes require Unix synchronization",
        ));
    }
    if bytes.len() > MAX_BLOB_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Panng result exceeds 16 MiB",
        ));
    }
    let home = codex_home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let home = fs::canonicalize(home)?;
        let directory = home.join(NAMESPACE);
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        // Never traverse a replacement namespace symlink outside the managed home.
        if !fs::symlink_metadata(&directory)?.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Panng result namespace must be a real directory",
            ));
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let digest = Digest::new(digest).map_err(io::Error::other)?;
        let reference = StoredResult::artifact(encoding, digest.clone(), bytes.len() as u64)
            .map_err(io::Error::other)?;
        let target = directory.join(digest.as_str());
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        temporary.write_all(&bytes)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&target) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                // A previous attempt or concurrent writer may already have published it.
                // Verify the existing inode rather than overwrite or trust its filename.
                let mut existing = checked_file(&target)?;
                verify(&mut existing, &digest, bytes.len() as u64)?;
                existing.sync_all()?;
            }
            Err(error) => return Err(error.error),
        }
        crate::recorder::sync_ancestors(&target)?;
        Ok(reference)
    })
    .await
    .map_err(io::Error::other)?
}

/// Load an artifact by its typed digest reference, checking exact length and SHA-256.
/// Inline/Unknown values are not paths. Reads are available on all supported platforms.
/// The caller must establish that this reference is reachable in the current branch.
pub async fn load_panng_result_blob(
    codex_home: &Path,
    result: &StoredResult,
) -> io::Result<Vec<u8>> {
    let (_, digest, length) = result.artifact_reference().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Panng result is not an artifact reference",
        )
    })?;
    let digest = digest.clone();
    let home = codex_home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let directory = fs::canonicalize(home)?.join(NAMESPACE);
        if !fs::symlink_metadata(&directory)?.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Panng result namespace must be a real directory",
            ));
        }
        verify(
            &mut checked_file(&directory.join(digest.as_str()))?,
            &digest,
            length,
        )
    })
    .await
    .map_err(io::Error::other)?
}

fn checked_file(path: &Path) -> io::Result<File> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Panng result must be a regular file",
        ));
    }
    File::open(path)
}

fn verify(file: &mut File, digest: &Digest, length: u64) -> io::Result<Vec<u8>> {
    if length > MAX_BLOB_BYTES as u64 || file.metadata()?.len() != length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Panng result length mismatch",
        ));
    }
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)?;
    let mut extra = [0];
    if file.read(&mut extra)? != 0 || format!("{:x}", Sha256::digest(&bytes)) != digest.as_str() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Panng result length or digest mismatch",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "result_blob_tests.rs"]
mod tests;
