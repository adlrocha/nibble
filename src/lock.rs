//! Single-instance locks: an exclusive `flock` on a file, released by the
//! kernel when the holder exits (crashes included). Used by the `nibble mux`
//! watcher (one per session) and the `nibble rc` daemon (one per machine).

use anyhow::{Context, Result};
use std::path::Path;

/// Holds an exclusive lock for as long as it lives.
pub(crate) struct InstanceLock {
    _file: std::fs::File,
}

/// Take `<dir>/<name>.lock` exclusively; `None` when another process
/// holds it.
pub(crate) fn try_lock_in(dir: &Path, name: &str) -> Result<Option<InstanceLock>> {
    use std::os::fd::AsRawFd;
    let path = dir.join(format!("{name}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    // SAFETY: flock on a valid, owned file descriptor.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(anyhow::Error::new(err).context("failed to take the lock"));
    }
    Ok(Some(InstanceLock { _file: file }))
}
