//! Live agent-status panel: `nibble status` / `nibble sidebar` and the
//! `nibble report status` verb that agent hooks/extensions call.
//!
//! Producers (claude hooks, pi/omp extension events, wrappers) report status
//! transitions; this module validates and persists them, renders a compact
//! two-line-per-task table for narrow panes, and can open the view as a
//! zellij side panel in the current session.

use anyhow::{Context, Result};

use crate::db::Database;
use crate::format::agent_display;
use crate::models::{Task, TaskStatus};

/// Apply a hook-reported status transition to a task.
/// Returns `false` for an unknown state string (caller reports and fails);
/// unknown *task IDs* are handled by the caller before this runs.
pub(crate) fn apply_status_transition(
    task: &mut Task,
    state: &str,
    message: Option<&str>,
) -> bool {
    let now = chrono::Utc::now();
    match state {
        "running" => {
            task.status = TaskStatus::Running;
            task.completed_at = None;
            task.attention_reason = None;
        }
        "blocked" => {
            task.status = TaskStatus::Completed;
            task.completed_at = Some(now);
            task.attention_reason = message
                .map(|m| m.chars().take(200).collect::<String>())
                .filter(|m| !m.is_empty())
                .or_else(|| Some("Needs input".to_string()));
        }
        "completed" => {
            task.status = TaskStatus::Completed;
            task.completed_at = Some(now);
            task.attention_reason = None;
        }
        "exited" => {
            task.set_exited(None);
            return true; // set_exited already stamps updated_at
        }
        _ => return false,
    }
    task.updated_at = now;
    true
}

/// Coarse display state for the sidebar, ordered by urgency for sorting.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SidebarState {
    Blocked,
    Running,
    Idle,
    Exited,
}

pub(crate) fn sidebar_state(task: &Task) -> SidebarState {
    match task.status {
        TaskStatus::Exited => SidebarState::Exited,
        _ if task.attention_reason.is_some() => SidebarState::Blocked,
        TaskStatus::Running => SidebarState::Running,
        TaskStatus::Completed => SidebarState::Idle,
    }
}

fn state_icon(state: &SidebarState) -> &'static str {
    match state {
        SidebarState::Blocked => "\u{1F534}", // red
        SidebarState::Running => "\u{1F7E2}", // green
        SidebarState::Idle => "\u{26AA}",     // white
        SidebarState::Exited => "\u{26AB}",   // black
    }
}

/// Host-process liveness (sandbox tasks have no meaningful host pid).
fn pid_alive(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        true
    }
}

fn relative_age(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - ts).num_seconds().max(0) as u64;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let taken: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

/// Render the status list for a terminal `width` columns wide.
/// Two lines per task so narrow (sidebar) panes stay readable.
pub(crate) fn render_status(tasks: &[&Task], width: usize) -> String {
    let mut out = String::new();
    for task in tasks {
        let state = sidebar_state(task);
        let icon = state_icon(&state);
        let (_emoji, agent_label) = agent_display(&task.agent_type);
        let location = task
            .container_name
            .as_deref()
            .map(|c| format!("sandbox {}", &c[..12.min(c.len())]))
            .unwrap_or_else(|| "host".to_string());

        let title_width = width.saturating_sub(4).max(10);
        out.push_str(&format!(
            "{} {}\n",
            icon,
            truncate_chars(task.title.trim(), title_width)
        ));

        let detail = match &state {
            SidebarState::Blocked => format!(
                "{} · {} · {}",
                agent_label,
                location,
                truncate_chars(
                    task.attention_reason.as_deref().unwrap_or("needs input"),
                    width.saturating_sub(14).max(10)
                )
            ),
            _ => format!(
                "{} · {} · {}",
                agent_label,
                location,
                relative_age(task.updated_at)
            ),
        };
        if state == SidebarState::Exited {
            out.push_str(&format!("\x1b[2m   {detail}\x1b[0m\n"));
        } else {
            out.push_str(&format!("   {detail}\n"));
        }
    }
    out
}

/// Collect tasks for display: reconcile dead host pids, drop stale exited
/// tasks (unless `include_exited` and fresh), sort by urgency then recency.
pub(crate) fn collect_status_tasks(db: &Database, include_exited: bool) -> Result<Vec<Task>> {
    let mut tasks = db.list_tasks()?;
    for task in &mut tasks {
        if task.status != TaskStatus::Exited && task.container_name.is_none() {
            if let Some(pid) = task.pid {
                if !pid_alive(pid) {
                    task.set_exited(None);
                    let _ = db.update_task(task);
                }
            }
        }
    }
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(1);
    tasks.retain(|t| {
        t.status != TaskStatus::Exited || (include_exited && t.updated_at > cutoff)
    });
    tasks.sort_by(|a, b| {
        sidebar_state(a)
            .cmp(&sidebar_state(b))
            .then(b.updated_at.cmp(&a.updated_at))
    });
    Ok(tasks)
}

