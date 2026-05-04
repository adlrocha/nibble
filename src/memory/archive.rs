//! Archive agent session transcripts into the memory repo.
//!
//! When agents garbage-collect their local session files, the memory repo
//! must still hold a complete, standalone copy of every transcript.
//! This module copies the original agent session file (Claude JSONL, Pi JSONL)
//! into ~/.nibble/memory/archive/<agent>/<task-id>.jsonl so it survives
//! independently of the agent's local storage.

use crate::db::Database;
use crate::models::task::AgentType;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

/// Archive a session by task_id.
///
/// 1. Looks up the task in the DB to find agent type and session IDs.
/// 2. Finds the original agent session file on disk.
/// 3. Copies it to archive/<agent>/<task-id>.jsonl in the memory repo.
/// 4. Returns the archive path.
pub fn archive_session(task_id: &str) -> Result<Option<PathBuf>> {
    let db_path = crate::db::default_db_path();
    let db = Database::open(&db_path)?;

    let task = match db.get_task_by_id(task_id)? {
        Some(t) => t,
        None => {
            eprintln!("[archive] Task not found: {task_id}");
            return Ok(None);
        }
    };

    // Determine which session ID to look for
    let session_id = match task.agent_type {
        AgentType::ClaudeCode => task
            .context
            .as_ref()
            .and_then(|c| c.claude_session_id.clone()),
        AgentType::Pi => task.context.as_ref().and_then(|c| c.session_id.clone()),
        AgentType::OpenCode => task
            .context
            .as_ref()
            .and_then(|c| c.opencode_session_id.clone()),
        AgentType::Hermes => task.context.as_ref().and_then(|c| c.session_id.clone()),
        AgentType::Unknown(_) => task.context.as_ref().and_then(|c| c.session_id.clone()),
    };

    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("[archive] No session ID recorded for task {task_id}");
            return Ok(None);
        }
    };

    // Find the session file on disk by scanning agent directories
    let session_file = find_agent_session_file(&task.agent_type, &session_id);

    let session_file = match session_file {
        Some(p) => p,
        None => {
            eprintln!(
                "[archive] Session file not found on disk for {task_id} (session {session_id})"
            );
            return Ok(None);
        }
    };

    let agent_str = agent_short_name(&task.agent_type);
    let base = crate::config::memory_dir();
    let archive_dir = base.join("archive").join(&agent_str);
    fs::create_dir_all(&archive_dir)?;

    let archive_path = archive_dir.join(format!("{}.jsonl", task_id));

    fs::copy(&session_file, &archive_path).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            session_file.display(),
            archive_path.display()
        )
    })?;

    eprintln!(
        "[archive] Copied {} session to {}",
        agent_str,
        archive_path.display()
    );

    Ok(Some(archive_path))
}

/// Archive a session from a known file path (used by summarize when the
/// capture file is the canonical source).
pub fn archive_from_path(src: &PathBuf, agent: &str, task_id: &str) -> Result<PathBuf> {
    let base = crate::config::memory_dir();
    let archive_dir = base.join("archive").join(agent);
    fs::create_dir_all(&archive_dir)?;

    let archive_path = archive_dir.join(format!("{}.jsonl", task_id));
    fs::copy(src, &archive_path).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            src.display(),
            archive_path.display()
        )
    })?;

    Ok(archive_path)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn agent_short_name(agent: &AgentType) -> String {
    match agent {
        AgentType::ClaudeCode => "claude".to_string(),
        AgentType::OpenCode => "opencode".to_string(),
        AgentType::Hermes => "hermes".to_string(),
        AgentType::Pi => "pi".to_string(),
        AgentType::Unknown(s) => s.clone(),
    }
}

/// Scan agent-specific directories for a session file matching the session_id.
fn find_agent_session_file(agent: &AgentType, session_id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;

    match agent {
        AgentType::ClaudeCode => find_claude_session(&home, session_id),
        AgentType::Pi => find_pi_session(&home, session_id),
        AgentType::OpenCode => find_opencode_session(&home, session_id),
        AgentType::Hermes => None, // Hermes sessions are inside the sandbox container
        AgentType::Unknown(_) => {
            // Try all known agents
            find_claude_session(&home, session_id)
                .or_else(|| find_pi_session(&home, session_id))
                .or_else(|| find_opencode_session(&home, session_id))
        }
    }
}

fn find_claude_session(home: &PathBuf, session_id: &str) -> Option<PathBuf> {
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.is_dir() {
        return None;
    }

    // Scan all project subdirectories
    for entry in fs::read_dir(&projects_dir).ok()? {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        let candidate = entry.path().join(format!("{}.jsonl", session_id));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn find_pi_session(home: &PathBuf, session_id: &str) -> Option<PathBuf> {
    let sessions_dir = home.join(".pi").join("agent").join("sessions");
    if !sessions_dir.is_dir() {
        return None;
    }

    // Scan all hash subdirectories
    for entry in fs::read_dir(&sessions_dir).ok()? {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        // Pi files are named: <timestamp>_<session-id>.jsonl
        for file in fs::read_dir(entry.path()).ok()? {
            let file = file.ok()?;
            let name = file.file_name().to_string_lossy().to_string();
            if name.ends_with(&format!("_{}.jsonl", session_id)) {
                return Some(file.path());
            }
        }
    }
    None
}

fn find_opencode_session(home: &PathBuf, session_id: &str) -> Option<PathBuf> {
    let data_dir = home.join(".local").join("share").join("opencode");
    if !data_dir.is_dir() {
        return None;
    }
    let candidate = data_dir.join(format!("{}.json", session_id));
    if candidate.exists() {
        return Some(candidate);
    }
    None
}
