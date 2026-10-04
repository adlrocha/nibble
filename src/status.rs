//! Live agent-status panel: `nibble status` / `nibble sidebar` and the
//! `nibble report status` verb that agent hooks/extensions call.
//!
//! Producers (claude hooks, pi/omp extension events, wrappers) report status
//! transitions; this module validates and persists them, renders a compact
//! two-line-per-task table (plus a dim topic line from the agent's zellij pane
//! title when present) for narrow panes, and can open the view as a zellij
//! side panel in the current session.

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

/// DB-level status report used by quota-watch and other long-running
/// callers: resolves the task (self-healing an unknown ID so wrapper-less
/// sessions — derived `claude-<session_id>` / `omp-<session_id>` — get a
/// row on first report; `exited` on an unknown ID never creates one),
/// normalises the reporting vocabulary (`working` → `running`,
/// `idle` → `completed`), applies the transition and persists it.
pub(crate) fn apply_report(db: &Database, task_id: &str, state: &str, reason: Option<&str>) -> Result<()> {
    let mut task = match db.get_task_by_id(task_id)? {
        Some(t) => t,
        None if state == "exited" => {
            eprintln!(
                "report status: unknown task {} (exited — ignored)",
                &task_id[..8.min(task_id.len())]
            );
            return Ok(());
        }
        None => db.ensure_task_or_create(task_id)?,
    };
    let normalised = match state {
        "working" => "running",
        "idle" => "completed",
        other => other,
    };
    if apply_status_transition(&mut task, normalised, reason) {
        db.update_task(&task)?;
        Ok(())
    } else {
        anyhow::bail!("Invalid status state '{state}' (running, blocked, completed, exited)")
    }
}

/// Coarse display state for the sidebar, ordered by urgency for sorting.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SidebarState {
    Blocked,
    Running,
    Idle,
    Exited,
}

/// A Running row whose last report is older than this renders as idle: a
/// working agent reports every few seconds, so long silence means the
/// process died without a shutdown report (killed, crashed) — especially
/// self-healed rows, which carry no pid for the liveness reconcile to
/// check and would otherwise glow "working" forever. Display-only: the
/// DB keeps the last reported status.
const STALE_RUNNING_SECS: i64 = 30 * 60;

pub(crate) fn sidebar_state(task: &Task) -> SidebarState {
    match task.status {
        TaskStatus::Exited => SidebarState::Exited,
        _ if task.attention_reason.is_some() => SidebarState::Blocked,
        TaskStatus::Running
            if (chrono::Utc::now() - task.updated_at).num_seconds() > STALE_RUNNING_SECS =>
        {
            SidebarState::Idle
        }
        TaskStatus::Running => SidebarState::Running,
        TaskStatus::Completed => SidebarState::Idle,
    }
}

