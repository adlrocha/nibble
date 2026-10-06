//! Cron job command handlers (Add/List/Edit/Run), extracted verbatim from
//! the pre-split main.rs during the origin/main integration.

use crate::cron;
use crate::db::Database;
use crate::models;
use crate::models::AgentType;
use crate::sandbox::podman::PodmanSandbox;
use crate::sandbox::SandboxHealth;
use anyhow::{Context, Result};

pub(crate) fn cmd_cron_add(
    db: &Database,
    repo_arg: Option<String>,
    schedule: Option<String>,
    prompt: Option<String>,
    file: Option<String>,
    label: Option<String>,
    expires: Option<String>,
) -> Result<()> {
    // Parse the cron definition
    let (schedule, prompt, label, enabled, skip_if_running, file_expires, file_repo) =
        if let Some(file_path) = file {
            let content = std::fs::read_to_string(&file_path)
                .with_context(|| format!("Failed to read cron file: {}", file_path))?;
            let (sched, prompt, lbl, en, skip, exp, rp) = cron::parse_cron_markdown(&content)?;
            (sched, prompt, lbl.or(label), en, skip, exp, rp)
        } else {
            let schedule = schedule.context("Either --schedule or --file must be provided")?;
            let prompt = prompt.context("Either --prompt or --file must be provided")?;
            cron::validate_schedule(&schedule)?;
            (schedule, prompt, label, true, true, None, None)
        };

    // Resolve repo path: --repo CLI arg takes precedence over markdown field
    let raw_repo = repo_arg
        .or(file_repo)
        .context("repo_path is required — use --repo /path/to/repo or add 'repo_path = \"...\"' to the markdown file")?;

    // Expand tilde and canonicalize
    let expanded = if raw_repo.starts_with('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        raw_repo.replacen('~', &home, 1)
    } else {
        raw_repo.clone()
    };
    let repo_path = std::fs::canonicalize(&expanded)
        .with_context(|| {
            format!(
                "repo_path does not exist or cannot be resolved: {}",
                expanded
            )
        })?
        .to_string_lossy()
        .to_string();

    // CLI --expires overrides file expires_at
    let expires = expires.or_else(|| file_expires.map(|exp| exp.to_rfc3339()));

    // Compute next run time
    let next_run = cron::compute_next_run(&schedule, chrono::Utc::now())?;

    // Create the cron job
    let mut job =
        models::CronJob::new(repo_path.clone(), schedule.clone(), prompt, label, next_run);
    job.enabled = enabled;
    job.skip_if_running = skip_if_running;
    if let Some(exp_str) = expires {
        job.expires_at = Some(
            chrono::DateTime::parse_from_rfc3339(&exp_str)
                .with_context(|| format!("Invalid expiry datetime: {exp_str} (use RFC3339, e.g. 2026-04-01T00:00:00Z)"))?
                .with_timezone(&chrono::Utc)
        );
    }

    if let Some(ref lbl) = job.label {
        if db.label_exists_for_repo(&repo_path, lbl)? {
            anyhow::bail!(
                "A cron job with label '{}' already exists for this repo. \
                 Use `nibble cron edit {}` to update it or choose a different label.",
                lbl,
                lbl
            );
        }
    }

    let id = db.insert_cron_job(&job)?;

    println!("Created cron job {} for repo {}", id, repo_path);
    println!("  Schedule: {}", job.schedule);
    println!("  Next run: {}", job.next_run);
    if let Some(ref lbl) = job.label {
        println!("  Label: {}", lbl);
    }
    println!("  Skip if running: {}", job.skip_if_running);
    if let Some(exp) = job.expires_at {
        println!("  Expires at: {}", exp.format("%Y-%m-%d %H:%M UTC"));
    }

    Ok(())
}

pub(crate) fn cmd_cron_list(db: &Database, repo_path_filter: Option<String>) -> Result<()> {
    // Canonicalize the filter path if provided
    let filter = repo_path_filter.map(|p| {
        let expanded = if p.starts_with('~') {
            let home = std::env::var("HOME").unwrap_or_default();
            p.replacen('~', &home, 1)
        } else {
            p
        };
        std::fs::canonicalize(&expanded)
            .map(|c| c.to_string_lossy().to_string())
            .unwrap_or(expanded)
    });

    let jobs = db.list_cron_jobs(filter.as_deref())?;

    if jobs.is_empty() {
        if filter.is_some() {
            println!("No cron jobs found for this repo.");
        } else {
            println!("No cron jobs found.");
        }
        return Ok(());
    }

    println!(
        "{:<5} {:<10} {:<20} {:<20} {:<10} {:<24} LABEL",
        "ID", "REPO", "SCHEDULE", "NEXT RUN", "STATUS", "EXPIRES (UTC)"
    );
    println!("{}", "─".repeat(114));

    let now = chrono::Utc::now();

    for job in jobs {
        let short_task = std::path::Path::new(&job.repo_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&job.repo_path)
            .chars()
            .take(10)
            .collect::<String>();
        let short_task = short_task.as_str();
        let label = job.label.as_deref().unwrap_or("-");
        let status = if job.enabled {
            if job.next_run <= now {
                "due".to_string()
            } else {
                "enabled".to_string()
            }
        } else {
            "disabled".to_string()
        };

        let next_run_str = if job.next_run <= now {
            "now".to_string()
        } else {
            let diff = job.next_run.signed_duration_since(now);
            let total_secs = diff.num_seconds();
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            if hours >= 24 {
                format!("in {}d", diff.num_days())
            } else if hours > 0 {
                format!("in {}h {}min", hours, mins)
            } else if mins > 0 {
                format!("in {}min {}s", mins, secs)
            } else {
                format!("in {}s", secs)
            }
        };

        let expires_str = match job.expires_at {
            Some(exp) => exp.format("%Y-%m-%d %H:%M UTC").to_string(),
            None => "-".to_string(),
        };

        println!(
            "{:<5} {:<10} {:<20} {:<20} {:<10} {:<24} {}",
            job.id.unwrap_or(0),
            short_task,
            job.schedule,
            next_run_str,
            status,
            expires_str,
            label.chars().take(25).collect::<String>(),
        );
    }

    Ok(())
}

