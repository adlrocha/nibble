//! Task display formatting helpers and path utilities.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::models::{AgentType, Task};

/// Expand a leading `~` (or `~/...`) to the user's home directory.
///
/// Home resolution: `dirs::home_dir()` first, `$HOME` env var as fallback;
/// errors if neither yields a home directory. Paths without a leading `~`
/// are returned unchanged.
pub(crate) fn expand_tilde(path: &str) -> Result<PathBuf> {
    if path != "~" && !path.starts_with("~/") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs::home_dir()
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        .context("Cannot determine home directory for ~ expansion")?;
    if path == "~" {
        Ok(home)
    } else {
        Ok(home.join(&path[2..]))
    }
}

/// Returns (emoji, display label) for an agent type string.
// Reused by the status table (Phase C) — currently only exercised in tests.
#[allow(dead_code)]
pub(crate) fn agent_display(agent_type: &AgentType) -> (&'static str, String) {
    match agent_type {
        AgentType::ClaudeCode => ("🤖", "Claude Code".to_string()),
        AgentType::Hermes => ("🧠", "Hermes".to_string()),
        AgentType::Pi => ("🥧", "Pi".to_string()),
        AgentType::Omp => ("⬡", "omp".to_string()),
        AgentType::Unknown(s) => ("🔧", s.clone()),
    }
}

/// Derive a human-readable location string from the task.
///
/// The wrapper sets `title` as `[repo:branch]` or `[dirname]`.  We parse that
/// to get repo + branch, and fall back to the project path from context.
// Reused by the status table (Phase C) — currently only exercised in tests.
#[allow(dead_code)]
pub(crate) fn format_location(task: &Task) -> String {
    // Try to parse [repo:branch] or [repo] from the title.
    let title = task.title.trim();
    if title.starts_with('[') && title.ends_with(']') {
        let inner = &title[1..title.len() - 1];
        if let Some((repo, branch)) = inner.split_once(':') {
            return format!("<code>{}</code> · <i>{}</i>", repo, branch);
        }
        // [dirname] — no branch info
        return format!("<code>{}</code>", inner);
    }

    // Fallback: use project_path from context, show only the last component.
    if let Some(ctx) = &task.context {
        if let Some(path) = &ctx.project_path {
            let dir = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path.as_str());
            return format!("<code>{}</code>", dir);
        }
    }

    // Last resort: show raw title.
    title.to_string()
}

/// Format the elapsed time since task creation as a human-readable string.
// Reused by the status table (Phase C) — currently only exercised in tests.
#[allow(dead_code)]
pub(crate) fn format_elapsed(task: &Task) -> String {
    let secs = (chrono::Utc::now() - task.created_at).num_seconds().max(0) as u64;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn make_task(agent_type: &str, title: &str) -> Task {
        Task::new(
            "test-id".to_string(),
            AgentType::from_str(agent_type).unwrap(),
            title.to_string(),
            None,
            None,
        )
    }

    #[test]
    fn test_format_location_repo_branch() {
        let task = make_task("claude_code", "[nibble:main]");
        let loc = format_location(&task);
        assert!(loc.contains("nibble"));
        assert!(loc.contains("main"));
    }

    #[test]
    fn test_format_location_repo_only() {
        let task = make_task("claude_code", "[my-project]");
        let loc = format_location(&task);
        assert!(loc.contains("my-project"));
    }

    #[test]
    fn test_format_location_fallback_to_path() {
        use crate::models::TaskContext;
        use std::collections::HashMap;
        let mut task = make_task("pi", "pi (interactive)");
        task.context = Some(TaskContext {
            url: None,
            project_path: Some("/home/user/projects/my-app".to_string()),
            session_id: None,
            claude_session_id: None,
            extra: HashMap::new(),
        });
        let loc = format_location(&task);
        assert!(loc.contains("my-app"));
    }

    #[test]
    fn test_agent_display_known_types() {
        assert_eq!(
            agent_display(&AgentType::ClaudeCode),
            ("🤖", "Claude Code".to_string())
        );
        assert_eq!(
            agent_display(&AgentType::Hermes),
            ("🧠", "Hermes".to_string())
        );
    }

    #[test]
    fn test_agent_display_unknown_type() {
        let (emoji, label) = agent_display(&AgentType::Unknown("my_custom_agent".to_string()));
        assert_eq!(emoji, "🔧");
        assert_eq!(label, "my_custom_agent".to_string());
    }

    #[test]
    fn test_format_elapsed_seconds() {
        let task = make_task("claude_code", "[repo:main]");
        // Task was just created so elapsed should be ~0s
        let elapsed = format_elapsed(&task);
        assert!(elapsed.ends_with('s'));
    }
}
