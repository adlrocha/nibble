//! One run of a job: a fresh headless Claude Code session in the repo's
//! sandbox that ends with a structured report.
//!
//! Each run is its own session, so it never types into, or resumes, the
//! conversation you work in. It shows in the sidebar as its own row while it
//! runs, and its session is kept like any other (`nibble cron show` prints
//! the id, to resume it).

use super::store::{CronStore, RunEnd, RunRecord, RunStatus};
use super::JobDef;
use crate::db::Database;
use crate::models::Task;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How bad a report or a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Ok,
    Warn,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Critical => "critical",
        }
    }
}

/// One problem a run found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// The same underlying problem gets the same fingerprint on every run,
    /// so it is reported once rather than every time.
    pub fingerprint: String,
    pub severity: Severity,
    pub title: String,
    /// Evidence: log lines, versions, advisory ids.
    #[serde(default)]
    pub details: String,
    #[serde(default)]
    pub suggested_action: String,
}

/// What a run reports, as the JSON schema below asks for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub status: Severity,
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
}

/// The schema Claude Code is told to fill (`--json-schema`).
pub const REPORT_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["status", "summary", "findings"],
  "properties": {
    "status": {"type": "string", "enum": ["ok", "warn", "critical"]},
    "summary": {"type": "string", "description": "One or two sentences on what was checked and what was found."},
    "findings": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["fingerprint", "severity", "title", "details", "suggested_action"],
        "properties": {
          "fingerprint": {"type": "string", "description": "Stable id of the underlying problem: identical across runs while the problem persists, e.g. 'error:worker:payment-timeout' or 'vuln:GHSA-xxxx:lodash'. No timestamps, counts or request ids."},
          "severity": {"type": "string", "enum": ["warn", "critical"]},
          "title": {"type": "string"},
          "details": {"type": "string", "description": "Evidence in markdown: log excerpts, affected versions, advisory links."},
          "suggested_action": {"type": "string"}
        }
      }
    }
  }
}"#;

/// Appended to every job's prompt.
const RUN_NOTES: &str = "\
---
This is an unattended, scheduled run (nibble cron). Nobody is watching it, so \
do not ask questions: decide, and say what you assumed. Do not commit, push, \
open pull requests or change files in the repository; investigate and report. \
A person reads your report and decides what to do. Report only problems \
within this job's scope; problems with your own tools (an MCP server that \
fails to connect, a missing CLI) are not findings unless they stop the job, \
in which case say so in the summary.

End with the structured report. status is ok when nothing needs attention, \
warn when something should be looked at, critical when something is broken \
or exploitable now. List each distinct problem once as a finding, with a \
fingerprint that stays the same on later runs while the problem lasts.";

/// Most findings kept from one report.
const MAX_FINDINGS: usize = 20;
/// Longest a report text field may be, in characters.
const MAX_FIELD: usize = 4000;
/// Exit status of `timeout` when it stopped the command.
const TIMEOUT_EXIT: i32 = 124;
/// Exit status when `timeout` had to SIGKILL it.
const KILLED_EXIT: i32 = 137;
/// How long past the job's timeout the host waits for `podman exec` before
/// killing it (the in-container `timeout -k 30` should end it first).
const HOST_GRACE: Duration = Duration::from_secs(90);

/// The prompt a run is given.
pub fn prompt_for(def: &JobDef) -> String {
    format!("{}\n\n{RUN_NOTES}", def.prompt)
}

