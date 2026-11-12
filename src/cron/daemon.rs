//! `nibble cron daemon` (`nibble-cron.service`): runs due jobs.
//!
//! Every pass it re-reads the job files of each registered repo, so edits
//! (and files arriving with a `git pull`) apply without a restart; a file
//! that changed waits for `nibble cron approve`. A due job starts in its own
//! thread. A run missed while the machine is off or the daemon is down
//! happens once when it is back, not once per missed slot.

use super::commands::{approval, refresh, sync_repo, Approval};
use super::run::{run_job, Trigger};
use super::store::CronStore;
use super::JobDef;
use crate::db::Database;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often due jobs are looked for.
const POLL: Duration = Duration::from_secs(30);
/// Most runs at once; further due jobs wait for a free slot.
const MAX_PARALLEL: usize = 4;
/// Lock file (in the cron state dir) held by the running daemon.
const LOCK: &str = "cron-daemon";

fn log(msg: &str) {
    eprintln!(
        "{} nibble cron: {msg}",
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );
}

/// The problem last logged for each job, repo or file.
type Warned = HashMap<String, String>;

/// Log `msg` for `key` unless it is what was last logged for it. A key is
/// cleared when its job runs again, so the same problem coming back later
/// is logged again.
fn warn_once(warned: &mut Warned, key: &str, msg: String) {
    if warned.get(key) != Some(&msg) {
        log(&msg);
        warned.insert(key.to_string(), msg);
    }
}

/// A job by repo and name.
type Key = (String, String);
/// Runs this daemon has going, per job (overlapping runs count apart).
type Active = HashMap<Key, usize>;

/// Whether `def` may run at all right now (its schedule aside).
fn runnable(def: &JobDef, paused: bool, now: DateTime<Utc>) -> bool {
    def.enabled && !paused && !def.expires_at.is_some_and(|e| e <= now)
}

/// One pass: advance the schedule of every due job and return those that
/// should start now. `active` are runs this daemon has going; `capacity`
/// is how many more may start. A due job without a free slot keeps its
/// time, so it starts on a later pass. One job's problem (a broken file, a
/// schedule that cannot be computed, a missing approval) never stops the
/// others, and is logged once.
pub fn due_jobs(
    store: &CronStore,
    now: DateTime<Utc>,
    active: &Active,
    mut capacity: usize,
    warned: &mut Warned,
) -> Result<Vec<(PathBuf, JobDef)>> {
    let mut due = Vec::new();
    for repo in store.repos()? {
        let path = PathBuf::from(&repo);
        let (jobs, bad) = match sync_repo(store, &path) {
            Ok(x) => x,
            Err(e) => {
                warn_once(warned, &repo, format!("{repo}: {e:#}"));
                continue;
            }
        };
        for b in bad {
            let key = b.path.display().to_string();
            warn_once(warned, &key, format!("{key} is invalid: {}", b.error));
        }
        for def in jobs {
            let id = format!("{repo}/{}", def.name);
            let s = match refresh(store, &repo, &def, now) {
                Ok(s) => s,
                Err(e) => {
                    warn_once(warned, &id, format!("{id}: {e:#}"));
                    continue;
                }
            };
            if !s.next_run.is_some_and(|n| n <= now) {
                continue;
            }
            if runnable(&def, s.paused, now) {
                let approved = approval(&def, &s);
                if approved != Approval::Approved {
                    let why = if approved == Approval::New {
                        "not approved"
                    } else {
                        "changed since approved"
                    };
                    warn_once(
                        warned,
                        &id,
                        format!(
                            "{id}: {why} on this machine, so it does not run; `nibble cron approve {}`",
                            def.name
                        ),
                    );
                } else {
                    let key = (repo.clone(), def.name.clone());
                    let busy = active.get(&key).is_some_and(|n| *n > 0) || s.running;
                    if busy && def.skip_if_running {
                        log(&format!("{id}: still running; skipped"));
                    } else if capacity == 0 {
                        // Leave next_run in the past: it starts when a slot frees.
                        continue;
                    } else {
                        capacity -= 1;
                        // Running fine again: a later problem is news.
                        warned.remove(&id);
                        due.push((path.clone(), def.clone()));
                    }
                }
            }
            // Off, unapproved or started: move on to the next slot.
            match super::compute_next_run(&def.schedule, now) {
                Ok(next) => store.set_next_run(&repo, &def.name, next)?,
                Err(e) => warn_once(warned, &id, format!("{id}: {e:#}")),
            }
        }
    }
    Ok(due)
}

