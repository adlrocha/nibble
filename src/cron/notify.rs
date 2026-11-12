//! After a run: decide whether it found anything worth telling you about,
//! and hand the report to `nibble rc` for Slack.
//!
//! A problem is reported when it first appears, again when it gets worse
//! (warn → critical), and again once a day while it lasts, unless it was
//! snoozed from Slack. A run that fails or times out counts as a problem
//! of its own (`run:failed`, `run:timeout`), reported the same way; it says
//! nothing about the job's other findings, which are kept as they were.

use super::run::{Finding, Severity};
use super::store::{CronStore, FindingState, RunRecord, RunStatus};
use super::{JobDef, Notify};
use anyhow::Result;
use chrono::{DateTime, Utc};

/// How long a lasting problem stays quiet after it was reported.
pub const RENOTIFY: chrono::Duration = chrono::Duration::hours(24);

/// The problems a run stands for: its findings, or the failure itself.
pub fn findings_of(r: &RunRecord) -> Vec<Finding> {
    match r.status {
        RunStatus::Failed | RunStatus::TimedOut => vec![Finding {
            fingerprint: format!("run:{}", r.status.as_str()),
            severity: Severity::Warn,
            title: if r.status == RunStatus::TimedOut {
                "The job timed out".to_string()
            } else {
                "The job failed".to_string()
            },
            details: r.error.clone().unwrap_or_default(),
            suggested_action: format!("See `nibble cron show {}`.", r.id),
        }],
        _ => r
            .report
            .as_ref()
            .map(|rep| rep.findings.clone())
            .unwrap_or_default(),
    }
}

/// Record what the run found and return the fingerprints to report now.
pub fn new_findings(store: &CronStore, r: &RunRecord, now: DateTime<Utc>) -> Result<Vec<String>> {
    store.atomically(|| decide(store, r, now))
}

fn decide(store: &CronStore, r: &RunRecord, now: DateTime<Utc>) -> Result<Vec<String>> {
    if r.status == RunStatus::Running {
        return Ok(vec![]);
    }
    let found = findings_of(r);
    let mut new = Vec::new();
    for f in &found {
        let old = store.finding(&r.repo_path, &r.name, &f.fingerprint)?;
        let snoozed = old
            .as_ref()
            .and_then(|o| o.snoozed_until)
            .is_some_and(|u| u > now);
        let due = match &old {
            None => true,
            Some(o) => {
                f.severity > o.severity
                    || o.notified_at
                        .is_none_or(|t| now.signed_duration_since(t) >= RENOTIFY)
            }
        };
        let report = due && !snoozed;
        if report {
            new.push(f.fingerprint.clone());
        }
        store.put_finding(
            &r.repo_path,
            &r.name,
            &f.fingerprint,
            &FindingState {
                severity: f.severity,
                notified_at: if report {
                    Some(now)
                } else {
                    old.as_ref().and_then(|o| o.notified_at)
                },
                snoozed_until: old
                    .as_ref()
                    .and_then(|o| o.snoozed_until)
                    .filter(|u| *u > now),
                last_seen: now,
            },
        )?;
    }
    // A failed run did not look, so what it did not report is not gone.
    if !matches!(r.status, RunStatus::Failed | RunStatus::TimedOut) {
        let keep: Vec<String> = found.into_iter().map(|f| f.fingerprint).collect();
        store.retain_findings(&r.repo_path, &r.name, &keep, now, r.started_at)?;
    }
    Ok(new)
}

