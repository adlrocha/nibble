//! Scheduled agent jobs (`nibble cron`).
//!
//! A job is a markdown file in the repository it runs against,
//! `<repo>/.nibble/cron/<name>.md`, so it travels with the code: any machine
//! that registers the repo (`nibble cron add`) runs the same jobs. This
//! machine's runtime state (which repos are registered, when each job runs
//! next, what past runs reported) lives in [`store`], outside every
//! sandbox's reach.
//!
//! ```markdown
//! # Production logs
//!
//! schedule = "*/30 * * * *"
//! timeout = "20m"
//! env = ["NORTHFLANK_API_TOKEN"]
//!
//! ## Prompt
//!
//! Read the last 30 minutes of production logs and report errors.
//! ```
//!
//! Everything between the `# ` title and `## Prompt` is TOML; unknown keys
//! are an error, so a typo never silently drops a setting.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Timelike, Utc};
use croner::Cron;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod commands;
pub mod daemon;
pub mod notify;
pub mod run;
pub mod secrets;
pub mod store;

/// Where a repository keeps its job files.
pub const JOBS_DIR: &str = ".nibble/cron";
/// How long a run may take when the file sets no `timeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Longest `timeout` a file may set.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
/// Env names a job may not take from the secrets file: nibble sets them.
const RESERVED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "TERM",
    "USER",
    "SHELL",
    "CLAUDE_CONFIG_DIR",
    "AGENT_TASK_ID",
];

/// Parse a cron schedule expression and compute the next run time
/// (evaluated in the local timezone).
pub fn compute_next_run(schedule: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let cron = Cron::new(schedule)
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid cron expression '{}': {}", schedule, e))?;
    let local_after = after.with_timezone(&Local);
    let next_local = cron
        .find_next_occurrence(&local_after, false)
        .map_err(|e| anyhow::anyhow!("Failed to compute next run: {}", e))?;
    Ok(next_local.with_timezone(&Utc))
}

/// Validate a cron expression: it must parse and fire at some point
/// (`0 0 30 2 *` never does).
pub fn validate_schedule(schedule: &str) -> Result<()> {
    Cron::new(schedule)
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid cron expression: {}", e))?;
    compute_next_run(schedule, Utc::now()).context("the schedule never fires")?;
    Ok(())
}

/// Where a run's report goes when it finds something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Notify {
    /// A card in your Slack DM, through `nibble rc`.
    Slack,
    /// Only `nibble cron runs`.
    None,
}

impl Notify {
    pub fn as_str(self) -> &'static str {
        match self {
            Notify::Slack => "slack",
            Notify::None => "none",
        }
    }
}

/// A job as its file defines it.
#[derive(Debug, Clone, PartialEq)]
pub struct JobDef {
    /// The file name without `.md`: the job's id within its repo.
    pub name: String,
    /// The `# ` title (the name when there is none).
    pub label: String,
    pub schedule: String,
    /// False switches the job off on every machine.
    pub enabled: bool,
    /// Skip a due run while the previous one is still going.
    pub skip_if_running: bool,
    /// No runs after this time.
    pub expires_at: Option<DateTime<Utc>>,
    pub timeout: Duration,
    /// Claude model for the run; Claude Code's default when unset.
    pub model: Option<String>,
    /// Names of variables the run gets from the secrets file.
    pub env: Vec<String>,
    pub notify: Notify,
    /// Withhold the file-editing tools, so the run cannot change your
    /// working tree (the sandbox mounts the repo read-write).
    pub read_only: bool,
    pub prompt: String,
}

/// The TOML header of a job file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    schedule: String,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default = "yes")]
    skip_if_running: bool,
    expires_at: Option<String>,
    timeout: Option<String>,
    model: Option<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default = "slack")]
    notify: Notify,
    #[serde(default = "yes")]
    read_only: bool,
    /// Only for `nibble cron add --file`, naming the repo the file is for.
    repo_path: Option<String>,
}

fn yes() -> bool {
    true
}

fn slack() -> Notify {
    Notify::Slack
}

/// A parsed job file plus the repo it names, if any (`add --file` only).
pub struct Parsed {
    pub def: JobDef,
    pub repo_path: Option<String>,
}