// Each argument maps to a distinct `nibble cron edit` flag.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_cron_edit(
    db: &Database,
    id: i64,
    schedule: Option<String>,
    prompt: Option<String>,
    label: Option<String>,
    enable: bool,
    disable: bool,
    expires: Option<String>,
) -> Result<()> {
    let mut job = db
        .get_cron_job(id)?
        .ok_or_else(|| anyhow::anyhow!("Cron job {} not found", id))?;

    let mut updated = false;

    if let Some(sched) = schedule {
        cron::validate_schedule(&sched)?;
        job.schedule = sched;
        // Recompute next run
        job.next_run = cron::compute_next_run(&job.schedule, chrono::Utc::now())?;
        updated = true;
    }

    if let Some(p) = prompt {
        job.prompt = p;
        updated = true;
    }

    if let Some(l) = label {
        job.label = Some(l);
        updated = true;
    }

    if enable && disable {
        anyhow::bail!("Cannot use both --enable and --disable");
    }

    if enable {
        job.enabled = true;
        updated = true;
    }

    if disable {
        job.enabled = false;
        updated = true;
    }

    if let Some(exp_str) = expires {
        if exp_str.eq_ignore_ascii_case("none") {
            job.expires_at = None;
        } else {
            job.expires_at = Some(
                chrono::DateTime::parse_from_rfc3339(&exp_str)
                    .with_context(|| format!("Invalid expiry datetime: {exp_str} (use RFC3339, e.g. 2026-04-01T00:00:00Z)"))?
                    .with_timezone(&chrono::Utc)
            );
        }
        updated = true;
    }

    if updated {
        db.update_cron_job(&job)?;
        println!("Updated cron job {}", id);
    } else {
        println!("No changes made to cron job {}", id);
    }

    Ok(())
}

pub(crate) fn cmd_cron_run(db: &Database, id: i64) -> Result<()> {
    let job = db
        .get_cron_job(id)?
        .ok_or_else(|| anyhow::anyhow!("Cron job {} not found", id))?;

    println!(
        "Running cron job {} ({})",
        id,
        job.label.as_deref().unwrap_or("unnamed")
    );
    println!("Target repo: {}", job.repo_path);
    println!(
        "Prompt: {}",
        job.prompt.chars().take(60).collect::<String>()
    );

    let task = find_healthy_sandbox_for_repo(db, &job.repo_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "No healthy sandbox found for repo {}.\n\
             Start one with: nibble sandbox spawn {}",
            job.repo_path,
            job.repo_path
        )
    })?;

    println!(
        "Injecting into sandbox {}...",
        &task.task_id[..task.task_id.len().min(8)]
    );
    crate::agent_input::inject(&task, &job.prompt)?;
    println!("Prompt injected successfully.");

    Ok(())
}

/// Find a healthy sandbox for the given repo path. Returns None if no healthy container exists.

pub(crate) fn find_healthy_sandbox_for_repo(
    db: &Database,
    repo_path: &str,
) -> Result<Option<models::Task>> {
    let Some(task) = db.get_task_by_repo_path(repo_path)? else {
        return Ok(None);
    };
    let sandbox = PodmanSandbox::new();
    let container_name = task
        .container_name
        .as_deref()
        .unwrap_or_else(|| task.container_id.as_deref().unwrap_or(""));
    match sandbox.health_check(container_name) {
        SandboxHealth::Healthy => Ok(Some(task)),
        _ => Ok(None),
    }
}

/// List all tracked sandboxes, auto-cleaning gone entries.

pub(crate) fn resolve_cron_id(db: &Database, id_or_label: &str) -> Result<i64> {
    // Try numeric first
    if let Ok(n) = id_or_label.parse::<i64>() {
        if db.get_cron_job(n)?.is_some() {
            return Ok(n);
        }
        anyhow::bail!("Cron job {} not found", n);
    }
    // Try label
    if let Some(job) = db.get_cron_job_by_label(id_or_label)? {
        return Ok(job.id.unwrap());
    }
    anyhow::bail!("Cron job '{}' not found (tried as label)", id_or_label)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SelectedAgent {
    Claude,
    Pi,
    Omp,
    Hermes,
}

impl std::fmt::Display for SelectedAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectedAgent::Claude => write!(f, "Claude Code"),
            SelectedAgent::Pi => write!(f, "pi"),
            SelectedAgent::Omp => write!(f, "omp"),
            SelectedAgent::Hermes => write!(f, "hermes"),
        }
    }
}
