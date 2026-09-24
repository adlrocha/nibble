//! Live agent view.
//!
//! Liveness is observed, not reported. A task stays on the live list only while
//! a process for it still exists (host agent, or a `podman exec` attach). A
//! closed pane or a killed process drops off on the next read. A running
//! sandbox container with nobody attached is detached: it stays in
//! `sandbox list`, and it is not an agent you can jump to.
//!
//! Hooks may annotate a live process (`running`, `blocked`, `completed`).
//! Those annotations never keep a dead process on the list.

use crate::db::Database;
use crate::models::{SandboxType, Task, TaskStatus};
use crate::sandbox::podman::PodmanSandbox;
use crate::sandbox::{ContainerStatus, Sandbox};
use anyhow::Result;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;

/// Whether a task is an agent the user can jump to right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// A process for this task is alive.
    Live,
    /// Sandbox container is up, but no attach process. Not a jump target.
    Detached,
    /// Nothing is alive. Drop it.
    Gone,
}

/// What a live process is doing. Reported by the agent; ignored once it is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Working,
    Blocked,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// `AGENT_TASK_ID` was seen on a live process, or the recorded pid still
    /// belongs to this task.
    pub attached: bool,
    pub container_running: bool,
}

pub fn presence(is_sandbox: bool, facts: Facts) -> Presence {
    if facts.attached {
        Presence::Live
    } else if is_sandbox && facts.container_running {
        Presence::Detached
    } else {
        Presence::Gone
    }
}

pub fn activity(status: &TaskStatus, reason: Option<&str>) -> Activity {
    if reason.is_some_and(|r| !r.trim().is_empty()) {
        Activity::Blocked
    } else if *status == TaskStatus::Completed {
        Activity::Idle
    } else {
        Activity::Working
    }
}

#[derive(Debug, Clone)]
pub struct LiveAgent {
    pub task_id: String,
    pub agent: String,
    pub title: String,
    /// Repo directory name when the task has a path. Preferred sidebar label.
    pub repo: Option<String>,
    /// Topic reported by the agent's pane title (sidebar only).
    pub topic: Option<String>,
    pub activity: Activity,
    pub reason: Option<String>,
    pub pane_id: Option<u32>,
}

/// Mark gone tasks exited. Detached sandboxes are left alone so re-attach works.
/// Returns how many rows were closed.
pub fn reconcile(db: &Database) -> Result<usize> {
    // Reports run inside the sandbox, whose /proc cannot see host attaches.
    // Exiting from there would wipe every other agent. The host `status` command
    // is the one that observes pane close and process death.
    if Path::new("/run/.containerenv").exists() || Path::new("/.dockerenv").exists() {
        return Ok(0);
    }
    let Some(scan) = scan_processes() else {
        return Ok(0);
    };
    let sandbox = PodmanSandbox::new();
    let mut closed = 0;
    for task in db.list_tasks()? {
        if task.status == TaskStatus::Exited {
            continue;
        }
        let facts = facts_for(&task, &scan, &sandbox);
        if presence(task.sandbox_type == SandboxType::Podman, facts) == Presence::Gone {
            let mut updated = task.clone();
            updated.set_exited(None);
            db.update_task(&updated)?;
            closed += 1;
        }
    }
    Ok(closed)
}

pub fn live_agents(db: &Database) -> Result<Vec<LiveAgent>> {
    let scan = scan_processes();
    let sandbox = PodmanSandbox::new();
    let mut agents = Vec::new();
    for task in db.list_tasks()? {
        if task.status == TaskStatus::Exited {
            continue;
        }
        let Some(scan) = &scan else {
            // No process table: show stored non-exited rows rather than an empty lie.
            agents.push(to_live(&task));
            continue;
        };
        let facts = facts_for(&task, scan, &sandbox);
        if presence(task.sandbox_type == SandboxType::Podman, facts) == Presence::Live {
            agents.push(to_live(&task));
        }
    }
    agents.sort_by_key(|a| match a.activity {
        Activity::Blocked => 0,
        Activity::Working => 1,
        Activity::Idle => 2,
    });
    Ok(agents)
}

