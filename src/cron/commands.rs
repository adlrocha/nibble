//! `nibble cron` command handlers.

use super::store::{CronStore, JobState};
use super::{secrets, JobDef};
use crate::db::Database;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

/// Open this machine's cron state. Reminds you on each command while an
/// older nibble's jobs are waiting for `nibble cron import`.
pub(crate) fn open(db: &Database) -> Result<CronStore> {
    let store = CronStore::open()?;
    if super::store::legacy_pending(&store, db)? {
        eprintln!(
            "Older cron jobs are still in ~/.nibble/tasks.db; review and move them into their repos with `nibble cron import`."
        );
    }
    Ok(store)
}

/// `nibble cron import`: show what would happen to older nibble's jobs,
/// and do it once confirmed.
pub(crate) fn cmd_import(db: &Database, store: &CronStore, yes: bool) -> Result<()> {
    use super::store::LegacyItem;
    let plan = super::store::plan_legacy(store, db)?;
    if plan.is_empty() {
        println!("No older cron jobs to import.");
        return super::store::mark_legacy_done(store);
    }
    for item in &plan {
        match item {
            LegacyItem::Import {
                repo, path, def, ..
            } => {
                // In full: importing approves it, so you see what you approve.
                println!(
                    "\n── import → {} (registers {}){}\n{}",
                    path.display(),
                    repo.display(),
                    if def.enabled { "" } else { " [disabled]" },
                    super::format_job(def)
                );
                println!("It may edit files (read_only = false), as older jobs could.");
            }
            LegacyItem::Skip {
                repo_path,
                label,
                reason,
            } => println!("  skip    {label} in {repo_path}: {reason}"),
        }
    }
    println!("These come from ~/.nibble/tasks.db, which sandboxes can write: check each one.");
    if !yes && !confirm("Import them?")? {
        println!("Nothing imported. Run `nibble cron import` again when ready.");
        return Ok(());
    }
    super::store::apply_legacy(store, &plan)?;
    println!("Imported. Commit the new files to keep them; the old rows stay in tasks.db.");
    Ok(())
}

/// Ask y/N on the terminal; no terminal counts as no.
fn confirm(question: &str) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

use crate::format::expand_tilde;

/// `~` expanded, canonical, and allowed to hold jobs (see
/// [`super::validate_repo`]). With no argument: the git repo around the
/// current directory.
pub(crate) fn canonical_repo(arg: Option<&str>) -> Result<PathBuf> {
    let raw = match arg {
        Some(a) => expand_tilde(a)?,
        None => {
            let cwd = std::env::current_dir().context("no current directory")?;
            cwd.ancestors()
                .find(|d| d.join(".git").exists())
                .map(Path::to_path_buf)
                .context("not inside a git repository; pass --repo <path>")?
        }
    };
    super::validate_repo(&raw, false)
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

/// Read a registered repo's job files and drop the state of jobs whose
/// files are gone. An invalid file, and a paused or approved job, keep
/// their state (see [`CronStore::prune_jobs`]).
pub(crate) fn sync_repo(
    store: &CronStore,
    repo: &Path,
) -> Result<(Vec<JobDef>, Vec<super::BadFile>)> {
    if !repo.is_dir() {
        bail!(
            "repo is missing; `nibble cron forget {}` it",
            repo.display()
        );
    }
    let (jobs, bad) = super::load_repo_jobs(repo)?;
    let names: Vec<String> = jobs
        .iter()
        .map(|j| j.name.clone())
        .chain(
            bad.iter()
                .filter_map(|b| b.path.file_stem()?.to_str().map(str::to_string)),
        )
        .collect();
    store.prune_jobs(&repo_str(repo), &names)?;
    Ok((jobs, bad))
}

/// [`refresh_state`], saved.
pub(crate) fn refresh(
    store: &CronStore,
    repo: &str,
    def: &JobDef,
    now: DateTime<Utc>,
) -> Result<JobState> {
    let old = store.job(repo, &def.name)?;
    let s = refresh_state(old.clone(), def, now)?;
    if let (true, Some(next)) = (s.next_run != old.next_run, s.next_run) {
        store.set_schedule(repo, &def.name, &def.schedule, next)?;
    }
    Ok(s)
}

/// Whether the file's definition is the one you approved here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Approval {
    Approved,
    /// Never approved on this machine.
    New,
    /// Approved once, but the file changed since.
    Changed,
}

