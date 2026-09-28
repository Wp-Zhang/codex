//! Compression is supported only for native journals; strict Panng reopen requires plain bytes.
use std::io;
use std::path::Path;

pub(super) fn is_native_compression_source(path: &Path) -> io::Result<bool> {
    // Unlike summary metadata discovery, this streaming discriminator inspects the first header
    // and rejects duplicate type/payload/history_mode fields. It bounds identifying tokens while
    // skipping large native base instructions without retaining their contents in memory.
    match crate::strict_jsonl::preflight_native_append(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(false),
        // Unreadable, malformed and oversized identifying tokens never default to native.
        Err(error) => Err(error),
    }
}