pub fn apply_report(db: &Database, task_id: &str, state: &str, reason: Option<&str>) -> Result<()> {
    let mut task = db
        .get_task_by_id(task_id)?
        .ok_or_else(|| anyhow::anyhow!("Task not found: {task_id}"))?;
    match state {
        "running" | "working" => {
            task.set_running();
            task.attention_reason = None;
        }
        "blocked" => {
            task.set_running();
            let reason = reason.map(str::trim).filter(|s| !s.is_empty());
            task.attention_reason = reason.map(str::to_string);
        }
        "completed" | "idle" => {
            task.status = TaskStatus::Completed;
            task.completed_at = Some(chrono::Utc::now());
            task.updated_at = chrono::Utc::now();
            task.attention_reason = None;
        }
        "exited" => {
            task.set_exited(None);
            task.attention_reason = None;
        }
        other => {
            anyhow::bail!("Invalid status state '{other}' (running, blocked, completed, exited)")
        }
    }
    db.update_task(&task)?;
    Ok(())
}

pub fn render(agents: &[LiveAgent]) -> String {
    render_inner(agents, false)
}

fn render_inner(agents: &[LiveAgent], color: bool) -> String {
    if agents.is_empty() {
        return "no live agents\n".to_string();
    }
    let mut out = String::new();
    for agent in agents {
        let mark = match agent.activity {
            Activity::Blocked => "!",
            Activity::Working => "*",
            Activity::Idle => "●",
        };
        let painted = if color {
            match agent.activity {
                Activity::Blocked => format!("\x1b[1;31m{mark}\x1b[0m"),
                Activity::Working => format!("\x1b[38;5;208m{mark}\x1b[0m"),
                Activity::Idle => format!("\x1b[32m{mark}\x1b[0m"),
            }
        } else {
            mark.to_string()
        };
        let who = short_agent(&agent.agent);
        let title = short_title(&agent.title);
        out.push_str(&format!("{painted} {who} {title}\n"));
        if let Some(reason) = agent.reason.as_deref().filter(|r| !r.is_empty()) {
            out.push_str(&format!("  {}\n", truncate(reason, 22)));
        }
    }
    out.push_str(&format!(
        "{}{}{}{}{}\n",
        paint(color, "1;31", "! needs input"),
        paint(color, "2", " · "),
        paint(color, "38;5;208", "* working"),
        paint(color, "2", " · "),
        paint(color, "32", "● ready"),
    ));
    out
}

fn short_agent(agent: &str) -> &str {
    match agent {
        "claude_code" => "claude",
        other => other,
    }
}

fn display_name(title: &str) -> String {
    let trimmed = title.trim().trim_matches(|c| c == '[' || c == ']');
    let name = trimmed.split(':').next().unwrap_or(trimmed);
    let name = name.rsplit('/').next().unwrap_or(name);
    name.trim().to_string()
}

fn short_title(title: &str) -> String {
    truncate(&display_name(title), 16)
}

fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i == max.saturating_sub(1) && s.chars().nth(max).is_some() {
            out.push('…');
            break;
        }
        if i >= max {
            break;
        }
        out.push(ch);
    }
    out
}

pub fn pane_id(task: &Task) -> Option<u32> {
    let n = task.context.as_ref()?.extra.get("zellij_pane_id")?;
    n.as_u64()
        .or_else(|| n.as_str().and_then(|s| s.parse().ok()))
        .map(|v| v as u32)
}

fn to_live(task: &Task) -> LiveAgent {
    LiveAgent {
        task_id: task.task_id.clone(),
        agent: task.agent_type.as_str().to_string(),
        title: task.title.clone(),
        repo: task.repo_path.as_deref().and_then(repo_name),
        topic: None,
        activity: activity(&task.status, task.attention_reason.as_deref()),
        reason: task.attention_reason.clone(),
        pane_id: pane_id(task),
    }
}

fn repo_name(path: &str) -> Option<String> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    if path == "__hermes__" {
        return Some("hermes".into());
    }
    let name = std::path::Path::new(path).file_name()?.to_str()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

struct ProcScan {
    task_ids: HashSet<String>,
    /// Recorded pids whose environ/cmdline still names this task.
    matched_pids: HashSet<i32>,
    /// Container ids/names that have a live `podman exec`.
    exec_containers: HashSet<String>,
}

