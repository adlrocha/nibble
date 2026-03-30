//! Inject text input into a sandboxed agent session.
//!
//! Only Podman-sandboxed agents are supported.  Input injection for
//! non-sandboxed agents is not implemented because writing to a host PTY
//! master from a third-party process is unreliable on Linux kernel 6.2+
//! (TIOCSTI was removed) and terminal emulators consume the bytes before
//! they reach the target process.
//!
//! **Strategy**: run `claude --continue` non-interactively via `podman exec -i`
//! with the message on stdin.  `--continue` resumes the most recent conversation
//! in /workspace without needing a session UUID.

use anyhow::{bail, Context, Result};

use crate::models::{SandboxType, Task};
use crate::sandbox::podman::PodmanSandbox;
use crate::sandbox::{ContainerStatus, Sandbox};

/// Send `message` to the Claude session running inside a Podman sandbox.
///
/// Blocks until the Claude process exits.  Only sandboxed (Podman) tasks are
/// supported.  Returns an error for any other task type.
pub fn inject(task: &Task, message: &str) -> Result<()> {
    let mut child = inject_returning_child(task, message)?;
    child
        .wait()
        .context("podman exec inject did not exit cleanly")?;
    Ok(())
}

/// Like [`inject`] but returns the spawned child process instead of waiting.
///
/// The caller is responsible for waiting on the child (or dropping it).
/// This allows the caller to poll the process and interleave heartbeats.
pub fn inject_returning_child(task: &Task, message: &str) -> Result<std::process::Child> {
    if task.sandbox_type != SandboxType::Podman {
        bail!(
            "Message injection is only supported for Podman-sandboxed tasks \
             (task {} has sandbox_type={:?})",
            task.task_id,
            task.sandbox_type
        );
    }

    let container_id = task
        .container_id
        .as_deref()
        .with_context(|| format!("Task {} is sandboxed but has no container_id", task.task_id))?;

    let session_id = task.context.as_ref().and_then(|c| c.session_id.as_deref());
    spawn_inject(container_id, session_id, &task.task_id, message)
}

/// Check if the container is healthy enough to accept an inject.
///
/// Returns `Ok(())` if the container is running and exec works.
/// Returns an error with a descriptive message if not.
pub fn check_container_health(container_id: &str) -> Result<()> {
    let sandbox = PodmanSandbox::new();
    match sandbox.status(container_id) {
        Ok(ContainerStatus::Running) => Ok(()),
        Ok(ContainerStatus::Stopped) => {
            bail!(
                "Container is stopped — restart it with `agent-sandbox resume {}`",
                &container_id[..container_id.len().min(8)]
            )
        }
        Ok(ContainerStatus::Paused) => {
            bail!("Container is paused — unpause it first")
        }
        Ok(ContainerStatus::Unknown) => {
            bail!("Container status unknown — it may have been removed")
        }
        Err(e) => {
            bail!("Failed to check container status: {e}")
        }
    }
}

// ── Container injection ───────────────────────────────────────────────────────

/// Spawn the Claude process inside the container, write `message` to its stdin,
/// and return the child handle.  The caller decides when to wait.
///
/// Run `claude --continue` inside the container with the message on stdin.
/// Returns the child process handle.
fn spawn_inject(
    container_id: &str,
    _session_id: Option<&str>,
    task_id: &str,
    message: &str,
) -> Result<std::process::Child> {
    let claude = "/home/node/.local/bin/claude";

    // AGENT_TASK_ID must be set so the Stop/SessionEnd hooks inside the
    // container know which task to report against.
    let agent_task_id_env = format!("AGENT_TASK_ID={}", task_id);

    let mut child = std::process::Command::new("podman")
        .args([
            "exec",
            "-i",
            "-e",
            "TERM=xterm-256color",
            "-e",
            "PATH=/home/node/.local/bin:/usr/local/bin:/usr/bin:/bin",
            "-e",
            "CLAUDE_CONFIG_DIR=/home/node/.claude",
            "-e",
            agent_task_id_env.as_str(),
            "-w",
            "/workspace",
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
        // stdin drops here — Claude sees EOF and processes the turn.
    }

    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Task, TaskContext};
    use std::collections::HashMap;

    fn make_non_sandbox_task() -> Task {
        let mut t = Task::new(
            "test-id".to_string(),
            "claude_code".to_string(),
            "[repo:main]".to_string(),
            Some(1234),
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
    fn test_inject_fails_for_non_sandbox_task() {
        let task = make_non_sandbox_task();
        let err = inject(&task, "hello").unwrap_err().to_string();
        assert!(
            err.contains("only supported for Podman-sandboxed tasks"),
            "got: {err}"
        );
    }

    #[test]
    fn test_inject_returning_child_fails_for_non_sandbox_task() {
        let task = make_non_sandbox_task();
        let err = inject_returning_child(&task, "hello")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("only supported for Podman-sandboxed tasks"),
            "got: {err}"
        );
    }
}