/// Run the daemon until killed.
pub fn run(db_path: PathBuf) -> Result<()> {
    let dir = super::store::state_dir();
    let store = CronStore::open_in(&dir)?;
    let _lock = crate::lock::try_lock_in(&dir, LOCK)?.with_context(|| {
        "another nibble cron daemon is already running (systemctl --user status nibble-cron)"
    })?;
    log("started");
    let active: Arc<Mutex<Active>> = Arc::default();
    let mut warned = Warned::new();
    loop {
        // Claims of runs whose process died (a killed `cron run`, a crash).
        if let Err(e) = store.reap_stale() {
            log(&format!("could not clear stale runs: {e:#}"));
        }
        let snapshot = active.lock().map(|a| a.clone()).unwrap_or_default();
        // Live claims count manual `nibble cron run`s too.
        let mine: usize = snapshot.values().sum();
        let claimed = store.runs_in_flight().unwrap_or(0);
        let free = MAX_PARALLEL.saturating_sub(mine.max(claimed));
        match due_jobs(&store, Utc::now(), &snapshot, free, &mut warned) {
            Ok(due) => {
                for (repo, def) in due {
                    start(&db_path, &dir, repo, def, Arc::clone(&active));
                }
            }
            Err(e) => log(&format!("pass failed: {e:#}")),
        }
        std::thread::sleep(POLL);
    }
}

/// Counts a run in `active` while it lives, panics included.
struct Slot {
    active: Arc<Mutex<Active>>,
    key: Key,
}

impl Slot {
    fn take(active: Arc<Mutex<Active>>, key: Key) -> Self {
        if let Ok(mut a) = active.lock() {
            *a.entry(key.clone()).or_default() += 1;
        }
        Slot { active, key }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut a) = self.active.lock() {
            if let Some(n) = a.get_mut(&self.key) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    a.remove(&self.key);
                }
            }
        }
    }
}