fn facts_for(task: &Task, scan: &ProcScan, sandbox: &PodmanSandbox) -> Facts {
    let attached = scan.task_ids.contains(&task.task_id)
        || task.pid.is_some_and(|pid| scan.matched_pids.contains(&pid))
        || task
            .container_id
            .as_deref()
            .is_some_and(|id| scan.exec_containers.contains(id))
        || task
            .container_name
            .as_deref()
            .is_some_and(|name| scan.exec_containers.contains(name));
    if attached {
        return Facts {
            attached: true,
            container_running: true,
        };
    }
    let container_running = task
        .container_id
        .as_deref()
        .or(task.container_name.as_deref())
        .is_some_and(|id| matches!(sandbox.status(id), Ok(ContainerStatus::Running)));
    Facts {
        attached: false,
        container_running,
    }
}

/// `None` when `/proc` cannot be read. Callers must not exit tasks in that case.
fn scan_processes() -> Option<ProcScan> {
    let proc = Path::new("/proc");
    if !proc.is_dir() {
        return None;
    }
    let mut scan = ProcScan {
        task_ids: HashSet::new(),
        matched_pids: HashSet::new(),
        exec_containers: HashSet::new(),
    };
    let entries = std::fs::read_dir(proc).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        let cmdline = read_proc_blob(&entry.path().join("cmdline"));
        if let Some(id) = process_holds_task(&cmdline, &entry.path().join("environ")) {
            scan.task_ids.insert(id);
            scan.matched_pids.insert(pid);
        }
        if cmdline.contains("podman") && cmdline.contains("exec") {
            for token in cmdline.split_whitespace() {
                if token.starts_with("nibble-") || looks_like_container_id(token) {
                    scan.exec_containers.insert(token.to_string());
                }
            }
        }
    }
    Some(scan)
}

/// The task a process is attached to, if any.
///
/// Two precise matches only:
/// - the id appears in the command line (a `podman exec -e AGENT_TASK_ID=…`
///   attach — the assignment is an argv token), or
/// - the id is in the environment of a process that looks like an agent
///   binary. The environment check alone is too broad: sandbox containers
///   are created with the id in their base env, so their long-lived init
///   (`sleep infinity`) would keep a killed agent "live" forever, and so
///   would any orphaned tool the agent spawned.
fn process_holds_task(cmdline: &str, environ_path: &Path) -> Option<String> {
    if extract_task_id(cmdline).is_some() {
        return extract_task_id(cmdline);
    }
    if !looks_like_agent_process(cmdline) {
        return None;
    }
    task_from_process(cmdline, &read_proc_blob(environ_path))
}

fn task_from_process(cmdline: &str, environ: &str) -> Option<String> {
    if let Some(id) = extract_task_id(cmdline) {
        return Some(id);
    }
    if !looks_like_agent_process(cmdline) {
        return None;
    }
    extract_task_id(environ)
}

const AGENT_BINARIES: [&str; 4] = ["claude", "omp", "pi", "hermes"];
/// Script hosts whose second argv token names the real program.
const SCRIPT_HOSTS: [&str; 3] = ["node", "bun", "deno"];

fn looks_like_agent_process(cmdline: &str) -> bool {
    let mut tokens = cmdline.split_whitespace();
    let Some(first) = tokens.next().and_then(basename) else {
        return false;
    };
    if AGENT_BINARIES.contains(&first) {
        return true;
    }
    if SCRIPT_HOSTS.contains(&first) {
        return tokens
            .next()
            .and_then(basename)
            .is_some_and(|second| AGENT_BINARIES.contains(&second));
    }
    false
}

fn basename(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn looks_like_container_id(token: &str) -> bool {
    token.len() >= 12 && token.chars().all(|c| c.is_ascii_hexdigit())
}

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

fn read_proc_blob(path: &Path) -> String {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = Vec::new();
    let _ = file.take(64 * 1024).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).replace('\0', " ")
}

