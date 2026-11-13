//! Moving the jobs an older nibble kept in `~/.nibble/tasks.db` into job
//! files (`nibble cron import`).

use super::store::CronStore;
use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};

/// Set once the jobs an older nibble kept in `~/.nibble/tasks.db` were
/// written out as files.
const LEGACY_EXPORTED: &str = "legacy_exported";

/// What `nibble cron import` would do with one job an older nibble kept
/// in `~/.nibble/tasks.db`.
#[derive(Debug, PartialEq)]
pub enum LegacyItem {
    /// Write `def` to `path` and register `repo`.
    Import {
        repo: PathBuf,
        path: PathBuf,
        def: super::JobDef,
        /// The legacy row, remembered once imported.
        row: i64,
    },
    Skip {
        repo_path: String,
        label: String,
        reason: String,
    },
}

/// Whether `nibble cron import` already ran on this machine.
#[cfg(test)]
pub fn legacy_done(store: &CronStore) -> Result<bool> {
    Ok(store.meta(LEGACY_EXPORTED)?.is_some())
}

/// Whether the jobs in `~/.nibble/tasks.db` are still to be imported.
pub fn legacy_pending(store: &CronStore, db: &crate::db::Database) -> Result<bool> {
    Ok(store.meta(LEGACY_EXPORTED)?.is_none() && db.has_cron_jobs()?)
}

/// Plan the import of older nibble's jobs. Nothing is written: the rows
/// come from `~/.nibble`, which sandboxes can write, so the user reviews
/// the plan first. Each job gets a file name no other file or planned job
/// has.
pub fn plan_legacy(store: &CronStore, db: &crate::db::Database) -> Result<Vec<LegacyItem>> {
    let mut taken: Vec<PathBuf> = Vec::new();
    let mut plan = Vec::new();
    // Oldest first, so the original of two same-named jobs keeps the name.
    for job in db.list_cron_jobs(None)?.into_iter().rev() {
        let row = job.id.unwrap_or_default();
        let label = job
            .label
            .clone()
            .map(|l| l.chars().filter(|c| !c.is_control()).collect::<String>())
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| "job".to_string());
        let skip = |reason: String| LegacyItem::Skip {
            repo_path: job.repo_path.clone(),
            label: label.clone(),
            reason,
        };
        // Imported before, even if its file was edited since.
        if store.meta(&imported_key(row))?.is_some() {
            plan.push(skip("already imported".to_string()));
            continue;
        }
        let repo = match super::validate_repo(Path::new(&job.repo_path), true) {
            Ok(r) => r,
            Err(e) => {
                plan.push(skip(format!("{e:#}")));
                continue;
            }
        };
        if job.prompt.trim().is_empty() || super::validate_schedule(&job.schedule).is_err() {
            plan.push(skip("empty prompt or invalid schedule".to_string()));
            continue;
        }
        let base = super::slug(&label);
        let mut wanted = super::JobDef {
            name: base.clone(),
            label: label.clone(),
            schedule: job.schedule.clone(),
            enabled: job.enabled,
            skip_if_running: job.skip_if_running,
            expires_at: job.expires_at,
            timeout: super::DEFAULT_TIMEOUT,
            model: None,
            env: vec![],
            notify: super::Notify::Slack,
            // Old jobs were plain prompts that could edit the repo.
            read_only: false,
            prompt: job.prompt.clone(),
        };
        // As the file will read back (titles and prompts are trimmed), so
        // what is approved is exactly what is on disk.
        if let Ok(p) = super::parse_job(&base, &super::format_job(&wanted)) {
            wanted = p.def;
        }
        // Walk base, base-2, ... once: a file holding this very job means a
        // retry after a partial import; the first free name is taken.
        let mut free = None;
        for n in 1.. {
            let name = if n == 1 {
                base.clone()
            } else {
                // Shortened so the suffix keeps the name within the cap.
                let suffix = format!("-{n}");
                let keep: String = base
                    .chars()
                    .take(super::MAX_NAME_LEN - suffix.len())
                    .collect();
                format!("{}{suffix}", keep.trim_end_matches('-'))
            };
            let path = super::job_path(&repo, &name);
            let mut d = wanted.clone();
            d.name = name.clone();
            if std::fs::read_to_string(&path).is_ok_and(|t| t == super::format_job(&d)) {
                break;
            }
            if super::nothing_at(&path) && !taken.contains(&path) {
                free = Some((name, path));
                break;
            }
        }
        let Some((name, path)) = free else {
            plan.push(skip("already imported".to_string()));
            continue;
        };
        taken.push(path.clone());
        wanted.name = name;
        plan.push(LegacyItem::Import {
            repo,
            path,
            def: wanted,
            row,
        });
    }
    Ok(plan)
}

