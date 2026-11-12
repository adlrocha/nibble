//! `nibble cron` command handlers.

use super::store::{CronStore, JobState};
use super::{secrets, JobDef};
use crate::db::Database;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

/// Open this machine's cron state, writing out (once) any jobs an older
/// nibble kept in `~/.nibble/tasks.db`.
pub(crate) fn open(db: &Database) -> Result<CronStore> {
    let store = CronStore::open()?;
    for path in super::store::export_legacy(&store, db)? {
        eprintln!(
            "Moved a cron job from nibble's database into {} (commit it to keep it).",
            path.display()
        );
    }
    Ok(store)
}

/// `~` expanded, canonical, and a directory. With no argument: the git
/// repo around the current directory, or the directory itself.
pub(crate) fn canonical_repo(arg: Option<&str>) -> Result<PathBuf> {
    let raw = match arg {
        Some(a) => match a.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().context("no home directory")?.join(rest),
            None => PathBuf::from(a),
        },
        None => {
            let cwd = std::env::current_dir().context("no current directory")?;
            cwd.ancestors()
                .find(|d| d.join(".git").exists())
                .map(Path::to_path_buf)
                .unwrap_or(cwd)
        }
    };
    let repo = std::fs::canonicalize(&raw)
        .with_context(|| format!("repo {} does not exist", raw.display()))?;
    if !repo.is_dir() {
        bail!("repo {} is not a directory", repo.display());
    }
    Ok(repo)
}

fn repo_str(repo: &Path) -> String {
    repo.to_string_lossy().to_string()
}

/// Bring a job's schedule state up to date with its file: a new job, or
/// one whose schedule changed, gets its next run computed from `now` (so
/// registering never fires a job at once).
pub(crate) fn refresh_state(mut s: JobState, def: &JobDef, now: DateTime<Utc>) -> Result<JobState> {
    if s.next_run.is_none() || s.schedule.as_deref() != Some(def.schedule.as_str()) {
        s.next_run = Some(super::compute_next_run(&def.schedule, now)?);
        s.schedule = Some(def.schedule.clone());
    }
    Ok(s)
}

/// What `list` shows for a job.
pub(crate) fn status(def: &JobDef, s: &JobState, now: DateTime<Utc>) -> &'static str {
    if s.running {
        "running"
    } else if !def.enabled {
        "disabled"
    } else if s.paused {
        "paused"
    } else if def.expires_at.is_some_and(|e| e <= now) {
        "expired"
    } else if s.next_run.is_some_and(|n| n <= now) {
        "due"
    } else {
        "enabled"
    }
}

/// A job the user named: `name`, `repo/name` (repo by its directory name),
/// or the path of a job file.
pub(crate) fn resolve_job(store: &CronStore, arg: &str) -> Result<(PathBuf, JobDef)> {
    let as_path = Path::new(arg);
    if arg.ends_with(".md") && as_path.is_file() {
        let path = std::fs::canonicalize(as_path)?;
        let repo = path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .filter(|_| path.parent().is_some_and(|d| d.ends_with(super::JOBS_DIR)))
            .with_context(|| format!("{arg} is not in a repo's {}", super::JOBS_DIR))?
            .to_path_buf();
        return Ok((repo, super::load_job_file(&path)?));
    }
    let (repo_name, name) = match arg.rsplit_once('/') {
        Some((r, n)) => (Some(r), n),
        None => (None, arg),
    };
    let here = canonical_repo(None).ok();
    let mut matches: Vec<PathBuf> = store
        .repos()?
        .into_iter()
        .map(PathBuf::from)
        .filter(|r| super::job_path(r, name).is_file())
        .filter(|r| repo_name.is_none_or(|want| r.file_name().is_some_and(|f| f == want)))
        .collect();
    if matches.len() > 1 {
        if let Some(h) = here.filter(|h| matches.contains(h)) {
            matches = vec![h];
        }
    }
    match matches.as_slice() {
        [repo] => {
            let def = super::load_job_file(&super::job_path(repo, name))?;
            Ok((repo.clone(), def))
        }
        [] => bail!("no job {arg:?} in any registered repo (see `nibble cron list`)"),
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|r| {
                    format!(
                        "{}/{name}",
                        r.file_name().unwrap_or_default().to_string_lossy()
                    )
                })
                .collect();
            bail!("{arg:?} is ambiguous: {}", names.join(", "))
        }
    }
}

