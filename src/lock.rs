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
    flock_in(dir, name, libc::LOCK_EX | libc::LOCK_NB)
}

/// Take `<dir>/<name>.lock` exclusively, waiting while another process (or
/// another thread, which opens its own file) holds it.
pub(crate) fn lock_in(dir: &Path, name: &str) -> Result<InstanceLock> {
    flock_in(dir, name, libc::LOCK_EX)?.context("failed to take the lock")
}

fn flock_in(dir: &Path, name: &str, op: libc::c_int) -> Result<Option<InstanceLock>> {
    use std::os::fd::AsRawFd;
    let path = dir.join(format!("{name}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    // SAFETY: flock on a valid, owned file descriptor.
    let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(anyhow::Error::new(err).context("failed to take the lock"));
    }
    Ok(Some(InstanceLock { _file: file }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lock_is_refused_until_dropped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let held = lock_in(tmp.path(), "x").unwrap();
        assert!(try_lock_in(tmp.path(), "x").unwrap().is_none());
        drop(held);
        let again = try_lock_in(tmp.path(), "x").unwrap();
        assert!(again.is_some());
        drop(again);
        // Waits for the holder instead of failing.
        let first = lock_in(tmp.path(), "x").unwrap();
        let dir = tmp.path().to_path_buf();
        let t = std::thread::spawn(move || lock_in(&dir, "x").map(drop));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!t.is_finished(), "blocked while held");
        drop(first);
        t.join().unwrap().unwrap();
    }
}