/// Report a finished run to Slack when it found something new and the job
/// asks for it. The card waits in `nibble rc`'s queue, so this works while
/// the Slack daemon is down.
pub fn after_run(store: &CronStore, def: &JobDef, r: &RunRecord) -> Result<bool> {
    // Only advance the dedupe state when a card can be delivered: otherwise
    // findings would be stamped as reported while nothing reached you, and
    // switching `notify` on (or setting up `nibble rc`) would stay silent.
    if def.notify == Notify::None || !crate::rc::reports::configured() {
        return Ok(false);
    }
    let new = new_findings(store, r, Utc::now())?;
    if new.is_empty() {
        return Ok(false);
    }
    let card = crate::rc::reports::ReportCard::new(r, findings_of(r), new);
    crate::rc::reports::queue(&card)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::run::Report;

    fn record(store: &CronStore, status: RunStatus, findings: &[(&str, Severity)]) -> RunRecord {
        let id = store
            .start_run("/r", "j", "J", "schedule", std::process::id())
            .unwrap();
        let report = Report {
            status: Severity::Warn,
            summary: "s".into(),
            findings: findings
                .iter()
                .map(|(fp, sev)| Finding {
                    fingerprint: fp.to_string(),
                    severity: *sev,
                    title: fp.to_string(),
                    details: String::new(),
                    suggested_action: String::new(),
                })
                .collect(),
        };
        store
            .finish_run(
                id,
                &super::super::store::RunEnd {
                    status,
                    report: Some(&report),
                    session_id: None,
                    cost_usd: None,
                    error: Some("boom"),
                },
            )
            .unwrap();
        store.run(id).unwrap().unwrap()
    }

    #[test]
    fn reports_new_worse_and_daily() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = CronStore::open_in(tmp.path()).unwrap();
        let t0 = Utc::now();
        let r = record(
            &s,
            RunStatus::Warn,
            &[("a", Severity::Warn), ("b", Severity::Warn)],
        );
        assert_eq!(new_findings(&s, &r, t0).unwrap(), ["a", "b"]);
        // Same problems an hour later: quiet.
        let t1 = t0 + chrono::Duration::hours(1);
        assert!(new_findings(&s, &r, t1).unwrap().is_empty());
        // Worse: reported again.
        let worse = record(
            &s,
            RunStatus::Critical,
            &[("a", Severity::Critical), ("b", Severity::Warn)],
        );
        assert_eq!(new_findings(&s, &worse, t1).unwrap(), ["a"]);
        // A day after the last report: reported again.
        let t2 = t0 + RENOTIFY + chrono::Duration::minutes(1);
        assert_eq!(
            new_findings(&s, &r, t2).unwrap(),
            ["b"],
            "a was reported at t1"
        );
    }

    #[test]
    fn resolved_problems_report_again_when_back() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = CronStore::open_in(tmp.path()).unwrap();
        // Findings recorded well before the runs that follow start.
        let t = Utc::now() - chrono::Duration::hours(1);
        let bad = record(&s, RunStatus::Warn, &[("a", Severity::Warn)]);
        assert_eq!(new_findings(&s, &bad, t).unwrap(), ["a"]);
        let ok = record(&s, RunStatus::Ok, &[]);
        assert!(new_findings(&s, &ok, t).unwrap().is_empty());
        assert!(
            s.finding("/r", "j", "a").unwrap().is_none(),
            "forgotten once gone"
        );
        assert_eq!(new_findings(&s, &bad, t).unwrap(), ["a"]);
    }

    #[test]
    fn snoozed_problems_stay_quiet_until_the_snooze_ends() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = CronStore::open_in(tmp.path()).unwrap();
        let t = Utc::now();
        let bad = record(&s, RunStatus::Warn, &[("a", Severity::Warn)]);
        new_findings(&s, &bad, t).unwrap();
        let until = t + chrono::Duration::days(7);
        s.snooze("/r", "j", &["a".to_string()], until).unwrap();
        let later = t + chrono::Duration::days(2);
        let worse = record(&s, RunStatus::Critical, &[("a", Severity::Critical)]);
        assert!(
            new_findings(&s, &worse, later).unwrap().is_empty(),
            "even when worse"
        );
        // Gone for a run while snoozed: the snooze is kept.
        let ok = record(&s, RunStatus::Ok, &[]);
        new_findings(&s, &ok, later).unwrap();
        assert!(new_findings(&s, &bad, later).unwrap().is_empty());
        assert_eq!(
            new_findings(&s, &bad, until + chrono::Duration::minutes(1)).unwrap(),
            ["a"]
        );
    }

    #[test]
    fn a_failed_run_does_not_reset_the_other_findings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = CronStore::open_in(tmp.path()).unwrap();
        // Findings recorded well before the runs that follow start.
        let t = Utc::now() - chrono::Duration::hours(1);
        let bad = record(&s, RunStatus::Warn, &[("a", Severity::Warn)]);
        assert_eq!(new_findings(&s, &bad, t).unwrap(), ["a"]);
        let failed = record(&s, RunStatus::Failed, &[]);
        new_findings(&s, &failed, t).unwrap();
        assert!(
            new_findings(&s, &bad, t).unwrap().is_empty(),
            "a is still known"
        );
        assert!(
            s.finding("/r", "j", "run:failed").unwrap().is_none(),
            "the failure is over"
        );
    }

    #[test]
    fn an_older_overlapping_run_keeps_what_a_newer_one_recorded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = CronStore::open_in(tmp.path()).unwrap();
        let old = record(&s, RunStatus::Ok, &[]);
        let newer = record(&s, RunStatus::Warn, &[("a", Severity::Warn)]);
        let later = Utc::now() + chrono::Duration::minutes(5);
        new_findings(&s, &newer, later).unwrap();
        // The older run finishes last, without `a`: `a` was seen after it began.
        new_findings(&s, &old, later).unwrap();
        assert!(s.finding("/r", "j", "a").unwrap().is_some());
    }

    #[test]
    fn severity_rises_report_each_time() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = CronStore::open_in(tmp.path()).unwrap();
        let t = Utc::now();
        let warn = record(&s, RunStatus::Warn, &[("a", Severity::Warn)]);
        let crit = record(&s, RunStatus::Critical, &[("a", Severity::Critical)]);
        assert_eq!(new_findings(&s, &warn, t).unwrap(), ["a"]);
        assert_eq!(new_findings(&s, &crit, t).unwrap(), ["a"]);
        assert!(
            new_findings(&s, &warn, t).unwrap().is_empty(),
            "a drop is quiet"
        );
        assert_eq!(new_findings(&s, &crit, t).unwrap(), ["a"]);
    }

    #[test]
    fn failures_are_problems_of_their_own() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = CronStore::open_in(tmp.path()).unwrap();
        let t = Utc::now();
        let failed = record(&s, RunStatus::Failed, &[("ignored", Severity::Warn)]);
        let f = findings_of(&failed);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].fingerprint, "run:failed");
        assert_eq!(f[0].details, "boom");
        assert_eq!(new_findings(&s, &failed, t).unwrap(), ["run:failed"]);
        assert!(
            new_findings(&s, &failed, t).unwrap().is_empty(),
            "once a day"
        );
        assert_eq!(
            new_findings(&s, &failed, t + RENOTIFY + chrono::Duration::minutes(1)).unwrap(),
            ["run:failed"],
            "a job that keeps failing is reported daily"
        );
        let timeout = record(&s, RunStatus::TimedOut, &[]);
        assert_eq!(findings_of(&timeout)[0].title, "The job timed out");
        let running = s
            .start_run("/r", "j", "J", "manual", std::process::id())
            .unwrap();
        assert!(new_findings(&s, &s.run(running).unwrap().unwrap(), t)
            .unwrap()
            .is_empty());
    }
}