/// Write a new job file, refusing to replace one.
fn write_new(repo: &Path, def: &JobDef) -> Result<PathBuf> {
    let path = super::job_path(repo, &def.name);
    if path.exists() {
        bail!(
            "{} already exists; edit it with `nibble cron edit {}`",
            path.display(),
            def.name
        );
    }
    std::fs::create_dir_all(super::jobs_dir(repo))?;
    std::fs::write(&path, super::format_job(def))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// The arguments of `nibble cron add`.
pub(crate) struct AddArgs {
    pub repo: Option<String>,
    pub schedule: Option<String>,
    pub prompt: Option<String>,
    pub file: Option<String>,
    pub label: Option<String>,
    pub expires: Option<String>,
}

/// `nibble cron add`: register a repo so its job files run on this machine,
/// optionally creating a job file first (from flags or another file).
pub(crate) fn cmd_add(store: &CronStore, a: AddArgs) -> Result<()> {
    let now = Utc::now();
    let mut new_def = None;
    let mut repo_arg = a.repo.clone();
    if let Some(file) = &a.file {
        if a.schedule.is_some() || a.prompt.is_some() {
            bail!("use --file or --schedule/--prompt, not both");
        }
        let src = Path::new(file);
        let text = std::fs::read_to_string(src)
            .with_context(|| format!("failed to read {}", src.display()))?;
        let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("job");
        let parsed = super::parse_job(&super::slug(stem), &text)?;
        repo_arg = repo_arg.or(parsed.repo_path);
        new_def = Some(parsed.def);
    } else if a.schedule.is_some() || a.prompt.is_some() {
        let (Some(schedule), Some(prompt)) = (a.schedule.clone(), a.prompt.clone()) else {
            bail!("a new job needs both --schedule and --prompt");
        };
        super::validate_schedule(&schedule)?;
        let label = a.label.clone().unwrap_or_else(|| {
            prompt
                .lines()
                .next()
                .unwrap_or("job")
                .chars()
                .take(40)
                .collect()
        });
        new_def = Some(JobDef {
            name: super::slug(&label),
            label,
            schedule,
            enabled: true,
            skip_if_running: true,
            expires_at: None,
            timeout: super::DEFAULT_TIMEOUT,
            model: None,
            env: vec![],
            notify: super::Notify::Slack,
            read_only: true,
            prompt,
        });
    } else if a.label.is_some() || a.expires.is_some() {
        bail!("--label and --expires only apply when creating a job");
    }
    let repo = canonical_repo(repo_arg.as_deref())?;
    if let Some(mut def) = new_def {
        if let Some(l) = a.label {
            def.name = super::slug(&l);
            def.label = l;
        }
        if let Some(e) = a.expires {
            def.expires_at = Some(super::parse_expiry(&e, now)?);
        }
        let path = write_new(&repo, &def)?;
        println!("Wrote {} (commit it to share it).", path.display());
    }
    if store.add_repo(&repo_str(&repo))? {
        println!(
            "Registered {}: its jobs now run on this machine.",
            repo.display()
        );
    } else {
        println!("{} is already registered.", repo.display());
    }
    cmd_list(store, Some(&repo_str(&repo)))
}

fn when(t: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(t) = t else {
        return "-".to_string();
    };
    let secs = t.signed_duration_since(now).num_seconds();
    let (n, past) = (secs.unsigned_abs(), secs < 0);
    let span = match n {
        0..=59 => format!("{n}s"),
        60..=3599 => format!("{}m", n / 60),
        3600..=86_399 => format!("{}h {}m", n / 3600, (n % 3600) / 60),
        _ => format!("{}d", n / 86_400),
    };
    if past {
        format!("{span} ago")
    } else {
        format!("in {span}")
    }
}

/// `nibble cron list`: every registered repo's jobs, with this machine's
/// state. Broken files are listed with their error.
pub(crate) fn cmd_list(store: &CronStore, repo_filter: Option<&str>) -> Result<()> {
    let filter = repo_filter.map(|r| canonical_repo(Some(r))).transpose()?;
    let repos: Vec<PathBuf> = store
        .repos()?
        .into_iter()
        .map(PathBuf::from)
        .filter(|r| filter.as_ref().is_none_or(|f| f == r))
        .collect();
    if repos.is_empty() {
        println!("No registered repos. Register one with: nibble cron add --repo <path>");
        return Ok(());
    }
    let now = Utc::now();
    println!(
        "{:<32} {:<16} {:<10} {:<12} {:<12} LABEL",
        "JOB", "SCHEDULE", "STATUS", "NEXT", "LAST"
    );
    for repo in repos {
        let short = repo.file_name().unwrap_or_default().to_string_lossy();
        if !repo.is_dir() {
            println!(
                "{short}: repo is missing ({}); `nibble cron forget` it",
                repo.display()
            );
            continue;
        }
        let (jobs, bad) = super::load_repo_jobs(&repo)?;
        let names: Vec<String> = jobs.iter().map(|j| j.name.clone()).collect();
        store.prune_jobs(&repo_str(&repo), &names)?;
        if jobs.is_empty() && bad.is_empty() {
            println!("{short}: no jobs in {}", super::jobs_dir(&repo).display());
        }
        for def in jobs {
            let s = refresh_state(store.job(&repo_str(&repo), &def.name)?, &def, now)?;
            store.put_job(&s)?;
            println!(
                "{:<32} {:<16} {:<10} {:<12} {:<12} {}",
                format!("{short}/{}", def.name),
                def.schedule,
                status(&def, &s, now),
                when(s.next_run, now),
                when(s.last_run, now),
                def.label
            );
        }
        for b in bad {
            println!("{short}: {} is invalid: {}", b.path.display(), b.error);
        }
    }
    Ok(())
}

/// The flags of `nibble cron edit`.
pub(crate) struct EditArgs {
    pub schedule: Option<String>,
    pub prompt: Option<String>,
    pub label: Option<String>,
    pub enable: bool,
    pub disable: bool,
    pub expires: Option<String>,
}

/// Apply `edit` flags to a definition; false when there were none.
fn apply_edit(def: &mut JobDef, e: EditArgs, now: DateTime<Utc>) -> Result<bool> {
    if e.enable && e.disable {
        bail!("use --enable or --disable, not both");
    }
    let mut changed = false;
    if let Some(s) = e.schedule {
        super::validate_schedule(&s)?;
        def.schedule = s;
        changed = true;
    }
    if let Some(p) = e.prompt.filter(|p| !p.trim().is_empty()) {
        def.prompt = p;
        changed = true;
    }
    if let Some(l) = e.label {
        def.label = l;
        changed = true;
    }
    if e.enable || e.disable {
        def.enabled = e.enable;
        changed = true;
    }
    if let Some(x) = e.expires {
        def.expires_at = if x.eq_ignore_ascii_case("none") {
            None
        } else {
            Some(super::parse_expiry(&x, now)?)
        };
        changed = true;
    }
    Ok(changed)
}

/// `nibble cron edit`: change the job file, with flags or in `$EDITOR`.
pub(crate) fn cmd_edit(store: &CronStore, job: &str, e: EditArgs) -> Result<()> {
    let (repo, mut def) = resolve_job(store, job)?;
    let path = super::job_path(&repo, &def.name);
    if apply_edit(&mut def, e, Utc::now())? {
        std::fs::write(&path, super::format_job(&def))
            .with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        let ok = std::process::Command::new("sh")
            .args(["-c", &format!("{editor} \"$1\""), "sh"])
            .arg(&path)
            .status()
            .with_context(|| format!("failed to start {editor}"))?
            .success();
        if !ok {
            bail!("{editor} exited with an error");
        }
        super::load_job_file(&path)
            .with_context(|| format!("{} is now invalid", path.display()))?;
    }
    println!("Updated {} (commit it to share it).", path.display());
    Ok(())
}

/// `nibble cron stop` / `start`: pause or resume a job on this machine only.
pub(crate) fn cmd_pause(store: &CronStore, job: &str, paused: bool) -> Result<()> {
    let (repo, def) = resolve_job(store, job)?;
    let mut s = store.job(&repo_str(&repo), &def.name)?;
    s.paused = paused;
    store.put_job(&s)?;
    let word = if paused { "Paused" } else { "Resumed" };
    println!("{word} {} on this machine.", def.name);
    if !paused && !def.enabled {
        println!("Note: its file has enabled = false, so it still does not run.");
    }
    Ok(())
}

/// `nibble cron kill`: delete the job file and its state.
pub(crate) fn cmd_kill(store: &CronStore, job: &str) -> Result<()> {
    let (repo, def) = resolve_job(store, job)?;
    let path = super::job_path(&repo, &def.name);
    std::fs::remove_file(&path).with_context(|| format!("failed to delete {}", path.display()))?;
    store.delete_job(&repo_str(&repo), &def.name)?;
    println!(
        "Deleted {} (commit the deletion to remove it everywhere).",
        path.display()
    );
    Ok(())
}

/// `nibble cron forget`: stop running a repo's jobs on this machine. Its
/// files are left alone.
pub(crate) fn cmd_forget(store: &CronStore, repo: &str) -> Result<()> {
    // A repo that was deleted cannot be canonicalised: match it as given.
    let key = canonical_repo(Some(repo))
        .map(|r| repo_str(&r))
        .unwrap_or_else(|_| repo.trim_end_matches('/').to_string());
    if store.remove_repo(&key)? {
        println!("Forgot {key}: its jobs no longer run here.");
    } else {
        bail!("{key} is not registered");
    }
    Ok(())
}

/// `nibble cron secret set`: store a value for one repo's jobs. Read
/// without echo from a terminal, or from stdin when piped.
pub(crate) fn cmd_secret_set(name: &str, repo: Option<&str>) -> Result<()> {
    let repo = repo_str(&canonical_repo(repo)?);
    // Reuse the job-file rule, so a name that a file could never ask for
    // is refused here too.
    super::parse_job(
        "check",
        &format!(
            "schedule = \"0 0 * * *\"\nenv = [{}]\n## Prompt\nx",
            toml::Value::from(name)
        ),
    )?;
    let value = crate::rc::prompt_line(&format!("Value for {name}"), true)?;
    if value.is_empty() {
        bail!("empty value; nothing saved");
    }
    let path = secrets::path();
    put_secret(&path, &repo, name, value)?;
    println!("Saved {name} for {repo} in {} (mode 600).", path.display());
    Ok(())
}

fn put_secret(path: &Path, repo: &str, name: &str, value: String) -> Result<()> {
    let mut table = secrets::load_from(path)?;
    table
        .entry(repo.to_string())
        .or_default()
        .insert(name.to_string(), value);
    secrets::save_to(path, &table)
}

fn remove_secret(path: &Path, repo: &str, name: &str) -> Result<()> {
    let mut table = secrets::load_from(path)?;
    let removed = table.get_mut(repo).and_then(|m| m.remove(name)).is_some();
    if !removed {
        bail!("{name} is not set for {repo}");
    }
    if table.get(repo).is_some_and(|m| m.is_empty()) {
        table.remove(repo);
    }
    secrets::save_to(path, &table)
}

/// `nibble cron secret list`: names only, never values.
pub(crate) fn cmd_secret_list(repo: Option<&str>) -> Result<()> {
    let only = repo
        .map(|r| canonical_repo(Some(r)).map(|p| repo_str(&p)))
        .transpose()?;
    let table = secrets::load_from(&secrets::path())?;
    let mut any = false;
    for (r, vars) in table
        .iter()
        .filter(|(r, _)| only.as_ref().is_none_or(|o| o == *r))
    {
        for name in vars.keys() {
            println!("{r}\t{name}");
            any = true;
        }
    }
    if !any {
        println!("No cron secrets.");
    }
    Ok(())
}

/// `nibble cron secret unset`.
pub(crate) fn cmd_secret_unset(name: &str, repo: Option<&str>) -> Result<()> {
    let repo = repo_str(&canonical_repo(repo)?);
    remove_secret(&secrets::path(), &repo, name)?;
    println!("Removed {name} for {repo}.");
    Ok(())
}

/// `nibble cron run`: send the job's prompt to the repo's running sandbox
/// session now.
pub(crate) fn cmd_run(db: &Database, store: &CronStore, job: &str) -> Result<()> {
    let (repo, def) = resolve_job(store, job)?;
    let task = db
        .get_task_by_repo_path(&repo_str(&repo))?
        .filter(|t| {
            let name = t
                .container_name
                .as_deref()
                .or(t.container_id.as_deref())
                .unwrap_or_default();
            crate::sandbox::podman::PodmanSandbox::new().health_check(name)
                == crate::sandbox::SandboxHealth::Healthy
        })
        .with_context(|| {
            format!(
                "no healthy sandbox for {}; start one with: nibble sandbox spawn {}",
                repo.display(),
                repo.display()
            )
        })?;
    println!("Running {} in sandbox {}…", def.name, task.task_id);
    crate::agent_input::inject(&task, &def.prompt)?;
    let mut s = store.job(&repo_str(&repo), &def.name)?;
    s.last_run = Some(Utc::now());
    store.put_job(&s)?;
    println!("Done.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(schedule: &str) -> JobDef {
        super::super::parse_job("j", &format!("schedule = \"{schedule}\"\n## Prompt\nx"))
            .unwrap()
            .def
    }

    #[test]
    fn state_follows_the_file_schedule() {
        let now = Utc::now();
        let d = def("0 9 * * *");
        let s = refresh_state(JobState::new("/r", "j"), &d, now).unwrap();
        let first = s.next_run.unwrap();
        assert!(first > now, "registering never fires at once");
        // Unchanged schedule: kept, even when due.
        let later = refresh_state(s.clone(), &d, first + chrono::Duration::hours(1)).unwrap();
        assert_eq!(later.next_run, Some(first));
        // A new schedule recomputes.
        let changed = refresh_state(s, &def("*/5 * * * *"), now).unwrap();
        assert_ne!(changed.next_run, Some(first));
        assert_eq!(changed.schedule.as_deref(), Some("*/5 * * * *"));
    }

    #[test]
    fn status_precedence() {
        let now = Utc::now();
        let mut d = def("0 9 * * *");
        let mut s = JobState::new("/r", "j");
        s.next_run = Some(now + chrono::Duration::hours(1));
        assert_eq!(status(&d, &s, now), "enabled");
        s.next_run = Some(now);
        assert_eq!(status(&d, &s, now), "due");
        d.expires_at = Some(now);
        assert_eq!(status(&d, &s, now), "expired");
        s.paused = true;
        assert_eq!(status(&d, &s, now), "paused");
        d.enabled = false;
        assert_eq!(status(&d, &s, now), "disabled");
        s.running = true;
        assert_eq!(status(&d, &s, now), "running");
    }

    fn no_flags() -> EditArgs {
        EditArgs {
            schedule: None,
            prompt: None,
            label: None,
            enable: false,
            disable: false,
            expires: None,
        }
    }

    #[test]
    fn edit_flags() {
        let now = Utc::now();
        let mut d = def("0 9 * * *");
        assert!(!apply_edit(&mut d, no_flags(), now).unwrap());
        let e = EditArgs {
            schedule: Some("*/5 * * * *".into()),
            label: Some("New".into()),
            disable: true,
            expires: Some("+1d".into()),
            ..no_flags()
        };
        assert!(apply_edit(&mut d, e, now).unwrap());
        assert_eq!(d.schedule, "*/5 * * * *");
        assert_eq!(d.label, "New");
        assert!(!d.enabled);
        assert!(d.expires_at.is_some());
        let clear = EditArgs {
            expires: Some("none".into()),
            enable: true,
            ..no_flags()
        };
        apply_edit(&mut d, clear, now).unwrap();
        assert!(d.enabled && d.expires_at.is_none());
        let both = EditArgs {
            enable: true,
            disable: true,
            ..no_flags()
        };
        assert!(apply_edit(&mut d, both, now).is_err());
        let bad = EditArgs {
            schedule: Some("nope".into()),
            ..no_flags()
        };
        assert!(apply_edit(&mut d, bad, now).is_err());
    }

    #[test]
    fn when_formats_both_directions() {
        let now = Utc::now();
        assert_eq!(when(None, now), "-");
        assert_eq!(
            when(Some(now + chrono::Duration::seconds(30)), now),
            "in 30s"
        );
        assert_eq!(
            when(Some(now - chrono::Duration::minutes(5)), now),
            "5m ago"
        );
        assert_eq!(when(Some(now + chrono::Duration::days(3)), now), "in 3d");
    }

    /// A registered repo with one job file, in a temp dir.
    fn setup() -> (tempfile::TempDir, CronStore, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = CronStore::open_in(&tmp.path().join("state")).unwrap();
        let repo = std::fs::canonicalize(tmp.path()).unwrap().join("shop");
        std::fs::create_dir_all(super::super::jobs_dir(&repo)).unwrap();
        std::fs::write(
            super::super::job_path(&repo, "logs"),
            "schedule = \"0 9 * * *\"\n## Prompt\nx",
        )
        .unwrap();
        store.add_repo(&repo_str(&repo)).unwrap();
        (tmp, store, repo)
    }

    #[test]
    fn resolves_jobs_by_name_repo_and_path() {
        let (tmp, store, repo) = setup();
        assert_eq!(resolve_job(&store, "logs").unwrap().0, repo);
        assert_eq!(resolve_job(&store, "shop/logs").unwrap().0, repo);
        let path = super::super::job_path(&repo, "logs");
        assert_eq!(resolve_job(&store, path.to_str().unwrap()).unwrap().0, repo);
        assert!(resolve_job(&store, "other/logs").is_err());
        assert!(resolve_job(&store, "nope").is_err());
        // A second repo with the same job name makes the bare name ambiguous.
        let other = std::fs::canonicalize(tmp.path()).unwrap().join("blog");
        std::fs::create_dir_all(super::super::jobs_dir(&other)).unwrap();
        std::fs::copy(&path, super::super::job_path(&other, "logs")).unwrap();
        store.add_repo(&repo_str(&other)).unwrap();
        let err = resolve_job(&store, "logs").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert_eq!(resolve_job(&store, "blog/logs").unwrap().0, other);
        // A markdown file outside .nibble/cron is not a job.
        let stray = tmp.path().join("x.md");
        std::fs::write(&stray, "schedule = \"0 9 * * *\"\n## Prompt\nx").unwrap();
        assert!(resolve_job(&store, stray.to_str().unwrap()).is_err());
    }

    #[test]
    fn add_writes_files_and_registers() {
        let (tmp, store, repo) = setup();
        let r = Some(repo_str(&repo));
        cmd_add(
            &store,
            AddArgs {
                repo: r.clone(),
                schedule: Some("0 8 * * *".into()),
                prompt: Some("Check deps".into()),
                file: None,
                label: Some("Deps Audit".into()),
                expires: Some("+30d".into()),
            },
        )
        .unwrap();
        let d = super::super::load_job_file(&super::super::job_path(&repo, "deps-audit")).unwrap();
        assert_eq!(
            (d.label.as_str(), d.prompt.as_str()),
            ("Deps Audit", "Check deps")
        );
        assert!(d.read_only && d.expires_at.is_some());
        // Same name again: refused, file untouched.
        let again = AddArgs {
            repo: r.clone(),
            schedule: Some("0 1 * * *".into()),
            prompt: Some("other".into()),
            file: None,
            label: Some("Deps Audit".into()),
            expires: None,
        };
        assert!(cmd_add(&store, again).is_err());
        // From a legacy file naming its repo.
        let src = tmp.path().join("Standup.md");
        std::fs::write(
            &src,
            format!(
                "# Standup\nschedule = \"0 9 * * 1-5\"\nrepo_path = \"{}\"\n## Prompt\nGo",
                repo.display()
            ),
        )
        .unwrap();
        cmd_add(
            &store,
            AddArgs {
                repo: None,
                schedule: None,
                prompt: None,
                file: Some(src.to_string_lossy().to_string()),
                label: None,
                expires: None,
            },
        )
        .unwrap();
        assert!(super::super::load_job_file(&super::super::job_path(&repo, "standup")).is_ok());
        // Half a job is refused.
        let half = AddArgs {
            repo: r,
            schedule: Some("0 1 * * *".into()),
            prompt: None,
            file: None,
            label: None,
            expires: None,
        };
        assert!(cmd_add(&store, half).is_err());
    }

    #[test]
    fn secrets_set_and_unset() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("s.toml");
        put_secret(&p, "/r", "TOKEN", "v1".into()).unwrap();
        put_secret(&p, "/r", "TOKEN", "v2".into()).unwrap();
        put_secret(&p, "/o", "TOKEN", "o".into()).unwrap();
        assert_eq!(secrets::load_from(&p).unwrap()["/r"]["TOKEN"], "v2");
        remove_secret(&p, "/r", "TOKEN").unwrap();
        let t = secrets::load_from(&p).unwrap();
        assert!(!t.contains_key("/r"), "empty repo tables are dropped");
        assert_eq!(t["/o"]["TOKEN"], "o");
        assert!(remove_secret(&p, "/r", "TOKEN").is_err());
    }

    #[test]
    fn pause_kill_and_forget() {
        let (_tmp, store, repo) = setup();
        let key = repo_str(&repo);
        cmd_pause(&store, "logs", true).unwrap();
        assert!(store.job(&key, "logs").unwrap().paused);
        cmd_pause(&store, "logs", false).unwrap();
        assert!(!store.job(&key, "logs").unwrap().paused);
        cmd_list(&store, None).unwrap();
        cmd_edit(
            &store,
            "logs",
            EditArgs {
                schedule: Some("0 10 * * *".into()),
                ..no_flags()
            },
        )
        .unwrap();
        assert_eq!(
            resolve_job(&store, "logs").unwrap().1.schedule,
            "0 10 * * *"
        );
        cmd_kill(&store, "logs").unwrap();
        assert!(!super::super::job_path(&repo, "logs").exists());
        cmd_forget(&store, &key).unwrap();
        assert!(store.repos().unwrap().is_empty());
        assert!(cmd_forget(&store, &key).is_err());
    }
}
