//! Notification backends for nibble.

pub mod telegram;
pub mod telegram_listener;

// Notification text builders (extracted verbatim from the pre-split
// main.rs during the origin/main integration).
use anyhow::Result;
use crate::db::Database;
use crate::models::{AgentType, TaskStatus};
use crate::models::Task;

pub(crate) fn build_notification_text(
    db: &Database,
    task_id: Option<&str>,
    message: &str,
    attention: bool,
) -> Result<String> {
    if let Some(id) = task_id {
        if let Some(task) = db.get_task_by_id(id)? {
            let header = format_header(&task, attention);
            let divider = "─".repeat(28);
            return Ok(format!("{}\n{}\n{}", header, divider, message));
        }
    }

    // No task context — still show the attention banner if requested.
    if attention {
        let banner = "🚨 <b>Agent needs your attention</b>";
        let divider = "─".repeat(28);
        return Ok(format!("{}\n{}\n{}", banner, divider, message));
    }

    Ok(message.to_string())
}

/// Build the rich header block for a task notification.
///
/// `attention` = true adds a prominent "needs your attention" banner above the
/// standard context lines so it is immediately obvious on a phone screen.
///
/// Normal completion example:
/// ```
/// 🤖 Claude Code
/// 📁 nibble · main
/// ⏱ 4m 32s
/// ```
///
/// Attention example:
/// ```
/// 🚨 Needs your attention
/// 🤖 Claude Code
/// 📁 nibble · main
/// ⏱ 4m 32s
/// ```
fn format_header(task: &Task, attention: bool) -> String {
    let (agent_emoji, agent_label) = agent_display(&task.agent_type);
    let elapsed = format_elapsed(task);
    let location = format_location(task);

    let mut lines: Vec<String> = Vec::new();

    if attention {
        lines.push("🚨 <b>Needs your attention</b>".to_string());
    }

    lines.push(format!("{} <b>{}</b>", agent_emoji, agent_label));
    lines.push(format!("📁 {}", location));
    lines.push(format!("⏱ {}", elapsed));

    // Append status only when it carries meaning (exited = something went wrong).
    if task.status == TaskStatus::Exited {
        let exit_str = task
            .exit_code
            .map(|c| format!(" (exit {})", c))
            .unwrap_or_default();
        lines.push(format!("⚠️ <i>session ended{}</i>", exit_str));
    }

    lines.join("\n")
}

/// Returns (emoji, display label) for an agent type string.
fn agent_display(agent_type: &AgentType) -> (&'static str, String) {
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
fn format_location(task: &Task) -> String {
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
fn format_elapsed(task: &Task) -> String {
    let secs = (chrono::Utc::now() - task.created_at).num_seconds().max(0) as u64;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}