/// Parse a job file's text. `name` is its file name without `.md`.
pub fn parse_job(name: &str, content: &str) -> Result<Parsed> {
    validate_name(name)?;
    let mut label = None;
    let mut header = String::new();
    let mut prompt: Option<Vec<&str>> = None;
    for line in content.lines() {
        if let Some(lines) = prompt.as_mut() {
            lines.push(line);
            continue;
        }
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("## prompt") {
            prompt = Some(Vec::new());
        } else if label.is_none() && header.trim().is_empty() && trimmed.starts_with("# ") {
            label = Some(trimmed[2..].trim().to_string());
        } else {
            header.push_str(line);
            header.push('\n');
        }
    }
    if let Some(l) = &label {
        validate_label(l)?;
    }
    let prompt = prompt
        .context("missing the `## Prompt` section")?
        .join("\n")
        .trim()
        .to_string();
    if prompt.is_empty() {
        bail!("the `## Prompt` section is empty");
    }
    let h: Header = toml::from_str(&header).context("invalid settings above `## Prompt`")?;
    validate_schedule(&h.schedule)?;
    let expires_at = h
        .expires_at
        .as_deref()
        .map(|s| {
            DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&Utc))
                .with_context(|| format!("expires_at {s:?} is not an RFC3339 time"))
        })
        .transpose()?;
    let timeout = match h.timeout.as_deref() {
        Some(s) => parse_duration(s).with_context(|| format!("timeout {s:?}"))?,
        None => DEFAULT_TIMEOUT,
    };
    if timeout > MAX_TIMEOUT {
        bail!("timeout may be at most 6h");
    }
    for var in &h.env {
        validate_env_name(var)?;
    }
    Ok(Parsed {
        def: JobDef {
            name: name.to_string(),
            label: label
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| name.to_string()),
            schedule: h.schedule,
            enabled: h.enabled,
            skip_if_running: h.skip_if_running,
            expires_at,
            timeout,
            model: h.model.filter(|m| !m.trim().is_empty()),
            env: h.env,
            notify: h.notify,
            read_only: h.read_only,
            prompt,
        },
        repo_path: h.repo_path,
    })
}

/// A title fits on its `# ` line: no line breaks or other control
/// characters, which would spill into the settings.
pub fn validate_label(label: &str) -> Result<()> {
    if label.trim().is_empty() || label.chars().any(char::is_control) {
        bail!("a job title must be one non-empty line without control characters");
    }
    Ok(())
}

/// A job file name: lowercase letters, digits, `-` and `_`.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !ok {
        bail!("job name {name:?} must be 1-64 lowercase letters, digits, '-' or '_'");
    }
    Ok(())
}

/// An env name a job may take from secrets.
pub(crate) fn validate_env_name(var: &str) -> Result<()> {
    let ok = var.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
        && var
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        bail!("env name {var:?} must be uppercase letters, digits and '_'");
    }
    if RESERVED_ENV.contains(&var) {
        bail!("env name {var} is set by nibble and cannot come from secrets");
    }
    Ok(())
}

/// A job name made from a label: `Daily Standup!` → `daily-standup`.
pub fn slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out: String = out.trim_end_matches('-').chars().take(64).collect();
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "job".to_string()
    } else {
        out
    }
}

/// `90s`, `20m`, `2h` or `1d`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .context("needs a unit: s, m, h or d")?;
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().context("needs a number before the unit")?;
    let per = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("unknown unit {unit:?}: use s, m, h or d"),
    };
    let secs = n.checked_mul(per).context("is too large")?;
    if secs == 0 {
        bail!("must be more than zero");
    }
    Ok(Duration::from_secs(secs))
}

/// An expiry given on the command line: RFC3339, or relative (`+7d`).
pub fn parse_expiry(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    if let Some(rel) = s.strip_prefix('+') {
        let d = parse_duration(rel).with_context(|| format!("relative expiry {s:?}"))?;
        // Whole seconds, so the time reads cleanly in the file.
        let at = chrono::Duration::from_std(d)
            .ok()
            .and_then(|d| now.checked_add_signed(d))
            .with_context(|| format!("relative expiry {s:?} is too far away"))?;
        return Ok(at.with_nanosecond(0).unwrap_or(at));
    }
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .with_context(|| format!("expiry {s:?}: use RFC3339 (2026-04-01T00:00:00Z) or +7d"))
}

fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        _ if s.is_multiple_of(86_400) => format!("{}d", s / 86_400),
        _ if s.is_multiple_of(3600) => format!("{}h", s / 3600),
        _ if s.is_multiple_of(60) => format!("{}m", s / 60),
        _ => format!("{s}s"),
    }
}