pub(crate) fn approval(def: &JobDef, s: &JobState) -> Approval {
    match &s.approved {
        None => Approval::New,
        Some(text) if *text == super::format_job(def) => Approval::Approved,
        Some(_) => Approval::Changed,
    }
}

/// What `list` shows for a job.
pub(crate) fn status(def: &JobDef, s: &JobState, now: DateTime<Utc>) -> &'static str {
    if s.running {
        "running"
    } else if !def.enabled {
        "disabled"
    } else if let a @ (Approval::New | Approval::Changed) = approval(def, s) {
        if a == Approval::New {
            "unapproved"
        } else {
            "changed"
        }
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
    let (repo, name) = resolve_job_file(store, arg)?;
    let def = super::load_job_file(&super::job_path(&repo, &name))?;
    Ok((repo, def))
}

/// [`resolve_job`] without parsing the file, for commands that must work
/// on a file that does not parse (`kill`). Returns the repo and job name.
pub(crate) fn resolve_job_file(store: &CronStore, arg: &str) -> Result<(PathBuf, String)> {
    let as_path = Path::new(arg);
    if arg.ends_with(".md") && as_path.is_file() {
        let path = std::fs::canonicalize(as_path)?;
        let dir = path.parent().context("job file has no directory")?;
        if !dir.ends_with(super::JOBS_DIR) {
            bail!("{arg} is not in a repo's {}", super::JOBS_DIR);
        }
        // `.nibble/cron` is two levels.
        let repo = dir
            .parent()
            .and_then(Path::parent)
            .context("job file is not under a repo")?
            .to_path_buf();
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("job file name is not UTF-8")?
            .to_string();
        return Ok((repo, name));
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
        [repo] => Ok((repo.clone(), name.to_string())),
        [] => bail!("no job {arg:?} in any registered repo (see `nibble cron list`)"),
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|r| super::job_path(r, name).display().to_string())
                .collect();
            bail!("{arg:?} is ambiguous: {}", names.join(", "))
        }
    }
}

/// Write a new job file, refusing to replace one.
fn write_new(repo: &Path, def: &JobDef) -> Result<PathBuf> {
    let path = super::job_path(repo, &def.name);
    // A dangling symlink "does not exist", yet writing would follow it.
    if !super::nothing_at(&path) {
        bail!(
            "{} already exists; edit it with `nibble cron edit {}`",
            path.display(),
            def.name
        );
    }
    let text = super::format_job(def);
    // Never write a file that would not load.
    super::parse_job(&def.name, &text).context("the new job is invalid")?;
    super::real_jobs_dir(repo)?;
    std::fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))?;
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
    /// Approve the repo's job files without asking.
    pub yes: bool,
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
        // `--label`, when given, replaces this below.
        let label: String = prompt
            .lines()
            .next()
            .unwrap_or("job")
            .chars()
            .take(40)
            .collect();
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
    let key = repo_str(&repo);
    // Registered first, so a refused write still leaves the repo usable.
    let added = store.add_repo(&key)?;
    if let Some(mut def) = new_def {
        if let Some(l) = a.label {
            super::validate_label(&l)?;
            def.name = super::slug(&l);
            def.label = l;
        }
        if let Some(e) = a.expires {
            def.expires_at = Some(super::parse_expiry(&e, now)?);
        }
        let path = write_new(&repo, &def)?;
        // You wrote it here: approved, as read back from the file.
        let written = super::load_job_file(&path)?;
        store.set_approved(&key, &written.name, &super::format_job(&written))?;
        println!("Wrote {} (commit it to share it).", path.display());
    }
    if added {
        println!("Registered {}.", repo.display());
    } else {
        println!("{} is already registered.", repo.display());
    }
    approve_pending(store, &repo, None, a.yes)?;
    cmd_list(store, Some(&key))
}

/// Refuse to run a definition you have not approved here.
pub(crate) fn require_approved(store: &CronStore, repo: &Path, def: &JobDef) -> Result<()> {
    match approval(def, &store.job(&repo_str(repo), &def.name)?) {
        Approval::Approved => Ok(()),
        a => bail!(
            "{} {} on this machine; review it with `nibble cron approve {}`",
            def.name,
            if a == Approval::New {
                "is not approved"
            } else {
                "has changed since you approved it"
            },
            def.name
        ),
    }
}

