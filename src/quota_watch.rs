//! Quota auto-continue daemon.
//!
//! Subscription quota errors ("usage limit reached", "You've hit your limit",
//! out-of-credits, …) kill an agent turn but not the task: the quota window
//! resets hours later and the session can simply continue. This daemon tails
//! agent session transcripts (Claude Code JSONL under `~/.claude/projects`,
//! pi/omp JSONL under `~/.pi` / `~/.omp`), detects quota errors, and once the
//! quota window resets, continues the task:
//!
//! - **Sandbox tasks**: a headless resume turn via `podman exec -i`
//!   (`claude --resume`, `omp --resume … -p`, `pi --session … -p`), the same
//!   mechanism the Telegram inject uses. The container is started first if it
//!   is down.
//! - **Host tasks**: if the session lives in a zellij pane (recorded by the
//!   wrappers), the continue message is typed into the live TUI via
//!   `zellij action write-chars --pane-id`. Otherwise a headless resume turn
//!   runs on the host in the task's working directory.
//!
//! Host and sandbox sessions are both visible because the agent config homes
//! (`~/.claude`, `~/.pi`, `~/.omp`) are bind-mounted into every sandbox — a
//! single host-side scan sees everything.
//!
//! Error sources (observed formats):
//! - Claude Code logs `<synthetic>` assistant messages with
//!   `isApiErrorMessage: true`, e.g. `You've hit your limit · resets 5pm (UTC)`.
//! - pi/omp log assistant messages with `stopReason: "error"` and an
//!   `errorMessage` body, e.g. `429 … Usage limit reached for 5 hour. Your
//!   limit will reset at 2026-09-22 00:18:31 … retry-after-ms=6565000`.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use crate::config::QuotaWatchConfig;
use crate::db::Database;
use crate::models::{AgentType, SandboxType, Task};
use crate::sandbox::container_working_dir;
use crate::sandbox::podman::PodmanSandbox;
use crate::sandbox::ContainerStatus;

const PENDING_PREFIX: &str = "qw:pending:";
const OFFSET_PREFIX: &str = "qw:offset:";
/// Only act on error lines this fresh — older ones belong to past episodes
/// (the daemon may have been down, or offsets were reset for a new file).
const FRESH_WINDOW: Duration = Duration::hours(3);
/// When a session file changes identity, start scanning this far from the end
/// so recent errors are still caught without replaying ancient history.
const NEW_FILE_SCAN_WINDOW: u64 = 64 * 1024;
/// Upper bound for one headless continue turn before the daemon gives up on
/// it and re-arms (the turn itself is left running; only the watcher moves on).
const MAX_TURN_SECS: u64 = 3600;

// ── Error classification ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Subscription/quota exhaustion — worth waiting for a reset.
    Quota,
    /// Provider hiccup — the agent retries these itself; ignore.
    Transient,
    /// Anything else (auth, plan-access, malformed) — not renewable by waiting.
    Other,
}

const QUOTA_MARKERS: &[&str] = &[
    "usage limit",
    "usage_limit",
    "usage_limit_reached",
    "quota exceeded",
    "quota will reset",
    "hit your limit",
    "run out of credits",
    "spending-limit",
    "limit will reset",
    "billing cycle",
];

const TRANSIENT_MARKERS: &[&str] = &[
    "overloaded",
    "temporarily",
    "try again later",
    "connection error",
    "timed out",
    "this operation was aborted",
    "terminated",
    "internal server error",
    "service unavailable",
    "bad gateway",
];

/// Classify an agent error message. Quota markers win over transient markers:
/// a "usage limit" body inside a retry-exhausted wrapper is still a quota
/// error, while a bare "temporarily overloaded" 429 is not.
pub fn classify_error(text: &str) -> ErrorClass {
    let t = text.to_lowercase();
    if QUOTA_MARKERS.iter().any(|m| t.contains(m)) {
        return ErrorClass::Quota;
    }
    if TRANSIENT_MARKERS.iter().any(|m| t.contains(m)) {
        return ErrorClass::Transient;
    }
    ErrorClass::Other
}