/// A TOML string literal.
fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// The file text for `def`; [`parse_job`] reads it back unchanged.
pub fn format_job(def: &JobDef) -> String {
    let mut out = format!("# {}\n\nschedule = {}\n", def.label, quote(&def.schedule));
    if !def.enabled {
        out.push_str("enabled = false\n");
    }
    if !def.skip_if_running {
        out.push_str("skip_if_running = false\n");
    }
    if let Some(exp) = def.expires_at {
        out.push_str(&format!(
            "expires_at = {}\n",
            quote(&exp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        ));
    }
    if def.timeout != DEFAULT_TIMEOUT {
        out.push_str(&format!(
            "timeout = {}\n",
            quote(&format_duration(def.timeout))
        ));
    }
    if let Some(m) = &def.model {
        out.push_str(&format!("model = {}\n", quote(m)));
    }
    if !def.env.is_empty() {
        let names: Vec<String> = def.env.iter().map(|e| quote(e)).collect();
        out.push_str(&format!("env = [{}]\n", names.join(", ")));
    }
    if def.notify != Notify::Slack {
        out.push_str(&format!("notify = {}\n", quote(def.notify.as_str())));
    }
    if !def.read_only {
        out.push_str("read_only = false\n");
    }
    out.push_str(&format!("\n## Prompt\n\n{}\n", def.prompt));
    out
}

/// `<repo>/.nibble/cron`.
pub fn jobs_dir(repo: &Path) -> PathBuf {
    repo.join(JOBS_DIR)
}

/// `<repo>/.nibble/cron/<name>.md`.
pub fn job_path(repo: &Path, name: &str) -> PathBuf {
    jobs_dir(repo).join(format!("{name}.md"))
}

/// `~/.nibble`, which every sandbox mounts read-write.
pub fn sandbox_mount() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".nibble"))
}

/// Refuse a path that resolves into `~/.nibble`: any sandbox could write
/// jobs there.
pub fn check_outside_sandbox_mount(path: &Path) -> Result<()> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mount = sandbox_mount().map(|m| std::fs::canonicalize(&m).unwrap_or(m));
    if mount.is_some_and(|m| real.starts_with(m)) {
        bail!(
            "{} is inside ~/.nibble, which sandboxes can write; jobs cannot live there",
            path.display()
        );
    }
    Ok(())
}

/// A directory jobs may be registered for, canonicalised. Your home
/// directory is refused (its job directory would be `~/.nibble/cron`), and
/// so is anything inside `~/.nibble`. With `require_git`, it must be a git
/// repository.
pub fn validate_repo(path: &Path, require_git: bool) -> Result<PathBuf> {
    let repo = std::fs::canonicalize(path)
        .with_context(|| format!("repo {} does not exist", path.display()))?;
    if !repo.is_dir() {
        bail!("repo {} is not a directory", repo.display());
    }
    let home = dirs::home_dir().and_then(|h| std::fs::canonicalize(h).ok());
    if home.as_deref() == Some(repo.as_path()) {
        bail!("your home directory cannot hold jobs; register a repository");
    }
    check_outside_sandbox_mount(&repo)?;
    if require_git && !repo.join(".git").exists() {
        bail!("{} is not a git repository", repo.display());
    }
    Ok(repo)
}

/// A job file that could not be read or parsed.
#[derive(Debug)]
pub struct BadFile {
    pub path: PathBuf,
    pub error: String,
}

/// Every job file in `repo`, sorted by name. Files that fail to parse are
/// returned apart, so one broken file does not stop the others.
pub fn load_repo_jobs(repo: &Path) -> Result<(Vec<JobDef>, Vec<BadFile>)> {
    let dir = jobs_dir(repo);
    if dir.exists() {
        check_outside_sandbox_mount(&dir)?;
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], vec![])),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", dir.display())),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md") && p.is_file())
        .collect();
    paths.sort();
    let mut jobs = Vec::new();
    let mut bad = Vec::new();
    for path in paths {
        match load_job_file(&path) {
            Ok(def) => jobs.push(def),
            Err(e) => bad.push(BadFile {
                path,
                error: format!("{e:#}"),
            }),
        }
    }
    Ok((jobs, bad))
}