/// Show each job of `repo` that is not approved (or only `only`), and
/// approve it on a yes. Returns how many were approved.
fn approve_pending(store: &CronStore, repo: &Path, only: Option<&str>, yes: bool) -> Result<usize> {
    let key = repo_str(repo);
    let (jobs, _) = super::load_repo_jobs(repo)?;
    let mut approved = 0;
    for def in jobs.iter().filter(|d| only.is_none_or(|o| o == d.name)) {
        let state = store.job(&key, &def.name)?;
        let a = approval(def, &state);
        if a == Approval::Approved {
            continue;
        }
        let text = super::format_job(def);
        println!(
            "\n── {} ({}) ──\n{text}",
            super::job_path(repo, &def.name).display(),
            if a == Approval::New { "new" } else { "changed" }
        );
        if !def.env.is_empty() {
            println!("It gets these secrets: {}", def.env.join(", "));
        }
        if !def.read_only {
            println!("It may edit files (read_only = false).");
        }
        if yes || confirm("Run this job on this machine?")? {
            store.set_approved(&key, &def.name, &text)?;
            approved += 1;
        } else {
            println!(
                "Not approved; it will not run. Approve later with `nibble cron approve {}`.",
                def.name
            );
        }
    }
    Ok(approved)
}

/// `nibble cron approve`: review and approve a job (or every pending job
/// of a repo) to run on this machine.
pub(crate) fn cmd_approve(
    store: &CronStore,
    job: Option<&str>,
    repo: Option<&str>,
    yes: bool,
) -> Result<()> {
    let (repo, only) = match job {
        Some(j) => {
            let (r, d) = resolve_job(store, j)?;
            (r, Some(d.name))
        }
        None => (canonical_repo(repo)?, None),
    };
    if !store.repos()?.contains(&repo_str(&repo)) {
        bail!(
            "{} is not registered; use `nibble cron add`",
            repo.display()
        );
    }
    let n = approve_pending(store, &repo, only.as_deref(), yes)?;
    println!("{n} job(s) approved.");
    Ok(())
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
        match filter {
            Some(f) => println!(
                "{} is not registered. Register it with: nibble cron add --repo {}",
                f.display(),
                f.display()
            ),
            None => {
                println!("No registered repos. Register one with: nibble cron add --repo <path>")
            }
        }
        return Ok(());
    }
    let now = Utc::now();
    println!(
        "{:<32} {:<16} {:<10} {:<12} {:<12} LABEL",
        "JOB", "SCHEDULE", "STATUS", "NEXT", "LAST"
    );
    for repo in repos {
        let short = repo.file_name().unwrap_or_default().to_string_lossy();
        let (jobs, bad) = match sync_repo(store, &repo) {
            Ok(x) => x,
            Err(e) => {
                println!("{short}: {e:#}");
                continue;
            }
        };
        if jobs.is_empty() && bad.is_empty() {
            println!("{short}: no jobs in {}", super::jobs_dir(&repo).display());
        }
        for def in jobs {
            let s = refresh(store, &repo_str(&repo), &def, now)?;
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
    if let Some(p) = e.prompt {
        if p.trim().is_empty() {
            bail!("--prompt cannot be empty");
        }
        def.prompt = p;
        changed = true;
    }
    if let Some(l) = e.label {
        super::validate_label(&l)?;
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
    super::real_jobs_dir(&repo)?;
    let path = super::job_path(&repo, &def.name);
    // Only a job you had approved stays approved: editing one that changed
    // under you (from the sandbox, say) must not approve what you never saw.
    let was_approved =
        approval(&def, &store.job(&repo_str(&repo), &def.name)?) == Approval::Approved;
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
    if was_approved {
        let def = super::load_job_file(&path)?;
        store.set_approved(&repo_str(&repo), &def.name, &super::format_job(&def))?;
    } else {
        println!(
            "It was not approved before the edit; review it with `nibble cron approve {}`.",
            def.name
        );
    }
    Ok(())
}

/// `nibble cron stop` / `start`: pause or resume a job on this machine only.
pub(crate) fn cmd_pause(store: &CronStore, job: &str, paused: bool) -> Result<()> {
    let (repo, def) = resolve_job(store, job)?;
    store.set_paused(&repo_str(&repo), &def.name, paused)?;
    let word = if paused { "Paused" } else { "Resumed" };
    println!("{word} {} on this machine.", def.name);
    if !paused && !def.enabled {
        println!("Note: its file has enabled = false, so it still does not run.");
    }
    Ok(())
}

/// `nibble cron kill`: delete the job file and its state.
pub(crate) fn cmd_kill(store: &CronStore, job: &str) -> Result<()> {
    // Not parsed: a broken file is the one most worth deleting.
    let (repo, name) = resolve_job_file(store, job)?;
    super::real_jobs_dir(&repo)?;
    let path = super::job_path(&repo, &name);
    std::fs::remove_file(&path).with_context(|| format!("failed to delete {}", path.display()))?;
    store.delete_job(&repo_str(&repo), &name)?;
    println!(
        "Deleted {} (commit the deletion to remove it everywhere).",
        path.display()
    );
    Ok(())
}

/// The key a repo is stored under. A repo that was deleted cannot be
/// canonicalised, yet what is stored for it must stay removable, so it is
/// then matched as given (`~` expanded).
fn repo_key(arg: &str) -> String {
    canonical_repo(Some(arg))
        .map(|r| repo_str(&r))
        .unwrap_or_else(|_| {
            expand_tilde(arg)
                .map(|p| repo_str(&p))
                .unwrap_or_else(|_| arg.to_string())
                .trim_end_matches('/')
                .to_string()
        })
}

/// `nibble cron forget`: stop running a repo's jobs on this machine. Its
/// files are left alone.
pub(crate) fn cmd_forget(store: &CronStore, repo: &str) -> Result<()> {
    let key = repo_key(repo);
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
    super::validate_env_name(name)?;
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
    let repo = match repo {
        Some(given) => repo_key(given),
        None => repo_str(&canonical_repo(None)?),
    };
    remove_secret(&secrets::path(), &repo, name)?;
    println!("Removed {name} for {repo}.");
    Ok(())
}

/// `nibble cron run`: run a job now, in the foreground, and print its
/// report.
pub(crate) fn cmd_run(db: &Database, store: &CronStore, job: &str) -> Result<()> {
    let (repo, def) = resolve_job(store, job)?;
    require_approved(store, &repo, &def)?;
    println!(
        "Running {} in {}'s sandbox (timeout {}s)…",
        def.name,
        repo.display(),
        def.timeout.as_secs()
    );
    let record = super::run::run_job(db, store, &repo, &def, super::run::Trigger::Manual)?
        .with_context(|| format!("{} is already running", def.name))?;
    print_record(&record);
    // The run's own failure is this command's failure, for scripts.
    match record.status {
        super::store::RunStatus::Failed | super::store::RunStatus::TimedOut => {
            bail!("run {} {}", record.id, record.status.as_str())
        }
        _ => Ok(()),
    }
}

/// `nibble cron runs`: recent runs, newest first.
pub(crate) fn cmd_runs(store: &CronStore, job: Option<&str>, limit: usize) -> Result<()> {
    // Not parsed: a broken file must not hide its own history.
    let resolved = job.map(|j| resolve_job_file(store, j)).transpose()?;
    let key = resolved.as_ref().map(|(r, n)| (repo_str(r), n.clone()));
    let runs = store.runs(key.as_ref().map(|(r, n)| (r.as_str(), n.as_str())), limit)?;
    if runs.is_empty() {
        println!("No runs yet.");
        return Ok(());
    }
    let now = Utc::now();
    println!(
        "{:<6} {:<32} {:<9} {:<9} {:<10} SUMMARY",
        "RUN", "JOB", "STATUS", "TRIGGER", "STARTED"
    );
    for r in runs {
        let short = Path::new(&r.repo_path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let text = r.summary.or(r.error).unwrap_or_default();
        println!(
            "{:<6} {:<32} {:<9} {:<9} {:<10} {}",
            r.id,
            format!("{short}/{}", r.name),
            r.status.as_str(),
            r.trigger,
            when(Some(r.started_at), now),
            text.lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect::<String>()
        );
    }
    Ok(())
}

/// `nibble cron show`: one run's full report.
pub(crate) fn cmd_show(store: &CronStore, id: i64) -> Result<()> {
    let r = store.run(id)?.with_context(|| format!("no run {id}"))?;
    print_record(&r);
    Ok(())
}

fn print_record(r: &super::store::RunRecord) {
    println!(
        "Run {} of {} ({}): {}",
        r.id,
        r.label,
        r.name,
        r.status.as_str()
    );
    println!("  repo:    {}", r.repo_path);
    println!(
        "  started: {} ({})",
        r.started_at.format("%Y-%m-%d %H:%M:%S UTC"),
        r.trigger
    );
    if let Some(f) = r.finished_at {
        println!("  took:    {}s", (f - r.started_at).num_seconds());
    }
    if let Some(c) = r.cost_usd {
        println!("  cost:    ${c:.2}");
    }
    if let Some(sid) = &r.session_id {
        println!("  session: {sid} (resume it inside the sandbox with `claude --resume {sid}`)");
    }
    if let Some(e) = &r.error {
        println!("  error:   {e}");
    }
    if let Some(rep) = &r.report {
        println!("\n{}", rep.summary);
        for f in &rep.findings {
            println!(
                "\n[{}] {}  ({})",
                f.severity.as_str(),
                f.title,
                f.fingerprint
            );
            if !f.details.is_empty() {
                println!("{}", f.details);
            }
            if !f.suggested_action.is_empty() {
                println!("→ {}", f.suggested_action);
            }
        }
    }
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
        s.approved = Some(super::super::format_job(&d));
        s.next_run = Some(now + chrono::Duration::hours(1));
        assert_eq!(status(&d, &s, now), "enabled");
        s.next_run = Some(now);
        assert_eq!(status(&d, &s, now), "due");
        d.expires_at = Some(now);
        assert_eq!(
            status(&d, &s, now),
            "changed",
            "the file differs from what was approved"
        );
        s.approved = Some(super::super::format_job(&d));
        assert_eq!(status(&d, &s, now), "expired");
        s.paused = true;
        assert_eq!(status(&d, &s, now), "paused");
        s.approved = None;
        assert_eq!(status(&d, &s, now), "unapproved");
        d.enabled = false;
        assert_eq!(status(&d, &s, now), "disabled");
        s.running = true;
        assert_eq!(status(&d, &s, now), "running");
    }

    #[test]
    fn approval_follows_the_definition() {
        let (_tmp, store, repo) = setup();
        let key = repo_str(&repo);
        let (_, d) = resolve_job(&store, "logs").unwrap();
        assert!(
            require_approved(&store, &repo, &d).is_err(),
            "a file found in the repo waits"
        );
        // No terminal and no --yes: nothing is approved.
        assert_eq!(approve_pending(&store, &repo, None, false).unwrap(), 0);
        cmd_approve(&store, Some("logs"), None, true).unwrap();
        assert!(require_approved(&store, &repo, &d).is_ok());
        // A cosmetic change (a comment) keeps the approval; a real one does not.
        let path = super::super::job_path(&repo, "logs");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("## Prompt", "# a comment\n## Prompt")).unwrap();
        let (_, d) = resolve_job(&store, "logs").unwrap();
        assert!(require_approved(&store, &repo, &d).is_ok());
        std::fs::write(
            &path,
            "schedule = \"* * * * *\"\nenv = [\"TOKEN\"]\n## Prompt\nexfiltrate",
        )
        .unwrap();
        let (_, d) = resolve_job(&store, "logs").unwrap();
        let err = require_approved(&store, &repo, &d).unwrap_err().to_string();
        assert!(err.contains("changed since"), "{err}");
        // Editing a changed job does not approve what you never saw...
        let flags = || EditArgs {
            schedule: Some("0 7 * * *".into()),
            ..no_flags()
        };
        cmd_edit(&store, "logs", flags()).unwrap();
        let (_, d) = resolve_job(&store, "logs").unwrap();
        assert!(require_approved(&store, &repo, &d).is_err());
        // ...while editing an approved one keeps it approved, being yours.
        cmd_approve(&store, Some("logs"), None, true).unwrap();
        cmd_edit(
            &store,
            "logs",
            EditArgs {
                schedule: Some("0 8 * * *".into()),
                ..no_flags()
            },
        )
        .unwrap();
        let (_, d) = resolve_job(&store, "logs").unwrap();
        assert!(require_approved(&store, &repo, &d).is_ok());
        assert!(store.job(&key, "logs").unwrap().approved.is_some());
        // Approving needs a registered repo.
        cmd_forget(&store, &key).unwrap();
        assert!(cmd_approve(&store, None, Some(&key), true).is_err());
    }

    #[test]
    fn writes_refuse_a_symlinked_job_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = CronStore::open_in(&tmp.path().join("state")).unwrap();
        let repo = std::fs::canonicalize(tmp.path()).unwrap().join("shop");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, repo.join(".nibble")).unwrap();
        let a = AddArgs {
            repo: Some(repo_str(&repo)),
            schedule: Some("0 1 * * *".into()),
            prompt: Some("p".into()),
            file: None,
            label: Some("Job".into()),
            expires: None,
            yes: false,
        };
        let err = cmd_add(&store, a).unwrap_err().to_string();
        assert!(err.contains("outside the repo"), "{err}");
        assert!(!elsewhere.join("cron/job.md").exists());
    }

    #[test]
    fn writes_refuse_a_dangling_symlinked_file() {
        let (tmp, store, repo) = setup();
        let target = tmp.path().join("outside.md");
        std::os::unix::fs::symlink(&target, super::super::job_path(&repo, "job")).unwrap();
        let a = AddArgs {
            repo: Some(repo_str(&repo)),
            schedule: Some("0 1 * * *".into()),
            prompt: Some("p".into()),
            file: None,
            label: Some("Job".into()),
            expires: None,
            yes: false,
        };
        assert!(cmd_add(&store, a)
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        assert!(!target.exists(), "nothing written through the link");
    }

    #[test]
    fn kill_deletes_a_broken_file() {
        let (_tmp, store, repo) = setup();
        std::fs::write(super::super::job_path(&repo, "logs"), "## Prompt\nbroken").unwrap();
        assert!(resolve_job(&store, "logs").is_err());
        cmd_kill(&store, "logs").unwrap();
        assert!(!super::super::job_path(&repo, "logs").exists());
    }

    #[test]
    fn secrets_of_a_deleted_repo_can_be_unset() {
        // Only the key matching: the real secrets file is not touched,
        // since the value was never there.
        let err = cmd_secret_unset("TOKEN", Some("/no/such/repo"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not set for /no/such/repo"), "{err}");
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
                yes: false,
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
            yes: false,
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
                yes: false,
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
            yes: false,
        };
        assert!(cmd_add(&store, half).is_err());
    }

    #[test]
    fn edge_cases_are_refused() {
        let (tmp, store, repo) = setup();
        assert!(canonical_repo(Some("~")).is_err(), "home is refused");
        assert!(canonical_repo(Some(&tmp.path().join("nope").to_string_lossy())).is_err());
        let bad_label = AddArgs {
            repo: Some(repo_str(&repo)),
            schedule: Some("0 1 * * *".into()),
            prompt: Some("p".into()),
            file: None,
            label: Some("a\nread_only = false".into()),
            expires: None,
            yes: false,
        };
        assert!(cmd_add(&store, bad_label).is_err());
        let empty_prompt = AddArgs {
            repo: Some(repo_str(&repo)),
            schedule: Some("0 1 * * *".into()),
            prompt: Some("  ".into()),
            file: None,
            label: Some("Empty".into()),
            expires: None,
            yes: false,
        };
        assert!(cmd_add(&store, empty_prompt).is_err());
        assert!(
            !super::super::job_path(&repo, "empty").exists(),
            "nothing written"
        );
        let e = EditArgs {
            prompt: Some(String::new()),
            ..no_flags()
        };
        assert!(cmd_edit(&store, "logs", e).is_err());
        // Listing an unregistered repo, or a repo that is gone, does not fail.
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        cmd_list(&store, Some(&other.to_string_lossy())).unwrap();
        let gone = tmp.path().join("gone");
        store.add_repo(&gone.to_string_lossy()).unwrap();
        cmd_list(&store, None).unwrap();
        cmd_forget(&store, &gone.to_string_lossy()).unwrap();
    }

    #[test]
    fn listing_keeps_state_of_invalid_files() {
        let (_tmp, store, repo) = setup();
        let key = repo_str(&repo);
        cmd_pause(&store, "logs", true).unwrap();
        let mut other = store.job(&key, "other").unwrap();
        other.last_run = Some(Utc::now());
        store.put_job(&other).unwrap();
        // A broken edit must not reset the job's state.
        std::fs::write(super::super::job_path(&repo, "logs"), "## Prompt\nbroken").unwrap();
        std::fs::write(super::super::job_path(&repo, "other"), "## Prompt\nbroken").unwrap();
        cmd_list(&store, None).unwrap();
        assert!(store.job(&key, "logs").unwrap().paused);
        assert!(store.job(&key, "other").unwrap().last_run.is_some());
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