fn status_json(tasks: &[Task]) -> Result<String> {
    let rows: Vec<serde_json::Value> = tasks
        .iter()
        .map(|t| {
            serde_json::json!({
                "task_id": t.task_id,
                "title": t.title,
                "agent": t.agent_type.as_str(),
                "state": match sidebar_state(t) {
                    SidebarState::Blocked => "blocked",
                    SidebarState::Running => "running",
                    SidebarState::Idle => "idle",
                    SidebarState::Exited => "exited",
                },
                "attention_reason": t.attention_reason,
                "sandbox": t.container_name.is_some(),
                "container_name": t.container_name,
                "updated_at": t.updated_at,
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&rows)?)
}

pub(crate) fn cmd_status(db: &Database, watch: bool, json: bool, all: bool) -> Result<()> {
    if json {
        let tasks = collect_status_tasks(db, all)?;
        println!("{}", status_json(&tasks)?);
        return Ok(());
    }

    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(40)
        .max(20);

    loop {
        let tasks = collect_status_tasks(db, all)?;
        let refs: Vec<&Task> = tasks.iter().collect();
        let body = if refs.is_empty() {
            "  no active agents\n".to_string()
        } else {
            render_status(&refs, width)
        };

        if watch {
            // Clear screen, cursor home.
            print!("\x1b[2J\x1b[H");
            println!(
                "\x1b[1m🍪 nibble agents\x1b[0m  {}",
                chrono::Utc::now().format("%H:%M:%S")
            );
            println!();
        }
        print!("{body}");
        use std::io::Write;
        std::io::stdout().flush()?;

        if !watch {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    Ok(())
}

/// Open the status side panel as a zellij pane in the current session.
pub(crate) fn cmd_sidebar() -> Result<()> {
    if std::env::var_os("ZELLIJ").is_none() {
        println!("Not inside a zellij session. To get the side panel:\n");
        println!("  1. Start zellij, then run:  nibble sidebar");
        println!("  2. Or run `nibble status --watch` in any narrow pane");
        println!("  3. Or add a permanent sidebar to your zellij layout:\n");
        println!("     pane split_direction=\"vertical\" {{");
        println!("       pane size=\"15%\" name=\"agents\" {{ command \"nibble\"; args \"status\" \"--watch\" }}");
        println!("       pane");
        println!("     }}");
        return Ok(());
    }

    let nibble_bin = std::env::current_exe()?;
    let status = std::process::Command::new("zellij")
        .args([
            "action",
            "new-pane",
            "--direction",
            "right",
            "--name",
            "agents",
            "--close-on-exit",
            "--",
        ])
        .arg(&nibble_bin)
        .args(["status", "--watch"])
        .status()
        .context("failed to run `zellij action new-pane`")?;
    if !status.success() {
        anyhow::bail!("zellij action new-pane exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AgentType;
    use std::str::FromStr;

    fn task() -> Task {
        Task::new(
            "t1".to_string(),
            AgentType::from_str("pi").unwrap(),
            "Test task".to_string(),
            None,
            None,
        )
    }

    #[test]
    fn status_transition_running_clears_attention() {
        let mut t = task();
        t.attention_reason = Some("stale".into());
        assert!(apply_status_transition(&mut t, "running", None));
        assert_eq!(t.status, TaskStatus::Running);
        assert!(t.attention_reason.is_none());
        assert!(t.completed_at.is_none());
    }

    #[test]
    fn status_transition_blocked_sets_reason() {
        let mut t = task();
        assert!(apply_status_transition(
            &mut t,
            "blocked",
            Some("permission prompt: rm -rf")
        ));
        assert!(t.attention_reason.is_some());
        assert_eq!(t.attention_reason.as_deref(), Some("permission prompt: rm -rf"));
        // blocked sorts first
        assert_eq!(sidebar_state(&t), SidebarState::Blocked);
    }

    #[test]
    fn status_transition_blocked_default_reason() {
        let mut t = task();
        assert!(apply_status_transition(&mut t, "blocked", None));
        assert_eq!(t.attention_reason.as_deref(), Some("Needs input"));
        // empty message falls back too
        assert!(apply_status_transition(&mut t, "blocked", Some("")));
        assert_eq!(t.attention_reason.as_deref(), Some("Needs input"));
    }

    #[test]
    fn status_transition_unknown_state_rejected() {
        let mut t = task();
        assert!(!apply_status_transition(&mut t, "bogus", None));
        assert!(!apply_status_transition(&mut t, "RUNNING", None));
    }

    #[test]
    fn render_two_lines_and_multibyte_safe() {
        let mut t = task();
        t.title = "🚧 emoji 标题 that is quite long indeed".to_string();
        let out = render_status(&[&t], 30);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "two lines per task");
        assert!(out.contains("Pi"), "agent label present");
        // truncation must not split a multibyte char (would panic or garble)
        assert!(lines[0].chars().count() <= 30 + 4, "respects width");
    }
}