/// Parse the quota reset time out of an error message.
///
/// Known formats (all observed in real transcripts):
/// - `Your limit will reset at 2026-09-22 00:18:31` (zai, UTC)
/// - `You've hit your limit · resets 5pm (UTC)` (Claude Code; only UTC handled)
/// - `retry-after-ms=6565000` (zai rate-limit header echo)
///
/// `msg_ts` anchors time-of-day formats; `now` anchors relative ones. Returns
/// `None` when the message carries no parseable time (caller falls back to a
/// fixed retry interval) or when the parsed time is already in the past.
pub fn parse_reset(text: &str, msg_ts: DateTime<Utc>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    // Providers mix case ("(UTC)" vs "(utc)"); search on a lowercased copy —
    // timestamps and digits are case-free, so slicing it is safe.
    let text = &text.to_lowercase();
    // 1. Absolute: "reset at YYYY-MM-DD HH:MM:SS"
    if let Some(idx) = text.find("reset at ") {
        let rest = &text[idx + "reset at ".len()..];
        if rest.len() >= 19 {
            if let Ok(naive) = NaiveDateTime::parse_from_str(&rest[..19], "%Y-%m-%d %H:%M:%S") {
                let ts = Utc.from_utc_datetime(&naive);
                if ts > now {
                    return Some(ts);
                }
            }
        }
    }

    // 2. Claude Code: "resets 5pm (UTC)" / "resets 2:50pm (UTC)" — UTC only.
    if let Some(idx) = text.find("resets ") {
        let rest = &text[idx + "resets ".len()..];
        if let Some(cut) = rest.find(" (utc)") {
            let tod = &rest[..cut];
            if let Some(ts) = parse_time_of_day_utc(tod, msg_ts) {
                if ts > now {
                    return Some(ts);
                }
            }
        }
    }

    // 3. Relative: "retry-after-ms=6565000"
    if let Some(idx) = text.find("retry-after-ms=") {
        let rest = &text[idx + "retry-after-ms=".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(ms) = digits.parse::<i64>() {
            if ms > 0 {
                return Some(now + Duration::milliseconds(ms));
            }
        }
    }

    None
}

/// Parse `5pm`, `2:50pm`, `12:30am` as a UTC time-of-day on `msg_ts`'s date.
/// Rolls to the next day when the result is not strictly after `msg_ts`
/// (Claude emits the message just before the reset, so a same-day time in the
/// past means the reset crosses midnight).
fn parse_time_of_day_utc(tod: &str, msg_ts: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let tod = tod.trim().to_lowercase();
    let (clock, meridiem) = if let Some(stripped) = tod.strip_suffix("pm") {
        (stripped, 12u32)
    } else if let Some(stripped) = tod.strip_suffix("am") {
        (stripped, 0u32)
    } else {
        return None;
    };
    let (h, m) = if let Some((hs, ms)) = clock.split_once(':') {
        (
            hs.trim().parse::<u32>().ok()?,
            ms.trim().parse::<u32>().ok()?,
        )
    } else {
        (clock.trim().parse::<u32>().ok()?, 0)
    };
    // 12am → 0h, 12pm → 12h, otherwise pm adds 12.
    let hour = match (meridiem, h) {
        (12, 12) => 12,
        (12, _) => h + 12,
        (0, 12) => 0,
        (0, _) => h,
        _ => h,
    };
    if h > 12 || m > 59 || hour > 23 {
        return None;
    }
    let date = msg_ts.date_naive();
    let naive = date.and_hms_opt(hour, m, 0)?;
    let mut ts = Utc.from_utc_datetime(&naive);
    if ts <= msg_ts {
        ts += Duration::days(1);
    }
    Some(ts)
}

// ── Session transcript error extraction ───────────────────────────────────────

struct SessionError {
    ts: Option<DateTime<Utc>>,
    text: String,
}

/// Extract a failed-turn error record from one JSONL line, if it is one.
fn extract_session_error(line: &str) -> Option<SessionError> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;

    // pi / omp: {"type":"message","message":{"role":"assistant",
    //   "stopReason":"error","errorMessage":"…","timestamp":1789730311939}}
    if let Some(m) = v.get("message").and_then(|m| m.as_object()) {
        if m.get("stopReason").and_then(|s| s.as_str()) == Some("error") {
            if let Some(e) = m.get("errorMessage").and_then(|e| e.as_str()) {
                let ts = m
                    .get("timestamp")
                    .and_then(|t| t.as_i64())
                    .and_then(|ms| Utc.timestamp_millis_opt(ms).single());
                return Some(SessionError {
                    ts,
                    text: e.to_string(),
                });
            }
        }
    }

    // Claude Code: {"type":"assistant","isApiErrorMessage":true,
    //   "timestamp":"…Z","message":{"model":"<synthetic>","content":[…]}}
    if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
        let is_err = v
            .get("isApiErrorMessage")
            .and_then(|b| b.as_bool())
            .or_else(|| {
                v.get("message")
                    .and_then(|m| m.get("isApiErrorMessage"))
                    .and_then(|b| b.as_bool())
            });
        if is_err == Some(true) {
            let text = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let ts = v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc));
            if !text.is_empty() {
                return Some(SessionError { ts, text });
            }
        }
    }

    None
}

// ── Session file resolution ───────────────────────────────────────────────────

/// Resolve the host-side transcript file for a task, if one is linked and
/// exists. pi/omp archive cold sessions as `.jsonl.gz` — those are not live
/// and are skipped.
fn session_file_for(task: &Task) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let ctx = task.context.as_ref()?;
    match &task.agent_type {
        AgentType::ClaudeCode => {
            // Legacy rows may only carry the generic session_id.
            let sid = ctx
                .claude_session_id
                .as_deref()
                .or(ctx.session_id.as_deref())?;
            if sid.starts_with("ses_") {
                return None; // legacy non-claude id
            }
            claude_session_file(sid, &home)
        }
        AgentType::Pi | AgentType::Omp => {
            let stored = ctx.extra.get("pi_session_path")?.as_str()?;
            pi_session_file(stored, &home)
        }
        _ => None,
    }
}