fn paint(color: bool, seq: &str, text: &str) -> String {
    if color {
        format!("\x1b[{seq}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn sidebar_label(agent: &LiveAgent) -> String {
    if let Some(repo) = agent
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return repo.to_string();
    }
    display_name(&agent.title)
}

fn header_line(width: usize, color: bool) -> String {
    let brand = " nibble";
    let sub = "agents";
    if brand.chars().count() + 2 + sub.chars().count() <= width {
        format!("{}  {}", paint(color, "1", brand), paint(color, "2", sub))
    } else {
        paint(color, "1", brand)
    }
}

fn summary_line(agents: &[LiveAgent], color: bool) -> String {
    if agents.is_empty() {
        return paint(color, "2", " no live agents");
    }
    let live = agents.len();
    let blocked = agents
        .iter()
        .filter(|a| a.activity == Activity::Blocked)
        .count();
    if blocked == 0 {
        paint(color, "2", &format!(" {live} live"))
    } else {
        format!(
            "{}  {}",
            paint(color, "2", &format!(" {live} live")),
            paint(color, "1;31", &format!("{blocked} blocked"))
        )
    }
}

fn rule_line(width: usize, color: bool) -> String {
    paint(color, "2", &"─".repeat(width.max(1)))
}

/// Display order for the sidebar and for digit-key jumps: blocked first.
pub(crate) fn sort_for_display(agents: &mut [LiveAgent]) {
    agents.sort_by_key(|a| match a.activity {
        Activity::Blocked => 0,
        Activity::Working => 1,
        Activity::Idle => 2,
    });
}

fn agent_line(index: usize, agent: &LiveAgent, width: usize, color: bool) -> String {
    let mark = match agent.activity {
        Activity::Blocked => "!",
        Activity::Working => "●",
        Activity::Idle => "●",
    };
    let num = if index < 9 {
        (index + 1).to_string()
    } else {
        "·".to_string()
    };
    let who = short_agent(&agent.agent);
    let who_w = who.chars().count();
    // Row prefix: " N ! " — digit (jump key), then the activity mark.
    let label_budget = width.saturating_sub(5 + who_w + 2).max(4);
    let label = truncate(&sidebar_label(agent), label_budget);
    let show_who = who_w > 0 && 5 + label.chars().count() + 2 + who_w <= width;
    let painted = match agent.activity {
        Activity::Blocked => paint(color, "1;31", mark),
        Activity::Working => paint(color, "38;5;208", mark),
        Activity::Idle => paint(color, "32", mark),
    };
    let mut row = format!(" {} {painted} {label}", paint(color, "36", &num));
    if show_who {
        row.push_str("  ");
        row.push_str(&paint(color, "2", who));
    }
    row
}

fn reason_line(reason: &str, width: usize, color: bool) -> String {
    let text = truncate(reason.trim(), width.saturating_sub(5));
    format!("     {}", paint(color, "33", &text))
}

fn fit_lines(mut lines: Vec<String>, rows: usize, footer: Vec<String>) -> String {
    if rows > footer.len() && lines.len() + footer.len() > rows {
        lines.truncate(rows - footer.len());
    }
    lines.extend(footer);
    lines.join("\n")
}

fn topic_line(topic: &str, width: usize, color: bool) -> String {
    let text = truncate(topic.trim(), width.saturating_sub(5));
    format!("     {}", paint(color, "2", &text))
}

/// Explains the activity marks so the pane is self-describing. Dropped when
/// the pane is too narrow to fit even the compact form.
fn legend_line(width: usize, color: bool) -> Option<String> {
    let (needs, working, ready) = if width >= 36 {
        ("! needs input", "● working", "● ready")
    } else if width >= 27 {
        ("! input", "● busy", "● ready")
    } else {
        return None;
    };
    let sep = paint(color, "2", " · ");
    Some(format!(
        " {}{sep}{}{sep}{}",
        paint(color, "1;31", needs),
        paint(color, "38;5;208", working),
        paint(color, "32", ready),
    ))
}

fn footer_line(width: usize, color: bool, notice: Option<&str>) -> String {
    if let Some(text) = notice.filter(|t| !t.trim().is_empty()) {
        return format!(
            " {}",
            paint(color, "31", &truncate(text.trim(), width.saturating_sub(1)))
        );
    }
    if width >= 19 {
        paint(color, "2", " 1-9 jump · q close")
    } else {
        paint(color, "2", " q close")
    }
}

fn footer_lines(width: usize, color: bool, notice: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(legend) = legend_line(width, color) {
        lines.push(legend);
    }
    lines.push(footer_line(width, color, notice));
    lines
}

/// Narrow live-agent pane. `rows` truncates when the list would scroll; `0`
/// does not. `notice` replaces the footer hint for one frame (jump errors).
pub fn render_sidebar(
    agents: &[LiveAgent],
    cols: usize,
    rows: usize,
    color: bool,
    notice: Option<&str>,
) -> String {
    let mut ordered = agents.to_vec();
    sort_for_display(&mut ordered);
    let agents = ordered.as_slice();
    let width = cols.max(16).saturating_sub(1).max(15);
    let mut lines = vec![
        header_line(width, color),
        summary_line(agents, color),
        rule_line(width, color),
    ];
    if agents.is_empty() {
        lines.push(String::new());
        lines.push(paint(color, "2", "  closed panes drop off"));
    } else {
        for (index, agent) in agents.iter().enumerate() {
            lines.push(agent_line(index, agent, width, color));
            if let Some(topic) = agent.topic.as_deref().filter(|t| !t.trim().is_empty()) {
                lines.push(topic_line(topic, width, color));
            }
            if agent.activity == Activity::Blocked {
                if let Some(reason) = agent.reason.as_deref().filter(|r| !r.trim().is_empty()) {
                    lines.push(reason_line(reason, width, color));
                }
            }
        }
    }
    lines.push(rule_line(width, color));
    fit_lines(lines, rows, footer_lines(width, color, notice))
}

struct RawTerm {
    saved: libc::termios,
    active: bool,
}

impl RawTerm {
    fn enable() -> Self {
        let mut saved = unsafe { std::mem::zeroed() };
        let fd = libc::STDIN_FILENO;
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return Self {
                saved,
                active: false,
            };
        }
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Self {
                saved,
                active: false,
            };
        }
        Self {
            saved,
            active: true,
        }
    }
}