/// Carry out a reviewed plan: write the files, register the repos, and
/// remember that the import happened.
pub fn apply_legacy(store: &CronStore, plan: &[LegacyItem]) -> Result<()> {
    for item in plan {
        if let LegacyItem::Import {
            repo,
            path,
            def,
            row,
        } = item
        {
            // The plan was made before you were asked: a file that appeared
            // since is not replaced.
            if !super::nothing_at(path) {
                eprintln!("  skipped {}: it appeared after the plan", path.display());
                continue;
            }
            // You reviewed it in the plan.
            super::create_job(store, repo, def)?;
            store.add_repo(&repo.to_string_lossy())?;
            store.set_meta(&imported_key(*row), &path.to_string_lossy())?;
        }
    }
    mark_legacy_done(store)
}

fn imported_key(row: i64) -> String {
    format!("legacy_imported:{row}")
}

/// Remember that older nibble's jobs were dealt with.
pub fn mark_legacy_done(store: &CronStore) -> Result<()> {
    store.set_meta(LEGACY_EXPORTED, &Utc::now().to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, CronStore) {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = CronStore::open_in(&tmp.path().join("cron")).unwrap();
        (tmp, s)
    }

    fn git_repo(tmp: &Path, name: &str) -> PathBuf {
        let repo = tmp.join(name);
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::canonicalize(repo).unwrap()
    }

    #[test]
    fn legacy_jobs_are_planned_then_imported() {
        let (tmp, s) = store();
        let repo = git_repo(tmp.path(), "repo");
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        let add = |repo: &str, label: Option<&str>, enabled: bool| {
            let mut job = crate::models::CronJob::new(
                repo.to_string(),
                "0 9 * * *".into(),
                "Do the thing.".into(),
                label.map(str::to_string),
                Utc::now(),
            );
            job.enabled = enabled;
            db.insert_cron_job(&job).unwrap();
        };
        let r = repo.to_string_lossy().to_string();
        assert!(
            !legacy_pending(&s, &db).unwrap(),
            "no rows: nothing pending"
        );
        add(&r, Some("Malone Daily Tasks"), false);
        add(&r, Some("malone daily tasks!"), true); // same slug
        add(&r, None, true);
        add(&format!("{r}/../repo"), Some("Line\nbreak"), true); // not canonical
        add("/gone", None, true);
        add(&plain.to_string_lossy(), None, true); // not a git repo
                                                   // An existing file is never replaced.
        std::fs::create_dir_all(super::super::jobs_dir(&repo)).unwrap();
        std::fs::write(super::super::job_path(&repo, "job"), "mine").unwrap();
        assert!(legacy_pending(&s, &db).unwrap());

        let plan = plan_legacy(&s, &db).unwrap();
        let mut imported = Vec::new();
        let mut skipped = 0;
        for item in &plan {
            match item {
                LegacyItem::Import {
                    repo: r2,
                    path,
                    def,
                    ..
                } => {
                    assert_eq!(r2, &repo, "registered by its canonical path");
                    imported.push((
                        path.file_name().unwrap().to_string_lossy().to_string(),
                        def.clone(),
                    ));
                }
                LegacyItem::Skip { .. } => skipped += 1,
            }
        }
        let mut names: Vec<&str> = imported.iter().map(|(n, _)| n.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "job-2.md",
                "linebreak.md",
                "malone-daily-tasks-2.md",
                "malone-daily-tasks.md"
            ]
        );
        assert_eq!(skipped, 2, "{plan:?}");
        assert!(s.repos().unwrap().is_empty(), "planning writes nothing");

        apply_legacy(&s, &plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(super::super::job_path(&repo, "job")).unwrap(),
            "mine"
        );
        let def = super::super::load_job_file(&super::super::job_path(&repo, "malone-daily-tasks"))
            .unwrap();
        assert!(!def.enabled && !def.read_only);
        assert_eq!(def.prompt, "Do the thing.");
        assert_eq!(s.repos().unwrap(), [r]);
        assert!(!legacy_pending(&s, &db).unwrap(), "done once");
    }

    #[test]
    fn a_retried_import_skips_what_it_wrote() {
        let (tmp, s) = store();
        let repo = git_repo(tmp.path(), "repo");
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        db.insert_cron_job(&crate::models::CronJob::new(
            repo.to_string_lossy().to_string(),
            "0 9 * * *".into(),
            "Go".into(),
            Some("Daily".into()),
            Utc::now(),
        ))
        .unwrap();
        let plan = plan_legacy(&s, &db).unwrap();
        apply_legacy(&s, &plan).unwrap();
        let again = plan_legacy(&s, &db).unwrap();
        assert!(
            matches!(&again[..], [LegacyItem::Skip { reason, .. }] if reason == "already imported"),
            "{again:?}"
        );
        // A file appearing between plan and apply is not replaced.
        std::fs::remove_file(super::super::job_path(&repo, "daily")).unwrap();
        let plan = plan_legacy(&s, &db).unwrap();
        std::fs::write(super::super::job_path(&repo, "daily"), "theirs").unwrap();
        apply_legacy(&s, &plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(super::super::job_path(&repo, "daily")).unwrap(),
            "theirs"
        );
        assert!(legacy_done(&s).unwrap());
    }

    #[test]
    fn an_edited_import_is_not_imported_again() {
        let (tmp, s) = store();
        let repo = git_repo(tmp.path(), "repo");
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        db.insert_cron_job(&crate::models::CronJob::new(
            repo.to_string_lossy().to_string(),
            "0 9 * * *".into(),
            "Go".into(),
            Some("Daily".into()),
            Utc::now(),
        ))
        .unwrap();
        apply_legacy(&s, &plan_legacy(&s, &db).unwrap()).unwrap();
        let path = super::super::job_path(&repo, "daily");
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("0 9", "0 8");
        std::fs::write(&path, edited).unwrap();
        let again = plan_legacy(&s, &db).unwrap();
        assert!(
            matches!(&again[..], [LegacyItem::Skip { reason, .. }] if reason == "already imported")
        );
        assert!(!super::super::job_path(&repo, "daily-2").exists());
        // A file appearing between plan and apply is not replaced.
        let (_t2, fresh) = store();
        let plan = plan_legacy(&fresh, &db).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "theirs").unwrap();
        apply_legacy(&fresh, &plan).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs");
    }

    #[test]
    fn numbered_names_stay_valid() {
        let (tmp, s) = store();
        let repo = git_repo(tmp.path(), "repo");
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        let long = "x".repeat(80);
        for p in ["a", "b"] {
            db.insert_cron_job(&crate::models::CronJob::new(
                repo.to_string_lossy().to_string(),
                "0 9 * * *".into(),
                p.into(),
                Some(long.clone()),
                Utc::now(),
            ))
            .unwrap();
        }
        apply_legacy(&s, &plan_legacy(&s, &db).unwrap()).unwrap();
        let (jobs, bad) = super::super::load_repo_jobs(&repo).unwrap();
        assert_eq!((jobs.len(), bad.len()), (2, 0), "{bad:?}");
    }

    #[test]
    fn a_bad_legacy_time_does_not_panic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        let job = crate::models::CronJob::new(
            "/r".into(),
            "0 9 * * *".into(),
            "p".into(),
            None,
            Utc::now(),
        );
        db.insert_cron_job(&job).unwrap();
        db.set_cron_next_run_for_test(i64::MAX).unwrap();
        assert!(db.has_cron_jobs().unwrap());
        assert_eq!(db.list_cron_jobs(None).unwrap().len(), 1);
    }

    #[test]
    fn imports_match_what_reads_back_and_any_earlier_copy() {
        let (tmp, s) = store();
        let repo = git_repo(tmp.path(), "repo");
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        // Untrimmed values, as older nibble stored them.
        db.insert_cron_job(&crate::models::CronJob::new(
            repo.to_string_lossy().to_string(),
            "0 9 * * *".into(),
            "  Audit deps \n".into(),
            Some(" Deps ".into()),
            Utc::now(),
        ))
        .unwrap();
        // Ten unrelated files with the same slug sit before the free name.
        std::fs::create_dir_all(super::super::jobs_dir(&repo)).unwrap();
        std::fs::write(super::super::job_path(&repo, "deps"), "other").unwrap();
        for n in 2..=11 {
            std::fs::write(super::super::job_path(&repo, &format!("deps-{n}")), "other").unwrap();
        }
        let plan = plan_legacy(&s, &db).unwrap();
        apply_legacy(&s, &plan).unwrap();
        let path = super::super::job_path(&repo, "deps-12");
        let def = super::super::load_job_file(&path).unwrap();
        let key = repo.to_string_lossy().to_string();
        assert_eq!(
            s.job(&key, "deps-12").unwrap().approved,
            Some(super::super::format_job(&def)),
            "approved exactly as it reads back"
        );
        let again = plan_legacy(&s, &db).unwrap();
        assert!(
            matches!(&again[..], [LegacyItem::Skip { reason, .. }] if reason == "already imported")
        );
    }
}