/// Find `~/.claude/projects/<any-slug>/<sid>.jsonl`. Session UUIDs are unique,
/// so a single walk over the project dirs is unambiguous.
fn claude_session_file(sid: &str, home: &Path) -> Option<PathBuf> {
    let projects = home.join(".claude").join("projects");
    let wanted = format!("{sid}.jsonl");
    for entry in std::fs::read_dir(&projects).ok()? {
        let dir = entry.ok()?.path();
        if !dir.is_dir() {
            continue;
        }
        let candidate = dir.join(&wanted);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Map a stored pi/omp session path to the host-side file. Sandbox sessions
/// store the container path (`/home/node/...`); the same directory is
/// bind-mounted from the host home, so rewriting the prefix recovers it.
fn pi_session_file(stored: &str, home: &Path) -> Option<PathBuf> {
    let home_str = home.to_string_lossy();
    let candidates: Vec<PathBuf> = if let Some(rest) = stored.strip_prefix("/home/node/") {
        vec![PathBuf::from(&*home_str).join(rest), PathBuf::from(stored)]
    } else if let Some(rest) = stored.strip_prefix("~/") {
        vec![PathBuf::from(&*home_str).join(rest), PathBuf::from(stored)]
    } else {
        vec![PathBuf::from(stored)]
    };
    candidates
        .into_iter()
        .find(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) != Some("gz"))
}

// ── Pending state ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    reset_at: DateTime<Utc>,
    attempts: u32,
    error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Offset {
    path: String,
    pos: u64,
}

fn pending_key(task_id: &str) -> String {
    format!("{PENDING_PREFIX}{task_id}")
}

fn offset_key(task_id: &str) -> String {
    format!("{OFFSET_PREFIX}{task_id}")
}

struct AttemptDone {
    task_id: String,
    pending: Pending,
    outcome: Result<&'static str>,
    /// Session file length before the turn started; the parent rescans from
    /// here to decide between re-arm and success.
    before: Option<u64>,
}

// ── Watcher ───────────────────────────────────────────────────────────────────

pub struct Watcher {
    cfg: QuotaWatchConfig,
    db_path: PathBuf,
    pendings: HashMap<String, Pending>,
    /// Tasks with a continue turn currently running.
    busy: HashSet<String>,
    done_rx: mpsc::Receiver<AttemptDone>,
    done_tx: mpsc::Sender<AttemptDone>,
}

impl Watcher {
    pub fn new(cfg: QuotaWatchConfig, db_path: PathBuf) -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        let mut w = Watcher {
            cfg,
            db_path,
            pendings: HashMap::new(),
            busy: HashSet::new(),
            done_rx,
            done_tx,
        };
        w.hydrate();
        w
    }

    /// Recover pending state from the kv store after a daemon restart.
    fn hydrate(&mut self) {
        let Ok(db) = Database::open(&self.db_path) else {
            return;
        };
        let Ok(keys) = db.kv_keys_with_prefix(PENDING_PREFIX) else {
            return;
        };
        for key in keys {
            let Some(val) = db.kv_get(&key).ok().flatten() else {
                continue;
            };
            let Ok(p) = serde_json::from_str::<Pending>(&val) else {
                continue;
            };
            let task_id = key.strip_prefix(PENDING_PREFIX).unwrap_or("").to_string();
            if !task_id.is_empty() {
                self.pendings.insert(task_id, p);
            }
        }
    }

    /// One scan-and-act pass: collect finished attempts, tail session files
    /// for new quota errors, then dispatch due continue attempts.
    /// `inline` runs attempts synchronously (`--once`); otherwise each turn
    /// runs in a thread so one long turn cannot starve the others.
    pub fn pass(&mut self, inline: bool) {
        self.collect_finished();
        if let Err(e) = self.scan_pass() {
            eprintln!("[quota-watch] scan error: {e:#}");
        }
        if let Err(e) = self.attempt_due(inline) {
            eprintln!("[quota-watch] attempt error: {e:#}");
        }
    }

    /// Apply outcomes reported by worker threads.
    fn collect_finished(&mut self) {
        while let Ok(done) = self.done_rx.try_recv() {
            self.handle_outcome(done);
        }
    }

    fn scan_pass(&mut self) -> Result<()> {
        let db = Database::open(&self.db_path)?;
        let now = Utc::now();
        let cutoff = now - Duration::days(7);
        let tasks: Vec<Task> = db
            .list_tasks()?
            .into_iter()
            .filter(|t| t.updated_at > cutoff && !self.busy.contains(&t.task_id))
            .filter(|t| session_file_for(t).is_some())
            .collect();

        for task in tasks {
            let file = session_file_for(&task).expect("filtered above");
            if let Err(e) = self.scan_task_file(&db, &task, &file, now) {
                eprintln!("[quota-watch] {} scan: {e:#}", short(&task.task_id));
            }
        }
        Ok(())
    }

    /// Tail one task's session file from the stored offset, arming pending
    /// state for fresh quota errors.
    fn scan_task_file(
        &mut self,
        db: &Database,
        task: &Task,
        file: &Path,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let tid = &task.task_id;
        let len = std::fs::metadata(file)?.len();

        let offset: Option<Offset> = db
            .kv_get(&offset_key(tid))?
            .and_then(|v| serde_json::from_str(&v).ok());
        let pos = match offset {
            Some(o) if o.path == file.to_string_lossy() && o.pos <= len => o.pos,
            _ => len.saturating_sub(NEW_FILE_SCAN_WINDOW), // new/rotated file
        };

        let mut f = std::fs::File::open(file)?;
        f.seek(SeekFrom::Start(pos))?;
        let mut buf = String::new();
        f.read_to_string(&mut buf)?;

        for line in buf.lines() {
            let Some(err) = extract_session_error(line) else {
                continue;
            };
            if classify_error(&err.text) != ErrorClass::Quota {
                continue;
            }
            // Fresh errors only: stale lines (rotated file, long daemon
            // outage) belong to episodes already handled or irrelevant.
            if let Some(ts) = err.ts {
                if ts < now - FRESH_WINDOW {
                    continue;
                }
            }

            let reset_at = parse_reset(&err.text, err.ts.unwrap_or(now), now)
                .unwrap_or(now + Duration::seconds(self.cfg.unknown_retry_secs as i64))
                .max(now);
            let pending = match self.pendings.get(tid) {
                Some(p) => {
                    let mut p = p.clone();
                    p.error = err.text.clone();
                    p.reset_at = reset_at;
                    p
                }
                None => Pending {
                    reset_at,
                    attempts: 0,
                    error: err.text.clone(),
                },
            };
            self.put_pending(
                db,
                tid,
                &pending,
                "blocked",
                &blocked_reason(pending.reset_at),
            )?;
            eprintln!(
                "[quota-watch] {} blocked by quota, reset at {}",
                short(tid),
                pending
                    .reset_at
                    .with_timezone(&chrono::Local)
                    .format("%a %H:%M")
            );
        }

        db.kv_set(
            &offset_key(tid),
            &serde_json::to_string(&Offset {
                path: file.to_string_lossy().into_owned(),
                pos: len,
            })?,
        )?;
        Ok(())
    }

    /// Dispatch continue attempts for pendings whose reset time (plus buffer)
    /// has passed.
    fn attempt_due(&mut self, inline: bool) -> Result<()> {
        let now = Utc::now();
        let due: Vec<(String, Pending)> = self
            .pendings
            .iter()
            .filter(|(tid, p)| {
                !self.busy.contains(*tid)
                    && now >= p.reset_at + Duration::seconds(self.cfg.reset_buffer_secs as i64)
            })
            .map(|(tid, p)| (tid.clone(), p.clone()))
            .collect();

        let db = Database::open(&self.db_path)?;
        for (tid, pending) in due {
            if pending.attempts >= self.cfg.max_attempts {
                let reason = format!(
                    "quota: gave up after {} auto-continue attempts",
                    pending.attempts
                );
                self.remove_pending(&db, &tid);
                let _ = crate::status::apply_report(&db, &tid, "blocked", Some(&reason));
                eprintln!("[quota-watch] {} {reason}", short(&tid));
                continue;
            }

            let Some(task) = db.get_task_by_id(&tid)? else {
                self.remove_pending(&db, &tid);
                continue;
            };

            // Bump and persist the attempt count BEFORE dispatching so a
            // daemon crash mid-turn cannot retry unboundedly.
            let mut bumped = pending;
            bumped.attempts += 1;
            db.kv_set(&pending_key(&tid), &serde_json::to_string(&bumped)?)?;
            self.pendings.insert(tid.clone(), bumped.clone());
            self.busy.insert(tid.clone());

            let before = session_file_for(&task)
                .and_then(|f| std::fs::metadata(f).ok())
                .map(|m| m.len());

            if inline {
                let outcome = continue_task(&task, &self.cfg);
                self.handle_outcome(AttemptDone {
                    task_id: tid,
                    pending: bumped,
                    outcome,
                    before,
                });
            } else {
                let cfg = self.cfg.clone();
                let tx = self.done_tx.clone();
                let task = task.clone();
                std::thread::spawn(move || {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        continue_task(&task, &cfg)
                    }))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("continue thread panicked")));
                    let _ = tx.send(AttemptDone {
                        task_id: task.task_id.clone(),
                        pending: bumped,
                        outcome,
                        before,
                    });
                });
            }
        }
        Ok(())
    }

    /// Update state from one finished attempt: re-arm on a fresh quota error
    /// or failure, clear on success.
    fn handle_outcome(&mut self, done: AttemptDone) {
        let AttemptDone {
            task_id,
            pending,
            outcome,
            before,
        } = done;
        self.busy.remove(&task_id);

        let db = match Database::open(&self.db_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "[quota-watch] {} post-attempt db open failed: {e:#}",
                    short(&task_id)
                );
                return;
            }
        };

        match &outcome {
            Ok(via) => eprintln!(
                "[quota-watch] {} continue attempt #{} via {via}",
                short(&task_id),
                pending.attempts
            ),
            Err(e) => eprintln!(
                "[quota-watch] {} continue attempt #{} failed: {e:#}",
                short(&task_id),
                pending.attempts
            ),
        }

        // Headless turns are finished by now — inspect what the turn wrote.
        // zellij keystrokes deliver to a live TUI whose response lands later;
        // a still-active quota surfaces again through the normal scan.
        let rearmed = match &outcome {
            Ok("zellij") => None,
            Ok(_) => {
                let file = db
                    .get_task_by_id(&task_id)
                    .ok()
                    .flatten()
                    .and_then(|t| session_file_for(&t));
                scan_new_quota_error(
                    file.as_deref(),
                    before,
                    Utc::now(),
                    self.cfg.unknown_retry_secs,
                )
            }
            // A failed attempt (container down, spawn error, turn timeout) is
            // worth retrying later — re-arm on the fallback interval. The
            // attempts budget bounds this.
            Err(_) => Some((
                Utc::now() + Duration::seconds(self.cfg.unknown_retry_secs as i64),
                pending.error.clone(),
            )),
        };

        match rearmed {
            Some((reset_at, error)) => {
                let mut p = pending;
                p.reset_at = reset_at.max(Utc::now());
                p.error = error;
                if let Err(e) =
                    self.put_pending(&db, &task_id, &p, "blocked", &blocked_reason(p.reset_at))
                {
                    eprintln!("[quota-watch] {} re-arm failed: {e:#}", short(&task_id));
                }
            }
            None => {
                self.remove_pending(&db, &task_id);
                // Unblock; the agent's own hooks take over status tracking.
                let _ = crate::status::apply_report(&db, &task_id, "running", None);
            }
        }
    }

    fn put_pending(
        &mut self,
        db: &Database,
        tid: &str,
        pending: &Pending,
        state: &str,
        reason: &str,
    ) -> Result<()> {
        db.kv_set(&pending_key(tid), &serde_json::to_string(pending)?)?;
        self.pendings.insert(tid.to_string(), pending.clone());
        crate::status::apply_report(db, tid, state, Some(reason))
            .context("updating task status")?;
        Ok(())
    }

    fn remove_pending(&mut self, db: &Database, tid: &str) {
        let _ = db.kv_delete(&pending_key(tid));
        self.pendings.remove(tid);
    }
}

