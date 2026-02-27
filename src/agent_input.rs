//! Inject text input into a running agent terminal session.
//!
//! Two strategies are supported:
//!
//! 1. **Sandbox (Podman) injection** — preferred for containerised agents.
//!    Uses `podman exec <container> tmux send-keys -t claude '<msg>' Enter`
//!    which delivers the message directly to the Claude tmux session inside
//!    the container.  Reliable, no PTY scanning needed.
//!
//! 2. **PTY master injection** — legacy fallback for non-sandboxed agents.
//!    Writes bytes directly to the PTY master fd via `/proc/{pid}/fd/*`.
//!    This requires the agent to be running on the host with an accessible PTY.
//!
//! `inject` automatically selects the right strategy based on `task.sandbox_type`.

use anyhow::{bail, Context, Result};
use std::process::Command;

use crate::models::{SandboxType, Task};

/// Send `message` followed by Enter to the agent session represented by `task`.
///
/// Automatically dispatches to the container or PTY strategy.
pub fn inject(task: &Task, message: &str) -> Result<()> {
    if task.sandbox_type == SandboxType::Podman {
        let container_id = task
            .container_id
            .as_deref()
            .with_context(|| format!("Task {} is sandboxed but has no container_id", task.task_id))?;
        return inject_via_container(container_id, message);
    }

    // Legacy PTY path for non-sandboxed agents
    let pid = task
        .pid
        .with_context(|| format!("Task {} has no PID recorded", task.task_id))?;

    if !is_pid_alive(pid) {
        bail!("Agent process {} is no longer running", pid);
    }

    let mut bytes = message.as_bytes().to_vec();
    bytes.push(b'\r'); // CR = Enter in raw terminal mode
    inject_via_pty_master(pid, &bytes)
}

// ── Container injection ───────────────────────────────────────────────────────

/// Send `message` to the Claude tmux session inside the container via
/// `podman exec <container> tmux send-keys -t claude '<message>' Enter`.
///
/// tmux send-keys is safe and reliable: it delivers the text directly to the
/// running pane without any PTY scanning or named-pipe machinery.
fn inject_via_container(container_id: &str, message: &str) -> Result<()> {
    // Validate container_id to prevent shell injection.
    if !container_id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        bail!("Invalid container_id: '{}'", container_id);
    }

    let output = Command::new("podman")
        .args([
            "exec",
            container_id,
            "/usr/bin/tmux",
            "send-keys",
            "-t",
            "claude",
            message,
            "Enter",
        ])
        .output()
        .context("Failed to run podman exec tmux send-keys")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux send-keys failed: {}", stderr.trim());
    }

    Ok(())
}

// ── Implementation ────────────────────────────────────────────────────────────

/// Write `bytes` to the PTY master fd that controls the target process's
/// terminal, simulating real keyboard input.
fn inject_via_pty_master(pid: i32, bytes: &[u8]) -> Result<()> {
    let pts_num = pts_number_for_pid(pid)
        .with_context(|| format!("Could not resolve pts device for pid {}", pid))?;

    let (master_pid, master_fd) = find_pty_master(pts_num)
        .with_context(|| format!("Could not find PTY master for pts/{}", pts_num))?;

    let path = format!("/proc/{}/fd/{}", master_pid, master_fd);
    std::fs::write(&path, bytes)
        .with_context(|| format!("Failed to write to PTY master {}", path))
}

/// Return the pts device number (the `N` in `/dev/pts/N`) for the process's
/// stdin fd.
fn pts_number_for_pid(pid: i32) -> Result<u32> {
    let link = std::fs::read_link(format!("/proc/{}/fd/0", pid))
        .with_context(|| format!("readlink /proc/{}/fd/0 failed", pid))?;

    let path_str = link.to_str().context("pts path is not valid UTF-8")?;

    path_str
        .strip_prefix("/dev/pts/")
        .and_then(|n| n.parse::<u32>().ok())
        .with_context(|| format!("Unexpected pts path: {}", path_str))
}

/// Scan all processes to find the one holding a `/dev/ptmx` fd whose
/// `tty-index` matches `pts_num`. Returns `(pid, fd)` of the master.
fn find_pty_master(pts_num: u32) -> Result<(u32, u32)> {
    let target = pts_num.to_string();

    for entry in std::fs::read_dir("/proc").context("Failed to read /proc")?.flatten() {
        let name = entry.file_name();
        let Ok(owner_pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };

        let fd_dir = format!("/proc/{}/fd", owner_pid);
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };

        for fd_entry in fds.flatten() {
            let fd_name = fd_entry.file_name();
            let Ok(fd_num) = fd_name.to_string_lossy().parse::<u32>() else {
                continue;
            };

            let fd_path = format!("{}/{}", fd_dir, fd_num);
            let Ok(link) = std::fs::read_link(&fd_path) else {
                continue;
            };
            if link.to_string_lossy() != "/dev/ptmx" {
                continue;
            }

            let fdinfo_path = format!("/proc/{}/fdinfo/{}", owner_pid, fd_num);
            let Ok(info) = std::fs::read_to_string(&fdinfo_path) else {
                continue;
            };
            for line in info.lines() {
                if let Some(rest) = line.strip_prefix("tty-index:") {
                    if rest.trim() == target {
                        return Ok((owner_pid, fd_num));
                    }
                }
            }
        }
    }

    bail!("No process found holding the PTY master for pts/{}", pts_num)
}

/// True if `/proc/{pid}` exists (Linux only).
pub fn is_pid_alive(pid: i32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Task, TaskContext};
    use std::collections::HashMap;

    fn make_task(pid: Option<i32>) -> Task {
        let mut t = Task::new(
            "test-id".to_string(),
            "claude_code".to_string(),
            "[repo:main]".to_string(),
            pid,
            None,
        );
        t.context = Some(TaskContext {
            url: None,
            project_path: None,
            session_id: None,
            extra: HashMap::new(),
        });
        t
    }

    #[test]
    fn test_inject_fails_when_no_pid() {
        let task = make_task(None);
        let err = inject(&task, "hello").unwrap_err().to_string();
        assert!(err.contains("no PID recorded"), "got: {err}");
    }

    #[test]
    fn test_inject_fails_when_process_dead() {
        let task = make_task(Some(i32::MAX));
        let err = inject(&task, "hello").unwrap_err().to_string();
        assert!(err.contains("no longer running"), "got: {err}");
    }

    #[test]
    fn test_is_pid_alive_current_process() {
        assert!(is_pid_alive(std::process::id() as i32));
    }

    #[test]
    fn test_is_pid_alive_nonexistent() {
        assert!(!is_pid_alive(0));
        assert!(!is_pid_alive(i32::MAX));
    }

    #[test]
    fn test_pts_number_for_current_process() {
        // Gracefully skip if there's no controlling terminal (e.g. CI).
        let pid = std::process::id() as i32;
        match pts_number_for_pid(pid) {
            Ok(n) => assert!(n < 4096, "pts number looks unreasonable: {n}"),
            Err(_) => {} // no terminal in this test environment
        }
    }
}
