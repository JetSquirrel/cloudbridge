//! The raw payload store, in a browser that has no filesystem.
//!
//! The desktop keeps every provider response and every imported bill on disk,
//! so that a mapping fix replays what is already there instead of paying for
//! another fetch. The web demo fetches nothing and imports nothing, so it has
//! no such directory — and no bytes to report from one.

use anyhow::Result;
use std::path::PathBuf;

/// Where raw payloads would be kept.
///
/// Answered rather than refused, because the Accounts page shows the path
/// beside the size; the string is what that row reads instead of a directory
/// that does not exist.
pub fn raw_dir_path() -> Result<PathBuf> {
    Ok(PathBuf::from("(not kept in a browser)"))
}

/// How many bytes the raw store holds.
pub fn raw_dir_size() -> Result<u64> {
    Ok(0)
}