fn blocked_reason(reset_at: DateTime<Utc>) -> String {
    format!(
        "quota limit reached — auto-continue scheduled for {}",
        reset_at.with_timezone(&chrono::Local).format("%a %H:%M")
    )
}

// ── Continuation ───────────────────────────────────────────────────────────────

/// Perform the continuation for a task. Returns a short label for the
/// mechanism used ("zellij", "podman claude", "host omp", …).
fn continue_task(task: &Task, cfg: &QuotaWatchConfig) -> Result<&'static str> {
    let msg = cfg.continue_message.as_str();
    if task.container_id.is_some() && task.sandbox_type == SandboxType::Podman {
        continue_sandbox(task, msg)
    } else {
        continue_host(task, msg)
    }
}

fn continue_sandbox(task: &Task, msg: &str) -> Result<&'static str> {
    let container_id = task.container_id.as_deref().context("no container id")?;
    let sandbox = PodmanSandbox::new();

    // Bring the container back if it is down (the agent TUI may have exited
    // long ago; PID 1 is `sleep infinity` and survives).
    if sandbox.status(container_id)? != ContainerStatus::Running {
        sandbox.start(container_id)?;
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    let cwd = task
        .context
        .as_ref()
        .and_then(|c| c.project_path.as_deref())
        .map(Path::new)
        .map(container_working_dir)
        .unwrap_or_else(|| "/workspace".to_string());

    match &task.agent_type {
        AgentType::ClaudeCode => {
            // Same mechanism as the Telegram inject: stdin + EOF drives
            // exactly one turn, then the process exits.
            let mut child = crate::agent_input::inject_returning_child(task, msg)?;
            wait_with_timeout(&mut child)?;
            Ok("podman claude")
        }
        AgentType::Omp => {
            let session_ref = omp_resume_ref(task)?;
            let mut cmd = podman_exec_base(task, &cwd, "omp");
            cmd.args(["--resume", &session_ref, "--auto-approve", "-p", msg]);
            run_headless(cmd)?;
            Ok("podman omp")
        }
        AgentType::Pi => {
            let session_ref = pi_session_ref(task)?;
            let mut cmd = podman_exec_base(task, &cwd, "pi");
            cmd.args(["--session", &session_ref, "-p", msg]);
            run_headless(cmd)?;
            Ok("podman pi")
        }
        other => anyhow::bail!("unsupported sandbox agent type: {other:?}"),
    }
}

