//! `nibble mux [NAME]`: named zellij sessions whose agents survive a killed
//! session, a crash, or a reboot.
//!
//! While a mux session runs, a detached watcher (`nibble mux NAME --watch`)
//! polls the session's panes and keeps a snapshot of which agent runs in
//! which tab. An agent you close drops out of the next snapshot; agents that
//! die with the session stay in it. Re-running `nibble mux NAME` after the
//! session is gone rebuilds its agent tabs from the snapshot, and each pane
//! resumes its own conversation through `nibble mux --resume <task-id>`.
//!
//! ```text
//! nibble mux            # open (or rebuild) the "main" session
//! nibble mux work       # same, for a session named "work"
//! nibble mux work --show    # print the saved snapshot
//! nibble mux work --forget  # drop the snapshot (nibble rebuilds nothing)
//! ```

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::db::Database;
use crate::models::{AgentType, SandboxType, Task, TaskStatus};
use crate::sandbox::podman::PodmanSandbox;
use crate::sandbox::ContainerStatus;
use crate::status::{pane_at_shell, PaneListError, ZellijPane};

/// Session opened by a bare `nibble mux`.
pub(crate) const DEFAULT_SESSION: &str = "main";
/// How often the watcher samples the session's panes.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Polls the watcher waits for a just-launched session to appear (~1 min).
const STARTUP_POLLS: u32 = 20;
/// Consecutive "session not found" polls before the watcher exits.
const GONE_POLLS: u32 = 2;
/// Consecutive failed samples (zellij missing, unparseable output) before
/// the watcher gives up and releases its lock (~1 min).
const ERROR_POLLS: u32 = 20;
/// How long a new watcher waits for the previous one (still sampling a
/// session that just died) to release the lock. Covers its gone-detection
/// and its sample-error budget.
const LOCK_WAIT: Duration = Duration::from_secs(90);
/// The flag a restored pane passes back to nibble; see [`resume_invocation`].
const RESUME_FLAG: &str = "--resume";
/// kv_store key prefix for session snapshots.
const KV_PREFIX: &str = "mux:";

/// What nibble remembers about a mux session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Manifest {
    /// When nibble last (re)created the session. Zellij pane ids restart
    /// from 0 in a new session, so only tasks registered after this moment
    /// can belong to its panes. `None` for an adopted session that nibble
    /// did not create (its pane ids are the ones tasks recorded).
    epoch: Option<DateTime<Utc>>,
    /// When the agent tabs below last changed.
    updated_at: Option<DateTime<Utc>>,
    tabs: Vec<MuxTab>,
}

/// One zellij tab and the agents in it, left to right.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct MuxTab {
    name: String,
    panes: Vec<MuxPane>,
}

/// One agent pane: the task to resume and where it ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct MuxPane {
    task_id: String,
    title: String,
    cwd: Option<String>,
}

// ── Entry point ────────────────────────────────────────────────────────────

/// `nibble mux` dispatcher. Exactly one mode runs: resume a pane, run the
/// watcher, show or forget the snapshot, or open the session.
pub(crate) fn cmd_mux(
    db: &Database,
    name: &str,
    show: bool,
    forget: bool,
    watch: bool,
    resume: Option<&str>,
) -> Result<()> {
    if let Some(task_id) = resume {
        return cmd_resume(db, task_id);
    }
    validate_session_name(name)?;
    if watch {
        return run_watcher(db, name);
    }
    if show {
        return cmd_show(db, name);
    }
    if forget {
        return cmd_forget(db, name);
    }
    cmd_open(db, name)
}

/// Session names become file names and zellij arguments: keep them plain.
fn validate_session_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!(
            "invalid mux session name '{name}': use 1-64 letters, digits, '-', '_' or '.', \
             not starting with '-'"
        );
    }
    Ok(())
}

fn kv_key(name: &str) -> String {
    format!("{KV_PREFIX}{name}")
}

fn load_manifest(db: &Database, name: &str) -> Result<Option<Manifest>> {
    db.kv_get(&kv_key(name))?
        .map(|raw| {
            serde_json::from_str(&raw)
                .with_context(|| format!("corrupt snapshot for mux session '{name}'"))
        })
        .transpose()
}

fn save_manifest(db: &Database, name: &str, manifest: &Manifest) -> Result<()> {
    db.kv_set(&kv_key(name), &serde_json::to_string(manifest)?)
}

fn cmd_show(db: &Database, name: &str) -> Result<()> {
    let Some(manifest) = load_manifest(db, name)? else {
        println!("No snapshot for mux session '{name}'. Open it with `nibble mux {name}`.");
        return Ok(());
    };
    let when = manifest
        .updated_at
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "never".to_string());
    println!("mux session '{name}' (agents last changed: {when})");
    if manifest.tabs.is_empty() {
        println!("  no agents recorded");
    }
    for tab in &manifest.tabs {
        println!("  tab '{}'", tab.name);
        for pane in &tab.panes {
            println!(
                "    {}  {}  {}",
                short_id(&pane.task_id),
                pane.title,
                pane.cwd.as_deref().unwrap_or("?")
            );
            // Same check a restore runs, so --show never promises more.
            if let Err(e) = resume_plan_for(db, &pane.task_id) {
                println!("      won't restore: {e:#}");
            }
        }
    }
    Ok(())
}

/// Drop the record, refusing while a watcher still records the session: it
/// would write the agents straight back within seconds.
fn cmd_forget(db: &Database, name: &str) -> Result<()> {
    let Some(_lock) = try_lock(name)? else {
        bail!(
            "mux session '{name}' is running and being recorded. Close the agents you \
             don't want back (or the session) first"
        );
    };
    db.kv_delete(&kv_key(name))?;
    println!(
        "Forgot the record for mux session '{name}'. nibble won't rebuild its agents; \
         zellij may still resurrect its own saved copy."
    );
    Ok(())
}

fn short_id(id: &str) -> &str {
    let end = id.char_indices().nth(8).map(|(i, _)| i).unwrap_or(id.len());
    &id[..end]
}

// ── Opening a session ──────────────────────────────────────────────────────

/// What `nibble mux NAME` does, decided from the session's zellij state and
/// whether nibble holds a snapshot with agents in it.
#[derive(Debug, PartialEq)]
enum OpenPlan {
    /// The session is running: attach to it.
    Attach,
    /// Rebuild the agent tabs from the snapshot. `discard_dead` deletes
    /// zellij's own dead copy of the session first (it would otherwise
    /// claim the name).
    Restore { discard_dead: bool },
    /// No agents to restore, but zellij saved the dead session: let zellij
    /// resurrect it.
    Resurrect,
    /// Nothing known: start a fresh session.
    Create,
}

/// Zellij's view of a session name.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ZellijState {
    Live,
    Dead,
    Missing,
}

fn plan_open(state: ZellijState, manifest: Option<&Manifest>) -> OpenPlan {
    let has_agents = manifest.is_some_and(|m| !m.tabs.is_empty());
    match state {
        ZellijState::Live => OpenPlan::Attach,
        ZellijState::Dead if has_agents => OpenPlan::Restore { discard_dead: true },
        ZellijState::Missing if has_agents => OpenPlan::Restore {
            discard_dead: false,
        },
        ZellijState::Dead => OpenPlan::Resurrect,
        ZellijState::Missing => OpenPlan::Create,
    }
}

