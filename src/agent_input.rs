//! Inject text input into a sandboxed agent session.
//!
//! Only Podman-sandboxed agents are supported.  Input injection for
//! non-sandboxed agents is not implemented because writing to a host PTY
//! master from a third-party process is unreliable on Linux kernel 6.2+
//! (TIOCSTI was removed) and terminal emulators consume the bytes before
//! they reach the target process.
//!
//! **Strategy**: kill any active interactive attach sessions for the container
//! (so there is no conflict with a local user), then run `claude --resume <sid>`
//! (or plain `claude` for the very first turn in a new container) non-interactively
//! via `podman exec -i` with the message on stdin.

use anyhow::{bail, Context, Result};

use crate::models::{SandboxType, Task};

/// Send `message` to the Claude session running inside a Podman sandbox.
///
/// Blocks until the Claude process exits.  Only sandboxed (Podman) tasks are
/// supported.  Returns an error for any other task type.
pub fn inject(task: &Task, message: &str) -> Result<()> {
    let mut child = inject_returning_child(task, message)?;
    child.wait().context("podman exec inject did not exit cleanly")?;
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

// ── Container injection ───────────────────────────────────────────────────────

/// Spawn the Claude process inside the container, write `message` to its stdin,
/// and return the child handle.  The caller decides when to wait.
///
/// Strategy:
/// 1. Kill any active interactive attach sessions (conmon --exec-attach) for
///    this container, to avoid conflicting conversations.
/// 2. Run `claude --resume <session_id>` (or bare `claude` for a brand-new
///    container with no prior session) non-interactively via `podman exec -i`,
///    piping the message through stdin.  Claude handles piped stdin fine and
///    produces a single response turn.
fn spawn_inject(container_id: &str, session_id: Option<&str>, task_id: &str, message: &str) -> Result<std::process::Child> {
    let short = &container_id[..container_id.len().min(12)];

    // Kill any interactive attach sessions so there's no conflict.
    kill_exec_attach_sessions(short);

    // Run claude non-interactively with the message on stdin.
    let claude = "/home/node/.local/bin/claude";
    // Use --resume <sid> when we have a known session, otherwise start fresh
    // (no flag).  Do NOT use --continue: in a brand-new container there is no
    // prior session to continue, and --continue would cause Claude to exit
    // immediately with a non-zero code (triggering "session ended unexpectedly").
    let resume_arg: Vec<&str> = if let Some(sid) = session_id {
        vec!["--resume", sid]
    } else {
        vec![]
    };

    // AGENT_TASK_ID must be set so the Stop/SessionEnd hooks inside the
    // container know which task to report against.
    let agent_task_id_env = format!("AGENT_TASK_ID={}", task_id);

    let mut args = vec![
        "exec", "-i",
        "-e", "TERM=xterm-256color",
        "-e", "PATH=/home/node/.local/bin:/usr/local/bin:/usr/bin:/bin",
        "-e", "CLAUDE_CONFIG_DIR=/home/node/.claude",
        "-e", agent_task_id_env.as_str(),
        "-w", "/workspace",
        container_id,
        claude,
    ];
    args.extend_from_slice(&resume_arg);
    args.push("--dangerously-skip-permissions");

    let mut child = std::process::Command::new("podman")
        .args(&args)
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

    Ok(child)
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
        assert!(err.contains("only supported for Podman-sandboxed tasks"), "got: {err}");
    }

    #[test]
    fn test_inject_returning_child_fails_for_non_sandbox_task() {
        let task = make_non_sandbox_task();
        let err = inject_returning_child(&task, "hello").unwrap_err().to_string();
        assert!(err.contains("only supported for Podman-sandboxed tasks"), "got: {err}");
    }

}