fn continue_host(task: &Task, msg: &str) -> Result<&'static str> {
    // Preferred: type the message into the live TUI pane.
    if let Some(via) = zellij_send(task, msg)? {
        return Ok(via);
    }
    // Fallback: headless resume turn in the task's working directory.
    let cwd = task
        .context
        .as_ref()
        .and_then(|c| c.project_path.as_deref())
        .context("task has no working directory")?;

    let mut cmd = Command::new(host_agent_bin(&task.agent_type));
    cmd.env("AGENT_TASK_ID", &task.task_id)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let uses_stdin;
    match &task.agent_type {
        AgentType::ClaudeCode => {
            let sid = task
                .context
                .as_ref()
                .and_then(|c| c.claude_session_id.as_deref())
                .context("no claude_session_id")?;
            cmd.args(["--resume", sid, "--dangerously-skip-permissions"]);
            uses_stdin = true;
        }
        AgentType::Omp => {
            let path = pi_session_ref(task)?;
            cmd.args(["--resume", &path, "--auto-approve", "-p", msg]);
            uses_stdin = false;
        }
        AgentType::Pi => {
            let path = pi_session_ref(task)?;
            cmd.args(["--session", &path, "-p", msg]);
            uses_stdin = false;
        }
        other => anyhow::bail!("unsupported host agent type: {other:?}"),
    }

    cmd.stdin(if uses_stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = cmd.spawn().context("spawning host resume turn")?;
    if uses_stdin {
        // Claude without -p reads the message from stdin, processes one turn,
        // and exits on EOF (same contract as the sandbox inject).
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(msg.as_bytes());
            let _ = stdin.write_all(b"\n");
        }
    }
    wait_with_timeout(&mut child)?;
    Ok("host headless")
}