/// Traffic-light activity mark (restored from the pre-merge renderer of
/// 757585c/ebbbfb3): red `!` needs input, orange `●` working, green `●`
/// ready for input, dim `·` exited.
fn state_mark(state: &SidebarState, color: bool) -> String {
    let (glyph, code) = match state {
        SidebarState::Blocked => ("!", "1;31"),
        SidebarState::Running => ("●", "38;5;208"),
        SidebarState::Idle => ("●", "32"),
        SidebarState::Exited => ("·", "2"),
    };
    if color {
        format!("\x1b[{code}m{glyph}\x1b[0m")
    } else {
        glyph.to_string()
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

/// Topics reported by agent pane titles, keyed by task id (via
/// `AGENT_TASK_ID` in the pane command) and by pane id — the live
/// "what is this agent doing" line under each row.
pub(crate) type PaneTopics = (HashMap<String, String>, HashMap<u32, String>);

/// Render the status list for a terminal `width` columns wide.
/// Two lines per task so narrow (sidebar) panes stay readable, plus a dim
/// topic line when the agent's pane title reports one. `numbered` prefixes
/// `N)` keys for the interactive watch mode (max 9) and appends the legend.
pub(crate) fn render_status(
    tasks: &[&Task],
    width: usize,
    numbered: bool,
    color: bool,
    topics: Option<&PaneTopics>,
) -> String {
    if tasks.is_empty() {
        let mut out = "  no active agents\n".to_string();
        if numbered {
            if let Some(legend) = legend_line(width, color) {
                out.push('\n');
                out.push_str(&legend);
                out.push('\n');
            }
        }
        return out;
    }
    let mut out = String::new();
    for (i, task) in tasks.iter().enumerate() {
        let state = sidebar_state(task);
        let icon = state_mark(&state, color);
        let (_emoji, agent_label) = agent_display(&task.agent_type);
        let location = task
            .container_name
            .as_deref()
            .map(|c| format!("sandbox {}", &c[..12.min(c.len())]))
            .unwrap_or_else(|| {
                if task
                    .context
                    .as_ref()
                    .and_then(|c| c.extra.get("parent_task_id"))
                    .is_some()
                {
                    "sandbox window".to_string()
                } else {
                    "host".to_string()
                }
            });

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
        // Dim topic line from the agent's pane title, when one is reported.
        if let Some((by_task, by_pane)) = topics {
            let topic = by_task
                .get(&task.task_id)
                .or_else(|| pane_id_of(task).and_then(|id| by_pane.get(&id)))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            if let Some(topic) = topic {
                let text = truncate_chars(topic, width.saturating_sub(3).max(10));
                if color {
                    out.push_str(&format!("\x1b[2m   {text}\x1b[0m\n"));
                } else {
                    out.push_str(&format!("   {text}\n"));
                }
            }
        }
    }
    if numbered {
        if let Some(legend) = legend_line(width, color) {
            out.push('\n');
            out.push_str(&legend);
            out.push('\n');
        }
    }
    out
}

/// Explains the activity marks so the sidebar pane is self-describing.
/// Dropped when the pane is too narrow to fit even the compact form.
fn legend_line(width: usize, color: bool) -> Option<String> {
    let (needs, working, ready) = if width >= 36 {
        ("! needs input", "● working", "● ready")
    } else if width >= 27 {
        ("! input", "● busy", "● ready")
    } else {
        return None;
    };
    let paint = |code: &str, text: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    let sep = if color { "\x1b[2m · \x1b[0m" } else { " · " };
    Some(format!(
        " {}{sep}{}{sep}{}",
        paint("1;31", needs),
        paint("38;5;208", working),
        paint("32", ready),
    ))
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

/// Rename the zellij pane the caller is running in to the given title, so
/// agent panes are recognisable in the frame border. Best-effort: silently
/// no-ops outside zellij and never fails the caller (hooks must not break
/// agents). Runs from inside the pane being renamed (focused by definition).
pub(crate) fn rename_current_pane(title: &str) {
    if std::env::var_os("ZELLIJ").is_none() {
        return;
    }
    let title = title.trim();
    if title.is_empty() {
        return;
    }
    let _ = std::process::Command::new("zellij")
        .args(["action", "rename-pane"])
        .arg(title)
        .status();
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

    // COLUMNS is a shell-exported convenience; zellij panes run nibble
    // directly, so fall back to the real terminal size (TIOCGWINSZ) — the
    // sidebar pane is a fraction of the tab and a wrong width wraps lines.
    let env_width = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok());

    // In watch mode grab single-key input so digits jump to the agent's pane.
    let mut raw = if watch { RawMode::enable() } else { None };
    let mut notice: Option<String> = None;
    use std::io::IsTerminal;
    let color = std::io::stdout().is_terminal();

    loop {
        // Re-read each frame so pane resizes are picked up live.
        let width = env_width
            .or_else(|| terminal_size().map(|(cols, _)| cols as usize))
            .unwrap_or(40)
            .max(20);
        let tasks = collect_status_tasks(db, all)?;
        // Topic lines come from zellij pane titles — sidebar (watch) only,
        // matching the pre-merge renderer; a no-op outside zellij.
        let topics = if watch { Some(zellij_pane_topics()) } else { None };
        let body = render_status(
            &tasks.iter().collect::<Vec<_>>(),
            width,
            watch,
            color,
            topics.as_ref(),
        );

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
        match stdin_keys(1000) {
            Ok(keys) => {
                let mut quit = false;
                for key in keys {
                    match key {
                        b'q' | b'Q' | 0x03 | 0x04 => {
                            quit = true;
                            break;
                        }
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
                    }
                }
                if quit {
                    break;
                }
            }
            // stdin went away (pane closed, piped input) — stop polling
            Err(()) => raw = None,
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
/// Terminal size of stdout, from the kernel (TIOCGWINSZ) — the source of
/// truth in zellij panes, where no shell has exported COLUMNS.
fn terminal_size() -> Option<(u16, u16)> {
    // SAFETY: ioctl(2) on fd 1 with a valid winsize pointer.
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } != 0
        || ws.ws_col == 0
    {
        return None;
    }
    Some((ws.ws_col, ws.ws_row))
}

/// Keys pressed during the refresh window, read as one batch. Batches that
/// start with ESC (arrows, function keys, shift+arrows — CSI sequences
/// carry digits like `ESC [ 1 ; 2 C`) are dropped wholesale so embedded
/// digits never trigger a jump. `Err(())` means stdin is closed (EOF) —
/// the caller should stop polling.
fn stdin_keys(timeout_ms: i32) -> Result<Vec<u8>, ()> {
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll(2) on a single stack pollfd.
    let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if ready <= 0 {
        return Ok(Vec::new());
    }
    let mut buf = [0u8; 32];
    // SAFETY: read(2) up to 32 bytes from fd 0.
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    match n {
        0 => Err(()), // EOF — pane closed / piped input exhausted
        n if n > 0 => {
            if buf[0] == 0x1b {
                Ok(Vec::new()) // escape-prefixed sequence: drop the batch
            } else {
                Ok(buf[..n as usize].to_vec())
            }
        }
        _ => Ok(Vec::new()), // read error (e.g. EINTR) — treat as no keys
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
    use crate::models::TaskContext;
    use std::collections::HashMap;
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
    fn stale_running_row_renders_idle() {
        let mut stale = task();
        apply_status_transition(&mut stale, "running", None);
        stale.updated_at =
            chrono::Utc::now() - chrono::Duration::seconds(STALE_RUNNING_SECS + 60);
        assert_eq!(
            sidebar_state(&stale),
            SidebarState::Idle,
            "silent running row demotes to idle"
        );

        let mut fresh = task();
        apply_status_transition(&mut fresh, "running", None);
        assert_eq!(sidebar_state(&fresh), SidebarState::Running);

        // Blocked never demotes — a live blocked agent is silent by nature.
        let mut blocked = task();
        apply_status_transition(&mut blocked, "blocked", Some("approve bash"));
        blocked.updated_at =
            chrono::Utc::now() - chrono::Duration::seconds(STALE_RUNNING_SECS + 60);
        assert_eq!(sidebar_state(&blocked), SidebarState::Blocked);
    }

    #[test]
    fn render_two_lines_and_multibyte_safe() {
        let mut t = task();
        t.title = "🚧 emoji 标题 that is quite long indeed".to_string();
        let out = render_status(&[&t], 30, false, false, None);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "two lines per task: {out}");
        assert!(out.contains("Pi"), "agent label present");
        // truncation must not split a multibyte char (would panic or garble)
        assert!(lines[0].chars().count() <= 30 + 4, "respects width");
    }

    #[test]
    fn render_window_task_shows_sandbox_location() {
        let mut t = task();
        let ctx = t.context.get_or_insert_with(|| TaskContext {
            url: None,
            project_path: None,
            session_id: None,
            claude_session_id: None,
            extra: HashMap::new(),
        });
        ctx.extra.insert(
            "parent_task_id".to_string(),
            serde_json::Value::String("parent-1".to_string()),
        );
        let out = render_status(&[&t], 40, false, false, None);
        assert!(out.contains("sandbox window"), "window rows locate in the sandbox: {out}");
        assert!(!out.contains("host"), "window rows must not claim host: {out}");
    }

    #[test]
    fn render_plain_task_shows_host_location() {
        let t = task();
        let out = render_status(&[&t], 40, false, false, None);
        assert!(out.contains("host"), "plain rows keep the host location: {out}");
    }

    #[test]
    fn render_numbered_shows_keys() {
        let t = task();
        let out = render_status(&[&t], 30, true, false, None);
        assert!(out.contains("1\x1b[2m)\x1b[0m"), "numbered key prefix");
        let plain = render_status(&[&t], 30, false, false, None);
        assert!(!plain.contains("1\u{1b}[2m)"), "plain render has no keys");
    }

    #[test]
    fn render_marks_match_traffic_light_semantics() {
        let mut blocked = task();
        apply_status_transition(&mut blocked, "blocked", Some("approve bash"));
        let mut working = task();
        apply_status_transition(&mut working, "running", None);
        let mut idle = task();
        apply_status_transition(&mut idle, "completed", None);
        let mut gone = task();
        apply_status_transition(&mut gone, "exited", None);

        let out = render_status(&[&blocked, &working, &idle, &gone], 40, false, false, None);
        let first_lines: Vec<&str> = out.lines().step_by(2).collect();
        assert!(first_lines[0].starts_with('!'), "blocked shows ! : {out}");
        assert!(first_lines[1].starts_with('●'), "working shows ● : {out}");
        assert!(first_lines[2].starts_with('●'), "idle shows ● : {out}");
        assert!(first_lines[3].starts_with('·'), "exited shows · : {out}");

        let colored = render_status(&[&working], 40, false, true, None);
        assert!(colored.contains("\x1b[38;5;208m●\x1b[0m"), "working mark is orange: {colored}");
    }

    #[test]
    fn render_topic_line_from_pane_title() {
        let mut t = task(); // task_id "t1"
        let by_task: PaneTopics = (
            [("t1".to_string(), "fixing sidebar legend".to_string())].into(),
            [].into(),
        );
        let out = render_status(&[&t], 40, false, false, Some(&by_task));
        assert!(out.contains("fixing sidebar legend"), "topic by task id: {out}");
        assert_eq!(out.lines().count(), 3, "topic line added under the row");

        // Fallback: topic keyed by the recorded zellij pane id.
        let mut t2 = task();
        t2.context = Some(TaskContext {
            url: None,
            project_path: None,
            session_id: None,
            claude_session_id: None,
            extra: HashMap::from([(
                "zellij_pane_id".to_string(),
                serde_json::Value::Number(7u32.into()),
            )]),
        });
        let by_pane: PaneTopics = ([].into(), [(7u32, "by pane".to_string())].into());
        let out = render_status(&[&t2], 40, false, false, Some(&by_pane));
        assert!(out.contains("by pane"), "topic by pane id: {out}");

        // No topic reported → plain two-line row, no stray line.
        let out = render_status(&[&t], 40, false, false, None);
        assert_eq!(out.lines().count(), 2, "no topic, no extra line: {out}");
    }

    #[test]
    fn legend_shown_in_watch_mode_wide_panes_only() {
        let t = task();
        let wide = render_status(&[&t], 40, true, false, None);
        assert!(
            wide.contains("! needs input · ● working · ● ready"),
            "full legend in wide sidebar: {wide}"
        );
        let compact = render_status(&[&t], 30, true, false, None);
        assert!(
            compact.contains("! input · ● busy · ● ready"),
            "compact legend in narrow sidebar: {compact}"
        );
        let narrow = render_status(&[&t], 20, true, false, None);
        assert!(!narrow.contains("needs input"), "too narrow drops legend: {narrow}");
        let oneshot = render_status(&[&t], 40, false, false, None);
        assert!(!oneshot.contains("needs input"), "legend is watch-only: {oneshot}");
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


// ── Pane management (ported from the pre-split main.rs during the
// origin/main integration) ────────────────────────────────────────────────────
use std::collections::HashMap;
use std::path::PathBuf;

const SIDEBAR_PANE_NAME: &str = "nibble";

/// Wide enough to read agent rows without resizing, capped as a strip.
fn sidebar_target_cols(tab_cols: u16) -> u16 {
    let target = (tab_cols / 9).clamp(28, 48);
    let room = tab_cols.saturating_sub(24).max(16);
    target.min(room)
}

fn is_sidebar_pane(title: &str, command: Option<&str>) -> bool {
    title == SIDEBAR_PANE_NAME
        || title == "agents"
        || command.is_some_and(|cmd| {
            cmd.contains("nibble") && cmd.contains("status") && cmd.contains("--watch")
        })
}

/// zellij 0.45: `resize <increase|decrease> [left|right|up|down]`.
/// `--direction` is rejected. The border to shrink is the inner one.
fn resize_args(pane_id: &str, resize: &str, border: &str) -> Vec<String> {
    ["action", "resize", resize, border, "--pane-id", pane_id]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn shrink_args(pane_id: &str, border: &str) -> Vec<String> {
    resize_args(pane_id, "decrease", border)
}

#[derive(Debug, serde::Deserialize)]
struct ZellijPane {
    id: u32,
    #[serde(default)]
    is_plugin: bool,
    #[serde(default)]
    title: String,
    #[serde(default)]
    pane_columns: u16,
    #[serde(default)]
    pane_x: u16,
    #[serde(default)]
    tab_id: u32,
    #[serde(default)]
    pane_command: Option<String>,
}

fn zellij_panes() -> Result<Vec<ZellijPane>> {
    let output = std::process::Command::new("zellij")
        .args(["action", "list-panes", "--all", "--json"])
        .output()
        .context("zellij not available. Run `nibble status --watch` in a narrow pane.")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let detail = err.trim();
        anyhow::bail!(
            "Not inside zellij{}. Run `nibble status --watch` in a pane you size yourself.",
            if detail.is_empty() {
                String::new()
            } else {
                format!(" ({detail})")
            }
        );
    }
    serde_json::from_slice(&output.stdout).context("could not parse zellij pane list")
}

fn current_tab_id(panes: &[ZellijPane]) -> Option<u32> {
    if let Ok(raw) = std::env::var("ZELLIJ_PANE_ID") {
        if let Ok(id) = raw.parse::<u32>() {
            if let Some(pane) = panes.iter().find(|p| !p.is_plugin && p.id == id) {
                return Some(pane.tab_id);
            }
        }
    }
    focused_pane_id()
        .and_then(|id| panes.iter().find(|p| !p.is_plugin && p.id == id))
        .map(|p| p.tab_id)
}

/// The pane the attached client is looking at. `list-clients` is the only
/// reliable focus probe in zellij 0.45: `is_focused` in the pane JSON and
/// `action focus-pane-id` both misbehave when driven from outside a client.
fn focused_pane_id() -> Option<u32> {
    let output = std::process::Command::new("zellij")
        .args(["action", "list-clients"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let _client = fields.next()?;
        let pane = fields.next()?;
        if let Some(id) = pane.strip_prefix("terminal_") {
            return id.parse::<u32>().ok();
        }
    }
    None
}

/// Focus a pane by cycling. `action focus-pane-id` is a silent no-op from
/// outside a client in zellij 0.45, while `focus-next-pane` works.
fn focus_pane_by_id(panes: &[ZellijPane], target: u32) -> Result<()> {
    let steps = panes.iter().filter(|p| !p.is_plugin).count() + 2;
    for _ in 0..steps {
        if focused_pane_id() == Some(target) {
            return Ok(());
        }
        zellij_action(&["focus-next-pane".into()])?;
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    anyhow::bail!("Could not focus pane {}", pane_ref(target))
}

fn pane_ref(id: u32) -> String {
    format!("terminal_{id}")
}

fn tab_columns(panes: &[ZellijPane], tab_id: u32) -> u16 {
    panes
        .iter()
        .filter(|p| !p.is_plugin && p.tab_id == tab_id)
        .map(|p| p.pane_x.saturating_add(p.pane_columns))
        .max()
        .unwrap_or(80)
}

fn sidebar_in_tab<'a>(panes: &'a [ZellijPane], tab_id: u32) -> Option<&'a ZellijPane> {
    panes.iter().find(|p| {
        !p.is_plugin && p.tab_id == tab_id && is_sidebar_pane(&p.title, p.pane_command.as_deref())
    })
}

fn pane_geometry(pane_id: &str) -> Option<(u16, u16)> {
    let id = pane_id
        .strip_prefix("terminal_")
        .unwrap_or(pane_id)
        .parse::<u32>()
        .ok()?;
    let pane = zellij_panes()
        .ok()?
        .into_iter()
        .find(|p| !p.is_plugin && p.id == id)?;
    if pane.pane_columns == 0 {
        return None;
    }
    Some((pane.pane_columns, pane.pane_x))
}

fn shrink_sidebar(pane_id: &str, target: u16) {
    let mut previous = u16::MAX;
    let mut missing = 0u8;
    for _ in 0..48 {
        let Some((cols, x)) = pane_geometry(pane_id) else {
            missing += 1;
            if missing > 5 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
            continue;
        };
        missing = 0;
        // Left edge of the screen: the inner border is right. Otherwise the
        // pane is to the right of something, and the inner border is left.
        let border = if x == 0 { "right" } else { "left" };
        if cols <= target || cols >= previous {
            if previous != u16::MAX && cols * 4 < target * 3 {
                // The last step overshot far below the target: grow back once.
                let _ = std::process::Command::new("zellij")
                    .args(resize_args(pane_id, "increase", border))
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            break;
        }
        let ok = std::process::Command::new("zellij")
            .args(shrink_args(pane_id, border))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            break;
        }
        previous = cols;
    }
}

/// Swap the pane left until nothing in its tab sits further left, so the
/// sidebar always lands at the left edge.
fn move_pane_leftmost(pane_id: &str) {
    let Some(id) = pane_id
        .strip_prefix("terminal_")
        .and_then(|n| n.parse::<u32>().ok())
    else {
        return;
    };
    for _ in 0..8 {
        let Ok(panes) = zellij_panes() else { break };
        let Some(me) = panes.iter().find(|p| !p.is_plugin && p.id == id) else {
            break;
        };
        let furthest_left = panes
            .iter()
            .filter(|p| !p.is_plugin && p.tab_id == me.tab_id && p.id != id)
            .map(|p| p.pane_x)
            .min()
            .unwrap_or(0);
        if me.pane_x <= furthest_left {
            break;
        }
        let moved = std::process::Command::new("zellij")
            .args(["action", "move-pane", "left", "--pane-id", pane_id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !moved {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
}

/// Strip the agent's logo/spinner glyphs from a zellij pane title, leaving
/// the reported topic. `None` for generic or nibble-owned panes. Only known
/// prefix glyphs are removed, so non-ASCII topic text survives.
fn clean_pane_title(title: &str) -> Option<String> {
    let cleaned = title
        .trim()
        .trim_start_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '\u{2800}'
                        ..='\u{28FF}' // braille spinner frames
                    | 'π' | '✳' | '✻' | '✱' | '⚡'
                    | '◐' | '◑' | '◒' | '◓' | '○' | '●' | '·'
                )
        })
        .trim();
    if cleaned.is_empty()
        || cleaned.starts_with("Pane #")
        || cleaned == SIDEBAR_PANE_NAME
        || cleaned == "agents"
    {
        None
    } else {
        Some(cleaned.to_string())
    }
}

/// Topics reported by agent pane titles, keyed two ways: by task id through
/// `AGENT_TASK_ID` in the pane command (attach panes), and by pane id (host
/// agents, matched through the task's recorded pane). Best-effort: empty
/// outside zellij.
pub(crate) fn zellij_pane_topics() -> (HashMap<String, String>, HashMap<u32, String>) {
    let mut by_task = HashMap::new();
    let mut by_pane = HashMap::new();
    let Ok(panes) = zellij_panes() else {
        return (by_task, by_pane);
    };
    for pane in panes.iter().filter(|p| !p.is_plugin) {
        let Some(title) = clean_pane_title(&pane.title) else {
            continue;
        };
        if let Some(cmd) = pane.pane_command.as_deref() {
            if let Some(task_id) = extract_task_id(cmd) {
                by_task.insert(task_id, title.clone());
            }
        }
        by_pane.insert(pane.id, title);
    }
    (by_task, by_pane)
}

/// Run a zellij action, surfacing failures instead of swallowing them.
fn zellij_action(args: &[String]) -> Result<()> {
    let status = std::process::Command::new("zellij")
        .arg("action")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("zellij not available")?;
    if !status.success() {
        anyhow::bail!("zellij action {} failed", args.join(" "));
    }
    Ok(())
}

fn create_sidebar(target: u16) -> Result<String> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nibble"));
    let opened = std::process::Command::new("zellij")
        .args([
            "action",
            "new-pane",
            "--direction",
            "right",
            "--name",
            SIDEBAR_PANE_NAME,
            "--close-on-exit",
            "--no-focus",
            "--",
        ])
        .arg(&exe)
        .args(["status", "--watch"])
        .output()
        .context("zellij not available. Run `nibble status --watch` in a narrow pane.")?;
    if !opened.status.success() {
        let err = String::from_utf8_lossy(&opened.stderr);
        let detail = err.trim();
        anyhow::bail!(
            "Could not open the sidebar{}. Run `nibble status --watch` in a pane you size yourself.",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    let pane_id = String::from_utf8_lossy(&opened.stdout).trim().to_string();
    if pane_id.is_empty() {
        anyhow::bail!("zellij opened a pane but did not return its id");
    }
    move_pane_leftmost(&pane_id);
    shrink_sidebar(&pane_id, target);
    Ok(pane_id)
}

pub(crate) fn open_sidebar_pane() -> Result<()> {
    let panes = zellij_panes()?;
    let tab = current_tab_id(&panes)
        .context("Not inside zellij. Run `nibble status --watch` in a pane you size yourself.")?;
    let target = sidebar_target_cols(tab_columns(&panes, tab));
    if let Some(existing) = sidebar_in_tab(&panes, tab) {
        let id = pane_ref(existing.id);
        if existing.title != SIDEBAR_PANE_NAME {
            let _ = std::process::Command::new("zellij")
                .args(["action", "rename-pane", "--pane-id", &id, SIDEBAR_PANE_NAME])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        if existing.pane_columns > target.saturating_add(8) {
            shrink_sidebar(&id, target);
        }
        println!(
            "Sidebar already open. Press q in that pane to close it, or run `nibble sidebar --close`."
        );
        return Ok(());
    }
    create_sidebar(target)?;
    println!("Sidebar open. Press q in that pane to close it, or run `nibble sidebar --close`.");
    Ok(())
}

/// Jump to the sidebar. Focus it in this tab, jump to its tab if it lives
/// elsewhere in the session, or open it first when it is not open.
///
/// Invoked from inside a zellij pane (the Alt-a keybind wrapper), this
/// detaches first: zellij 0.45 scopes focus actions issued from within a
/// pane to that pane's focus group, and restores the pre-wrapper focus when
/// the wrapper pane closes. The detached copy acts once the wrapper is gone.
pub(crate) fn focus_sidebar_pane() -> Result<()> {
    if std::env::var_os("ZELLIJ_PANE_ID").is_some()
        && std::env::var_os("NIBBLE_SIDEBAR_DETACHED").is_none()
    {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nibble"));
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["sidebar", "--focus"])
            .env("NIBBLE_SIDEBAR_DETACHED", "1")
            .env_remove("ZELLIJ_PANE_ID")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn().context("could not detach sidebar focus")?;
        return Ok(());
    }
    if std::env::var_os("NIBBLE_SIDEBAR_DETACHED").is_some() {
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    let panes = zellij_panes()?;
    let tab = current_tab_id(&panes)
        .context("Not inside zellij. Run `nibble sidebar` from a zellij pane instead.")?;
    if let Some(p) = sidebar_in_tab(&panes, tab) {
        focus_pane_by_id(&panes, p.id)?;
        return Ok(());
    }
    if let Some(p) = panes
        .iter()
        .find(|p| !p.is_plugin && is_sidebar_pane(&p.title, p.pane_command.as_deref()))
    {
        let id = p.id;
        zellij_action(&["go-to-tab-by-id".into(), p.tab_id.to_string()])?;
        focus_pane_by_id(&panes, id)?;
        return Ok(());
    }
    let target = sidebar_target_cols(tab_columns(&panes, tab));
    let id = create_sidebar(target)?;
    let id_n = id
        .strip_prefix("terminal_")
        .and_then(|n| n.parse::<u32>().ok())
        .context("zellij returned an unexpected pane id")?;
    focus_pane_by_id(&zellij_panes()?, id_n)?;
    Ok(())
}

/// Jump to an agent's pane from the sidebar's digit keys. The pane is found
/// through `AGENT_TASK_ID` in its command (attach panes carry it); the
/// task's recorded pane id is the fallback. Crosses tabs when needed.
pub(crate) fn focus_agent_pane(task_id: &str, recorded_pane: Option<u32>) -> Result<()> {
    let panes = zellij_panes()?;
    let needle = format!("AGENT_TASK_ID={task_id}");
    let target = panes
        .iter()
        .find(|p| {
            !p.is_plugin
                && p.pane_command
                    .as_deref()
                    .is_some_and(|c| c.contains(&needle))
        })
        .map(|p| p.id)
        .or_else(|| recorded_pane.filter(|id| panes.iter().any(|p| !p.is_plugin && p.id == *id)));
    let Some(id) = target else {
        anyhow::bail!("no live pane for this agent");
    };
    let pane_tab = panes.iter().find(|p| p.id == id).map(|p| p.tab_id);
    if let (Some(target_tab), Some(current)) = (pane_tab, current_tab_id(&panes)) {
        if target_tab != current {
            zellij_action(&["go-to-tab-by-id".into(), target_tab.to_string()])?;
        }
    }
    focus_pane_by_id(&panes, id)
}

pub(crate) fn close_sidebar_pane() -> Result<()> {
    let panes = zellij_panes()?;
    let tab = current_tab_id(&panes).context(
        "Not inside zellij. Focus the sidebar pane and press q, or close that pane from zellij.",
    )?;
    let hits: Vec<u32> = panes
        .iter()
        .filter(|p| {
            !p.is_plugin && p.tab_id == tab && is_sidebar_pane(&p.title, p.pane_command.as_deref())
        })
        .map(|p| p.id)
        .collect();
    if hits.is_empty() {
        println!("No sidebar in this tab.");
        return Ok(());
    }
    for id in hits {
        let status = std::process::Command::new("zellij")
            .args(["action", "close-pane", "--pane-id", &pane_ref(id)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("zellij not available")?;
        if !status.success() {
            anyhow::bail!("Failed to close sidebar pane {}", pane_ref(id));
        }
    }
    println!("Sidebar closed.");
    Ok(())
}

/// Pull an AGENT_TASK_ID value out of a pane's command line / environ blob.
pub(crate) fn extract_task_id(blob: &str) -> Option<String> {
    const KEY: &str = "AGENT_TASK_ID=";
    let start = blob.find(KEY)? + KEY.len();
    let rest = &blob[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '\0')
        .unwrap_or(rest.len());
    let id = &rest[..end];
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}