/// Read and parse one job file inside a repo. `repo_path` is not allowed
/// there: the file's location already says which repo it is for.
pub fn load_job_file(path: &Path) -> Result<JobDef> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("job file name is not UTF-8")?;
    // A link could make edits write outside the repo, or read from where a
    // sandbox can write.
    if std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .file_type()
        .is_symlink()
    {
        bail!(
            "{} is a symlink; job files must be regular files",
            path.display()
        );
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let parsed = parse_job(name, &text)?;
    if parsed.repo_path.is_some() {
        bail!("repo_path is implied by where the file lives; remove it");
    }
    Ok(parsed.def)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const FULL: &str = r#"# Production logs

schedule = "*/30 * * * *"
enabled = true
skip_if_running = false
expires_at = "2030-01-01T00:00:00Z"
timeout = "20m"
model = "claude-sonnet-5-5"
env = ["NORTHFLANK_API_TOKEN"]
notify = "none"
read_only = false

## Prompt

Read the logs.
enabled = false is prompt text, not a setting.
# Not a title either
"#;

    #[test]
    fn parses_every_field() {
        let def = parse_job("logs", FULL).unwrap().def;
        assert_eq!(def.label, "Production logs");
        assert_eq!(def.schedule, "*/30 * * * *");
        assert!(def.enabled);
        assert!(!def.skip_if_running);
        assert_eq!(
            def.expires_at,
            Some(Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap())
        );
        assert_eq!(def.timeout, Duration::from_secs(20 * 60));
        assert_eq!(def.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(def.env, vec!["NORTHFLANK_API_TOKEN"]);
        assert_eq!(def.notify, Notify::None);
        assert!(!def.read_only);
        // Lines in the prompt that look like settings stay in the prompt.
        assert!(def.prompt.contains("enabled = false is prompt text"));
        assert!(def.prompt.ends_with("# Not a title either"));
    }

    #[test]
    fn minimal_file_gets_defaults() {
        let def = parse_job("t", "schedule = \"0 9 * * *\"\n## Prompt\nHi")
            .unwrap()
            .def;
        assert_eq!(def.label, "t", "no title: the name");
        assert!(def.enabled && def.skip_if_running && def.read_only);
        assert_eq!(def.timeout, DEFAULT_TIMEOUT);
        assert_eq!(def.notify, Notify::Slack);
        assert!(def.env.is_empty() && def.model.is_none() && def.expires_at.is_none());
        assert_eq!(def.prompt, "Hi");
    }

    #[test]
    fn legacy_files_still_parse() {
        // The format older nibble documented, `## prompt` and quotes included.
        let text = "# Daily Standup\n\nschedule = \"0 9 * * 1-5\"\nrepo_path = \"/r\"\nenabled = true\nskip_if_running = true\n\n## prompt\n\nSummarise.\n";
        let parsed = parse_job("daily-standup", text).unwrap();
        assert_eq!(parsed.repo_path.as_deref(), Some("/r"));
        assert_eq!(parsed.def.prompt, "Summarise.");
    }

    #[test]
    fn rejects_mistakes() {
        let cases = [
            (
                "schedule = \"0 9 * * *\"\nschedul = \"x\"\n## Prompt\nHi",
                "unknown key",
            ),
            (
                "schedule = \"0 9 * * *\"\nenabled = \"maybe\"\n## Prompt\nHi",
                "bad bool",
            ),
            ("## Prompt\nHi", "no schedule"),
            ("schedule = \"nope\"\n## Prompt\nHi", "bad schedule"),
            ("schedule = \"0 9 * * *\"\n", "no prompt section"),
            ("schedule = \"0 9 * * *\"\n## Prompt\n\n", "empty prompt"),
            (
                "schedule = \"0 9 * * *\"\ntimeout = \"7h\"\n## Prompt\nHi",
                "too long",
            ),
            (
                "schedule = \"0 9 * * *\"\ntimeout = \"10\"\n## Prompt\nHi",
                "no unit",
            ),
            (
                "schedule = \"0 9 * * *\"\nexpires_at = \"+7d\"\n## Prompt\nHi",
                "relative expiry",
            ),
            (
                "schedule = \"0 9 * * *\"\nenv = [\"PATH\"]\n## Prompt\nHi",
                "reserved env",
            ),
            (
                "schedule = \"0 9 * * *\"\nenv = [\"a b\"]\n## Prompt\nHi",
                "bad env",
            ),
            (
                "schedule = \"0 9 * * *\"\nnotify = \"email\"\n## Prompt\nHi",
                "bad notify",
            ),
            (
                "Some prose\nschedule = \"0 9 * * *\"\n## Prompt\nHi",
                "prose",
            ),
        ];
        for (text, why) in cases {
            assert!(parse_job("t", text).is_err(), "{why} should fail");
        }
        assert!(parse_job("Bad Name", "schedule = \"0 9 * * *\"\n## Prompt\nHi").is_err());
    }

    #[test]
    fn format_round_trips() {
        for text in [
            FULL,
            "# Minimal\n\nschedule = \"0 9 * * *\"\n\n## Prompt\n\nHi\n",
        ] {
            let def = parse_job("x", text).unwrap().def;
            let again = parse_job("x", &format_job(&def)).unwrap().def;
            assert_eq!(def, again);
        }
        // Quotes and backslashes survive.
        let mut def = parse_job("x", FULL).unwrap().def;
        def.label = "Quote \" and \\".to_string();
        def.model = Some("a\"b".to_string());
        assert_eq!(parse_job("x", &format_job(&def)).unwrap().def, def);
    }

    #[test]
    fn titles_stay_on_one_line() {
        assert!(validate_label("Prod logs").is_ok());
        assert!(validate_label("a\nread_only = false").is_err());
        assert!(validate_label("tab\there").is_err());
        assert!(validate_label("  ").is_err());
    }

    #[test]
    fn symlinked_job_files_are_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = jobs_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        let real = tmp.path().join("elsewhere.md");
        std::fs::write(&real, "schedule = \"0 9 * * *\"\n## Prompt\nx").unwrap();
        std::os::unix::fs::symlink(&real, dir.join("link.md")).unwrap();
        let (jobs, bad) = load_repo_jobs(tmp.path()).unwrap();
        assert!(jobs.is_empty());
        assert!(bad[0].error.contains("symlink"), "{:?}", bad);
    }

    #[test]
    fn the_sandbox_mount_is_refused() {
        let home = dirs::home_dir().unwrap();
        assert!(validate_repo(&home, false).is_err(), "home is refused");
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(validate_repo(tmp.path(), false).is_ok());
        assert!(validate_repo(tmp.path(), true).is_err(), "not a git repo");
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        assert!(validate_repo(tmp.path(), true).is_ok());
        assert!(validate_repo(&tmp.path().join("nope"), false).is_err());
        let mount = sandbox_mount().unwrap();
        assert!(check_outside_sandbox_mount(&mount.join("cron")).is_err());
        assert!(check_outside_sandbox_mount(Path::new("/tmp")).is_ok());
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Daily Standup!"), "daily-standup");
        assert_eq!(slug("  --Ünïcode  "), "n-code");
        assert_eq!(slug("!!!"), "job");
        assert_eq!(slug(&"a".repeat(80)).len(), 64);
        assert!(validate_name(&slug("Malone Daily Tasks")).is_ok());
    }

    #[test]
    fn durations_and_expiry() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("m").is_err());
        assert!(
            parse_duration("5é").is_err(),
            "multibyte unit must not panic"
        );
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(
            parse_expiry("+7d", now).unwrap(),
            Utc.with_ymd_and_hms(2026, 1, 8, 0, 0, 0).unwrap()
        );
        assert!(parse_expiry("2026-04-01T00:00:00Z", now).is_ok());
        let odd =
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::nanoseconds(5);
        assert_eq!(parse_expiry("+1s", odd).unwrap().nanosecond(), 0);
        assert!(parse_expiry("tomorrow", now).is_err());
        assert!(
            parse_expiry("+100000000000d", now).is_err(),
            "no panic past chrono's range"
        );
        assert!(
            parse_duration("99999999999999999d").is_err(),
            "overflow is an error"
        );
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::from_secs(86_400)), "1d");
    }

    #[test]
    fn loads_a_repo_and_isolates_bad_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = jobs_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.md"), "schedule = \"0 9 * * *\"\n## Prompt\nB").unwrap();
        std::fs::write(dir.join("a.md"), "schedule = \"0 9 * * *\"\n## Prompt\nA").unwrap();
        std::fs::write(dir.join("broken.md"), "## Prompt\nno schedule").unwrap();
        std::fs::write(
            dir.join("named.md"),
            "schedule = \"0 9 * * *\"\nrepo_path = \"/x\"\n## Prompt\nA",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let (jobs, bad) = load_repo_jobs(tmp.path()).unwrap();
        let names: Vec<_> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert!(bad.iter().any(|b| b.error.contains("repo_path is implied")));
        // A repo without the directory has no jobs.
        let empty = tempfile::TempDir::new().unwrap();
        assert!(load_repo_jobs(empty.path()).unwrap().0.is_empty());
    }

    #[test]
    fn next_run_is_after_and_on_schedule() {
        let now = Utc::now();
        let next = compute_next_run("0 0 * * *", now).unwrap();
        assert!(next > now);
        let local = next.with_timezone(&Local);
        assert_eq!((local.hour(), local.minute()), (0, 0));
        assert!(compute_next_run("bad", now).is_err());
        assert!(validate_schedule("0 0 30 2 *").is_err(), "never fires");
        assert!(validate_schedule("*/5 * * * *").is_ok());
    }
}