impl Drop for RawTerm {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
            }
        }
    }
}

struct AltScreen;

impl AltScreen {
    fn enter() -> Self {
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b[?1049h\x1b[?25l");
        let _ = out.flush();
        Self
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
    }
}

fn terminal_size() -> Option<(u16, u16)> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } != 0 || ws.ws_col == 0
    {
        return None;
    }
    Some((ws.ws_col, ws.ws_row))
}

/// Keys pressed during the refresh window. Escape-prefixed sequences
/// (arrows, function keys) are dropped wholesale so embedded digits in CSI
/// parameters never trigger a jump.
fn stdin_keys(timeout: std::time::Duration) -> Vec<u8> {
    let mut fds = [libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    }];
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    if unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) } <= 0 {
        return Vec::new();
    }
    let mut buf = [0u8; 32];
    let n = unsafe {
        libc::read(
            libc::STDIN_FILENO,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if n <= 0 || buf[0] == 0x1b {
        return Vec::new();
    }
    buf[..n as usize].to_vec()
}

pub fn watch(db: &Database) -> Result<()> {
    let raw = RawTerm::enable();
    let _screen = AltScreen::enter();
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]0;nibble\x07\x1b]2;nibble\x07");
    watch_loop(db, &mut out, raw.active)
}

fn watch_loop(db: &Database, out: &mut std::io::Stdout, raw: bool) -> Result<()> {
    let mut notice: Option<String> = None;
    loop {
        reconcile(db)?;
        let mut agents = live_agents(db)?;
        sort_for_display(&mut agents);
        let (topics_by_task, topics_by_pane) = crate::zellij_pane_topics();
        for agent in &mut agents {
            agent.topic = topics_by_task
                .get(&agent.task_id)
                .or_else(|| agent.pane_id.and_then(|id| topics_by_pane.get(&id)))
                .cloned();
        }
        let (cols, rows) = terminal_size().unwrap_or((32, 24));
        let frame = render_sidebar(
            &agents,
            cols as usize,
            rows as usize,
            true,
            notice.as_deref(),
        );
        notice = None;
        write!(out, "\x1b[2J\x1b[H{frame}")?;
        out.flush()?;
        if raw {
            for key in stdin_keys(std::time::Duration::from_secs(1)) {
                match key {
                    b'q' | b'Q' | 0x03 | 0x04 => return Ok(()),
                    b'1'..=b'9' => {
                        if let Some(agent) = agents.get((key - b'1') as usize) {
                            notice = crate::focus_agent_pane(&agent.task_id, agent.pane_id)
                                .err()
                                .map(|e| e.to_string());
                        }
                    }
                    _ => {}
                }
            }
        } else {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AgentType, Task};

    fn task(sandbox: bool) -> Task {
        let mut t = Task::new(
            "task-1".into(),
            AgentType::Omp,
            "[nibble]".into(),
            Some(42),
            None,
        );
        if sandbox {
            t.sandbox_type = SandboxType::Podman;
            t.container_id = Some("abc123abc123".into());
        }
        t
    }

    #[test]
    fn attached_process_is_live() {
        assert_eq!(
            presence(
                false,
                Facts {
                    attached: true,
                    container_running: false
                }
            ),
            Presence::Live
        );
    }

    #[test]
    fn dead_host_process_is_gone() {
        assert_eq!(
            presence(
                false,
                Facts {
                    attached: false,
                    container_running: false
                }
            ),
            Presence::Gone
        );
    }

    #[test]
    fn sandbox_without_attach_is_detached_not_live() {
        assert_eq!(
            presence(
                true,
                Facts {
                    attached: false,
                    container_running: true
                }
            ),
            Presence::Detached
        );
    }

    #[test]
    fn dead_sandbox_is_gone() {
        assert_eq!(
            presence(
                true,
                Facts {
                    attached: false,
                    container_running: false
                }
            ),
            Presence::Gone
        );
    }

    #[test]
    fn attention_reason_is_blocked_even_if_status_says_running() {
        assert_eq!(
            activity(&TaskStatus::Running, Some("needs approval")),
            Activity::Blocked
        );
        assert_eq!(activity(&TaskStatus::Completed, None), Activity::Idle);
        assert_eq!(activity(&TaskStatus::Running, None), Activity::Working);
        assert_eq!(
            activity(&TaskStatus::Running, Some("  ")),
            Activity::Working
        );
    }

    #[test]
    fn extract_task_id_from_cmdline_or_environ() {
        assert_eq!(
            extract_task_id("podman exec -e AGENT_TASK_ID=abc-123 -it nibble-x"),
            Some("abc-123".into())
        );
        assert_eq!(extract_task_id("no id here"), None);
    }

    #[test]
    fn report_blocked_then_running_clears_reason() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("t.db")).unwrap();
        let t = task(false);
        db.insert_task(&t).unwrap();
        apply_report(&db, "task-1", "blocked", Some("needs approval")).unwrap();
        let stored = db.get_task_by_id("task-1").unwrap().unwrap();
        assert_eq!(stored.attention_reason.as_deref(), Some("needs approval"));
        assert_eq!(
            activity(&stored.status, stored.attention_reason.as_deref()),
            Activity::Blocked
        );
        apply_report(&db, "task-1", "running", None).unwrap();
        let stored = db.get_task_by_id("task-1").unwrap().unwrap();
        assert!(stored.attention_reason.is_none());
        assert_eq!(stored.status, TaskStatus::Running);
    }

    #[test]
    fn unknown_report_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("t.db")).unwrap();
        db.insert_task(&task(false)).unwrap();
        assert!(apply_report(&db, "task-1", "nope", None).is_err());
    }

    #[test]
    fn container_init_env_does_not_keep_task_live() {
        // The killed-agent bug: sandbox containers carry AGENT_TASK_ID in
        // their base env, so `sleep infinity` matched the environ scan and
        // kept dead agents on the live list forever.
        let env = "PATH=/usr/bin\0AGENT_TASK_ID=deadbeef-1234\0HOME=/root";
        assert_eq!(task_from_process("sleep infinity ", env), None);
        assert_eq!(task_from_process("node /opt/mcp-server --stdio", env), None);
        assert_eq!(task_from_process("/bin/bash -c 'some hook'", env), None);
    }

    #[test]
    fn agent_process_env_keeps_task_live() {
        let env = "PATH=/usr/bin\0AGENT_TASK_ID=abc-123\0";
        assert_eq!(
            task_from_process("/home/node/.local/bin/claude --resume s1", env),
            Some("abc-123".to_string())
        );
        assert_eq!(task_from_process("omp", env), Some("abc-123".to_string()));
        assert_eq!(
            task_from_process("node /home/node/.local/bin/pi --session s", env),
            Some("abc-123".to_string())
        );
    }

    #[test]
    fn attach_cmdline_matches_without_env() {
        assert_eq!(
            task_from_process(
                "podman exec -it -e AGENT_TASK_ID=xyz-789 -w /atlas 1c7036c0ac43 /bin/bash -c claude",
                ""
            ),
            Some("xyz-789".to_string())
        );
    }

    #[test]
    fn render_lists_blocked_before_idle() {
        let agents = vec![
            LiveAgent {
                task_id: "aaaaaaaa-bbbb".into(),
                agent: "omp".into(),
                title: "[nibble]".into(),
                repo: None,
                topic: None,
                activity: Activity::Idle,
                reason: None,
                pane_id: None,
            },
            LiveAgent {
                task_id: "cccccccc-dddd".into(),
                agent: "claude_code".into(),
                title: "[atlas]".into(),
                repo: Some("atlas".into()),
                topic: None,
                activity: Activity::Blocked,
                reason: Some("needs approval".into()),
                pane_id: Some(3),
            },
        ];
        let mut sorted = agents;
        sorted.sort_by_key(|a| match a.activity {
            Activity::Blocked => 0,
            Activity::Working => 1,
            Activity::Idle => 2,
        });
        let text = render(&sorted);
        let blocked = text.find('!').unwrap();
        let idle = text.find("● omp").unwrap();
        assert!(blocked < idle);
        assert!(text.contains("needs approval"));
        assert!(text.contains("omp"));
        assert!(text.contains("nibble"));
        assert!(!text.contains("aaaaaaaa"));
    }

    #[test]
    fn sidebar_shows_repo_reason_and_close_hint() {
        let agents = vec![
            LiveAgent {
                task_id: "aaaaaaaa-bbbb".into(),
                agent: "omp".into(),
                title: "[nibble]".into(),
                repo: None,
                topic: Some("Fix the sidebar resize loop".into()),
                activity: Activity::Idle,
                reason: None,
                pane_id: None,
            },
            LiveAgent {
                task_id: "cccccccc-dddd".into(),
                agent: "claude_code".into(),
                title: "needs a decision on the proposal".into(),
                repo: Some("atlas".into()),
                topic: Some("Founder in residence proposal".into()),
                activity: Activity::Blocked,
                reason: Some("needs approval".into()),
                pane_id: Some(3),
            },
        ];
        let text = render_sidebar(&agents, 48, 12, false, None);
        let blocked = text.find("atlas").unwrap();
        let idle = text.find("omp").unwrap();
        assert!(blocked < idle, "{text}");
        assert!(text.contains("1 ! atlas"));
        assert!(text.contains("2 ● nibble"));
        assert!(text.contains("1-9 jump"));
        assert!(
            text.contains("! needs input · ● working · ● ready"),
            "{text}"
        );
        assert!(text.contains("Founder in residence proposal"));
        assert!(text.contains("Fix the sidebar resize loop"));
        let topic_at = text.find("Founder in residence").unwrap();
        let reason_at = text.find("needs approval").unwrap();
        assert!(topic_at < reason_at, "{text}");
        assert!(!text.contains("aaaaaaaa"));
        assert_eq!(text.lines().last(), Some(" 1-9 jump · q close"));
    }
    #[test]
    fn sidebar_empty_state_offers_close() {
        let text = render_sidebar(&[], 28, 8, false, None);
        assert!(text.lines().count() <= 8, "{text}");
        assert!(text.contains("no live agents"));
        assert!(text.contains("closed panes drop off"));
        assert_eq!(text.lines().last(), Some(" 1-9 jump · q close"));
    }
}
