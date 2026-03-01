//! Inject text input into a running agent terminal session.
//!
//! Two strategies are supported:
//!
//! 1. **Sandbox (Podman) injection** — for containerised agents.
//!    Scans host `/proc` for the `podman exec` process that is currently
//!    attached to the container (i.e. running `bash -c claude ...`), then
//!    writes directly to that process's PTY master fd.  This works because
//!    `podman exec -it` allocates a PTY on the host; we just find and write
//!    to it.  If nobody is attached the inject fails with a clear error.
//!
//! 2. **PTY master injection** — for non-sandboxed agents.
//!    Writes bytes directly to the PTY master fd via `/proc/{pid}/fd/*`.
//!
//! `inject` automatically selects the right strategy based on `task.sandbox_type`.

use anyhow::{bail, Context, Result};

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

/// Inject `message` into the Claude session running inside a Podman container.
///
/// Strategy:
/// 1. Kill any active interactive attach sessions (conmon --exec-attach) for
///    this container, to avoid conflicting conversations.
/// 2. Run `claude --continue` non-interactively via `podman exec -i`, piping
///    the message through stdin. Claude handles piped stdin fine and produces
///    a single response turn.
fn inject_via_container(container_id: &str, message: &str) -> Result<()> {
    let short = &container_id[..container_id.len().min(12)];

    // Kill any interactive attach sessions so there's no conflict.
    kill_exec_attach_sessions(short);

    // Run claude non-interactively with the message on stdin.
    let claude = "/home/node/.local/bin/claude";
    let mut child = std::process::Command::new("podman")
        .args([
            "exec", "-i",
            "-e", "TERM=xterm-256color",
            "-e", "PATH=/home/node/.local/bin:/usr/local/bin:/usr/bin:/bin",
            "-e", "CLAUDE_CONFIG_DIR=/home/node/.claude",
            "-w", "/workspace",
            container_id,
            claude,
            "--continue",
            "--dangerously-skip-permissions",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn podman exec for inject")?;

    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(message.as_bytes())
            .context("Failed to write message to claude stdin")?;
        // stdin drops here, closing the pipe — Claude sees EOF and processes the turn.
    }

    child.wait().context("podman exec inject did not exit cleanly")?;
    Ok(())
}

/// Kill all active `podman exec -it` (conmon --exec-attach) sessions for the
/// given container, so that a remote inject doesn't conflict with a local
/// interactive session on the same repo.
fn kill_exec_attach_sessions(container_id_prefix: &str) {
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{}/cmdline", pid)) else { continue };
        if cmdline.contains("conmon")
            && cmdline.contains("exec-attach")
            && cmdline.contains(container_id_prefix)
        {
            // SIGTERM — lets conmon clean up its PTY and exit gracefully.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
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
    use std::io::Write;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .and_then(|mut f| f.write_all(bytes))
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
