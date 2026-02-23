mod cli;
mod config;
mod db;
mod display;
mod models;
mod monitor;
mod notifications;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands, ReportAction};
use db::Database;
use models::{Task, TaskContext, TaskStatus};
use std::str::FromStr;
use std::collections::HashMap;
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Ensure data directory exists
    db::ensure_data_dir()?;

    // Open database
    let db_path = db::default_db_path();
    let db = Database::open(&db_path).context("Failed to open database")?;

    // Run cleanup on every invocation
    let _ = db.cleanup_old_completed(3600); // 1 hour default

    match cli.command {
        None => {
            // Default: show running tasks (actively generating)
            let tasks = db.list_tasks(Some(TaskStatus::Running))?;
            display::display_task_list(&tasks);
        }
        Some(Commands::List { all, status }) => {
            let tasks = if let Some(status_str) = status {
                let status = TaskStatus::from_str(&status_str)
                    .map_err(|e| anyhow::anyhow!(e))?;
                db.list_tasks(Some(status))?
            } else if all {
                db.list_tasks(None)?
            } else {
                // Show running tasks by default
                db.list_tasks(Some(TaskStatus::Running))?
            };

            display::display_task_list(&tasks);
        }
        Some(Commands::Show { task_id }) => {
            let task = db
                .get_task_by_id(&task_id)?
                .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

            display::display_task_detail(&task);
        }
        Some(Commands::Clear { task_id }) => {
            let deleted = db.delete_task(&task_id)?;
            if deleted {
                println!("Task {} cleared", task_id);
            } else {
                println!("Task not found: {}", task_id);
            }
        }
        Some(Commands::ClearAll) => {
            let completed = db.list_tasks(Some(TaskStatus::Completed))?;
            let exited = db.list_tasks(Some(TaskStatus::Exited))?;

            let mut count = 0;
            for task in completed.iter().chain(exited.iter()) {
                db.delete_task(&task.task_id)?;
                count += 1;
            }

            println!("Cleared {} tasks", count);
        }
        Some(Commands::Reset { force }) => {
            let all_tasks = db.list_tasks(None)?;
            let task_count = all_tasks.len();

            if task_count == 0 {
                println!("No tasks to clear.");
                return Ok(());
            }

            // Show what will be cleared
            println!("This will delete ALL {} tasks:", task_count);
            for task in &all_tasks {
                println!("  - [{}] {}", task.agent_type, task.title);
            }
            println!();

            // Confirm unless --force
            if !force {
                use std::io::{self, Write};
                print!("Are you sure you want to delete ALL tasks? (yes/no): ");
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;

                if input.trim().to_lowercase() != "yes" {
                    println!("Aborted. No tasks were deleted.");
                    return Ok(());
                }
            }

            // Delete all tasks
            let mut count = 0;
            for task in all_tasks {
                db.delete_task(&task.task_id)?;
                count += 1;
            }

            println!("✓ Cleared all {} tasks", count);
        }
        Some(Commands::Watch) => {
            println!("Watching tasks (Ctrl+C to exit)...\n");

            loop {
                // Clear screen
                print!("\x1B[2J\x1B[1;1H");

                let tasks = db.list_tasks(None)?;
                display::display_task_list(&tasks);

                thread::sleep(Duration::from_secs(2));
            }
        }
        Some(Commands::Cleanup { retention_secs }) => {
            let deleted = db.cleanup_old_completed(retention_secs)?;
            println!("Cleaned up {} old completed tasks", deleted);
        }
        Some(Commands::Report { action }) => match action {
            ReportAction::Start {
                task_id,
                agent_type,
                cwd,
                title,
                pid,
                ppid,
            } => {
                let mut task = Task::new(task_id, agent_type, title, pid, ppid);

                // Add context
                task.context = Some(TaskContext {
                    url: None,
                    project_path: Some(cwd),
                    session_id: None,
                    extra: HashMap::new(),
                });

                db.insert_task(&task)?;
                println!("Task started: {}", task.task_id);
            }
            ReportAction::Complete { task_id, exit_code } => {
                let mut task = db
                    .get_task_by_id(&task_id)?
                    .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

                // If exit_code is provided and non-zero, mark as exited (failed)
                // Otherwise mark as completed (finished generating)
                if let Some(code) = exit_code {
                    if code != 0 {
                        task.set_exited(Some(code));
                    } else {
                        task.complete();
                    }
                } else {
                    task.complete();
                }
                db.update_task(&task)?;
                println!("Task completed: {}", task_id);
            }
            ReportAction::Running { task_id } => {
                let mut task = db
                    .get_task_by_id(&task_id)?
                    .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

                task.set_running();
                db.update_task(&task)?;
                println!("Task running: {}", task_id);
            }
            ReportAction::Exited { task_id, exit_code } => {
                let mut task = db
                    .get_task_by_id(&task_id)?
                    .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

                task.set_exited(exit_code);
                db.update_task(&task)?;
                println!("Task exited: {}", task_id);
            }
        },
        Some(Commands::Monitor { task_id, pid }) => {
            // Create a monitor and start monitoring
            let monitor = monitor::TaskMonitor::new(db);
            monitor.monitor_task(task_id, pid)?;
        }
        Some(Commands::Notify { message, task_id, attention }) => {
            let cfg = config::load().unwrap_or_default();

            if !cfg.telegram.is_configured() {
                eprintln!(
                    "Telegram notifications are not configured. \
                     Run scripts/setup-telegram.sh to set them up."
                );
                // Exit cleanly — missing config is not a fatal error for hooks.
                return Ok(());
            }

            // Build the notification text: header with task context + message body.
            let text = build_notification_text(&db, task_id.as_deref(), &message, attention)?;

            notifications::telegram::send(&cfg.telegram, &text)
                .context("Failed to send Telegram notification")?;
        }
    }

    Ok(())
}