fn clip(s: &str) -> String {
    // No control characters but line breaks and tabs: report text is
    // printed to terminals, and an escape sequence from a log excerpt must
    // not drive the reader's terminal.
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    let s = cleaned.trim();
    match s.char_indices().nth(MAX_FIELD) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Make a report safe to store and post: bounded sizes, a fingerprint on
/// every finding (one per fingerprint), and a status no lower than its
/// worst finding.
pub fn normalize(mut r: Report) -> Report {
    r.summary = clip(&r.summary);
    let mut seen = std::collections::HashSet::new();
    r.findings = r
        .findings
        .into_iter()
        .map(|mut f| {
            f.title = clip(&f.title);
            f.details = clip(&f.details);
            f.suggested_action = clip(&f.suggested_action);
            f.fingerprint = clip(&f.fingerprint);
            if f.fingerprint.is_empty() {
                f.fingerprint = format!("title:{}", f.title.to_lowercase());
            }
            if f.severity == Severity::Ok {
                f.severity = Severity::Warn;
            }
            f
        })
        .filter(|f| seen.insert(f.fingerprint.clone()))
        .take(MAX_FINDINGS)
        .collect();
    let worst = r.findings.iter().map(|f| f.severity).max();
    r.status = r.status.max(worst.unwrap_or(Severity::Ok));
    r
}

impl Parsed {
    /// A run that ended without a report.
    fn failed(
        status: RunStatus,
        error: &str,
        session_id: Option<String>,
        cost_usd: Option<f64>,
    ) -> Self {
        Parsed {
            status,
            report: None,
            session_id,
            cost_usd,
            // Shown in terminals and logs: same cleaning as report fields.
            error: Some(clip(error)),
        }
    }
}

/// What Claude Code's `--output-format json` result says about a run.
#[derive(Debug, PartialEq)]
pub struct Parsed {
    pub status: RunStatus,
    pub report: Option<Report>,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.trim().lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Read a finished run's exit code and output. `overran` is true when it
/// ran at least its timeout, so a kill (137) reads as a timeout.
pub fn parse_output(exit: Option<i32>, stdout: &str, stderr: &str, overran: bool) -> Parsed {
    let v: Option<serde_json::Value> = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .filter(|v: &serde_json::Value| v["type"] == "result");
    let session_id = v
        .as_ref()
        .and_then(|v| v["session_id"].as_str())
        .map(str::to_string);
    let cost_usd = v.as_ref().and_then(|v| v["total_cost_usd"].as_f64());
    let fail = |status, error: String| Parsed::failed(status, &error, session_id.clone(), cost_usd);
    if exit == Some(TIMEOUT_EXIT) || (overran && exit.is_none_or(|c| c == KILLED_EXIT)) {
        return fail(RunStatus::TimedOut, "the run hit its timeout".to_string());
    }
    let Some(v) = v else {
        let why = tail(stderr, 5);
        return fail(
            RunStatus::Failed,
            format!(
                "Claude Code gave no result (exit {}){}",
                exit.map_or("signal".to_string(), |c| c.to_string()),
                if why.is_empty() {
                    String::new()
                } else {
                    format!(": {why}")
                }
            ),
        );
    };
    if v["is_error"].as_bool() == Some(true) {
        let msg = v["result"].as_str().unwrap_or("unknown error");
        return fail(
            RunStatus::Failed,
            format!("Claude Code failed: {}", clip(msg)),
        );
    }
    match serde_json::from_value::<Report>(v["structured_output"].clone()) {
        Ok(r) => {
            let r = normalize(r);
            let status = match r.status {
                Severity::Ok => RunStatus::Ok,
                Severity::Warn => RunStatus::Warn,
                Severity::Critical => RunStatus::Critical,
            };
            Parsed {
                status,
                report: Some(r),
                session_id,
                cost_usd,
                error: None,
            }
        }
        Err(e) => fail(
            RunStatus::Failed,
            format!("the run gave no valid report: {e}"),
        ),
    }
}

/// Whether `podman inspect`'s mount list (`source=destination` lines) has
/// the repo at the sandbox's working directory.
pub fn mounts_repo(mounts: &str, repo: &Path) -> bool {
    let want = format!(
        "{}={}",
        repo.display(),
        crate::sandbox::container_working_dir(repo)
    );
    mounts.lines().any(|l| l.trim() == want)
}

/// Refuse a container that does not mount `repo`. The task row naming the
/// container is in `~/.nibble`, which sandboxes can write: without this
/// check a sandbox could point another repo's jobs, and their secrets, at
/// itself.
/// Returns the container's id, which the run then execs into, so the
/// container checked is the one used.
fn verify_container(container: &str, repo: &Path) -> Result<String> {
    if !valid_container_ref(container) {
        bail!("{container:?} is not a container name");
    }
    let out = Command::new("podman")
        .args([
            "inspect",
            "--type",
            "container",
            "--format",
            "{{.Id}}\n{{range .Mounts}}{{.Source}}={{.Destination}}\n{{end}}",
            "--",
            container,
        ])
        .output()
        .context("failed to run podman inspect")?;
    if !out.status.success() {
        bail!(
            "podman inspect {container} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let id = text.lines().next().unwrap_or_default().trim().to_string();
    if !mounts_repo(&text, repo) || !valid_container_ref(&id) {
        bail!(
            "container {container} does not mount {}; refusing to run there",
            repo.display()
        );
    }
    Ok(id)
}

/// A container name or id as podman makes them; anything else (such as a
/// flag like `-l`) is refused before it reaches a podman command line.
pub fn valid_container_ref(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Kill whatever still runs in the container for this run (found by its
/// AGENT_TASK_ID). Called when the host gives up on `podman exec`, and by
/// the daemon for runs whose process died (a restart, an upgrade), whose
/// sessions would otherwise keep going with the job's secrets.
pub fn kill_in_container(container: &str, task_id: &str) {
    let script = format!(
        "for p in /proc/[0-9]*; do grep -qz '^AGENT_TASK_ID={task_id}$' \"$p/environ\" 2>/dev/null && kill -9 \"${{p#/proc/}}\"; done; true"
    );
    let _ = Command::new("podman")
        .args(["exec", container, "/bin/sh", "-c", &script])
        .output();
}

/// One sandbox spawn at a time on this machine (the daemon's threads and
/// any `nibble cron run`): two jobs of a repo due together must not start
/// two sandboxes. A file lock in cron's private state dir.
const SPAWN_LOCK: &str = "spawn";

fn container_of(task: &Task) -> Option<&str> {
    task.container_name
        .as_deref()
        .or(task.container_id.as_deref())
        .filter(|c| !c.is_empty())
}

/// The repo's healthy sandbox, spawning one (without attaching) if needed.
fn ensure_sandbox(db: &Database, repo: &Path) -> Result<Task> {
    let key = repo.to_string_lossy().to_string();
    let healthy = |db: &Database| -> Result<Option<Task>> {
        let sandbox = crate::sandbox::podman::PodmanSandbox::new();
        Ok(db.get_task_by_repo_path(&key)?.filter(|t| {
            container_of(t)
                .is_some_and(|c| sandbox.health_check(c) == crate::sandbox::SandboxHealth::Healthy)
        }))
    };
    if let Some(t) = healthy(db)? {
        return can_run_claude(t);
    }
    let _one = crate::lock::lock_in(&super::store::state_dir(), SPAWN_LOCK)?;
    // Another job may have started it while this one waited.
    if let Some(t) = healthy(db)? {
        return can_run_claude(t);
    }
    crate::commands::cmd_sandbox_spawn(
        db,
        crate::commands::SpawnOptions {
            repo_path: key.clone(),
            task_desc: None,
            image: crate::models::SandboxConfig::default().image,
            fresh: false,
            session_id: None,
            no_attach: true,
            hermes: false,
            pi: false,
            omp: false,
        },
    )
    .context("failed to start a sandbox")?;
    can_run_claude(healthy(db)?.context("the sandbox started but is not healthy")?)
}

/// A Hermes sandbox has no Claude Code: say so instead of failing every
/// run with exit 127.
fn can_run_claude(t: Task) -> Result<Task> {
    if t.agent_type == crate::models::AgentType::Hermes {
        bail!(
            "the repo's sandbox ({}) is a Hermes sandbox, which cannot run Claude Code; \
             replace it with `nibble sandbox kill {}` and a normal sandbox",
            t.task_id,
            t.task_id
        );
    }
    Ok(t)
}

/// The `podman exec` that runs one job. The prompt goes on stdin; secret
/// values are passed by name (`-e NAME`) and set on the podman process, so
/// they never appear in a command line.
pub fn command(
    container: &str,
    cwd: &str,
    task_id: &str,
    def: &JobDef,
    env: &[(String, String)],
) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["exec", "-i"]);
    for (k, v) in [
        ("PATH", "/home/node/.local/bin:/usr/local/bin:/usr/bin:/bin"),
        ("CLAUDE_CONFIG_DIR", "/home/node/.claude"),
        ("AGENT_TASK_ID", task_id),
    ] {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    for (k, v) in env {
        cmd.arg("-e").arg(k);
        cmd.env(k, v);
    }
    cmd.args(["-w", cwd, container]);
    cmd.args(
        crate::sandbox::context::exec_with_sandbox_prompt_under_timeout(def.timeout.as_secs()),
    );
    cmd.arg("/home/node/.local/bin/claude");
    cmd.args([
        "-p",
        "--output-format",
        "json",
        "--json-schema",
        REPORT_SCHEMA,
    ]);
    cmd.arg("--dangerously-skip-permissions");
    if let Some(m) = &def.model {
        cmd.args(["--model", m]);
    }
    if def.read_only {
        cmd.args(["--disallowedTools", "Edit,Write,NotebookEdit"]);
    }
    cmd
}

/// The sidebar row for a run: a window of the sandbox task, marked so it
/// never becomes the session the sandbox resumes.
fn run_task(parent: &Task, def: &JobDef, pid: i32) -> Task {
    let mut t = crate::commands::build_window_task(
        parent,
        crate::commands::SelectedAgent::Claude,
        None,
        None,
        pid,
        0,
        true,
    );
    t.title = format!("{} · cron {}", parent.title, def.name);
    t
}

/// Why a run was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Schedule,
    Manual,
}

impl Trigger {
    fn as_str(self) -> &'static str {
        match self {
            Trigger::Schedule => "schedule",
            Trigger::Manual => "manual",
        }
    }
}

/// Releases a job's claim when the run ends, panics included.
struct Claim<'a> {
    store: &'a CronStore,
    id: i64,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let _ = self.store.release(self.id);
    }
}

/// Run `def` once in `repo`'s sandbox and record it. `None` when the job is
/// already running and may not overlap. A failure to start or finish is
/// recorded as a failed run, not returned; `Err` is only for state that
/// could not be written.
pub fn run_job(
    db: &Database,
    store: &CronStore,
    repo: &Path,
    def: &JobDef,
    trigger: Trigger,
) -> Result<Option<RunRecord>> {
    let key = repo.to_string_lossy().to_string();
    let pid = std::process::id();
    let Some(claim) = store.claim(&key, &def.name, pid, !def.skip_if_running)? else {
        return Ok(None);
    };
    let _claim = Claim { store, id: claim };
    let id = store.start_run(&key, &def.name, &def.label, trigger.as_str(), pid)?;
    let parsed = execute(db, store, id, repo, def)
        .unwrap_or_else(|e| Parsed::failed(RunStatus::Failed, &format!("{e:#}"), None, None));
    store.finish_run(
        id,
        &RunEnd {
            status: parsed.status,
            report: parsed.report.as_ref(),
            session_id: parsed.session_id.as_deref(),
            cost_usd: parsed.cost_usd,
            error: parsed.error.as_deref(),
        },
    )?;
    store.set_last_run(&key, &def.name, chrono::Utc::now())?;
    store
        .run(id)?
        .context("the run's record disappeared")
        .map(Some)
}

fn execute(
    db: &Database,
    store: &CronStore,
    run_id: i64,
    repo: &Path,
    def: &JobDef,
) -> Result<Parsed> {
    let key = repo.to_string_lossy().to_string();
    let table = super::secrets::load_from(&super::secrets::path())?;
    let env = super::secrets::resolve(&table, &key, &def.env)?;
    let sandbox = ensure_sandbox(db, repo)?;
    let named = container_of(&sandbox).context("the sandbox has no container")?;
    let container = verify_container(named, repo)?;
    let cwd = crate::sandbox::container_working_dir(repo);
    // The row goes in before the process starts, so no failure can leave
    // a run going that nothing waits on.
    let task = run_task(&sandbox, def, std::process::id() as i32);
    db.insert_task(&task)?;
    store.set_run_target(run_id, &container, &task.task_id)?;
    let started = Instant::now();
    let mut child = match command(&container, &cwd, &task.task_id, def, &env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = db.delete_task_by_id(&task.task_id);
            return Err(e).context("failed to start podman exec");
        }
    };
    let prompt = prompt_for(def);
    // Pipes are served from threads: a large prompt or output would
    // otherwise block before the process is waited on.
    let mut stdin = child.stdin.take().context("no stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(prompt.as_bytes()));
    let read = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = read(child.stdout.take().map(|p| Box::new(p) as _));
    let err = read(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = def.timeout + HOST_GRACE;
    let status = loop {
        if let Some(st) = child.try_wait().context("failed to wait for the run")? {
            break Some(st);
        }
        if started.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            kill_in_container(&container, &task.task_id);
            break None;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    let _ = writer.join();
    let (stdout, stderr) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    let code = status.and_then(|s| s.code());
    // The row only shows the run while it goes; the run's record keeps the
    // rest. Losing it must not lose the report.
    if let Err(e) = db.delete_task_by_id(&task.task_id) {
        eprintln!("nibble cron: could not remove the run's sidebar row: {e:#}");
    }
    Ok(parse_output(
        code,
        &String::from_utf8_lossy(&stdout),
        &String::from_utf8_lossy(&stderr),
        started.elapsed() >= def.timeout,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(fp: &str, sev: Severity) -> Finding {
        Finding {
            fingerprint: fp.into(),
            severity: sev,
            title: format!("T {fp}"),
            details: "d".into(),
            suggested_action: "a".into(),
        }
    }

    #[test]
    fn schema_is_json_and_matches_the_types() {
        let schema: serde_json::Value = serde_json::from_str(REPORT_SCHEMA).unwrap();
        assert_eq!(schema["type"], "object");
        let sample = serde_json::json!({
            "status": "warn", "summary": "s",
            "findings": [{"fingerprint": "f", "severity": "critical", "title": "t",
                          "details": "d", "suggested_action": "a"}]
        });
        let r: Report = serde_json::from_value(sample).unwrap();
        assert_eq!(r.findings[0].severity, Severity::Critical);
    }

    #[test]
    fn normalize_bounds_and_dedupes() {
        let long = "x".repeat(MAX_FIELD + 50);
        let mut many: Vec<Finding> = (0..30)
            .map(|i| finding(&format!("f{i}"), Severity::Warn))
            .collect();
        many.insert(0, finding("dup", Severity::Critical));
        many.insert(1, finding("dup", Severity::Warn));
        many.push(Finding {
            fingerprint: " ".into(),
            severity: Severity::Ok,
            ..finding("", Severity::Ok)
        });
        let r = normalize(Report {
            status: Severity::Ok,
            summary: long.clone(),
            findings: many,
        });
        assert_eq!(
            r.summary.chars().count(),
            MAX_FIELD + 1,
            "clipped with an ellipsis"
        );
        assert_eq!(r.findings.len(), MAX_FINDINGS);
        assert_eq!(
            r.findings.iter().filter(|f| f.fingerprint == "dup").count(),
            1
        );
        assert_eq!(r.status, Severity::Critical, "raised to the worst finding");
        let blank = normalize(Report {
            status: Severity::Ok,
            summary: "s".into(),
            findings: vec![Finding {
                fingerprint: "".into(),
                severity: Severity::Ok,
                ..finding("", Severity::Ok)
            }],
        });
        assert_eq!(blank.findings[0].fingerprint, "title:t");
        assert_eq!(blank.findings[0].severity, Severity::Warn);
        assert_eq!(blank.status, Severity::Warn);
        // Multibyte text is clipped on a character boundary.
        assert!(clip(&"é".repeat(MAX_FIELD + 3)).ends_with('…'));
        assert_eq!(
            clip("\u{1b}[31mred\u{1b}[0m\tok\nnext\u{7}"),
            "[31mred[0m\tok\nnext"
        );
    }

    fn result(extra: serde_json::Value) -> String {
        let mut v = serde_json::json!({
            "type": "result", "session_id": "sid", "total_cost_usd": 0.25, "is_error": false
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        format!("noise\n{v}\n")
    }

    #[test]
    fn parses_a_report() {
        let out = result(serde_json::json!({
            "structured_output": {"status": "ok", "summary": "fine", "findings": []}
        }));
        let p = parse_output(Some(0), &out, "", false);
        assert_eq!(p.status, RunStatus::Ok);
        assert_eq!(p.report.unwrap().summary, "fine");
        assert_eq!(p.session_id.as_deref(), Some("sid"));
        assert_eq!(p.cost_usd, Some(0.25));
        let crit = result(serde_json::json!({
            "structured_output": {"status": "warn", "summary": "s",
              "findings": [{"fingerprint": "f", "severity": "critical", "title": "t"}]}
        }));
        assert_eq!(
            parse_output(Some(0), &crit, "", false).status,
            RunStatus::Critical
        );
    }

    #[test]
    fn parses_failures() {
        let t = parse_output(Some(TIMEOUT_EXIT), "", "", false);
        assert_eq!(t.status, RunStatus::TimedOut);
        assert_eq!(
            parse_output(Some(KILLED_EXIT), "", "", true).status,
            RunStatus::TimedOut
        );
        assert_eq!(
            parse_output(None, "", "", true).status,
            RunStatus::TimedOut,
            "host kill"
        );
        assert_eq!(
            parse_output(Some(KILLED_EXIT), "", "", false).status,
            RunStatus::Failed
        );
        let dirty = parse_output(Some(1), "", "\u{1b}]0;title\u{7}boom", false);
        assert!(
            !dirty.error.unwrap().contains('\u{1b}'),
            "errors are cleaned too"
        );
        let none = parse_output(Some(1), "not json", "line1\nboom", false);
        assert_eq!(none.status, RunStatus::Failed);
        assert!(none.error.unwrap().contains("exit 1): line1\nboom"));
        assert!(parse_output(None, "", "", false)
            .error
            .unwrap()
            .contains("exit signal"));
        let err = parse_output(
            Some(1),
            &result(serde_json::json!({"is_error": true, "result": "quota"})),
            "",
            false,
        );
        assert!(err.error.unwrap().contains("quota"));
        assert_eq!(
            err.session_id.as_deref(),
            Some("sid"),
            "kept to look into it"
        );
        let bad = parse_output(
            Some(0),
            &result(serde_json::json!({"structured_output": {"status": "maybe"}})),
            "",
            false,
        );
        assert!(bad.error.unwrap().contains("no valid report"));
        // A JSON line that is not the result is not mistaken for one.
        assert_eq!(
            parse_output(Some(0), "{\"type\":\"other\"}", "", false).status,
            RunStatus::Failed
        );
    }

    #[test]
    fn a_claimed_job_does_not_start_again() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = CronStore::open_in(&tmp.path().join("s")).unwrap();
        let db = Database::open(tmp.path().join("t.db")).unwrap();
        let repo = tmp.path().join("shop");
        let d = def();
        let key = repo.to_string_lossy().to_string();
        // A live owner (this test process) holds it.
        let held = store
            .claim(&key, &d.name, std::process::id(), false)
            .unwrap()
            .unwrap();
        assert!(run_job(&db, &store, &repo, &d, Trigger::Manual)
            .unwrap()
            .is_none());
        assert!(store.runs(10).unwrap().is_empty(), "nothing recorded");
        store.release(held).unwrap();
        // Unclaimed: it runs, and without a sandbox the failure is recorded.
        let r = run_job(&db, &store, &repo, &d, Trigger::Manual)
            .unwrap()
            .unwrap();
        assert_eq!(r.status, RunStatus::Failed);
        assert!(
            !store.job(&key, &d.name).unwrap().running,
            "the claim is released"
        );
    }

    #[test]
    fn hermes_sandboxes_are_refused() {
        let mut t = Task::new(
            "h".into(),
            crate::models::AgentType::Hermes,
            "[x:hermes]".into(),
            None,
            None,
        );
        assert!(can_run_claude(t.clone())
            .unwrap_err()
            .to_string()
            .contains("nibble sandbox kill h"));
        t.agent_type = crate::models::AgentType::ClaudeCode;
        assert!(can_run_claude(t).is_ok());
    }

    #[test]
    fn container_refs_cannot_be_flags() {
        assert!(valid_container_ref("nibble-shop-1a2b"));
        assert!(valid_container_ref("4f2c9e0d1a"));
        for bad in ["-l", "--latest", "", "a b", "x;rm", "a/b"] {
            assert!(!valid_container_ref(bad), "{bad}");
        }
    }

    #[test]
    fn mount_check() {
        let repo = Path::new("/home/me/shop");
        assert!(mounts_repo("/x=/y\n/home/me/shop=/shop\n", repo));
        assert!(!mounts_repo("/home/me/other=/shop\n", repo));
        assert!(!mounts_repo("/home/me/shop=/elsewhere\n", repo));
        assert!(!mounts_repo("", repo));
    }

    fn def() -> JobDef {
        let mut d = super::super::parse_job(
            "logs",
            "schedule = \"0 9 * * *\"\ntimeout = \"10m\"\nmodel = \"m\"\nenv = [\"TOKEN\"]\n## Prompt\nCheck",
        )
        .unwrap()
        .def;
        d.read_only = true;
        d
    }

    #[test]
    fn command_shape() {
        let env = vec![("TOKEN".to_string(), "s3cret".to_string())];
        let cmd = command("ctr", "/shop", "task-1", &def(), &env);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let joined = args.join(" ");
        assert!(
            !joined.contains("s3cret"),
            "secret values stay off the command line"
        );
        assert!(args.windows(2).any(|w| w == ["-e", "TOKEN"]));
        assert!(args.windows(2).any(|w| w == ["-e", "AGENT_TASK_ID=task-1"]));
        assert!(args.windows(2).any(|w| w == ["--model", "m"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--disallowedTools", "Edit,Write,NotebookEdit"]));
        assert!(joined.contains("exec timeout -k 30 600 \"$0\""));
        let pos = |s: &str| args.iter().position(|a| a == s).unwrap();
        assert!(pos("ctr") < pos("/bin/bash"));
        assert_eq!(
            args[pos("-c") + 2],
            "/home/node/.local/bin/claude",
            "$0 is claude"
        );
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs
            .iter()
            .any(|(k, v)| *k == "TOKEN" && v.is_some_and(|v| v == "s3cret")));
        let mut write = def();
        write.read_only = false;
        write.model = None;
        let args: Vec<String> = command("ctr", "/shop", "t", &write, &[])
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(!args
            .iter()
            .any(|a| a == "--disallowedTools" || a == "--model"));
    }

    #[test]
    fn prompt_carries_the_run_notes() {
        let p = prompt_for(&def());
        assert!(p.starts_with("Check\n"));
        assert!(p.contains("unattended"));
    }

    #[test]
    fn run_rows_are_side_windows() {
        let mut parent = Task::new(
            "parent".into(),
            crate::models::AgentType::ClaudeCode,
            "[shop:sandbox]".into(),
            None,
            None,
        );
        parent.repo_path = Some("/home/me/shop".into());
        let t = run_task(&parent, &def(), 42);
        assert_eq!(t.title, "[shop:sandbox] · cron logs");
        assert_eq!(t.pid, Some(42));
        let extra = &t.context.as_ref().unwrap().extra;
        assert_eq!(extra.get("btw"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(extra["parent_task_id"], "parent");
    }
}