/// Type `msg` + Enter into the task's zellij pane, if it is still alive.
/// Returns `Ok(Some(label))` when the keystrokes were delivered.
fn zellij_send(task: &Task, msg: &str) -> Result<Option<&'static str>> {
    let ctx = task.context.as_ref();
    let pane_id = ctx
        .and_then(|c| c.extra.get("zellij_pane_id"))
        .and_then(|v| v.as_u64());
    let session = ctx
        .and_then(|c| c.extra.get("zellij_session_name"))
        .and_then(|v| v.as_str());
    let (Some(pane_id), Some(session)) = (pane_id, session) else {
        return Ok(None);
    };

    // Is the pane still there?
    let out = Command::new("zellij")
        .args([
            "--session",
            session,
            "action",
            "list-panes",
            "--all",
            "--json",
        ])
        .output()
        .context("running zellij list-panes")?;
    if !out.status.success() {
        return Ok(None); // session gone
    }
    let panes: Vec<serde_json::Value> =
        serde_json::from_slice(&out.stdout).context("parsing zellij pane list")?;
    let alive = panes.iter().any(|p| {
        p.get("id").and_then(|v| v.as_u64()) == Some(pane_id)
            && !p
                .get("is_plugin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    });
    if !alive {
        return Ok(None);
    }

    let pane = pane_id.to_string();
    let chars = Command::new("zellij")
        .args([
            "--session",
            session,
            "action",
            "write-chars",
            msg,
            "--pane-id",
            &pane,
        ])
        .status()
        .context("zellij write-chars")?;
    if !chars.success() {
        anyhow::bail!("zellij write-chars failed for pane {pane}");
    }
    // CR submits the TUI input line.
    let enter = Command::new("zellij")
        .args([
            "--session",
            session,
            "action",
            "write",
            "13",
            "--pane-id",
            &pane,
        ])
        .status()
        .context("zellij write")?;
    if !enter.success() {
        anyhow::bail!("zellij write failed for pane {pane}");
    }
    Ok(Some("zellij"))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn short(tid: &str) -> String {
    tid.chars().take(8).collect()
}

/// The stored pi/omp session path (container path for sandbox tasks).
fn pi_session_ref(task: &Task) -> Result<String> {
    task.context
        .as_ref()
        .and_then(|c| c.extra.get("pi_session_path"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .context("no pi_session_path")
}

/// omp accepts a session path or ID; archived (`.jsonl.gz`) sessions must be
/// resumed by ID.
fn omp_resume_ref(task: &Task) -> Result<String> {
    let stored = pi_session_ref(task)?;
    if stored.ends_with(".jsonl.gz") {
        Ok(crate::session::session_id_from_filename(Path::new(&stored)))
    } else {
        Ok(stored)
    }
}

fn podman_exec_base(task: &Task, cwd: &str, bin: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args([
        "exec",
        "-i",
        "-e",
        "TERM=xterm-256color",
        "-e",
        "PATH=/home/node/.local/bin:/usr/local/bin:/usr/bin:/bin",
        "-e",
        &format!("AGENT_TASK_ID={}", task.task_id),
        "-w",
        cwd,
    ]);
    if let Some(cid) = task.container_id.as_deref() {
        cmd.arg(cid);
    }
    cmd.arg(bin);
    cmd
}

fn run_headless(mut cmd: Command) -> Result<()> {
    cmd.stdin(Stdio::null());
    let mut child = cmd.spawn().context("spawning headless turn")?;
    wait_with_timeout(&mut child)
}

/// Wait for a child, capped at [`MAX_TURN_SECS`]. On timeout the child is
/// killed and an error is returned (the watcher re-arms; the turn's partial
/// transcript stays on disk).
fn wait_with_timeout(child: &mut std::process::Child) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(MAX_TURN_SECS);
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => anyhow::bail!("agent exited with {status}"),
            None => {}
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            anyhow::bail!("turn exceeded {MAX_TURN_SECS}s");
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

/// Prefer the standard install location, then fall back to PATH.
fn host_agent_bin(agent: &AgentType) -> String {
    let name = match agent {
        AgentType::ClaudeCode => "claude",
        AgentType::Pi => "pi",
        AgentType::Omp => "omp",
        _ => "claude",
    };
    if let Some(home) = dirs::home_dir() {
        let candidate = home.join(".local").join("bin").join(name);
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    name.to_string()
}

/// Inspect bytes appended to a session file since `before`; return a new
/// reset time when the latest quota error indicates the limit is still in
/// force.
fn scan_new_quota_error(
    file: Option<&Path>,
    before: Option<u64>,
    now: DateTime<Utc>,
    unknown_retry_secs: u64,
) -> Option<(DateTime<Utc>, String)> {
    let file = file?;
    let len = std::fs::metadata(file).ok()?.len();
    let start = before?;
    if len <= start {
        return None;
    }
    let mut f = std::fs::File::open(file).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = String::new();
    f.read_to_string(&mut buf).ok()?;

    let mut latest: Option<(DateTime<Utc>, String)> = None;
    for line in buf.lines() {
        let Some(err) = extract_session_error(line) else {
            continue;
        };
        if classify_error(&err.text) != ErrorClass::Quota {
            continue;
        }
        let reset = parse_reset(&err.text, err.ts.unwrap_or(now), now)
            .unwrap_or(now + Duration::seconds(unknown_retry_secs as i64))
            .max(now);
        latest = Some((reset, err.text));
    }
    latest
}

// ── Daemon entry points ───────────────────────────────────────────────────────

/// Run the quota-watch daemon forever (`nibble quota-watch`).
pub fn run(cfg: QuotaWatchConfig, db_path: PathBuf) -> Result<()> {
    if !cfg.enabled {
        eprintln!("[quota-watch] disabled in config ([quota_watch] enabled = false); exiting");
        return Ok(());
    }
    eprintln!(
        "[quota-watch] starting: poll every {}s, message {:?}, max {} attempts",
        cfg.poll_secs, cfg.continue_message, cfg.max_attempts
    );
    let mut watcher = Watcher::new(cfg, db_path);
    loop {
        watcher.pass(false);
        let poll = watcher.cfg.poll_secs.max(10);
        std::thread::sleep(std::time::Duration::from_secs(poll));
    }
}

/// Single scan + inline attempt pass (`nibble quota-watch --once`).
pub fn run_once(cfg: QuotaWatchConfig, db_path: PathBuf) -> Result<()> {
    let mut watcher = Watcher::new(cfg, db_path);
    watcher.pass(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 25, 12, 0, 0).unwrap()
    }

    #[test]
    fn classifies_real_quota_errors() {
        let cases = [
            (
                "429 {\"error\":{\"type\":\"rate_limit_error\",\"code\":\"1308\",\"message\":\"[1308][Usage limit reached for 5 hour. Your limit will reset at 2026-09-22 00:18:31][x]\"}} retry-after-ms=6565000",
                ErrorClass::Quota,
            ),
            (
                "429: {\"code\":\"1308\",\"message\":\"Usage limit reached for 5 hour. Your limit will reset at 2026-08-04 20:35:25\"}",
                ErrorClass::Quota,
            ),
            (
                "403 {\"error\":{\"type\":\"permission_error\",\"message\":\"You've reached your 5-hour usage limit. Your quota will reset when the current 5-hour window ends.\"}}",
                ErrorClass::Quota,
            ),
            (
                "403 You have run out of credits or need a Grok subscription. (type=personal-team-blocked:spending-limit)",
                ErrorClass::Quota,
            ),
            ("429 quota exceeded\n", ErrorClass::Quota),
            ("You've hit your limit · resets 5pm (UTC)", ErrorClass::Quota),
            (
                "403 {\"error\":{\"message\":\"You've reached your usage limit for this billing cycle.\"}}",
                ErrorClass::Quota,
            ),
            (
                "Retry budget exhausted after 10 retries: 429 {\"code\":\"1308\",\"message\":\"Usage limit reached for 5 hour.\"}",
                ErrorClass::Quota,
            ),
        ];
        for (text, want) in cases {
            assert_eq!(classify_error(text), want, "text: {text}");
        }
    }

    #[test]
    fn classifies_transient_and_other_errors() {
        // Overloaded 429s are transient — the agent retries them itself.
        assert_eq!(
            classify_error("429 The service may be temporarily overloaded, please try again later"),
            ErrorClass::Transient
        );
        assert_eq!(
            classify_error("429 {\"error\":{\"message\":\"The engine is currently overloaded, please try again later\"}}"),
            ErrorClass::Transient
        );
        assert_eq!(classify_error("Connection error."), ErrorClass::Transient);
        assert_eq!(classify_error("Request timed out."), ErrorClass::Transient);
        // Auth / plan-access problems are not renewable by waiting.
        assert_eq!(
            classify_error("401 Authentication Failed"),
            ErrorClass::Other
        );
        assert_eq!(
            classify_error(
                "429 {\"error\":{\"message\":\"[1311][Your current subscription plan does not yet include access to GLM-5.3-FlashX]\"}}"
            ),
            ErrorClass::Other
        );
    }

    #[test]
    fn parses_absolute_reset_time() {
        let t = now();
        let text = "Usage limit reached for 5 hour. Your limit will reset at 2026-09-25 18:18:31";
        let reset = parse_reset(text, t, t).unwrap();
        assert_eq!(
            reset,
            Utc.with_ymd_and_hms(2026, 9, 25, 18, 18, 31).unwrap()
        );
    }

    #[test]
    fn parses_claude_time_of_day_utc() {
        let t = now(); // 12:00 UTC
        let reset = parse_reset("You've hit your limit · resets 5pm (UTC)", t, t).unwrap();
        assert_eq!(reset, Utc.with_ymd_and_hms(2026, 9, 25, 17, 0, 0).unwrap());
        let reset = parse_reset("resets 2:50pm (UTC)", t, t).unwrap();
        assert_eq!(reset, Utc.with_ymd_and_hms(2026, 9, 25, 14, 50, 0).unwrap());
        // Time already passed today → tomorrow.
        let reset = parse_reset("resets 5am (UTC)", t, t).unwrap();
        assert_eq!(reset, Utc.with_ymd_and_hms(2026, 9, 26, 5, 0, 0).unwrap());
        // 12pm noon and 12am midnight edge cases.
        let reset = parse_reset("resets 12pm (UTC)", t, t).unwrap();
        // Noon == msg_ts rolls to the next occurrence (reset is strictly future).
        assert_eq!(reset, Utc.with_ymd_and_hms(2026, 9, 26, 12, 0, 0).unwrap());
        let reset = parse_reset("resets 12:30am (UTC)", t, t).unwrap();
        assert_eq!(reset, Utc.with_ymd_and_hms(2026, 9, 26, 0, 30, 0).unwrap());
    }

    #[test]
    fn parses_retry_after_ms() {
        let t = now();
        let reset = parse_reset("… retry-after-ms=6565000", t, t).unwrap();
        assert_eq!(reset, t + Duration::milliseconds(6_565_000));
    }

    #[test]
    fn no_reset_for_unknown_or_stale_formats() {
        let t = now();
        assert!(
            parse_reset("You've reached your 5-hour usage limit.", t, t).is_none(),
            "no timestamp → unknown"
        );
        assert!(
            parse_reset("reset at 2026-09-25 10:00:00", t, t).is_none(),
            "stale absolute time ignored"
        );
        // Non-UTC timezones are not guessed.
        assert!(parse_reset("resets 5pm (Europe/Berlin)", t, t).is_none());
    }

    #[test]
    fn extracts_pi_family_error_line() {
        // Real omp transcripts carry an epoch-millis timestamp inside message.
        let line = r#"{"type":"message","id":"74f72a88","parentId":"e7bfbd70","timestamp":"2026-09-18T11:18:31.939Z","message":{"role":"assistant","content":[],"provider":"kimi-coding","model":"k3","stopReason":"error","errorMessage":"429 quota exceeded\n","timestamp":1789730311939}}"#;
        let err = extract_session_error(line).unwrap();
        assert_eq!(err.text, "429 quota exceeded\n");
        assert_eq!(
            err.ts,
            Some(Utc.timestamp_millis_opt(1789730311939).single().unwrap())
        );
    }

    #[test]
    fn extracts_claude_error_line() {
        let line = r#"{"type":"assistant","isApiErrorMessage":true,"timestamp":"2026-04-14T12:54:44.922Z","message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"You've hit your limit · resets 5pm (UTC)"}]}}"#;
        let err = extract_session_error(line).unwrap();
        assert!(err.text.contains("hit your limit"));
        assert!(err.ts.is_some());
    }

    #[test]
    fn ignores_normal_lines() {
        let line = r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop"}}"#;
        assert!(extract_session_error(line).is_none());
        let line = r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6","content":[{"type":"text","text":"ok"}]}}"#;
        assert!(extract_session_error(line).is_none());
    }
    #[test]
    fn resolves_container_pi_path_to_host() {
        let dir = std::env::temp_dir().join("qw-test-pathmap");
        let host_file =
            dir.join(".omp/agent/sessions/--repo--/2026-09-18T11-09-41-646Z_019x.jsonl");
        std::fs::create_dir_all(host_file.parent().unwrap()).unwrap();
        std::fs::write(&host_file, b"{}").unwrap();
        let p = pi_session_file(
            "/home/node/.omp/agent/sessions/--repo--/2026-09-18T11-09-41-646Z_019x.jsonl",
            &dir,
        );
        assert_eq!(p, Some(host_file));
    }

    #[test]
    fn skips_archived_pi_sessions() {
        let dir = std::env::temp_dir().join("qw-test-gz");
        let _ = std::fs::create_dir_all(&dir);
        let gz = dir.join("s.jsonl.gz");
        std::fs::write(&gz, b"x").unwrap();
        assert!(pi_session_file(gz.to_str().unwrap(), Path::new("/home/x")).is_none());
    }
}