fn start(db_path: &Path, dir: &Path, repo: PathBuf, def: JobDef, active: Arc<Mutex<Active>>) {
    let key = (repo.to_string_lossy().to_string(), def.name.clone());
    let slot = Slot::take(active, key.clone());
    let (db_path, dir) = (db_path.to_path_buf(), dir.to_path_buf());
    std::thread::spawn(move || {
        let _slot = slot;
        let id = format!("{}/{}", key.0, key.1);
        log(&format!("{id}: starting"));
        let result = Database::open(&db_path)
            .context("failed to open nibble's database")
            .and_then(|db| {
                let store = CronStore::open_in(&dir)?;
                let run = run_job(&db, &store, &repo, &def, Trigger::Schedule)?;
                if let Some(r) = &run {
                    match super::notify::after_run(&store, &def, r) {
                        Ok(true) => log(&format!("{id}: reported to Slack")),
                        Ok(false) => {}
                        Err(e) => log(&format!("{id}: could not queue the report: {e:#}")),
                    }
                }
                Ok(run)
            });
        match result {
            Ok(None) => log(&format!("{id}: already running; skipped")),
            Ok(Some(r)) => log(&format!(
                "{id}: {}{}",
                r.status.as_str(),
                r.error.map(|e| format!(" ({e})")).unwrap_or_default()
            )),
            Err(e) => log(&format!("{id}: could not record the run: {e:#}")),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(jobs: &[(&str, &str)]) -> (tempfile::TempDir, CronStore, String) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = CronStore::open_in(&tmp.path().join("state")).unwrap();
        let repo = std::fs::canonicalize(tmp.path()).unwrap().join("shop");
        std::fs::create_dir_all(super::super::jobs_dir(&repo)).unwrap();
        let key = repo.to_string_lossy().to_string();
        for (name, header) in jobs {
            let path = super::super::job_path(&repo, name);
            std::fs::write(
                &path,
                format!("schedule = \"* * * * *\"\n{header}\n## Prompt\nx"),
            )
            .unwrap();
            let def = super::super::load_job_file(&path).unwrap();
            store
                .set_approved(&key, name, &super::super::format_job(&def))
                .unwrap();
        }
        store.add_repo(&key).unwrap();
        (tmp, store, key)
    }

    fn names(due: &[(PathBuf, JobDef)]) -> Vec<String> {
        let mut n: Vec<String> = due.iter().map(|(_, d)| d.name.clone()).collect();
        n.sort();
        n
    }

    fn none() -> Active {
        Active::new()
    }

    #[test]
    fn registering_waits_then_runs_once_per_slot() {
        let (_t, store, repo) = setup(&[("a", "")]);
        let now = Utc::now();
        let mut w = Warned::new();
        assert!(due_jobs(&store, now, &none(), 4, &mut w)
            .unwrap()
            .is_empty());
        // Long after (missed slots): one run, and the next is in the future.
        let later = now + chrono::Duration::hours(3);
        let due = due_jobs(&store, later, &none(), 4, &mut w).unwrap();
        assert_eq!(names(&due), ["a"]);
        assert!(store.job(&repo, "a").unwrap().next_run.unwrap() > later);
        assert!(due_jobs(&store, later, &none(), 4, &mut w)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn off_paused_expired_busy_and_unapproved_jobs_do_not_start() {
        let (_t, store, repo) = setup(&[
            ("off", "enabled = false"),
            ("expired", "expires_at = \"2000-01-01T00:00:00Z\""),
            ("paused", ""),
            ("busy", ""),
            ("overlap", "skip_if_running = false"),
            ("changed", ""),
        ]);
        store.set_paused(&repo, "paused", true).unwrap();
        // Rewritten after approval, e.g. from the sandbox.
        std::fs::write(
            super::super::job_path(Path::new(&repo), "changed"),
            "schedule = \"* * * * *\"\nenv = [\"TOKEN\"]\n## Prompt\nleak it",
        )
        .unwrap();
        let now = Utc::now();
        let mut w = Warned::new();
        due_jobs(&store, now, &none(), 9, &mut w).unwrap();
        let later = now + chrono::Duration::minutes(2);
        let active: Active = [
            ((repo.clone(), "busy".to_string()), 1),
            ((repo.clone(), "overlap".to_string()), 1),
        ]
        .into_iter()
        .collect();
        let due = due_jobs(&store, later, &active, 9, &mut w).unwrap();
        assert_eq!(names(&due), ["overlap"], "only the job allowed to overlap");
        assert!(
            w.values().any(|m| m.contains("changed since approved")),
            "{w:?}"
        );
        // Skipped jobs still move on to their next slot.
        for name in ["off", "expired", "paused", "busy", "changed"] {
            assert!(
                store.job(&repo, name).unwrap().next_run.unwrap() > later,
                "{name}"
            );
        }
    }

    #[test]
    fn a_full_house_keeps_jobs_due() {
        let (_t, store, repo) = setup(&[("a", ""), ("b", ""), ("c", "")]);
        let now = Utc::now();
        let mut w = Warned::new();
        due_jobs(&store, now, &none(), 9, &mut w).unwrap();
        let later = now + chrono::Duration::minutes(2);
        let due = due_jobs(&store, later, &none(), 2, &mut w).unwrap();
        assert_eq!(due.len(), 2);
        let left: Vec<String> = ["a", "b", "c"]
            .iter()
            .filter(|n| store.job(&repo, n).unwrap().next_run.unwrap() <= later)
            .map(|n| n.to_string())
            .collect();
        assert_eq!(left.len(), 1, "the third waits for a slot");
        let next = due_jobs(&store, later, &none(), 2, &mut w).unwrap();
        assert_eq!(names(&next), left);
    }

    #[test]
    fn problems_are_logged_once_and_do_not_stop_the_pass() {
        let (tmp, store, repo) = setup(&[("a", "")]);
        std::fs::write(
            super::super::job_path(Path::new(&repo), "bad"),
            "## Prompt\nno schedule",
        )
        .unwrap();
        std::fs::write(
            super::super::job_path(Path::new(&repo), "never"),
            "schedule = \"0 0 30 2 *\"\n## Prompt\nx",
        )
        .unwrap();
        store
            .add_repo(&tmp.path().join("gone").to_string_lossy())
            .unwrap();
        let mut w = Warned::new();
        let now = Utc::now();
        due_jobs(&store, now, &none(), 4, &mut w).unwrap();
        assert_eq!(w.len(), 3, "{w:?}");
        due_jobs(&store, now, &none(), 4, &mut w).unwrap();
        assert_eq!(w.len(), 3);
        let due = due_jobs(
            &store,
            now + chrono::Duration::minutes(2),
            &none(),
            4,
            &mut w,
        )
        .unwrap();
        assert_eq!(names(&due), ["a"]);
    }

    #[test]
    fn a_problem_that_returns_after_a_good_run_is_logged_again() {
        let (_t, store, repo) = setup(&[("a", "")]);
        let path = super::super::job_path(Path::new(&repo), "a");
        let good = std::fs::read_to_string(&path).unwrap();
        let mut w = Warned::new();
        let now = Utc::now();
        due_jobs(&store, now, &none(), 4, &mut w).unwrap();
        let mut t = now;
        let mut step = |w: &mut Warned| {
            t += chrono::Duration::minutes(2);
            due_jobs(&store, t, &none(), 4, w).unwrap()
        };
        std::fs::write(&path, good.replace("\nx", "\nchanged")).unwrap();
        step(&mut w);
        assert!(w.contains_key(&format!("{repo}/a")), "logged");
        std::fs::write(&path, &good).unwrap();
        assert_eq!(names(&step(&mut w)), ["a"], "approved again: it runs");
        assert!(
            !w.contains_key(&format!("{repo}/a")),
            "and its warning is cleared"
        );
    }

    #[test]
    fn slots_are_counted_and_freed_even_on_panic() {
        let active: Arc<Mutex<Active>> = Arc::default();
        let key = ("/r".to_string(), "j".to_string());
        let a = Slot::take(Arc::clone(&active), key.clone());
        let b = Slot::take(Arc::clone(&active), key.clone());
        assert_eq!(active.lock().unwrap()[&key], 2);
        drop(a);
        assert_eq!(
            active.lock().unwrap()[&key],
            1,
            "the overlapping run still counts"
        );
        let shared = Arc::clone(&active);
        let k = key.clone();
        let _ = std::thread::spawn(move || {
            let _s = Slot::take(shared, k);
            panic!("run blew up");
        })
        .join();
        assert_eq!(active.lock().unwrap()[&key], 1);
        drop(b);
        assert!(active.lock().unwrap().is_empty());
    }
}