fn cmd_open(db: &Database, name: &str) -> Result<()> {
    if std::env::var_os("ZELLIJ").is_some() {
        let current = std::env::var("ZELLIJ_SESSION_NAME").unwrap_or_default();
        if current != name {
            bail!(
                "already inside zellij session '{current}'. Detach first (Ctrl o, then d) \
                 and run `nibble mux {name}` from a plain terminal"
            );
        }
        // Adopt the session we are in: start recording its agents.
        if load_manifest(db, name)?.is_none() {
            save_manifest(db, name, &Manifest::default())?;
        }
        spawn_watcher(name)?;
        println!("Mux session '{name}' is now recorded. Its agents will come back after a crash: run `nibble mux {name}`.");
        return Ok(());
    }

    let state = zellij_session_state(name)?;
    let manifest = load_manifest(db, name)?;
    // Only agents that can actually be resumed count: zellij's dead copy is
    // deleted for them, so check before anything is destroyed. A missing
    // wrapper aborts here (fixable: nothing has been touched yet).
    let restorable = match (&manifest, state) {
        (Some(m), ZellijState::Dead | ZellijState::Missing) => Some(filter_restorable(m, &|id| {
            resume_plan_for(db, id).map(drop)
        })?),
        (m, _) => m.clone(),
    };
    match plan_open(state, restorable.as_ref()) {
        OpenPlan::Attach => {
            record_running_session(db, name, manifest.as_ref())?;
            Err(exec_zellij(&["attach", name]))
        }
        OpenPlan::Restore { discard_dead } => {
            let to_restore = restorable.unwrap_or_default();
            let agents: usize = to_restore.tabs.iter().map(|t| t.panes.len()).sum();
            println!(
                "Restoring {agents} agent(s) in {} tab(s) for mux session '{name}'...",
                to_restore.tabs.len()
            );
            let nibble_bin = std::env::current_exe().context("cannot locate nibble binary")?;
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            let sidebar = crate::status::sidebar_is_default_layout();
            let tabs = without_missing_dirs(&to_restore.tabs, &|p| p.exists());
            let layout = render_layout(&tabs, &nibble_bin, &shell, sidebar);
            let layout_path = mux_dir()?.join(format!("{name}.kdl"));
            std::fs::write(&layout_path, layout)
                .with_context(|| format!("failed to write {}", layout_path.display()))?;
            // Keep the full record (skipped panes included) until the new
            // session's watcher records what actually came back.
            let mut manifest = manifest.unwrap_or_default();
            manifest.epoch = Some(Utc::now());
            save_manifest(db, name, &manifest)?;
            spawn_watcher(name)?;
            // Last step before zellij takes over: everything that could still
            // fail (layout, record, watcher) is in place, so a failure above
            // leaves zellij's dead copy to resurrect.
            if discard_dead {
                let out = Command::new("zellij")
                    .args(["delete-session", name])
                    .output()
                    .context("failed to run zellij delete-session")?;
                if !out.status.success() {
                    bail!(
                        "could not delete zellij's dead copy of '{name}': {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            }
            let layout_arg = layout_path.to_string_lossy().to_string();
            Err(exec_zellij(&[
                "--session",
                name,
                "--new-session-with-layout",
                &layout_arg,
            ]))
        }
        OpenPlan::Resurrect | OpenPlan::Create => {
            let mut manifest = manifest.unwrap_or_default();
            manifest.epoch = Some(Utc::now());
            save_manifest(db, name, &manifest)?;
            spawn_watcher(name)?;
            if state == ZellijState::Dead {
                Err(exec_zellij(&["attach", name]))
            } else {
                Err(exec_zellij(&["attach", "--create", name]))
            }
        }
    }
}

/// The recorded agents whose resume can be planned by `plan`, warning about
/// the rest (task deleted, agent not resumable). Empty tabs go. A missing
/// nibble wrapper is an error instead: it is fixable, so the restore stops
/// before zellij's dead copy is touched.
fn filter_restorable(manifest: &Manifest, plan: &dyn Fn(&str) -> Result<()>) -> Result<Manifest> {
    let mut tabs = Vec::new();
    for tab in &manifest.tabs {
        let mut panes = Vec::new();
        for pane in &tab.panes {
            match plan(&pane.task_id) {
                Ok(()) => panes.push(pane.clone()),
                Err(e) if e.downcast_ref::<MissingWrapper>().is_some() => {
                    return Err(e.context(format!("cannot restore '{}'", pane.title)));
                }
                Err(e) => eprintln!("nibble: not restoring '{}': {e:#}", pane.title),
            }
        }
        if !panes.is_empty() {
            tabs.push(MuxTab {
                name: tab.name.clone(),
                panes,
            });
        }
    }
    Ok(Manifest {
        tabs,
        ..manifest.clone()
    })
}

/// The tabs with directories that no longer exist (a pruned worktree, a
/// moved repo) dropped from their panes, so zellij opens those panes in its
/// default directory instead of failing to start them.
fn without_missing_dirs(tabs: &[MuxTab], exists: &dyn Fn(&Path) -> bool) -> Vec<MuxTab> {
    tabs.iter()
        .map(|tab| MuxTab {
            name: tab.name.clone(),
            panes: tab
                .panes
                .iter()
                .map(|pane| MuxPane {
                    cwd: pane.cwd.clone().filter(|c| exists(Path::new(c))),
                    ..pane.clone()
                })
                .collect(),
        })
        .collect()
}

/// Replace this process with zellij. Only returns on failure.
fn exec_zellij(args: &[&str]) -> anyhow::Error {
    let err = Command::new("zellij").args(args).exec();
    anyhow::Error::new(err).context("failed to start zellij")
}

/// `~/.nibble/mux`, holding generated layouts and watcher locks.
fn mux_dir() -> Result<PathBuf> {
    let dir = crate::db::ensure_data_dir()?.join("mux");
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

fn zellij_session_state(name: &str) -> Result<ZellijState> {
    let out = Command::new("zellij")
        .args(["list-sessions", "--no-formatting"])
        .output()
        .context("zellij is not installed or not on PATH")?;
    // With no sessions at all zellij exits non-zero; that is "missing".
    Ok(parse_session_state(
        &String::from_utf8_lossy(&out.stdout),
        name,
    ))
}

/// Find `name` in `zellij list-sessions --no-formatting` output, e.g.
/// `main [Created 1day ago] (current)` or
/// `old [Created 2days ago] (EXITED - attach to resurrect)`.
fn parse_session_state(listing: &str, name: &str) -> ZellijState {
    listing
        .lines()
        .find(|line| line.split_whitespace().next() == Some(name))
        .map_or(ZellijState::Missing, |line| {
            if line.contains("(EXITED") {
                ZellijState::Dead
            } else {
                ZellijState::Live
            }
        })
}

// ── Layout ─────────────────────────────────────────────────────────────────

/// Tab template for rebuilt sessions without the nibble sidebar: zellij's
/// stock tab bar and status bar.
const BARS_TAB_TEMPLATE: &str = "    default_tab_template {
        pane size=1 borderless=true {
            plugin location=\"zellij:tab-bar\"
        }
        children
        pane size=2 borderless=true {
            plugin location=\"zellij:status-bar\"
        }
    }
";

/// A zellij layout that recreates every recorded tab, each agent pane
/// running `nibble mux --resume <task>` in its old directory, then dropping
/// to the user's shell once the agent exits (as if it had been started from
/// that shell). `close_on_exit` makes leaving that shell close the pane:
/// zellij would otherwise hold it, and Enter would re-run the resume. With
/// `sidebar`, tabs get the template `nibble sidebar
/// --install` puts in the default layout (an explicit layout replaces it);
/// other custom default layouts are not carried over.
fn render_layout(tabs: &[MuxTab], nibble_bin: &Path, shell: &str, sidebar: bool) -> String {
    let mut out = String::from("// generated by `nibble mux` — rebuilt agent tabs\nlayout {\n");
    out.push_str(if sidebar {
        crate::status::SIDEBAR_TAB_TEMPLATE
    } else {
        BARS_TAB_TEMPLATE
    });
    let bin = sh_quote(&nibble_bin.to_string_lossy());
    for tab in tabs {
        out.push_str(&format!("    tab name={} {{\n", kdl_str(&tab.name)));
        for pane in &tab.panes {
            let script = format!(
                "{}; exec {}",
                resume_invocation(&bin, &pane.task_id),
                sh_quote(shell)
            );
            let cwd = pane
                .cwd
                .as_deref()
                .map(|c| format!(" cwd={}", kdl_str(c)))
                .unwrap_or_default();
            out.push_str(&format!(
                "        pane name={} command={}{cwd} {{\n            close_on_exit true\n            args \"-c\" {}\n        }}\n",
                kdl_str(&pane.title),
                kdl_str(shell),
                kdl_str(&script),
            ));
        }
        out.push_str("    }\n");
    }
    out.push_str("}\n");
    out
}

/// The shell text a restored pane runs: `'<bin>' mux --resume '<id>'`.
/// [`resume_target`] reads the id back out of it, so the two must agree.
fn resume_invocation(quoted_bin: &str, task_id: &str) -> String {
    format!("{quoted_bin} mux {RESUME_FLAG} {}", sh_quote(task_id))
}

/// A KDL string literal.
fn kdl_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A POSIX shell single-quoted word.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ── Snapshot ───────────────────────────────────────────────────────────────

/// The agent tabs of a live session, from its panes and nibble's tasks.
///
/// A pane counts as an agent pane when a task registered in it (same zellij
/// session and pane id, registered after `epoch`) is still alive, or when it
/// is a restored pane whose resume command has not registered its agent
/// yet. Panes whose (known) foreground process is a plain shell hold no agent,
/// and only agents `nibble mux` can resume are recorded.
fn snapshot(
    panes: &[ZellijPane],
    tasks: &[Task],
    session: &str,
    epoch: Option<DateTime<Utc>>,
    pid_alive: &dyn Fn(i32) -> bool,
) -> Vec<MuxTab> {
    let mut rows: Vec<(&ZellijPane, MuxPane)> = Vec::new();
    for pane in panes.iter().filter(|p| !p.is_plugin && !p.exited) {
        // For a pane started with a command (restored panes), zellij keeps
        // reporting that launch command, so only a pane_command that
        // differs from it is a live reading of the foreground process.
        if pane_at_shell(pane) {
            continue;
        }
        let in_pane = |t: &&Task| {
            t.sandbox_type != SandboxType::Podman
                && crate::status::session_of(t) == Some(session)
                && crate::status::pane_id_of(t) == Some(pane.id)
                // Whole seconds: the DB truncates created_at.
                && epoch.is_none_or(|e| t.created_at.timestamp() >= e.timestamp())
        };
        let live = tasks
            .iter()
            .filter(in_pane)
            .filter(|t| t.status != TaskStatus::Exited && t.pid.is_none_or(pid_alive))
            .max_by_key(|t| t.created_at);
        let task = match live {
            Some(t) => t,
            // An agent registered here and is gone: the pane is done.
            None if tasks.iter().any(|t| in_pane(&t)) => continue,
            None => {
                let target = pane.terminal_command.as_deref().and_then(resume_target);
                match target.and_then(|id| tasks.iter().find(|t| t.task_id == id)) {
                    Some(t) => t,
                    None => continue,
                }
            }
        };
        if resumable_agent(&task.agent_type) {
            rows.push((pane, mux_pane(task, pane)));
        }
    }
    rows.sort_by_key(|(p, _)| (p.tab_position, p.pane_x, p.pane_y));

    let mut tabs: Vec<MuxTab> = Vec::new();
    let mut last_tab: Option<u32> = None;
    for (pane, entry) in rows {
        if last_tab != Some(pane.tab_position) {
            tabs.push(MuxTab {
                name: pane.tab_name.clone(),
                panes: Vec::new(),
            });
            last_tab = Some(pane.tab_position);
        }
        if let Some(tab) = tabs.last_mut() {
            tab.panes.push(entry);
        }
    }
    tabs
}

fn mux_pane(task: &Task, pane: &ZellijPane) -> MuxPane {
    MuxPane {
        task_id: task.task_id.clone(),
        title: task.title.clone(),
        cwd: task
            .context
            .as_ref()
            .and_then(|c| c.project_path.clone())
            .or_else(|| pane.pane_cwd.clone()),
    }
}

/// Agents `plan_resume` knows how to bring back.
fn resumable_agent(agent: &AgentType) -> bool {
    matches!(agent, AgentType::ClaudeCode | AgentType::Pi)
}

/// The task id in a restored pane's command, as written by
/// [`resume_invocation`] (`... mux --resume '<id>'; ...`).
fn resume_target(command: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    words.find(|w| *w == RESUME_FLAG)?;
    let id = words
        .next()?
        .trim_end_matches(';')
        .trim_matches(|c| c == '\'' || c == '"');
    is_task_id(id).then(|| id.to_string())
}

/// Task ids are UUIDs or `<agent>-<uuid>`: plain enough to embed in a
/// shell command without quoting surprises.
fn is_task_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

// ── Watcher ────────────────────────────────────────────────────────────────

/// Start the snapshot watcher for `name` in the background, detached from
/// this terminal. A second watcher for the same session waits up to
/// `LOCK_WAIT` for the lock in `run_watcher` and exits if it never gets it,
/// so calling this on every open is safe.
fn spawn_watcher(name: &str) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate nibble binary")?;
    let log_dir = crate::db::ensure_data_dir()?.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("mux-{name}.log"));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let mut cmd = Command::new(exe);
    cmd.args(["mux", name, "--watch"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        // Target the session by name, never by inherited pane context.
        .env_remove("ZELLIJ")
        .env_remove("ZELLIJ_SESSION_NAME")
        .env_remove("ZELLIJ_PANE_ID");
    // SAFETY: setsid is async-signal-safe; it detaches the watcher from the
    // terminal so closing it (or the zellij client) does not signal it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("failed to start the mux watcher")?;
    Ok(())
}

/// Debounces snapshots: a new set of agent tabs is committed only once two
/// consecutive samples agree, so a session caught mid-teardown (panes
/// closing one by one while it is killed) never overwrites a good snapshot.
#[derive(Default)]
struct Debounce {
    pending: Option<Vec<MuxTab>>,
}

impl Debounce {
    /// Feed a sample; returns the tabs to commit when they differ from
    /// `stored` and the previous sample matched.
    pub(crate) fn feed(&mut self, tabs: Vec<MuxTab>, stored: &[MuxTab]) -> Option<Vec<MuxTab>> {
        let stable = self.pending.as_deref() == Some(tabs.as_slice());
        let commit = (stable && tabs.as_slice() != stored).then(|| tabs.clone());
        self.pending = Some(tabs);
        commit
    }

    fn reset(&mut self) {
        self.pending = None;
    }
}

/// Holds an exclusive lock on `~/.nibble/mux/<name>.lock` for the watcher's
/// lifetime; released by the kernel when the process exits.
struct WatcherLock {
    _file: std::fs::File,
}

fn try_lock(name: &str) -> Result<Option<WatcherLock>> {
    try_lock_in(&mux_dir()?, name)
}

/// Retry [`try_lock_in`] every `interval` until `timeout` passes.
fn wait_for_lock(
    dir: &Path,
    name: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<Option<WatcherLock>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(lock) = try_lock_in(dir, name)? {
            return Ok(Some(lock));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(interval);
    }
}

/// `None` when another process (a running watcher) holds the lock.
fn try_lock_in(dir: &Path, name: &str) -> Result<Option<WatcherLock>> {
    use std::os::fd::AsRawFd;
    let path = dir.join(format!("{name}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    // SAFETY: flock on a valid, owned file descriptor.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(anyhow::Error::new(err).context("failed to lock mux watcher"));
    }
    Ok(Some(WatcherLock { _file: file }))
}

fn log_line(name: &str, msg: &str) {
    eprintln!(
        "{} mux[{name}] {msg}",
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );
}

/// The watcher loop: sample the session's panes and commit snapshot changes
/// until the session has been gone for `GONE_POLLS` samples (or never
/// appeared within `STARTUP_POLLS`).
fn run_watcher(db: &Database, name: &str) -> Result<()> {
    // A watcher of the session's previous incarnation may still be noticing
    // that it died: wait for it rather than leaving the new one unrecorded.
    let Some(_lock) = wait_for_lock(&mux_dir()?, name, LOCK_WAIT, Duration::from_secs(1))? else {
        return Ok(()); // another watcher records this (live) session
    };
    log_line(name, "watcher started");
    let mut debounce = Debounce::default();
    let mut seen = false;
    let mut misses = 0u32;
    let mut errors = 0u32;
    loop {
        match crate::status::list_zellij_panes(Some(name)) {
            Ok(panes) => {
                seen = true;
                misses = 0;
                errors = 0;
                if let Err(e) = record(db, name, &panes, &mut debounce) {
                    log_line(name, &format!("snapshot failed: {e:#}"));
                }
            }
            Err(PaneListError::Gone(_)) => {
                misses += 1;
                debounce.reset();
                let limit = if seen { GONE_POLLS } else { STARTUP_POLLS };
                if misses >= limit {
                    log_line(name, "session gone; watcher exiting");
                    return Ok(());
                }
            }
            Err(PaneListError::Failed(e)) => {
                errors += 1;
                debounce.reset();
                log_line(name, &format!("sample failed: {e}"));
                // Release the lock so the next `nibble mux` can take over.
                if errors >= ERROR_POLLS {
                    log_line(name, "sampling keeps failing; watcher exiting");
                    return Ok(());
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn record(db: &Database, name: &str, panes: &[ZellijPane], debounce: &mut Debounce) -> Result<()> {
    let mut manifest = load_manifest(db, name)?.unwrap_or_default();
    let tasks = db.list_tasks()?;
    let tabs = snapshot(
        panes,
        &tasks,
        name,
        manifest.epoch,
        &crate::status::pid_alive,
    );
    if let Some(tabs) = debounce.feed(tabs, &manifest.tabs) {
        let agents: usize = tabs.iter().map(|t| t.panes.len()).sum();
        manifest.tabs = tabs;
        manifest.updated_at = Some(Utc::now());
        save_manifest(db, name, &manifest)?;
        log_line(name, &format!("recorded {agents} agent(s)"));
    }
    Ok(())
}

// ── Resuming one pane ──────────────────────────────────────────────────────

/// How to bring one recorded agent back.
#[derive(Debug, PartialEq)]
struct ResumePlan {
    program: PathBuf,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    /// Sandbox container to start before attaching, when it is stopped.
    container: Option<String>,
}

/// Work out the command that resumes `task`'s conversation.
///
/// Host agents go through their nibble wrapper (so they register and report
/// status like any other launch): Claude with `--resume <session>`, pi with
/// `--session <file>`. Sandbox windows re-run `nibble sandbox attach` on
/// their repo with the window's own session. Other agents are not resumable.
fn plan_resume(
    task: &Task,
    parent: Option<&Task>,
    wrappers_dir: &Path,
    nibble_bin: &Path,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<ResumePlan> {
    let ctx = task.context.as_ref();
    let cwd = ctx
        .and_then(|c| c.project_path.as_deref())
        .map(PathBuf::from);
    let claude_session = ctx
        .and_then(|c| c.claude_session_id.as_deref())
        .filter(|id| !id.starts_with("ses_"))
        .map(str::to_string);
    let pi_session = ctx
        .and_then(|c| c.extra.get("pi_session_path"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let sandbox = parent.or_else(|| (task.sandbox_type == SandboxType::Podman).then_some(task));
    if let Some(sandbox) = sandbox {
        let path = sandbox
            .repo_path
            .clone()
            .or_else(|| cwd.as_ref().map(|p| p.to_string_lossy().to_string()))
            .with_context(|| format!("sandbox task {} has no repo path", task.task_id))?;
        let mut args = vec!["sandbox".to_string(), "attach".to_string(), path];
        let session = match task.agent_type {
            AgentType::ClaudeCode => claude_session,
            AgentType::Pi => {
                args.push("--pi".to_string());
                pi_session.map(|p| crate::session::session_id_from_filename(Path::new(&p)))
            }
            ref other => bail!("cannot restore {other} sandbox agents (only claude_code and pi)"),
        };
        if let Some(id) = session {
            args.push("--session".to_string());
            args.push(id);
        }
        return Ok(ResumePlan {
            program: nibble_bin.to_path_buf(),
            args,
            cwd: cwd.filter(|p| exists(p)),
            container: sandbox
                .container_name
                .clone()
                .or_else(|| sandbox.container_id.clone()),
        });
    }

    let (wrapper, args) = match task.agent_type {
        AgentType::ClaudeCode => (
            "claude-wrapper",
            claude_session
                .map(|id| vec!["--resume".to_string(), id])
                .unwrap_or_default(),
        ),
        AgentType::Pi => (
            "pi-wrapper",
            match pi_session.filter(|p| exists(Path::new(p))) {
                Some(path) => vec!["--session".to_string(), path],
                // The recorded file is gone: continue the newest session
                // for this directory instead.
                None => vec!["--continue".to_string()],
            },
        ),
        ref other => bail!("cannot restore {other} agents (only claude_code and pi)"),
    };
    let program = wrappers_dir.join(wrapper);
    if !exists(&program) {
        return Err(MissingWrapper(program).into());
    }
    Ok(ResumePlan {
        program,
        args,
        cwd: cwd.filter(|p| exists(p)),
        container: None,
    })
}

/// What `nibble rc` may do so a tab can be opened in a session from
/// outside it, given the same inputs `nibble mux NAME` decides from.
#[derive(Debug, PartialEq)]
enum BackgroundPlan {
    /// The session is running.
    Ready,
    /// Start it (fresh, or zellij's resurrectable copy) in the background.
    Start,
    /// It is down with recorded agent tabs. Only `nibble mux NAME` rebuilds
    /// those (each pane resuming its conversation); starting it here would
    /// let the watcher record the session without them and forget them.
    Refuse { tabs: usize },
}

fn plan_background(state: ZellijState, manifest: Option<&Manifest>) -> BackgroundPlan {
    match plan_open(state, manifest) {
        OpenPlan::Attach => BackgroundPlan::Ready,
        OpenPlan::Create | OpenPlan::Resurrect => BackgroundPlan::Start,
        OpenPlan::Restore { .. } => BackgroundPlan::Refuse {
            tabs: manifest.map_or(0, |m| m.tabs.len()),
        },
    }
}

/// Make sure zellij session `name` is running so tabs can be opened in it
/// from outside (`nibble rc`). A live session is used as it is; a missing
/// or dead one without recorded agents is started in the background and
/// recorded like `nibble mux NAME`. A session that is down with recorded
/// agent tabs is refused: `nibble mux NAME` must restore it first.
pub(crate) fn ensure_background_session(db: &Database, name: &str) -> Result<()> {
    validate_session_name(name)?;
    let state = zellij_session_state(name)?;
    let manifest = load_manifest(db, name)?;
    // Judge a down session by what `nibble mux NAME` would actually
    // restore (the same filter cmd_open applies), not the raw record.
    let restorable = match (&manifest, state) {
        (Some(m), ZellijState::Dead | ZellijState::Missing) => Some(filter_restorable(m, &|id| {
            resume_plan_for(db, id).map(drop)
        })?),
        (m, _) => m.clone(),
    };
    match plan_background(state, restorable.as_ref()) {
        // Same as `nibble mux NAME` attaching: the tab opened next comes
        // back after a crash.
        BackgroundPlan::Ready => return record_running_session(db, name, manifest.as_ref()),
        BackgroundPlan::Refuse { tabs } => bail!(
            "zellij session '{name}' is not running and nibble mux has {tabs} recorded agent tab(s) \
             for it. Run `nibble mux {name}` to bring them back, then start the session again"
        ),
        BackgroundPlan::Start => {}
    }
    // Same bookkeeping as `nibble mux NAME` creating or resurrecting it.
    let mut manifest = manifest.unwrap_or_default();
    manifest.epoch = Some(Utc::now());
    save_manifest(db, name, &manifest)?;
    let out = Command::new("zellij")
        .args(["attach", "--create-background", name])
        .env_remove("ZELLIJ")
        .env_remove("ZELLIJ_SESSION_NAME")
        .env_remove("ZELLIJ_PANE_ID")
        .output()
        .context("zellij is not installed or not on PATH")?;
    if !out.status.success() {
        bail!(
            "could not start zellij session '{name}': {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    spawn_watcher(name)
}

/// Record a running session (an empty manifest if nibble has none) and make
/// sure a watcher keeps its snapshot. Shared by `nibble mux NAME` attaching
/// and `nibble rc` opening a tab in it.
fn record_running_session(db: &Database, name: &str, manifest: Option<&Manifest>) -> Result<()> {
    if manifest.is_none() {
        save_manifest(db, name, &Manifest::default())?;
    }
    ensure_watcher(name)
}

/// Start the session's watcher unless one is running (it holds the lock).
fn ensure_watcher(name: &str) -> Result<()> {
    match try_lock(name)? {
        // Nobody holds it: release it for the watcher we start.
        Some(lock) => {
            drop(lock);
            spawn_watcher(name)
        }
        None => Ok(()),
    }
}

/// Fail as a restore does when a nibble agent wrapper is not installed.
pub(crate) fn require_wrapper(path: &Path) -> Result<()> {
    if path.exists() {
        Ok(())
    } else {
        Err(MissingWrapper(path.to_path_buf()).into())
    }
}

/// A nibble agent wrapper is not installed: fixable by re-running
/// install.sh, so restores stop instead of skipping the agent.
#[derive(Debug)]
struct MissingWrapper(PathBuf);

impl std::fmt::Display for MissingWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "nibble wrapper {} is missing; run ./install.sh from the nibble repo",
            self.0.display()
        )
    }
}

impl std::error::Error for MissingWrapper {}

/// Where install.sh puts the agent wrappers (`~/.nibble/wrappers`).
pub(crate) fn wrappers_dir() -> Result<PathBuf> {
    Ok(crate::db::ensure_data_dir()?.join("wrappers"))
}

/// Look up a recorded task (and its sandbox, for sandbox windows) and plan
/// its resume against the real filesystem.
fn resume_plan_for(db: &Database, task_id: &str) -> Result<(Task, ResumePlan)> {
    if !is_task_id(task_id) {
        bail!("invalid task id '{task_id}'");
    }
    let task = db
        .get_task_by_id(task_id)?
        .with_context(|| format!("task {task_id} is no longer in nibble's database"))?;
    let parent = match task
        .context
        .as_ref()
        .and_then(|c| c.extra.get("parent_task_id"))
        .and_then(|v| v.as_str())
    {
        Some(id) => Some(
            db.get_task_by_id(id)?
                .with_context(|| format!("sandbox {id} of task {task_id} is gone"))?,
        ),
        None => None,
    };
    let nibble_bin = std::env::current_exe().context("cannot locate nibble binary")?;
    let plan = plan_resume(
        &task,
        parent.as_ref(),
        &wrappers_dir()?,
        &nibble_bin,
        &|p| p.exists(),
    )?;
    Ok((task, plan))
}

/// `nibble mux --resume <task>`: run in a restored pane, replaced by the
/// agent it resumes.
fn cmd_resume(db: &Database, task_id: &str) -> Result<()> {
    let (task, plan) = resume_plan_for(db, task_id)?;

    if let Some(container) = &plan.container {
        start_container_if_stopped(container);
    }
    eprintln!(
        "nibble: resuming {} ({})",
        task.title,
        short_id(&task.task_id)
    );
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args);
    if let Some(dir) = &plan.cwd {
        cmd.current_dir(dir);
    }
    let err = cmd.exec();
    Err(anyhow::Error::new(err).context(format!("failed to run {}", plan.program.display())))
}

/// Best effort: `sandbox attach` re-spawns a missing sandbox by path, but a
/// merely stopped container (after a reboot) only needs starting.
fn start_container_if_stopped(container: &str) {
    let sandbox = PodmanSandbox::new();
    if matches!(sandbox.status(container), Ok(ContainerStatus::Running)) {
        return;
    }
    eprintln!("nibble: starting sandbox {container}...");
    if let Err(e) = sandbox.start(container) {
        eprintln!("nibble: could not start {container} ({e:#}); attach will re-spawn it");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TaskContext;
    use std::collections::HashMap;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    fn task(id: &str, agent: AgentType, session: &str, pane: u32, created: i64) -> Task {
        let mut t = Task::new(id.to_string(), agent, format!("[{id}]"), None, None);
        t.created_at = ts(created);
        let mut extra = HashMap::new();
        extra.insert(
            crate::models::ZELLIJ_SESSION_KEY.to_string(),
            serde_json::json!(session),
        );
        extra.insert("zellij_pane_id".to_string(), serde_json::json!(pane));
        t.context = Some(TaskContext {
            project_path: Some(format!("/repo/{id}")),
            extra,
            ..Default::default()
        });
        t
    }

    fn pane(id: u32, tab: u32, x: u16, command: Option<&str>) -> ZellijPane {
        ZellijPane {
            id,
            tab_position: tab,
            tab_name: format!("Tab {tab}"),
            pane_x: x,
            pane_command: command.map(str::to_string),
            ..Default::default()
        }
    }

    fn ids(tabs: &[MuxTab]) -> Vec<Vec<&str>> {
        tabs.iter()
            .map(|t| t.panes.iter().map(|p| p.task_id.as_str()).collect())
            .collect()
    }

    const ALIVE: &dyn Fn(i32) -> bool = &|_| true;

    #[test]
    fn snapshot_groups_live_agents_by_tab_left_to_right() {
        let tasks = vec![
            task("a", AgentType::ClaudeCode, "main", 3, 0),
            task("b", AgentType::Pi, "main", 1, 0),
            task("c", AgentType::ClaudeCode, "main", 7, 0),
        ];
        let panes = vec![
            pane(3, 0, 100, Some("claude")),
            pane(1, 0, 0, Some("pi")),
            pane(7, 1, 0, Some("/usr/bin/claude")),
        ];
        let tabs = snapshot(&panes, &tasks, "main", None, ALIVE);
        assert_eq!(ids(&tabs), vec![vec!["b", "a"], vec!["c"]]);
        assert_eq!(tabs[0].name, "Tab 0");
        assert_eq!(tabs[0].panes[0].cwd.as_deref(), Some("/repo/b"));
    }

    #[test]
    fn snapshot_ignores_other_sessions_plugins_and_shell_panes() {
        let tasks = vec![
            task("other", AgentType::ClaudeCode, "work", 1, 0),
            task("shell", AgentType::ClaudeCode, "main", 2, 0),
        ];
        let mut plugin = pane(1, 0, 0, Some("claude"));
        plugin.is_plugin = true;
        let panes = vec![
            plugin,
            pane(1, 0, 0, Some("claude")),
            pane(2, 0, 9, Some("-zsh")),
        ];
        assert!(snapshot(&panes, &tasks, "main", None, ALIVE).is_empty());
    }

    #[test]
    fn snapshot_drops_exited_and_dead_pid_agents() {
        let mut exited = task("x", AgentType::ClaudeCode, "main", 1, 0);
        exited.set_exited(None);
        let mut dead = task("d", AgentType::ClaudeCode, "main", 2, 0);
        dead.pid = Some(42);
        let tasks = vec![exited, dead];
        let panes = vec![pane(1, 0, 0, Some("claude")), pane(2, 0, 5, Some("claude"))];
        assert!(snapshot(&panes, &tasks, "main", None, &|_| false).is_empty());
    }

    #[test]
    fn snapshot_prefers_newest_task_in_a_pane_and_respects_epoch() {
        let tasks = vec![
            task("old", AgentType::ClaudeCode, "main", 1, 0),
            task("new", AgentType::ClaudeCode, "main", 1, 50),
        ];
        let panes = vec![pane(1, 0, 0, Some("claude"))];
        assert_eq!(
            ids(&snapshot(&panes, &tasks, "main", None, ALIVE)),
            vec![vec!["new"]]
        );
        // The DB keeps whole seconds: an agent registering within the
        // epoch's own second (restore → resume is that fast) still counts.
        let fast = vec![task("fast", AgentType::ClaudeCode, "main", 1, 10)];
        let sub_second_epoch = ts(10) + chrono::Duration::milliseconds(500);
        assert_eq!(
            ids(&snapshot(
                &panes,
                &fast,
                "main",
                Some(sub_second_epoch),
                ALIVE
            )),
            vec![vec!["fast"]]
        );
        // A task from before the session was recreated can't own its panes.
        let only_old = vec![task("old", AgentType::ClaudeCode, "main", 1, 0)];
        assert!(snapshot(&panes, &only_old, "main", Some(ts(10)), ALIVE).is_empty());
    }

    #[test]
    fn snapshot_keeps_restored_pane_until_its_agent_registers() {
        let tasks = vec![task("prev", AgentType::ClaudeCode, "main", 4, 0)];
        let mut restoring = pane(0, 0, 0, Some("/home/u/.local/bin/nibble"));
        restoring.terminal_command =
            Some("/bin/zsh -c '/x/nibble' mux --resume 'prev'; exec '/bin/zsh'".to_string());
        let tabs = snapshot(&[restoring.clone()], &tasks, "main", Some(ts(10)), ALIVE);
        assert_eq!(ids(&tabs), vec![vec!["prev"]]);

        // Once the resumed agent registers in the pane, it takes over...
        let mut tasks2 = tasks.clone();
        tasks2.push(task("next", AgentType::ClaudeCode, "main", 0, 20));
        let tabs = snapshot(&[restoring.clone()], &tasks2, "main", Some(ts(10)), ALIVE);
        assert_eq!(ids(&tabs), vec![vec!["next"]]);

        // Zellij reports a command pane's launch command (a shell, here) as
        // its pane_command, not the agent running in it: that must not read
        // as "no agent here".
        let mut as_zellij_reports = restoring.clone();
        as_zellij_reports.pane_command = as_zellij_reports.terminal_command.clone();
        let tabs = snapshot(&[as_zellij_reports], &tasks2, "main", Some(ts(10)), ALIVE);
        assert_eq!(ids(&tabs), vec![vec!["next"]]);

        // ...and when that agent exits, the pane is no longer an agent pane.
        tasks2[1].set_exited(None);
        assert!(snapshot(&[restoring], &tasks2, "main", Some(ts(10)), ALIVE).is_empty());
    }

    #[test]
    fn snapshot_skips_sandbox_parent_rows() {
        let mut parent = task("sb", AgentType::ClaudeCode, "main", 1, 0);
        parent.sandbox_type = SandboxType::Podman;
        let panes = vec![pane(1, 0, 0, Some("podman"))];
        assert!(snapshot(&panes, &[parent], "main", None, ALIVE).is_empty());
    }

    #[test]
    fn is_shell_matches_shell_basenames_only() {
        use crate::status::is_shell;
        assert!(is_shell("zsh"));
        assert!(is_shell("/usr/bin/bash --login"));
        assert!(is_shell("-zsh"));
        assert!(!is_shell(
            "/home/u/.local/share/mise/installs/claude/latest/claude"
        ));
        assert!(!is_shell("pi"));
        assert!(!is_shell(""));
    }

    #[test]
    fn resume_target_extracts_valid_ids() {
        assert_eq!(
            resume_target("zsh -c '/b/nibble' mux --resume 'claude-1a2b'; exec 'zsh'").as_deref(),
            Some("claude-1a2b")
        );
        assert_eq!(
            resume_target("nibble mux --resume abc;").as_deref(),
            Some("abc")
        );
        assert_eq!(resume_target("claude --resume").as_deref(), None);
        assert_eq!(resume_target("x --resume 'a$b'").as_deref(), None);
        assert_eq!(resume_target("pi").as_deref(), None);
    }

    #[test]
    fn task_and_session_name_validation() {
        assert!(is_task_id("0f8e-AB_c"));
        assert!(!is_task_id(""));
        assert!(!is_task_id("a b"));
        assert!(validate_session_name("main").is_ok());
        assert!(validate_session_name("my.work_2").is_ok());
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("-x").is_err());
        assert!(validate_session_name("a/b").is_err());
        assert!(validate_session_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn debounce_commits_only_stable_changes() {
        let tab = |id: &str| MuxTab {
            name: "t".into(),
            panes: vec![MuxPane {
                task_id: id.into(),
                title: id.into(),
                cwd: None,
            }],
        };
        let stored = vec![tab("a")];
        let mut d = Debounce::default();
        assert_eq!(d.feed(vec![tab("b")], &stored), None); // first sight
        assert_eq!(d.feed(vec![tab("b")], &stored), Some(vec![tab("b")]));
        // Same as stored: nothing to commit.
        let mut d = Debounce::default();
        d.feed(stored.clone(), &stored);
        assert_eq!(d.feed(stored.clone(), &stored), None);
        // A transient sample in between breaks the streak.
        let mut d = Debounce::default();
        d.feed(vec![], &stored);
        d.reset();
        assert_eq!(d.feed(vec![], &stored), None);
        assert_eq!(d.feed(vec![], &stored), Some(vec![]));
    }

    #[test]
    fn parse_session_state_reads_zellij_listing() {
        let listing = "old [Created 2days ago] (EXITED - attach to resurrect)\n\
                       main [Created 1day ago] (current)\n\
                       mainly [Created 1h ago]\n";
        assert_eq!(parse_session_state(listing, "main"), ZellijState::Live);
        assert_eq!(parse_session_state(listing, "old"), ZellijState::Dead);
        assert_eq!(parse_session_state(listing, "mainly"), ZellijState::Live);
        assert_eq!(parse_session_state(listing, "nope"), ZellijState::Missing);
        assert_eq!(parse_session_state("", "main"), ZellijState::Missing);
    }

    #[test]
    fn background_start_never_bypasses_a_restore() {
        let recorded = Manifest {
            tabs: one_tab(&[("t1", Some("/r"))]),
            ..Manifest::default()
        };
        let empty = Manifest::default();
        assert_eq!(
            plan_background(ZellijState::Live, Some(&recorded)),
            BackgroundPlan::Ready
        );
        for state in [ZellijState::Dead, ZellijState::Missing] {
            assert_eq!(
                plan_background(state, Some(&recorded)),
                BackgroundPlan::Refuse { tabs: 1 },
                "{state:?} with recorded agents must be left to `nibble mux`"
            );
            assert_eq!(plan_background(state, Some(&empty)), BackgroundPlan::Start);
            assert_eq!(plan_background(state, None), BackgroundPlan::Start);
            // Tabs `nibble mux` could not restore (their tasks are gone) do
            // not block a start: the filtered record is what is judged.
            let unrestorable = filter_restorable(&recorded, &|_| bail!("task gone")).unwrap();
            assert_eq!(
                plan_background(state, Some(&unrestorable)),
                BackgroundPlan::Start
            );
        }
    }

    #[test]
    fn plan_open_covers_every_state() {
        let empty = Manifest::default();
        let full = Manifest {
            tabs: vec![MuxTab {
                name: "t".into(),
                panes: vec![],
            }],
            ..Default::default()
        };
        use OpenPlan::*;
        use ZellijState::*;
        assert_eq!(plan_open(Live, Some(&full)), Attach);
        assert_eq!(plan_open(Live, None), Attach);
        assert_eq!(plan_open(Dead, Some(&full)), Restore { discard_dead: true });
        assert_eq!(
            plan_open(Missing, Some(&full)),
            Restore {
                discard_dead: false
            }
        );
        assert_eq!(plan_open(Dead, Some(&empty)), Resurrect);
        assert_eq!(plan_open(Dead, None), Resurrect);
        assert_eq!(plan_open(Missing, Some(&empty)), Create);
        assert_eq!(plan_open(Missing, None), Create);
    }

    #[test]
    fn render_layout_builds_resume_panes_with_quoting() {
        let tabs = vec![MuxTab {
            name: "Tab \"1\"".into(),
            panes: vec![
                MuxPane {
                    task_id: "t1".into(),
                    title: "[nibble:main]".into(),
                    cwd: Some("/w/it's".into()),
                },
                MuxPane {
                    task_id: "t2".into(),
                    title: "x".into(),
                    cwd: None,
                },
            ],
        }];
        let kdl = render_layout(&tabs, Path::new("/b/nib ble"), "/bin/zsh", false);
        assert!(kdl.contains("tab name=\"Tab \\\"1\\\"\" {"));
        assert!(kdl.contains("pane name=\"[nibble:main]\" command=\"/bin/zsh\" cwd=\"/w/it's\" {"));
        assert!(kdl.contains("args \"-c\" \"'/b/nib ble' mux --resume 't1'; exec '/bin/zsh'\""));
        assert!(kdl.contains("pane name=\"x\" command=\"/bin/zsh\" {"));
        assert!(kdl.contains("zellij:tab-bar"));
        assert!(!kdl.contains("status\" \"--watch"));
        // Without this zellij holds an exited pane and Enter re-runs the
        // resume, so the agent could never be closed.
        assert_eq!(kdl.matches("close_on_exit true").count(), 2);
        // The snapshot can read its own resume command back.
        assert_eq!(
            resume_target("/bin/zsh -c '/b/nib ble' mux --resume 't1'; exec '/bin/zsh'").as_deref(),
            Some("t1")
        );
        let invocation = resume_invocation(&sh_quote("/b/nib ble"), "claude-1a2b");
        assert_eq!(resume_target(&invocation).as_deref(), Some("claude-1a2b"));
    }

    #[test]
    fn render_layout_keeps_the_nibble_sidebar_when_installed() {
        let tabs = vec![MuxTab {
            name: "t".into(),
            panes: vec![MuxPane {
                task_id: "t1".into(),
                title: "x".into(),
                cwd: None,
            }],
        }];
        let kdl = render_layout(&tabs, Path::new("/n"), "/bin/sh", true);
        assert!(kdl.contains(crate::status::SIDEBAR_TAB_TEMPLATE));
        assert!(kdl.contains("zellij:tab-bar"));
        assert!(kdl.contains("mux --resume 't1'"));
    }

    #[test]
    fn snapshot_records_only_resumable_agents() {
        let tasks = vec![
            task("omp", AgentType::Omp, "main", 1, 0),
            task("hermes", AgentType::Hermes, "main", 2, 0),
            task("pi", AgentType::Pi, "main", 3, 0),
        ];
        let panes = vec![
            pane(1, 0, 0, Some("omp")),
            pane(2, 0, 5, Some("podman")),
            pane(3, 0, 9, Some("pi")),
        ];
        assert_eq!(
            ids(&snapshot(&panes, &tasks, "main", None, ALIVE)),
            vec![vec!["pi"]]
        );
    }

    #[test]
    fn watcher_lock_is_exclusive_per_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let held = try_lock_in(dir.path(), "main").unwrap();
        assert!(held.is_some());
        // flock is per open file description: a second open is refused.
        assert!(try_lock_in(dir.path(), "main").unwrap().is_none());
        assert!(try_lock_in(dir.path(), "work").unwrap().is_some());
        drop(held);
        assert!(try_lock_in(dir.path(), "main").unwrap().is_some());
    }

    #[test]
    fn wait_for_lock_takes_over_once_the_holder_exits() {
        let dir = tempfile::TempDir::new().unwrap();
        let held = try_lock_in(dir.path(), "main").unwrap();
        let tick = Duration::from_millis(10);
        assert!(
            wait_for_lock(dir.path(), "main", Duration::from_millis(30), tick)
                .unwrap()
                .is_none()
        );
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
        });
        assert!(
            wait_for_lock(dir.path(), "main", Duration::from_secs(5), tick)
                .unwrap()
                .is_some()
        );
        releaser.join().unwrap();
    }

    fn one_tab(panes: &[(&str, Option<&str>)]) -> Vec<MuxTab> {
        vec![MuxTab {
            name: "t".into(),
            panes: panes
                .iter()
                .map(|(id, cwd)| MuxPane {
                    task_id: (*id).into(),
                    title: (*id).into(),
                    cwd: cwd.map(str::to_string),
                })
                .collect(),
        }]
    }

    #[test]
    fn filter_restorable_skips_unplannable_and_stops_on_missing_wrapper() {
        let mut manifest = Manifest {
            epoch: Some(ts(1)),
            tabs: one_tab(&[("ok", None), ("gone", None)]),
            ..Default::default()
        };
        manifest.tabs.push(MuxTab {
            name: "only-gone".into(),
            panes: vec![MuxPane {
                task_id: "gone".into(),
                title: "gone".into(),
                cwd: None,
            }],
        });
        let plan = |id: &str| -> Result<()> {
            match id {
                "ok" => Ok(()),
                "nowrap" => Err(MissingWrapper(PathBuf::from("/w/pi-wrapper")).into()),
                _ => bail!("task {id} is no longer in nibble's database"),
            }
        };
        let kept = filter_restorable(&manifest, &plan).unwrap();
        assert_eq!(ids(&kept.tabs), vec![vec!["ok"]], "empty tabs dropped");
        assert_eq!(kept.epoch, manifest.epoch);

        let with_missing = Manifest {
            tabs: one_tab(&[("ok", None), ("nowrap", None)]),
            ..Default::default()
        };
        let err = filter_restorable(&with_missing, &plan).unwrap_err();
        assert!(format!("{err:#}").contains("install.sh"), "{err:#}");
    }

    #[test]
    fn without_missing_dirs_clears_only_gone_directories() {
        let tabs = one_tab(&[("a", Some("/here")), ("b", Some("/gone")), ("c", None)]);
        let out = without_missing_dirs(&tabs, &|p| p == Path::new("/here"));
        let cwds: Vec<_> = out[0].panes.iter().map(|p| p.cwd.as_deref()).collect();
        assert_eq!(cwds, vec![Some("/here"), None, None]);
        assert_eq!(ids(&out), ids(&tabs));
    }

    fn temp_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::open(dir.path().join("t.db")).unwrap();
        (db, dir)
    }

    #[test]
    fn record_commits_stable_samples_and_keeps_the_epoch() {
        let (db, _dir) = temp_db();
        // Pane 1 holds a task from a previous incarnation of the session;
        // pane 2 an agent registered after the session was (re)created.
        let stale = task("stale", AgentType::ClaudeCode, "main", 1, 0);
        let mut fresh = task("fresh", AgentType::ClaudeCode, "main", 2, 0);
        db.insert_task(&stale).unwrap();
        db.insert_task(&fresh).unwrap();
        fresh = db.get_task_by_id("fresh").unwrap().unwrap();
        let epoch = fresh.created_at - chrono::Duration::seconds(1);
        // The stale row predates the epoch.
        let mut old = db.get_task_by_id("stale").unwrap().unwrap();
        old.created_at = epoch - chrono::Duration::hours(1);
        db.delete_task_by_id("stale").unwrap();
        db.insert_task(&old).unwrap();
        let start = Manifest {
            epoch: Some(epoch),
            ..Default::default()
        };
        save_manifest(&db, "main", &start).unwrap();

        let panes = vec![pane(1, 0, 0, Some("claude")), pane(2, 0, 5, Some("claude"))];
        let mut debounce = Debounce::default();
        record(&db, "main", &panes, &mut debounce).unwrap();
        assert_eq!(
            load_manifest(&db, "main").unwrap().unwrap(),
            start,
            "one sample commits nothing"
        );

        record(&db, "main", &panes, &mut debounce).unwrap();
        let saved = load_manifest(&db, "main").unwrap().unwrap();
        assert_eq!(ids(&saved.tabs), vec![vec!["fresh"]], "stale pane ignored");
        assert_eq!(saved.epoch, Some(epoch), "epoch survives a commit");
        let committed_at = saved.updated_at.expect("commit stamps updated_at");

        // Unchanged samples leave the record (and its timestamp) alone.
        record(&db, "main", &panes, &mut debounce).unwrap();
        let again = load_manifest(&db, "main").unwrap().unwrap();
        assert_eq!(again.updated_at, Some(committed_at));
    }

    #[test]
    fn quoting_helpers_escape_specials() {
        assert_eq!(kdl_str("a\\b\"c\nd"), "\"a\\\\b\\\"c\\nd\"");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    fn with_session(mut t: Task, claude: Option<&str>, pi: Option<&str>) -> Task {
        let ctx = t.context.get_or_insert_default();
        ctx.claude_session_id = claude.map(str::to_string);
        if let Some(p) = pi {
            ctx.extra
                .insert("pi_session_path".to_string(), serde_json::json!(p));
        }
        t
    }

    const ALL_EXIST: &dyn Fn(&Path) -> bool = &|_| true;

    #[test]
    fn plan_resume_host_claude_uses_wrapper_and_session() {
        let t = with_session(
            task("c", AgentType::ClaudeCode, "main", 1, 0),
            Some("uuid-1"),
            None,
        );
        let plan = plan_resume(&t, None, Path::new("/w"), Path::new("/nib"), ALL_EXIST).unwrap();
        assert_eq!(
            plan,
            ResumePlan {
                program: PathBuf::from("/w/claude-wrapper"),
                args: vec!["--resume".into(), "uuid-1".into()],
                cwd: Some(PathBuf::from("/repo/c")),
                container: None,
            }
        );
        // No recorded session (or a legacy ses_ id): let the wrapper pick.
        let legacy = with_session(
            task("c", AgentType::ClaudeCode, "main", 1, 0),
            Some("ses_x"),
            None,
        );
        let plan =
            plan_resume(&legacy, None, Path::new("/w"), Path::new("/nib"), ALL_EXIST).unwrap();
        assert!(plan.args.is_empty());
    }

    #[test]
    fn plan_resume_host_pi_uses_session_file_or_continue() {
        let t = with_session(
            task("p", AgentType::Pi, "main", 1, 0),
            None,
            Some("/h/.pi/s.jsonl"),
        );
        let plan = plan_resume(&t, None, Path::new("/w"), Path::new("/nib"), ALL_EXIST).unwrap();
        assert_eq!(plan.program, PathBuf::from("/w/pi-wrapper"));
        assert_eq!(plan.args, vec!["--session", "/h/.pi/s.jsonl"]);

        let only_wrapper = |p: &Path| p.ends_with("pi-wrapper");
        let plan =
            plan_resume(&t, None, Path::new("/w"), Path::new("/nib"), &only_wrapper).unwrap();
        assert_eq!(plan.args, vec!["--continue"]);
        assert_eq!(plan.cwd, None); // missing directories are not entered
    }

    #[test]
    fn plan_resume_rejects_unsupported_agents_and_missing_wrappers() {
        for agent in [
            AgentType::Omp,
            AgentType::Hermes,
            AgentType::Unknown("x".into()),
        ] {
            let t = task("u", agent, "main", 1, 0);
            assert!(plan_resume(&t, None, Path::new("/w"), Path::new("/n"), ALL_EXIST).is_err());
        }
        let t = task("c", AgentType::ClaudeCode, "main", 1, 0);
        let err = plan_resume(&t, None, Path::new("/w"), Path::new("/n"), &|_| false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("install.sh"), "{err}");
    }

    #[test]
    fn plan_resume_sandbox_windows_reattach_by_repo() {
        let mut parent = task("sb", AgentType::ClaudeCode, "main", 1, 0);
        parent.sandbox_type = SandboxType::Podman;
        parent.repo_path = Some("/repos/app".into());
        parent.container_name = Some("nibble-app".into());

        let win = with_session(
            task("w", AgentType::ClaudeCode, "main", 2, 0),
            Some("uuid-9"),
            None,
        );
        let plan = plan_resume(
            &win,
            Some(&parent),
            Path::new("/w"),
            Path::new("/nib"),
            ALL_EXIST,
        )
        .unwrap();
        assert_eq!(plan.program, PathBuf::from("/nib"));
        assert_eq!(
            plan.args,
            vec!["sandbox", "attach", "/repos/app", "--session", "uuid-9"]
        );
        assert_eq!(plan.container.as_deref(), Some("nibble-app"));

        let pi_win = with_session(
            task("w", AgentType::Pi, "main", 2, 0),
            None,
            Some("/home/node/.pi/agent/sessions/x/2026_abc-123.jsonl"),
        );
        let plan = plan_resume(
            &pi_win,
            Some(&parent),
            Path::new("/w"),
            Path::new("/nib"),
            ALL_EXIST,
        )
        .unwrap();
        assert_eq!(
            plan.args,
            vec![
                "sandbox",
                "attach",
                "/repos/app",
                "--pi",
                "--session",
                "abc-123"
            ]
        );

        let omp_win = task("w", AgentType::Omp, "main", 2, 0);
        assert!(plan_resume(
            &omp_win,
            Some(&parent),
            Path::new("/w"),
            Path::new("/n"),
            ALL_EXIST
        )
        .is_err());

        let mut no_repo = parent.clone();
        no_repo.repo_path = None;
        let mut bare = win.clone();
        bare.context.as_mut().unwrap().project_path = None;
        assert!(plan_resume(
            &bare,
            Some(&no_repo),
            Path::new("/w"),
            Path::new("/n"),
            ALL_EXIST
        )
        .is_err());
    }
}