/// Build the full notification text sent to Telegram.
///
/// When a `task_id` is supplied and found in the database a rich header is
/// prepended so the user can immediately identify which agent and repo the
/// message comes from without reading the body.
///
/// `attention` = true produces a visually distinct header for permission
/// requests and questions that require an immediate response.
fn build_notification_text(
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
/// 📁 agent-inbox · main
/// ⏱ 4m 32s
/// ```
///
/// Attention example:
/// ```
/// 🚨 Needs your attention
/// 🤖 Claude Code
/// 📁 agent-inbox · main
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
fn agent_display(agent_type: &str) -> (&'static str, String) {
    match agent_type {
        "claude_code" => ("🤖", "Claude Code".to_string()),
        "opencode"    => ("⚡", "OpenCode".to_string()),
        "claude_web"  => ("🌐", "Claude Web".to_string()),
        "gemini_web"  => ("✨", "Gemini".to_string()),
        other         => ("🔧", other.to_string()),
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

#[cfg(test)]
mod notification_tests {
    use super::*;
    use crate::models::{Task, TaskStatus};

    fn make_task(agent_type: &str, title: &str) -> Task {
        Task::new(
            "test-id".to_string(),
            agent_type.to_string(),
            title.to_string(),
            None,
            None,
        )
    }

    #[test]
    fn test_format_location_repo_branch() {
        let task = make_task("claude_code", "[agent-inbox:main]");
        let loc = format_location(&task);
        assert!(loc.contains("agent-inbox"));
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
        let mut task = make_task("opencode", "opencode (interactive)");
        task.context = Some(TaskContext {
            url: None,
            project_path: Some("/home/user/projects/my-app".to_string()),
            session_id: None,
            extra: HashMap::new(),
        });
        let loc = format_location(&task);
        assert!(loc.contains("my-app"));
    }

    #[test]
    fn test_agent_display_known_types() {
        assert_eq!(agent_display("claude_code"), ("🤖", "Claude Code".to_string()));
        assert_eq!(agent_display("opencode"),    ("⚡", "OpenCode".to_string()));
        assert_eq!(agent_display("claude_web"),  ("🌐", "Claude Web".to_string()));
        assert_eq!(agent_display("gemini_web"),  ("✨", "Gemini".to_string()));
    }

    #[test]
    fn test_agent_display_unknown_type() {
        let (emoji, label) = agent_display("my_custom_agent");
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

    #[test]
    fn test_header_contains_agent_and_location() {
        let task = make_task("claude_code", "[myrepo:feature-x]");
        let header = format_header(&task, false);
        assert!(header.contains("Claude Code"));
        assert!(header.contains("myrepo"));
        assert!(header.contains("feature-x"));
        assert!(header.contains('⏱'));
        assert!(header.contains('📁'));
    }

    #[test]
    fn test_header_exited_task_shows_warning() {
        let mut task = make_task("opencode", "[proj:main]");
        task.status = TaskStatus::Exited;
        task.exit_code = Some(1);
        let header = format_header(&task, false);
        assert!(header.contains('⚠'));
        assert!(header.contains("exit 1"));
    }

    #[test]
    fn test_header_attention_shows_banner() {
        let task = make_task("claude_code", "[myrepo:main]");
        let header = format_header(&task, true);
        assert!(header.contains('🚨'));
        assert!(header.contains("Needs your attention"));
        // Standard fields still present
        assert!(header.contains("Claude Code"));
        assert!(header.contains("myrepo"));
    }

    #[test]
    fn test_attention_without_task_id_shows_banner() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let db = crate::db::Database::open(&db_path).unwrap();
        let text = build_notification_text(&db, None, "need permission", true).unwrap();
        assert!(text.contains('🚨'));
        assert!(text.contains("need permission"));
    }
}
