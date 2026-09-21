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
/// `numbered` prefixes `N)` keys for the interactive watch mode (max 9).
pub(crate) fn render_status(tasks: &[&Task], width: usize, numbered: bool) -> String {
    let mut out = String::new();
    for (i, task) in tasks.iter().enumerate() {
        let state = sidebar_state(task);
        let icon = state_icon(&state);
        let (_emoji, agent_label) = agent_display(&task.agent_type);
        let location = task
            .container_name
            .as_deref()
            .map(|c| format!("sandbox {}", &c[..12.min(c.len())]))
            .unwrap_or_else(|| "host".to_string());

        let key = if numbered && i < 9 {
            format!("{}\u{1b}[2m)\u{1b}[0m", i + 1)
        } else {
            " ".to_string()
        };
        let title_width = width.saturating_sub(4).max(10);
        out.push_str(&format!(
            "{icon}{key} {}\n",
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

/// The zellij pane id recorded for this task — written by the wrappers at
/// agent start (`report start --zellij-pane-id`) and by sandbox attach.
pub(crate) fn pane_id_of(task: &Task) -> Option<u32> {
    task.context
        .as_ref()?
        .extra
        .get("zellij_pane_id")?
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
}

fn focus_pane_id(pane_id: u32) -> Result<()> {
    let status = std::process::Command::new("zellij")
        .args(["action", "focus-pane-id"])
        .arg(pane_id.to_string())
        .status()
        .context("failed to run `zellij action focus-pane-id`")?;
    if !status.success() {
        anyhow::bail!("zellij action focus-pane-id exited with {status}");
    }
    Ok(())
}

/// Resolve a task by full ID or unique ID prefix.
fn resolve_task(db: &Database, target: &str) -> Result<Task> {
    if let Some(t) = db.get_task_by_id(target)? {
        return Ok(t);
    }
    let tasks = collect_status_tasks(db, true)?;
    let matches: Vec<&Task> = tasks.iter().filter(|t| t.task_id.starts_with(target)).collect();
    match matches.as_slice() {
        [t] => Ok((*t).clone()),
        [] => anyhow::bail!("no task matches '{target}'"),
        many => anyhow::bail!(
            "'{target}' is ambiguous ({}): {}",
            many.len(),
            many.iter().map(|t| &t.task_id[..8.min(t.task_id.len())]).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// `nibble goto <task>` — jump to the zellij pane hosting the agent.
pub(crate) fn cmd_goto(db: &Database, target: &str) -> Result<()> {
    if std::env::var_os("ZELLIJ").is_none() {
        anyhow::bail!("not inside a zellij session");
    }
    let task = resolve_task(db, target)?;
    match pane_id_of(&task) {
        Some(pane) => focus_pane_id(pane),
        None => anyhow::bail!(
            "no zellij pane recorded for task {} (host agents started before \
             the wrappers were installed, or a dead pane, have none)",
            &task.task_id[..8.min(task.task_id.len())]
        ),
    }
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

    // In watch mode grab single-key input so digits jump to the agent's pane.
    let mut raw = if watch { RawMode::enable() } else { None };
    let mut notice: Option<String> = None;

    loop {
        let tasks = collect_status_tasks(db, all)?;
        let body = if tasks.is_empty() {
            "  no active agents\n".to_string()
        } else {
            render_status(&tasks.iter().collect::<Vec<_>>(), width, watch)
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
        if let Some(n) = &notice {
            println!("\x1b[1;33m{n}\x1b[0m");
        }
        use std::io::Write;
        std::io::stdout().flush()?;

        if !watch {
            break;
        }

        notice = None;
        if raw.is_none() {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        match read_key(1000) {
            Some(Some(key)) => match key {
                b'q' | 0x03 => break,
                b'1'..=b'9' => {
                    let idx = (key - b'1') as usize;
                    notice = Some(match tasks.get(idx) {
                        Some(t) => match pane_id_of(t) {
                            Some(pane) => focus_pane_id(pane)
                                .map_err(|e| e.to_string())
                                .err()
                                .unwrap_or_else(|| format!("→ {}", t.title.trim())),
                            None => format!("no pane recorded for {}", t.title.trim()),
                        },
                        None => format!("no row {idx}"),
                    });
                }
                _ => {}
            },
            // stdin went away (pane closed, piped input) — stop polling
            Some(None) => raw = None,
            None => {}
        }
    }
    drop(raw);
    Ok(())
}

/// Put stdin in cbreak mode; restored on drop (also on panic/unwind).
struct RawMode {
    orig: libc::termios,
}

impl RawMode {
    fn enable() -> Option<Self> {
        // SAFETY: tcgetattr/tcsetattr on fd 0 with a valid termios pointer.
        unsafe {
            let mut orig = std::mem::zeroed();
            if libc::tcgetattr(0, &mut orig) != 0 {
                return None;
            }
            let mut raw = orig;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode { orig })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restoring the previously-saved termios on fd 0.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.orig);
        }
    }
}
/// Wait up to `timeout_ms` for a byte on stdin.
/// `Some(Some(b))` = key, `Some(None)` = stdin closed/EOF (stop reading),
/// `None` = nothing arrived in time.
fn read_key(timeout_ms: i32) -> Option<Option<u8>> {
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll(2) on a single stack pollfd.
    let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if ready <= 0 {
        return None;
    }
    let mut buf = [0u8; 1];
    // SAFETY: read(2) one byte from fd 0.
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, 1) };
    match n {
        1 => Some(Some(buf[0])),
        0 => Some(None), // EOF — stdin closed, poll would spin forever
        _ => None,       // read error (e.g. EINTR) — treat as no key
    }
}

/// The zellij layout managed by `nibble sidebar --install`.
const SIDEBAR_LAYOUT: &str = r#"// managed by nibble — sidebar on every tab. Remove with: nibble sidebar --uninstall
layout {
    default_tab_template {
        pane split_direction="vertical" {
            pane size="22%" name="agents" {
                command "nibble"
                args "status" "--watch"
            }
            children
        }
    }
}
"#;

const MANAGED_MARKER: &str = "// managed by nibble";

fn zellij_config_dir() -> std::path::PathBuf {
    std::env::var_os("ZELLIJ_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config/zellij")))
        .unwrap_or_else(|| std::path::PathBuf::from(".config/zellij"))
}

/// Install the always-on sidebar: every tab of every new zellij session gets
/// a narrow `nibble status --watch` pane on the right.
fn sidebar_install() -> Result<()> {
    let layouts_dir = zellij_config_dir().join("layouts");
    std::fs::create_dir_all(&layouts_dir)?;

    let default_kdl = layouts_dir.join("default.kdl");
    if default_kdl.exists() {
        let existing = std::fs::read_to_string(&default_kdl)?;
        if existing.contains(MANAGED_MARKER) {
            std::fs::write(&default_kdl, SIDEBAR_LAYOUT)?;
            println!("Updated {}", default_kdl.display());
        } else {
            // Never clobber a hand-written layout: install side-by-side.
            let nibble_kdl = layouts_dir.join("nibble.kdl");
            std::fs::write(&nibble_kdl, SIDEBAR_LAYOUT)?;
            println!("You already have a custom {}", default_kdl.display());
            println!("Wrote {} instead. To use it:\n", nibble_kdl.display());
            println!("  zellij --layout nibble        # per session");
            println!("  # or in ~/.config/zellij/config.kdl:");
            println!("  default_layout \"nibble\"\n");
            println!("Or merge this into your default.kdl:\n");
            print!("{SIDEBAR_LAYOUT}");
        }
    } else {
        std::fs::write(&default_kdl, SIDEBAR_LAYOUT)?;
        println!("Wrote {}", default_kdl.display());
    }

    println!();
    println!("New zellij sessions (and every tab opened in them) now get the");
    println!("sidebar. Restart zellij to pick it up — running sessions keep");
    println!("their current layout.");
    println!();
    println!("In the sidebar: 1-9 jumps to that agent's pane, q quits.");
    Ok(())
}

/// Remove nibble-managed zellij layouts.
fn sidebar_uninstall() -> Result<()> {
    let layouts_dir = zellij_config_dir().join("layouts");
    for name in ["default.kdl", "nibble.kdl"] {
        let path = layouts_dir.join(name);
        if !path.exists() {
            continue;
        }
        let body = std::fs::read_to_string(&path)?;
        if body.contains(MANAGED_MARKER) {
            std::fs::remove_file(&path)?;
            println!("Removed {}", path.display());
        } else {
            println!("Skipping {} (not nibble-managed)", path.display());
        }
    }
    Ok(())
}

/// Open the status side panel as a zellij pane in the current session.
pub(crate) fn cmd_sidebar(install: bool, uninstall: bool) -> Result<()> {
    if install {
        return sidebar_install();
    }
    if uninstall {
        return sidebar_uninstall();
    }

    if std::env::var_os("ZELLIJ").is_none() {
        println!("Not inside a zellij session. To get the side panel:\n");
        println!("  1. nibble sidebar --install   # every tab of every new session (recommended)");
        println!("  2. Start zellij, then run:    nibble sidebar  (current tab only)");
        println!("  3. Or run `nibble status --watch` in any narrow pane");
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
        let out = render_status(&[&t], 30, false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "two lines per task");
        assert!(out.contains("Pi"), "agent label present");
        // truncation must not split a multibyte char (would panic or garble)
        assert!(lines[0].chars().count() <= 30 + 4, "respects width");
    }

    #[test]
    fn render_numbered_shows_keys() {
        let t = task();
        let out = render_status(&[&t], 30, true);
        assert!(out.contains("1\x1b[2m)\x1b[0m"), "numbered key prefix");
        let plain = render_status(&[&t], 30, false);
        assert!(!plain.contains("1\u{1b}[2m)"), "plain render has no keys");
    }

    #[test]
    fn pane_id_read_from_context_extra() {
        let mut t = task();
        assert_eq!(pane_id_of(&t), None, "no context → no pane");
        let mut ctx = crate::models::TaskContext {
            url: None,
            project_path: None,
            session_id: None,
            claude_session_id: None,
            extra: std::collections::HashMap::new(),
        };
        ctx.extra.insert(
            "zellij_pane_id".to_string(),
            serde_json::Value::Number(42u32.into()),
        );
        t.context = Some(ctx);
        assert_eq!(pane_id_of(&t), Some(42));
    }

    #[test]
    fn resolve_task_by_prefix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = Database::open(tmp.path().join("t.db")).unwrap();
        let mut a = task();
        a.task_id = "aaaa1111-0000-0000-0000-000000000000".to_string();
        let mut b = task();
        b.task_id = "bbbb2222-0000-0000-0000-000000000000".to_string();
        db.insert_task(&a).unwrap();
        db.insert_task(&b).unwrap();

        let hit = resolve_task(&db, "aaaa").unwrap();
        assert_eq!(hit.task_id, a.task_id);
        assert!(resolve_task(&db, "zzzz").is_err(), "unknown prefix errors");
        assert!(resolve_task(&db, "0").is_err(), "ambiguous/unknown errors");
    }
}
