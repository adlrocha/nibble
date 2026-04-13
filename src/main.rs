mod agent_input;
mod cli;
mod config;
mod db;
mod display;
mod models;
mod monitor;
mod notifications;
mod sandbox;

mod cron;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands, CronAction, ReportAction, SandboxAction};
use db::Database;
use models::{SandboxConfig, SandboxType, Task, TaskContext, TaskStatus};
use sandbox::podman::PodmanSandbox;
use sandbox::{Sandbox, SandboxHealth};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::thread;
use std::time::Duration;
use uuid::Uuid;

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Ensure data directory exists
    db::ensure_data_dir()?;

    // Open database
    let db_path = db::default_db_path();
    let db = Database::open(&db_path).context("Failed to open database")?;

    // Run cleanup on every invocation (1 hour retention)
    let _ = db.cleanup_old_completed(3600);
    let _ = db.cleanup_old_bot_messages(3600);

    match cli.command {
        None => {
            // Default: show running tasks (actively generating)
            let tasks = db.list_tasks(Some(TaskStatus::Running))?;
            display::display_task_list(&tasks);
        }
        Some(Commands::List { all, status }) => {
            let tasks = if let Some(status_str) = status {
                let status = TaskStatus::from_str(&status_str).map_err(|e| anyhow::anyhow!(e))?;
                db.list_tasks(Some(status))?
            } else if all {
                db.list_tasks(None)?
            } else {
                // Show running tasks by default
                db.list_tasks(Some(TaskStatus::Running))?
            };

            display::display_task_list(&tasks);
        }
        Some(Commands::Show { task_id }) => {
            let task = db
                .get_task_by_id(&task_id)?
                .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

            display::display_task_detail(&task);
        }
        Some(Commands::Clear { task_id }) => {
            let deleted = db.delete_task(&task_id)?;
            if deleted {
                println!("Task {} cleared", task_id);
            } else {
                println!("Task not found: {}", task_id);
            }
        }
        Some(Commands::ClearAll) => {
            let completed = db.list_tasks(Some(TaskStatus::Completed))?;
            let exited = db.list_tasks(Some(TaskStatus::Exited))?;

            let mut count = 0;
            for task in completed.iter().chain(exited.iter()) {
                db.delete_task(&task.task_id)?;
                count += 1;
            }

            println!("Cleared {} tasks", count);
        }
        Some(Commands::Reset { force }) => {
            let all_tasks = db.list_tasks(None)?;
            let task_count = all_tasks.len();

            if task_count == 0 {
                println!("No tasks to clear.");
                return Ok(());
            }

            // Show what will be cleared
            println!("This will delete ALL {} tasks:", task_count);
            for task in &all_tasks {
                println!("  - [{}] {}", task.agent_type, task.title);
            }
            println!();

            // Confirm unless --force
            if !force {
                use std::io::{self, Write};
                print!("Are you sure you want to delete ALL tasks? (yes/no): ");
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;

                if input.trim().to_lowercase() != "yes" {
                    println!("Aborted. No tasks were deleted.");
                    return Ok(());
                }
            }

            // Delete all tasks
            let mut count = 0;
            for task in all_tasks {
                db.delete_task(&task.task_id)?;
                count += 1;
            }

            println!("✓ Cleared all {} tasks", count);
        }
        Some(Commands::Watch) => {
            println!("Watching tasks (Ctrl+C to exit)...\n");

            loop {
                // Clear screen
                print!("\x1B[2J\x1B[1;1H");

                let tasks = db.list_tasks(None)?;
                display::display_task_list(&tasks);

                thread::sleep(Duration::from_secs(2));
            }
        }
        Some(Commands::Cleanup { retention_secs }) => {
            let deleted = db.cleanup_old_completed(retention_secs)?;
            println!("Cleaned up {} old completed tasks", deleted);
        }
        Some(Commands::Prune) => {
            let pruned = prune_stale_tasks(&db)?;
            println!("Pruned {} stale task(s)", pruned);
        }
        Some(Commands::Report { action }) => match action {
            ReportAction::Start {
                task_id,
                agent_type,
                cwd,
                title,
                pid,
                ppid,
                zellij_pane_id,
                session_id,
            } => {
                let mut task = Task::new(task_id, agent_type, title, pid, ppid);

                let mut extra = HashMap::new();
                if let Some(pane_id) = zellij_pane_id {
                    extra.insert(
                        "zellij_pane_id".to_string(),
                        serde_json::Value::Number(pane_id.into()),
                    );
                }

                task.context = Some(TaskContext {
                    url: None,
                    project_path: Some(cwd),
                    session_id,
                    extra,
                });

                db.insert_task(&task)?;
                println!("Task started: {}", task.task_id);
            }
            ReportAction::Complete { task_id, exit_code } => {
                let mut task = db
                    .get_task_by_id(&task_id)?
                    .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

                // If exit_code is provided and non-zero, mark as exited (failed)
                // Otherwise mark as completed (finished generating)
                if let Some(code) = exit_code {
                    if code != 0 {
                        task.set_exited(Some(code));
                    } else {
                        task.complete();
                    }
                } else {
                    task.complete();
                }
                db.update_task(&task)?;
                println!("Task completed: {}", task_id);
            }
            ReportAction::Running { task_id } => {
                let mut task = db
                    .get_task_by_id(&task_id)?
                    .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

                task.set_running();
                db.update_task(&task)?;
                println!("Task running: {}", task_id);
            }
            ReportAction::Exited { task_id, exit_code } => {
                let mut task = db
                    .get_task_by_id(&task_id)?
                    .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

                task.set_exited(exit_code);
                db.update_task(&task)?;
                println!("Task exited: {}", task_id);
                // No Telegram notification here: the sandbox container is still
                // running. Session exits are normal (detach, turn complete, etc.).
                // Container crashes are detected separately by prune_stale_tasks.
            }
            ReportAction::LastMessage { task_id, message } => {
                let mut task = db
                    .get_task_by_id(&task_id)?
                    .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

                // Store on attention_reason so it's available when the exit notification fires.
                task.attention_reason = Some(message);
                db.update_task(&task)?;
            }
            ReportAction::SessionId {
                task_id,
                session_id,
            } => {
                let mut task = db
                    .get_task_by_id(&task_id)?
                    .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

                let ctx = task.context.get_or_insert_with(|| TaskContext {
                    url: None,
                    project_path: None,
                    session_id: None,
                    extra: HashMap::new(),
                });
                ctx.session_id = Some(session_id);
                db.update_task(&task)?;
            }
        },
        Some(Commands::Monitor { task_id, pid }) => {
            // Create a monitor and start monitoring
            let monitor = monitor::TaskMonitor::new(db);
            monitor.monitor_task(task_id, pid)?;
        }
        Some(Commands::Notify {
            message,
            task_id,
            attention,
        }) => {
            let cfg = config::load().unwrap_or_default();

            if !cfg.telegram.is_configured() {
                eprintln!(
                    "Telegram notifications are not configured. \
                     Run scripts/setup-telegram.sh to set them up."
                );
                // Exit cleanly — missing config is not a fatal error for hooks.
                return Ok(());
            }

            // Build the notification text: header with task context + message body.
            let text = build_notification_text(&db, task_id.as_deref(), &message, attention)?;

            let msg_id = if let Some(ref tid) = task_id {
                // Always attach a Reply button so the user can respond from Telegram.
                notifications::telegram::send_with_reply_button(&cfg.telegram, &text, tid)
                    .context("Failed to send Telegram notification")?
            } else {
                notifications::telegram::send(&cfg.telegram, &text)
                    .context("Failed to send Telegram notification")?
            };

            // Record the Telegram message_id → task_id mapping so the listener
            // can route phone replies back to the right agent session.
            if let Some(ref tid) = task_id {
                let _ = db.insert_bot_message(msg_id, tid);
            }
        }
        Some(Commands::Cron { action }) => match action {
            CronAction::Add {
                repo,
                schedule,
                prompt,
                file,
                label,
                expires,
            } => {
                cmd_cron_add(&db, repo, schedule, prompt, file, label, expires)?;
            }
            CronAction::List { repo_path } => {
                cmd_cron_list(&db, repo_path)?;
            }
            CronAction::Edit {
                id,
                schedule,
                prompt,
                label,
                enable,
                disable,
                expires,
            } => {
                let cron_id = resolve_cron_id(&db, &id)?;
                cmd_cron_edit(
                    &db, cron_id, schedule, prompt, label, enable, disable, expires,
                )?;
            }
            CronAction::Del { id } => {
                let cron_id = resolve_cron_id(&db, &id)?;
                let deleted = db.delete_cron_job(cron_id)?;
                if deleted {
                    println!("Deleted cron job {}", id);
                } else {
                    println!("Cron job {} not found", id);
                }
            }
            CronAction::Run { id } => {
                let cron_id = resolve_cron_id(&db, &id)?;
                cmd_cron_run(&db, cron_id)?;
            }
        },
        // ── Sandbox subcommands ────────────────────────────────────────────
        Some(Commands::Sandbox { action }) => match action {
            SandboxAction::Spawn {
                repo_path,
                task,
                image,
                fresh,
                session_id,
                branch,
                factory,
            } => {
                let effective_repo_path = if let Some(ref branch_name) = branch {
                    let worktree = create_worktree(std::path::Path::new(&repo_path), branch_name)?;
                    worktree.to_string_lossy().to_string()
                } else {
                    repo_path
                };
                let cfg = config::load().unwrap_or_default();
                let factory_enabled = factory.unwrap_or(cfg.factory.enabled);
                cmd_sandbox_spawn(
                    &db,
                    effective_repo_path,
                    task,
                    image,
                    fresh,
                    session_id,
                    false, // no_attach
                    false, // kimi
                    false, // glm
                    false, // opencode — spawn is always Claude-centric
                    factory_enabled,
                )?;
            }
            SandboxAction::List => {
                cmd_sandbox_list(&db)?;
            }
            SandboxAction::Attach {
                container_or_path,
                fresh,
                btw,
                kimi,
                glm,
                opencode,
                branch,
            } => {
                // If --branch is given, resolve the worktree path (creating it if needed)
                // and use that as the effective target instead of the original repo.
                let effective_path = if let Some(ref branch_name) = branch {
                    let worktree =
                        create_worktree(std::path::Path::new(&container_or_path), branch_name)?;
                    worktree.to_string_lossy().to_string()
                } else {
                    container_or_path.clone()
                };

                match resolve_sandbox_id(&db, &effective_path) {
                    Ok(task_id) => {
                        cmd_sandbox_attach(&db, task_id, fresh, btw, kimi, glm, opencode)?;
                    }
                    Err(e) => {
                        // If the input looks like a repo path and no sandbox exists,
                        // spawn one and then attach to it.
                        let looks_like_path = effective_path.starts_with('.')
                            || effective_path.starts_with('/')
                            || effective_path.starts_with('~')
                            || effective_path.contains('/')
                            || std::path::Path::new(&effective_path).exists();

                        if looks_like_path {
                            eprintln!("No sandbox found for '{}', spawning one...", effective_path);
                            let cfg = config::load().unwrap_or_default();
                            let task_id = cmd_sandbox_spawn(
                                &db,
                                effective_path,
                                None, // task_desc
                                "nibble-sandbox:latest".to_string(),
                                fresh,
                                None, // session_id
                                true, // no_attach — we attach below with the correct flags
                                kimi,
                                glm,
                                opencode,
                                cfg.factory.enabled,
                            )?;
                            cmd_sandbox_attach(&db, task_id, fresh, btw, kimi, glm, opencode)?;
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
            SandboxAction::Kill {
                container_or_path,
                all,
                worktree,
                force,
                branch,
            } => {
                if all {
                    cmd_sandbox_kill_all(&db)?;
                } else {
                    let raw_input = container_or_path.ok_or_else(|| {
                        anyhow::anyhow!("Provide a repo path, container name, or --all")
                    })?;

                    // --branch <name> derives the worktree path from the repo + branch slug,
                    // exactly mirroring how `spawn --branch` and `attach --branch` create it.
                    let input = if let Some(ref branch_name) = branch {
                        let branch_slug = branch_name
                            .chars()
                            .map(|c| {
                                if c.is_alphanumeric() || c == '-' || c == '_' {
                                    c
                                } else {
                                    '-'
                                }
                            })
                            .collect::<String>();
                        let abs = std::fs::canonicalize(&raw_input)
                            .unwrap_or_else(|_| std::path::PathBuf::from(&raw_input));
                        let repo_name = abs
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(&raw_input)
                            .to_string();
                        let parent = abs.parent().unwrap_or(&abs);
                        parent
                            .join(format!("{}--{}", repo_name, branch_slug))
                            .to_string_lossy()
                            .to_string()
                    } else {
                        raw_input
                    };

                    // --branch implies --worktree
                    let remove_worktree_flag = worktree || force || branch.is_some();

                    // Resolve the sandbox — but if --worktree is set and no sandbox is
                    // running, we still want to remove the worktree directory.
                    match resolve_sandbox_id(&db, &input) {
                        Ok(id) => {
                            // Check for worktree *before* killing (task record is deleted after).
                            let wt_path = if remove_worktree_flag {
                                db.get_worktree_path(&id)?
                            } else {
                                None
                            };
                            cmd_sandbox_kill(&db, id)?;
                            if let Some(wt) = wt_path {
                                let wt_pb = std::path::PathBuf::from(&wt);
                                let removed = remove_worktree(&wt_pb, force)?;
                                if removed {
                                    // Disable any cron jobs pointing at this worktree.
                                    let affected = db.list_cron_jobs(Some(&wt))?;
                                    if !affected.is_empty() {
                                        eprintln!("⚠️  Disabling {} cron job(s) that targeted this worktree:", affected.len());
                                        for mut job in affected {
                                            eprintln!(
                                                "   - {} (id {})",
                                                job.label.as_deref().unwrap_or("unnamed"),
                                                job.id.unwrap_or(0)
                                            );
                                            job.enabled = false;
                                            let _ = db.update_cron_job(&job);
                                        }
                                    }
                                }
                            } else if remove_worktree_flag {
                                eprintln!("Note: no worktree recorded for this sandbox.");
                            }
                        }
                        Err(_) if remove_worktree_flag => {
                            // No sandbox running, but user asked to remove the worktree directory.
                            let abs = std::fs::canonicalize(&input)
                                .unwrap_or_else(|_| std::path::PathBuf::from(&input));
                            let removed = remove_worktree(&abs, force)?;
                            if removed {
                                let affected =
                                    db.list_cron_jobs(Some(abs.to_str().unwrap_or(&input)))?;
                                if !affected.is_empty() {
                                    eprintln!(
                                        "⚠️  Disabling {} cron job(s) that targeted this worktree:",
                                        affected.len()
                                    );
                                    for mut job in affected {
                                        eprintln!(
                                            "   - {} (id {})",
                                            job.label.as_deref().unwrap_or("unnamed"),
                                            job.id.unwrap_or(0)
                                        );
                                        job.enabled = false;
                                        let _ = db.update_cron_job(&job);
                                    }
                                }
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            SandboxAction::Restart => {
                cmd_sandbox_resume(&db, true)?;
            }
            SandboxAction::Resume { all } => {
                cmd_sandbox_resume(&db, all)?;
            }
            SandboxAction::Build { image, rebuild } => {
                let sandbox = PodmanSandbox::new();
                sandbox.ensure_image_with_opts(&image, rebuild)?;
                println!("Sandbox image ready.");
            }
            SandboxAction::Gc {
                container_or_path,
                all,
            } => {
                let task_id = resolve_sandbox_id(&db, &container_or_path)?;
                cmd_sandbox_gc(&db, task_id, all)?;
            }
        },
        Some(Commands::Inject { task_id, message }) => {
            let task = db
                .get_task_by_id(&task_id)?
                .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;
            agent_input::inject(&task, &message)?;
            println!("Message injected into task {}", task_id);
        }

        Some(Commands::Listen) => {
            let cfg = config::load().unwrap_or_default();

            if !cfg.telegram.is_configured() {
                anyhow::bail!("Telegram is not configured. Run scripts/setup-telegram.sh first.");
            }

            // Run an initial prune before entering the listener loop so stale
            // tasks from a previous crash or reboot are cleaned up immediately.
            let _ = prune_stale_tasks(&db);

            notifications::telegram_listener::run(&db, &cfg.telegram)?;
        }
    }

    Ok(())
}

// ── Sandbox command handlers ──────────────────────────────────────────────────

/// Create a git worktree for `branch` next to `repo_path`, returning the worktree path.
///
/// The worktree is placed at `<repo_parent>/<repo_name>--<branch-slug>`, where the
/// branch slug replaces `/` and non-alphanumeric chars with `-`.
/// If the branch doesn't exist it is auto-created from the repo's current HEAD.
fn create_worktree(repo_path: &std::path::Path, branch: &str) -> Result<std::path::PathBuf> {
    let abs_repo = repo_path
        .canonicalize()
        .with_context(|| format!("Cannot resolve repo path: {}", repo_path.display()))?;

    let repo_name = abs_repo
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("Cannot determine repo name from path"))?;

    let branch_slug = branch
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();

    let parent = abs_repo
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Repo path has no parent directory"))?;

    let worktree_path = parent.join(format!("{}--{}", repo_name, branch_slug));

    if worktree_path.exists() {
        anyhow::bail!(
            "Worktree path already exists: {}. \
             Remove it first or use a different branch name.",
            worktree_path.display()
        );
    }

    // Check if branch already exists in the repo.
    let branch_exists = std::process::Command::new("git")
        .args([
            "-C",
            abs_repo.to_str().unwrap_or(""),
            "rev-parse",
            "--verify",
            branch,
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if branch_exists {
        // Check out the existing branch into the new worktree.
        let out = std::process::Command::new("git")
            .args([
                "-C",
                abs_repo.to_str().unwrap_or(""),
                "worktree",
                "add",
                worktree_path.to_str().unwrap_or(""),
                branch,
            ])
            .output()
            .context("Failed to run git worktree add")?;
        if !out.status.success() {
            anyhow::bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    } else {
        // Auto-create the branch from current HEAD.
        let out = std::process::Command::new("git")
            .args([
                "-C",
                abs_repo.to_str().unwrap_or(""),
                "worktree",
                "add",
                "-b",
                branch,
                worktree_path.to_str().unwrap_or(""),
            ])
            .output()
            .context("Failed to run git worktree add -b")?;
        if !out.status.success() {
            anyhow::bail!(
                "git worktree add -b failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        println!("  Branch:    created '{}' from HEAD", branch);
    }

    println!("  Worktree:  {} → {}", branch, worktree_path.display());
    Ok(worktree_path)
}

/// Remove a git worktree directory.
///
/// Returns `true` if removed, `false` if skipped by the user.
/// `force` skips the dirty-check prompt (but still prints a warning).
fn remove_worktree(worktree_path: &std::path::Path, force: bool) -> Result<bool> {
    use std::io::{BufRead, Write};

    if !worktree_path.exists() {
        return Ok(true); // already gone, nothing to do
    }

    // Detect uncommitted changes inside the worktree.
    let dirty = std::process::Command::new("git")
        .args([
            "-C",
            worktree_path.to_str().unwrap_or(""),
            "status",
            "--porcelain",
        ])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if dirty {
        if force {
            eprintln!(
                "⚠️  Warning: worktree {} has uncommitted changes — removing anyway (--force).",
                worktree_path.display()
            );
        } else {
            eprint!(
                "⚠️  Worktree {} has uncommitted changes. Remove it anyway? [y/N] ",
                worktree_path.display()
            );
            std::io::stderr().flush().ok();
            let mut input = String::new();
            std::io::BufReader::new(std::io::stdin())
                .read_line(&mut input)
                .ok();
            if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
                println!("Aborted. Worktree kept.");
                return Ok(false);
            }
        }
    }

    // `git worktree remove --force` removes the directory and unregisters from .git/worktrees.
    let out = std::process::Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            worktree_path.to_str().unwrap_or(""),
        ])
        .output()
        .context("Failed to run git worktree remove")?;

    if out.status.success() {
        println!("Removed worktree: {}", worktree_path.display());
    } else {
        // Fall back to plain directory removal if git worktree remove fails
        // (e.g. the .git link is already broken).
        eprintln!(
            "git worktree remove failed ({}), falling back to rm -rf",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        std::fs::remove_dir_all(worktree_path)
            .with_context(|| format!("Failed to remove {}", worktree_path.display()))?;
        println!("Removed worktree directory: {}", worktree_path.display());
    }

    Ok(true)
}

/// Derive a deterministic UUID v5 for a repo, keyed on its canonical host path.
///
/// Every sandbox on the same repo gets the same UUID, so Telegram injection and
/// re-attach always resume the right conversation. `--resume <uuid>` is a direct
/// UUID lookup by Claude — it doesn't depend on the in-container path.
fn repo_session_id(repo_path: &str) -> Uuid {
    let canonical =
        std::fs::canonicalize(repo_path).unwrap_or_else(|_| std::path::PathBuf::from(repo_path));
    let key = canonical.to_string_lossy();
    Uuid::new_v5(&Uuid::NAMESPACE_OID, key.as_bytes())
}

/// Back up the Claude conversation file for a given session UUID.
///
/// Claude Code stores each session as `~/.claude/projects/<hash>/<uuid>.jsonl`.
/// Rather than deleting it, we rename it to `<uuid>.<timestamp>.jsonl.bak` so it
/// is invisible to Claude (wrong extension) but recoverable until `gc` cleans it up.
fn backup_session_file(session_id: &str) {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return,
    };
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.exists() {
        return;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for entry in std::fs::read_dir(&projects_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let candidate = entry.path().join(format!("{}.jsonl", session_id));
        if candidate.exists() {
            let size = std::fs::metadata(&candidate).map(|m| m.len()).unwrap_or(0);
            let backup = entry
                .path()
                .join(format!("{}.{}.jsonl.bak", session_id, ts));
            match std::fs::rename(&candidate, &backup) {
                Ok(()) => eprintln!(
                    "  Backed up: {} → {}.{}.jsonl.bak ({:.1} MB)",
                    candidate.display(),
                    session_id,
                    ts,
                    size as f64 / 1_048_576.0
                ),
                Err(e) => eprintln!(
                    "  Warning:   could not back up {}: {}",
                    candidate.display(),
                    e
                ),
            }
            return;
        }
    }
    // File not found — session hasn't started yet, nothing to back up.
}

/// Spawn a sandboxed Claude Code agent.  Returns the new task_id on success.
pub(crate) fn cmd_sandbox_spawn(
    db: &Database,
    repo_path: String,
    task_desc: Option<String>,
    image: String,
    fresh: bool,
    session_id: Option<String>,
    no_attach: bool,
    kimi: bool,
    glm: bool,
    opencode: bool,
    factory_enabled: bool,
) -> Result<String> {
    let repo = PathBuf::from(&repo_path);
    if !repo.exists() {
        anyhow::bail!("Repository path does not exist: {}", repo_path);
    }

    let sandbox = PodmanSandbox::new();
    if !sandbox.is_available()? {
        anyhow::bail!("Podman is not installed. Run ./install.sh to set it up.");
    }

    // Check if a sandbox already exists for this repo and re-use it.
    let abs_repo_path_early = repo
        .canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| repo_path.clone());
    if let Some((existing_task_id, _)) =
        db.get_container_state_by_repo_path(&abs_repo_path_early)?
    {
        if let Some(task) = db.get_task_by_id(&existing_task_id)? {
            if let Some(ref cid) = task.container_id {
                if let Ok(crate::sandbox::ContainerStatus::Running) = sandbox.status(cid) {
                    eprintln!(
                        "⚠️  A sandbox for '{}' already exists (task {}).",
                        abs_repo_path_early,
                        &existing_task_id[..existing_task_id.len().min(8)]
                    );
                    eprintln!(
                        "   Attaching to the existing sandbox instead of spawning a new one."
                    );
                    eprintln!();
                    if no_attach {
                        eprintln!("Attach with:");
                        eprintln!("  nibble sandbox attach {}", abs_repo_path_early);
                    } else {
                        cmd_sandbox_attach(
                            db,
                            existing_task_id.clone(),
                            fresh,
                            false,
                            kimi,
                            glm,
                            opencode,
                        )?;
                    }
                    return Ok(existing_task_id);
                }
            }
        }
    }

    sandbox.ensure_image_with_opts(&image, false)?;

    let task_id = uuid::Uuid::new_v4().to_string();

    let mut env_vars = HashMap::new();
    for key in &[
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "HOME",
        "CLAUDE_CONFIG_DIR",
    ] {
        if let Ok(val) = std::env::var(key) {
            env_vars.insert(key.to_string(), val);
        }
    }

    let config = SandboxConfig {
        image,
        env_vars,
        ..SandboxConfig::default()
    };

    println!("Spawning sandbox for '{}'…", repo_path);
    let info = sandbox.spawn(&task_id, &repo, &config)?;

    // Restore .claude.json from backup if missing.
    // This propagates the host Claude login into the container so the user
    // doesn't have to re-authenticate inside the sandbox.
    let restore_result = std::process::Command::new("podman")
        .args([
            "exec", &info.id,
            "/bin/bash", "-c",
            r#"if [ ! -f /home/node/.claude/.claude.json ]; then
                 backup=$(ls /home/node/.claude/backups/.claude.json.backup.* 2>/dev/null | sort -t. -k6 -n | tail -1)
                 if [ -n "$backup" ]; then
                   cp "$backup" /home/node/.claude/.claude.json && echo "restored"
                 fi
               fi"#,
        ])
        .output();
    if let Ok(out) = restore_result {
        if String::from_utf8_lossy(&out.stdout).contains("restored") {
            println!("  Auth:      restored .claude.json from backup");
        }
    }

    // Upgrade opencode to the latest version on every spawn so sandboxes don't
    // drift behind the image-baked version. Claude Code self-updates automatically;
    // opencode does not, so we drive it here. Failures are non-fatal — the baked
    // version still works, just may be older.
    {
        let status = std::process::Command::new("podman")
            .args([
                "exec",
                "-u",
                "node",
                &info.id,
                "/home/node/.opencode/bin/opencode",
                "upgrade",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => println!("  Tools:     opencode upgraded to latest"),
            Ok(_) => eprintln!(
                "  Tools:     ⚠️  opencode upgrade exited non-zero (baked version still usable)"
            ),
            Err(e) => eprintln!("  Tools:     ⚠️  opencode upgrade failed to run: {e}"),
        }
    }

    // Run .nibble/setup.sh if present, otherwise warn the user.
    let setup_script = repo.join(".nibble").join("setup.sh");
    if setup_script.exists() {
        println!("  Setup:     running .nibble/setup.sh …");
        let status = std::process::Command::new("podman")
            .args([
                "exec",
                "--user",
                "node",
                &info.id,
                "/bin/bash",
                "/workspace/.nibble/setup.sh",
            ])
            .status()
            .context("Failed to run .nibble/setup.sh")?;
        if status.success() {
            println!("  Setup:     .nibble/setup.sh completed successfully");
        } else {
            eprintln!("  Setup:     ⚠️  .nibble/setup.sh exited with non-zero status — dependencies may be missing");
        }
    } else {
        eprintln!(
            "  Setup:     ⚠️  No .nibble/setup.sh found — dependencies won't be pre-installed."
        );
        eprintln!(
            "             Create .nibble/setup.sh in the repo to auto-install deps on spawn."
        );
        eprintln!("             (Ask Claude to write it for you once inside the sandbox.)");
    }

    let repo_name = repo
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| repo_path.clone());

    // Detect project toolchains and write AGENTS.md + CLAUDE.md into the
    // container so the agent knows it is in a sandbox and how to set up the
    // project.  AGENTS.md is the primary instruction file (read by both
    // OpenCode and Claude Code); CLAUDE.md contains only the @../AGENTS.md import.
    let toolchains = detect_toolchains(&repo);
    let agents_md = build_sandbox_agents_md(&repo_name, &toolchains, factory_enabled);
    match inject_sandbox_claude_md(&info.id, &agents_md) {
        Ok(()) => {
            if toolchains.is_empty() {
                println!("  Context:   AGENTS.md + CLAUDE.md updated (no toolchain detected)");
            } else {
                let names: Vec<&str> = toolchains.iter().map(|(e, _, _)| *e).collect();
                println!(
                    "  Context:   AGENTS.md + CLAUDE.md updated (detected: {})",
                    names.join(", ")
                );
            }
        }
        Err(e) => eprintln!("  Warning:   Could not write AGENTS.md/CLAUDE.md: {e:#}"),
    }

    let title = task_desc.unwrap_or_else(|| format!("[{}:sandbox]", repo_name));

    let mut task = Task::new(
        task_id.clone(),
        "claude_code".to_string(),
        title,
        None,
        None,
    );
    task.sandbox_type = SandboxType::Podman;
    let abs_repo_path = repo
        .canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(repo_path);

    // Determine the session UUID for this sandbox.
    // - Normal spawn: derive a deterministic UUID v5 from the canonical repo path.
    //   All sandboxes on the same repo share a session, enabling Telegram injection to always
    //   reach the right conversation context without starting a new session.
    // - --fresh: generate a new random UUID v4, replacing the stored ID so subsequent
    //   attaches start from this new session.
    // - explicit --session-id: honour the caller's choice verbatim.
    let resolved_session_id = if let Some(sid) = session_id {
        println!("  Session:   using explicit {}", &sid[..sid.len().min(8)]);
        sid
    } else if fresh {
        let new_sid = Uuid::new_v4().to_string();
        println!("  Session:   fresh start ({})", &new_sid[..8]);
        new_sid
    } else {
        let det_sid = repo_session_id(&abs_repo_path).to_string();
        println!("  Session:   {} (deterministic for repo)", &det_sid[..8]);
        det_sid
    };

    task.container_id = Some(info.id.clone());
    task.sandbox_config = Some(config);
    task.context = Some(TaskContext {
        url: None,
        project_path: Some(abs_repo_path.clone()),
        session_id: Some(resolved_session_id),
        extra: HashMap::new(),
    });
    db.insert_task(&task)?;

    // Detect if this repo is a git worktree (has a `.git` file rather than a directory).
    // If so, record the worktree path so `kill --worktree` knows what to clean up.
    let worktree_marker = repo.join(".git");
    let is_worktree = worktree_marker.is_file();
    let worktree_path_opt = if is_worktree {
        Some(abs_repo_path.as_str())
    } else {
        None
    };
    db.upsert_container_state_with_worktree(
        &task_id,
        &info.name,
        &abs_repo_path,
        worktree_path_opt,
    )?;

    let short_id = &task_id[..task_id.len().min(8)];
    println!("\nSandbox started:");
    println!("  Task ID:   {} ({})", short_id, task_id);
    println!("  Container: {}", info.name);
    println!("  Repo:      {}", abs_repo_path);
    println!();

    // Warn if loginctl linger is not enabled — without it, rootless Podman
    // containers with --restart=always are NOT automatically restarted after
    // a system reboot (the user's systemd session is simply not started).
    let linger_ok = std::process::Command::new("loginctl")
        .args(["show-user", "--property=Linger"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Linger=yes"))
        .unwrap_or(false);
    if !linger_ok {
        eprintln!("  ⚠️  loginctl linger is not enabled for your user.");
        eprintln!("     Without it, Podman containers won't auto-restart after a reboot.");
        eprintln!("     Enable it with:  loginctl enable-linger");
        eprintln!();
    }

    if no_attach {
        println!("Attach to the Claude session:");
        println!(
            "  nibble sandbox attach {}          (by repo path)",
            abs_repo_path
        );
        println!("  nibble sandbox attach {}   (by task ID)", short_id);
        println!(
            "  nibble sandbox attach {} --fresh  (start a new conversation)",
            short_id
        );
        println!("  (The container keeps running after you exit — re-attach any time)");
        println!();
        println!("After a system reboot, restart stopped containers with:");
        println!("  nibble sandbox resume --all");
    } else {
        let agent_label = if opencode { "opencode" } else { "Claude" };
        println!(
            "Attaching to {} session (exit to detach — container keeps running)…",
            agent_label
        );
        println!(
            "  Re-attach later: nibble sandbox attach {}  (or by path: {})",
            short_id, abs_repo_path
        );
        println!();
        cmd_sandbox_attach(db, task_id.clone(), fresh, false, kimi, glm, opencode)?;
    }

    Ok(task_id)
}

// ── Cron job commands ─────────────────────────────────────────────────────────

fn cmd_cron_add(
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

fn cmd_cron_list(db: &Database, repo_path_filter: Option<String>) -> Result<()> {
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
        "{:<5} {:<10} {:<20} {:<20} {:<10} {:<24} {}",
        "ID", "REPO", "SCHEDULE", "NEXT RUN", "STATUS", "EXPIRES (UTC)", "LABEL"
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

fn cmd_cron_edit(
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

fn cmd_cron_run(db: &Database, id: i64) -> Result<()> {
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
    agent_input::inject(&task, &job.prompt)?;
    println!("Prompt injected successfully.");

    Ok(())
}

/// Find a healthy sandbox for the given repo path. Returns None if no healthy container exists.
fn find_healthy_sandbox_for_repo(db: &Database, repo_path: &str) -> Result<Option<models::Task>> {
    let Some((task_id, container_name)) = db.get_container_state_by_repo_path(repo_path)? else {
        return Ok(None);
    };
    let Some(task) = db.get_task_by_id(&task_id)? else {
        return Ok(None);
    };
    let sandbox = PodmanSandbox::new();
    match sandbox.health_check(&container_name) {
        SandboxHealth::Healthy => Ok(Some(task)),
        _ => Ok(None),
    }
}

/// List all tracked sandboxes, auto-cleaning gone entries.
fn cmd_sandbox_list(db: &Database) -> Result<()> {
    let states = db.list_container_states()?;

    if states.is_empty() {
        println!("No sandbox containers found.");
        println!("Start one with:  nibble sandbox spawn <repo_path>");
        return Ok(());
    }

    let sandbox = PodmanSandbox::new();

    println!(
        "{:<20} {:<18} {:<12} {}",
        "TASK ID", "STARTED", "STATUS", "REPO"
    );
    println!("{}", "─".repeat(82));

    let mut any_gone = false;
    for (task_id, container_name, repo_path, worktree_path, _created) in &states {
        let health = sandbox.health_check(container_name);

        let status = match health {
            SandboxHealth::Healthy => "healthy",
            SandboxHealth::Degraded => "degraded",
            SandboxHealth::Stopped => "stopped",
            SandboxHealth::Dead => {
                // Container is gone — prune state silently
                any_gone = true;
                let _ = db.delete_container_state(task_id);
                continue;
            }
        };

        // Parse timestamp from name: nibble-YYYYMMDD-HHMM-shortid
        let started = container_name
            .strip_prefix("nibble-")
            .and_then(|s| {
                let parts: Vec<&str> = s.splitn(3, '-').collect();
                if parts.len() >= 2 {
                    let date = parts[0];
                    let time = parts[1];
                    if date.len() == 8 && time.len() == 4 {
                        return Some(format!(
                            "{}-{}-{} {}:{}",
                            &date[..4],
                            &date[4..6],
                            &date[6..8],
                            &time[..2],
                            &time[2..4]
                        ));
                    }
                }
                None
            })
            .unwrap_or_else(|| container_name.chars().take(17).collect());

        let short_id = &task_id[..task_id.len().min(8)];

        // For worktree sandboxes, derive and display the branch name from the path suffix.
        let display_path = if worktree_path.is_some() {
            // The worktree path IS the repo_path for worktree sandboxes; extract branch from suffix.
            let branch_hint = std::path::Path::new(repo_path)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|name| {
                    // name is like "myrepo--feature-auth", extract part after first "--"
                    name.find("--").map(|i| &name[i + 2..])
                })
                .unwrap_or("");
            if branch_hint.is_empty() {
                format!("{} [worktree]", repo_path)
            } else {
                format!("{} [branch: {}]", repo_path, branch_hint)
            }
        } else {
            repo_path.clone()
        };

        println!(
            "{:<20} {:<18} {:<12} {}",
            short_id, started, status, display_path
        );
    }

    if any_gone {
        println!("\n(Gone containers were removed from tracking.)");
    }

    Ok(())
}

/// Resolve a user-supplied sandbox identifier to a full task_id.
///
/// Accepts either:
/// - A task ID or prefix (UUID hex string)
/// - A repo path (starts with `.`, `/`, `~`, contains a path separator, or exists as a directory)
///
/// For repo paths the canonical absolute path is looked up in the container_state table,
/// returning the most recently spawned sandbox for that repo.
fn resolve_sandbox_id(db: &Database, input: &str) -> Result<String> {
    // Heuristic: treat as a path if it looks like one or actually exists on disk.
    let looks_like_path = input.starts_with('.')
        || input.starts_with('/')
        || input.starts_with('~')
        || input.contains('/')
        || std::path::Path::new(input).exists();

    if looks_like_path {
        // Expand ~ manually (std::fs::canonicalize won't expand it).
        let expanded = if let Some(rest) = input.strip_prefix("~/") {
            if let Ok(home) = std::env::var("HOME") {
                format!("{}/{}", home, rest)
            } else {
                input.to_string()
            }
        } else {
            input.to_string()
        };

        let canonical = std::fs::canonicalize(&expanded)
            .with_context(|| format!("Cannot resolve path: {}", input))?;
        let path_str = canonical.to_string_lossy();

        let result = db
            .get_container_state_by_repo_path(&path_str)
            .with_context(|| format!("DB error looking up repo path: {}", path_str))?;

        if let Some((task_id, _)) = result {
            return Ok(task_id);
        }

        anyhow::bail!(
            "No sandbox found for repo path: {}\n\
             Start one with:  nibble sandbox spawn {}",
            path_str,
            input
        );
    }

    // Treat as a task ID (or prefix).
    // First try exact match, then prefix scan.
    if db.get_task_by_id(input)?.is_some() {
        return Ok(input.to_string());
    }

    // Prefix match against container_states (more efficient than scanning all tasks).
    let states = db.list_container_states()?;
    let matches: Vec<_> = states
        .iter()
        .filter(|(tid, _, _, _, _)| tid.starts_with(input))
        .collect();

    match matches.len() {
        0 => anyhow::bail!("No sandbox found with ID or path: {}", input),
        1 => Ok(matches[0].0.clone()),
        _ => {
            let ids: Vec<&str> = matches
                .iter()
                .map(|(tid, _, _, _, _)| tid.as_str())
                .collect();
            anyhow::bail!(
                "Ambiguous prefix '{}' matches multiple sandboxes:\n  {}",
                input,
                ids.join("\n  ")
            )
        }
    }
}

/// Resolve a cron job ID-or-label string to a numeric cron job id.
fn resolve_cron_id(db: &Database, id_or_label: &str) -> Result<i64> {
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

/// Attach to the Claude session inside a running sandbox.
///
/// Resumes the session UUID stored on the task (derived deterministically from the repo
/// path at spawn, then updated by the Stop hook after each session). The UUID is stable
/// for the lifetime of the sandbox — `--fresh` wipes the conversation history for that
/// UUID rather than minting a new one, so Telegram injection always uses the same ID.
fn cmd_sandbox_attach(
    db: &Database,
    task_id: String,
    fresh: bool,
    btw: bool,
    kimi: bool,
    glm: bool,
    opencode: bool,
) -> Result<()> {
    let task = db
        .get_task_by_id(&task_id)?
        .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

    if task.sandbox_type != SandboxType::Podman {
        anyhow::bail!("Task {} is not a sandbox task", task_id);
    }

    let container_id = task
        .container_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Task {} has no container_id", task_id))?;

    let sandbox = PodmanSandbox::new();
    match sandbox.status(&container_id)? {
        sandbox::ContainerStatus::Running => {}
        _ => anyhow::bail!("Container {} is not running", container_id),
    }

    // Validate flag combinations before doing anything.
    if opencode {
        if btw {
            anyhow::bail!(
                "--btw is not supported with --opencode (throwaway sessions require Claude Code hooks)"
            );
        }
        if kimi {
            anyhow::bail!(
                "--kimi is not supported with --opencode (Kimi backend routing requires Claude Code)"
            );
        }
        if glm {
            anyhow::bail!(
                "--glm is not supported with --opencode (GLM backend routing requires Claude Code)"
            );
        }
    }

    // Resolve the session ID stored for this task (set by the Claude Stop hook).
    let session_id = task.context.as_ref().and_then(|c| c.session_id.as_deref());

    let shell_cmd = if opencode {
        // opencode mode: session resume via --session <id>, or bare opencode for
        // a fresh start. Hooks / AGENT_TASK_ID are not used — Telegram integration
        // is Claude Code-only for now.
        let opencode = "/home/node/.opencode/bin/opencode";
        if fresh {
            eprintln!("  Session:   starting fresh opencode session");
            format!("cd /workspace && {opencode}")
        } else if let Some(sid) = session_id {
            format!("cd /workspace && {opencode} --session {sid}")
        } else {
            format!("cd /workspace && {opencode}")
        }
    } else {
        let claude = "/home/node/.local/bin/claude --dangerously-skip-permissions";

        // --fresh: delete the .jsonl for the current session so Claude starts with a
        // blank slate while still using the same UUID. The UUID stays stable so
        // Telegram injection keeps working without any DB changes.
        if fresh && !btw {
            if let Some(sid) = session_id {
                backup_session_file(sid);
                eprintln!(
                    "  Session:   {} — previous history backed up, starting fresh",
                    &sid[..8.min(sid.len())]
                );
            } else {
                eprintln!("  Session:   no stored session, starting fresh");
            }
        }

        // Build the shell command that runs inside the container.
        //
        // Normal attach:
        //  1. Try --resume <sid> to load existing history for this repo.
        //  2. Fall back to --session-id <sid> which pins the UUID without requiring
        //     an existing file — Claude creates a new session under that exact UUID.
        //  We never fall back to bare `claude` or `claude --continue` because those
        //  would resume the most-recent session across ALL repos (cross-repo contamination).
        //
        // --btw (throwaway) session:
        //  Start a completely independent session with a fresh random UUID so it
        //  never touches the main session's history. The Stop hook is not injected
        //  (AGENT_TASK_ID is omitted) so the main task's stored session_id is untouched.
        if btw {
            let throwaway_id = uuid::Uuid::new_v4();
            format!("cd /workspace && {claude} --session-id {throwaway_id}")
        } else if let Some(sid) = session_id {
            format!("cd /workspace && {claude} --resume {sid} 2>&1 || {claude} --session-id {sid}")
        } else {
            // No session ID stored yet — start fresh; the Stop hook will record the UUID.
            format!("cd /workspace && {claude}")
        }
    };

    // Build podman exec args, injecting Kimi credentials if requested.
    // KIMI_BASE_URL and KIMI_API_KEY must be set in the host environment
    // (e.g. via the claude-kimi alias definition in ~/.zshrc).
    let mut podman_args: Vec<String> = vec![
        "exec".into(),
        "-it".into(),
        "-e".into(),
        "TERM=xterm-256color".into(),
        "-e".into(),
        "PATH=/home/node/.local/bin:/usr/local/bin:/usr/bin:/bin".into(),
        "-e".into(),
        "CLAUDE_CONFIG_DIR=/home/node/.claude".into(),
    ];

    // --btw sessions are throwaway: omit AGENT_TASK_ID so the Stop/SessionEnd
    // hooks inside the container no-op and don't overwrite the main task's
    // stored session_id.
    // opencode sessions never set AGENT_TASK_ID — hooks are Claude Code-only.
    if !btw && !opencode {
        podman_args.extend(["-e".into(), format!("AGENT_TASK_ID={}", task_id)]);
    }

    // opencode yolo mode: auto-approve all tool calls (equivalent to Claude Code's
    // --dangerously-skip-permissions). Set via OPENCODE_PERMISSION env var rather
    // than a CLI flag — opencode merges this JSON into its permission config.
    // TODO: opencode added --dangerously-skip-permissions in ~April 2026. Once that
    // version is available via the installer, replace the env var with that flag in
    // the shell_cmd above and remove this block.
    if opencode {
        podman_args.extend([
            "-e".into(),
            r#"OPENCODE_PERMISSION={"bash":"allow","edit":"allow","read":"allow","grep":"allow","question":"allow","external_directory":"allow","todowrite":"allow","codesearch":"allow"}"#.into(),
        ]);
    }

    if kimi {
        let base_url = std::env::var("KIMI_BASE_URL")
            .context("--kimi requires KIMI_BASE_URL to be set in the host environment")?;
        let api_key = std::env::var("KIMI_API_KEY")
            .context("--kimi requires KIMI_API_KEY to be set in the host environment")?;
        eprintln!("Using Kimi backend ({})", base_url);
        podman_args.extend([
            "-e".into(),
            format!("ANTHROPIC_BASE_URL={}", base_url),
            "-e".into(),
            format!("ANTHROPIC_API_KEY={}", api_key),
            "-e".into(),
            "ENABLE_TOOL_SEARCH=FALSE".into(),
        ]);
    }

    if glm {
        let base_url = std::env::var("GLM_BASE_URL")
            .context("--glm requires GLM_BASE_URL to be set in the host environment")?;
        let api_key = std::env::var("GLM_API_KEY")
            .context("--glm requires GLM_API_KEY to be set in the host environment")?;
        eprintln!("Using GLM backend ({})", base_url);
        podman_args.extend([
            "-e".into(),
            format!("ANTHROPIC_BASE_URL={}", base_url),
            "-e".into(),
            format!("ANTHROPIC_API_KEY={}", api_key),
            "-e".into(),
            "ENABLE_TOOL_SEARCH=FALSE".into(),
            "-e".into(),
            "ANTHROPIC_DEFAULT_SONNET_MODEL=glm-5.1".into(),
            "-e".into(),
            "ANTHROPIC_DEFAULT_OPUS_MODEL=glm-5.1".into(),
            "-e".into(),
            "ANTHROPIC_DEFAULT_HAIKU_MODEL=glm-5-turbo".into(),
        ]);
    }

    podman_args.extend([
        "-w".into(),
        "/workspace".into(),
        container_id.clone(),
        "/bin/bash".into(),
        "-c".into(),
        shell_cmd,
    ]);

    if opencode {
        eprintln!(
            "Attaching to sandbox {} ({}) [opencode]…",
            task.title, container_id
        );
        eprintln!("(Exit opencode or press Ctrl+C to detach — the container keeps running)");
    } else if btw {
        eprintln!(
            "Attaching to sandbox {} ({}) [btw — throwaway session]…",
            task.title, container_id
        );
        eprintln!("(Independent session — main history untouched. Exit to close.)");
    } else {
        eprintln!("Attaching to sandbox {} ({})…", task.title, container_id);
        eprintln!("(Exit Claude or press Ctrl+C to detach — the container keeps running)");
    }

    let err = std::os::unix::process::CommandExt::exec(
        std::process::Command::new("podman").args(&podman_args),
    );
    anyhow::bail!("Failed to exec podman: {}", err)
}

/// Kill a sandbox container and mark its task as exited.
fn cmd_sandbox_kill(db: &Database, task_id: String) -> Result<()> {
    let mut task = db
        .get_task_by_id(&task_id)?
        .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

    if task.sandbox_type != SandboxType::Podman {
        anyhow::bail!("Task {} is not a sandbox task", task_id);
    }

    let container_id = task
        .container_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Task {} has no container_id", task_id))?;

    PodmanSandbox::new().kill(&container_id)?;
    task.set_exited(None);
    db.update_task(&task)?;
    db.delete_container_state(&task_id)?;

    println!("Killed sandbox {} (task {})", container_id, task_id);

    Ok(())
}

/// Delete old Claude conversation files for a sandbox to free memory.
///
/// Claude Code stores conversation history as .jsonl files under
/// ~/.claude/projects/<hash>/. Each project directory corresponds to a
/// working directory path (hashed). This command finds the right project
/// directory for the sandbox's repo and removes old conversation files,
/// keeping the most recent session unless `--all` is passed.
fn cmd_sandbox_gc(db: &Database, task_id: String, all: bool) -> Result<()> {
    let task = db
        .get_task_by_id(&task_id)?
        .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

    let repo_path = task
        .context
        .as_ref()
        .and_then(|c| c.project_path.as_deref())
        .ok_or_else(|| anyhow::anyhow!("Task {} has no repo path in context", task_id))?;

    let current_session_id = task.context.as_ref().and_then(|c| c.session_id.as_deref());

    // Claude Code hashes the working directory path to produce the project folder name.
    // The hash is a URL-safe base64 of the SHA256 of the canonical path.
    // We find the right folder by scanning ~/.claude/projects/ for a metadata file
    // that references this path, or by matching the known session file names.
    let home = dirs::home_dir().context("Failed to get home directory")?;
    let projects_dir = home.join(".claude").join("projects");

    if !projects_dir.exists() {
        println!(
            "No Claude projects directory found at {}",
            projects_dir.display()
        );
        return Ok(());
    }

    // Find all .jsonl files across all project subdirectories.
    // Each file is named <session-uuid>.jsonl. We identify which project
    // directory belongs to this repo by checking if the current session file exists there.
    let mut target_dir: Option<std::path::PathBuf> = None;

    if let Some(sid) = current_session_id {
        // Fast path: look for the known session file
        for entry in std::fs::read_dir(&projects_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if entry.path().join(format!("{}.jsonl", sid)).exists() {
                target_dir = Some(entry.path());
                break;
            }
        }
    }

    if target_dir.is_none() {
        // Fallback: check all project dirs for any session that matches this repo path
        // by reading the first line of each .jsonl (contains the cwd in Claude's format).
        let canonical_repo = std::fs::canonicalize(repo_path)
            .unwrap_or_else(|_| std::path::PathBuf::from(repo_path));
        'outer: for entry in std::fs::read_dir(&projects_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(entry.path())? {
                let file = file?;
                if file.path().extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(file.path()) {
                    if let Some(first_line) = content.lines().next() {
                        if first_line.contains(canonical_repo.to_string_lossy().as_ref()) {
                            target_dir = Some(entry.path());
                            break 'outer;
                        }
                    }
                }
            }
        }
    }

    let dir = match target_dir {
        Some(d) => d,
        None => {
            println!("No Claude project directory found for repo: {}", repo_path);
            println!("(Conversation may not have started yet, or was already cleaned up.)");
            return Ok(());
        }
    };

    // Collect all files: .jsonl (active sessions) and .jsonl.bak (backed-up sessions).
    // Sort by modification time, oldest first.
    let mut all_files: Vec<(std::path::PathBuf, std::time::SystemTime, bool)> =
        std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                let name = path.file_name()?.to_string_lossy().to_string();
                let is_bak = name.ends_with(".jsonl.bak");
                let is_jsonl = !is_bak && name.ends_with(".jsonl");
                if !is_jsonl && !is_bak {
                    return None;
                }
                let mtime = e.metadata().ok()?.modified().ok()?;
                Some((path, mtime, is_bak))
            })
            .collect();

    all_files.sort_by_key(|(_, mtime, _)| *mtime);

    let jsonl_count = all_files.iter().filter(|(_, _, is_bak)| !is_bak).count();
    let bak_count = all_files.iter().filter(|(_, _, is_bak)| *is_bak).count();

    if all_files.is_empty() {
        println!("No conversation files found in {}", dir.display());
        return Ok(());
    }

    // Backups (.bak) are always deleted — they exist only as a safety net for --fresh.
    // For active .jsonl files: keep the most recent one unless --all is passed.
    let to_delete: Vec<_> = if all {
        all_files
    } else {
        // Split: delete all .bak + all .jsonl except the most recent active one.
        let last_jsonl_idx = all_files.iter().rposition(|(_, _, is_bak)| !is_bak);
        all_files
            .into_iter()
            .enumerate()
            .filter(|(i, (_, _, is_bak))| *is_bak || Some(*i) != last_jsonl_idx)
            .map(|(_, f)| f)
            .collect()
    };

    if to_delete.is_empty() {
        println!("Nothing to delete (1 active session, no backups). Use --all to wipe.");
        return Ok(());
    }

    let mut deleted = 0u64;
    let mut freed_bytes: u64 = 0;
    for (path, _, _) in &to_delete {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(path) {
            Ok(()) => {
                deleted += 1;
                freed_bytes += size;
            }
            Err(e) => eprintln!("  Warning: could not delete {}: {}", path.display(), e),
        }
    }

    let freed_mb = freed_bytes as f64 / 1_048_576.0;
    println!(
        "GC: deleted {} file(s) ({} session(s), {} backup(s)), freed {:.1} MB  [{}]",
        deleted,
        jsonl_count.saturating_sub(if all { 0 } else { 1 }),
        bak_count,
        freed_mb,
        dir.display()
    );
    if !all {
        println!("  Most recent session kept. Use --all to wipe everything.");
    }

    Ok(())
}

/// Kill all running sandbox containers.
fn cmd_sandbox_kill_all(db: &Database) -> Result<()> {
    let sandbox = PodmanSandbox::new();
    let states = db.list_container_states()?;

    if states.is_empty() {
        println!("No sandbox agents to kill.");
        return Ok(());
    }

    let mut killed = 0;
    for (task_id, container_name, _, _, _) in &states {
        match sandbox.kill(container_name) {
            Ok(()) => {
                if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                    task.set_exited(None);
                    let _ = db.update_task(&task);
                }
                let _ = db.delete_container_state(task_id);
                println!("Killed {}", container_name);
                killed += 1;
            }
            Err(e) => eprintln!("Failed to kill {}: {}", container_name, e),
        }
    }

    println!("Killed {} sandbox(es)", killed);
    Ok(())
}

/// Re-sync sandbox state with running containers after a reboot.
fn cmd_sandbox_resume(db: &Database, all: bool) -> Result<()> {
    if !all {
        eprintln!("Use --all to resume all recoverable sandbox agents.");
        return Ok(());
    }

    let sandbox = PodmanSandbox::new();
    let states = db.list_container_states()?;

    if states.is_empty() {
        println!("No sandbox agents to resume.");
        return Ok(());
    }

    let mut resumed = 0;
    let mut stale = 0;

    for (task_id, container_name, repo_path, _, _) in &states {
        match sandbox.health_check(container_name) {
            SandboxHealth::Healthy => {
                if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                    if task.status != TaskStatus::Running {
                        task.set_running();
                        let _ = db.update_task(&task);
                    }
                }
                println!("  Healthy: {} ({})", container_name, task_id);
                resumed += 1;
            }
            SandboxHealth::Degraded => {
                // Container alive but Claude session gone — keep container, mark task exited.
                if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                    task.set_exited(None);
                    let _ = db.update_task(&task);
                }
                println!(
                    "  Degraded: {} (container up, Claude session gone, repo: {})",
                    container_name, repo_path
                );
                stale += 1;
            }
            SandboxHealth::Stopped => {
                // Container stopped (e.g. host reboot) — try to restart it.
                match sandbox.start(container_name) {
                    Ok(()) => match sandbox.health_check(container_name) {
                        SandboxHealth::Healthy => {
                            if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                                if task.status != TaskStatus::Running {
                                    task.set_running();
                                    let _ = db.update_task(&task);
                                }
                            }
                            println!("  Restarted: {} ({})", container_name, repo_path);
                            resumed += 1;
                        }
                        _ => {
                            if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                                task.set_exited(None);
                                let _ = db.update_task(&task);
                            }
                            let _ = db.delete_container_state(task_id);
                            println!(
                                "  Cleaned: {} (start failed health check, repo: {})",
                                container_name, repo_path
                            );
                            stale += 1;
                        }
                    },
                    Err(e) => {
                        eprintln!("  Failed to restart {container_name}: {e:#}");
                        if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                            task.set_exited(None);
                            let _ = db.update_task(&task);
                        }
                        let _ = db.delete_container_state(task_id);
                        println!(
                            "  Cleaned: {} (start error, repo: {})",
                            container_name, repo_path
                        );
                        stale += 1;
                    }
                }
            }
            SandboxHealth::Dead => {
                // Container no longer exists — clean up DB state.
                if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                    task.set_exited(None);
                    let _ = db.update_task(&task);
                }
                let _ = db.delete_container_state(task_id);
                println!("  Cleaned: {} (gone, repo: {})", container_name, repo_path);
                stale += 1;
            }
        }
    }

    println!("\n{} running, {} stale cleaned up.", resumed, stale);
    Ok(())
}

// ── Stale task pruning ────────────────────────────────────────────────────────

/// Mark Running tasks as Exited when their process (PID) is no longer alive,
/// and clean up sandbox DB state for containers that have disappeared.
///
/// Returns the number of tasks pruned.
pub(crate) fn prune_stale_tasks(db: &Database) -> Result<usize> {
    let mut pruned = 0;

    // 1. Non-sandbox tasks: check PID liveness.
    let running = db.list_tasks(Some(TaskStatus::Running))?;
    for mut task in running {
        if task.sandbox_type != SandboxType::Podman {
            if let Some(pid) = task.pid {
                let alive = std::path::Path::new(&format!("/proc/{}", pid)).exists();
                if !alive {
                    task.set_exited(None);
                    db.update_task(&task)?;
                    eprintln!(
                        "[prune] Task {} (pid {}) is dead → exited",
                        &task.task_id[..8.min(task.task_id.len())],
                        pid
                    );
                    pruned += 1;
                }
            }
        }
    }

    // 2. Sandbox tasks: health-check each container.
    //    - Dead      → container crashed; notify via Telegram, clean up state.
    //    - Degraded  → container running but exec fails; update DB only.
    //    - Healthy   → all good, leave it alone.
    let states = db.list_container_states()?;
    if !states.is_empty() {
        let sandbox = PodmanSandbox::new();
        let cfg = config::load().unwrap_or_default();
        for (task_id, container_name, repo_path, _, _) in &states {
            match sandbox.health_check(container_name) {
                SandboxHealth::Healthy => {}
                SandboxHealth::Stopped => {
                    // Container stopped (reboot) — try to restart silently.
                    eprintln!(
                        "[prune] Sandbox {} stopped → attempting restart",
                        container_name
                    );
                    let restarted = match sandbox.start(container_name) {
                        Ok(()) => {
                            // Give the container a moment to fully start before
                            // health-checking — avoids a false "failed" due to a
                            // race between `podman start` and the container being
                            // ready to accept `exec` commands.
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            sandbox.health_check(container_name) == SandboxHealth::Healthy
                        }
                        Err(_) => false,
                    };
                    if restarted {
                        eprintln!("[prune] Sandbox {} restarted successfully", container_name);
                        if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                            if task.status != TaskStatus::Running {
                                task.set_running();
                                let _ = db.update_task(&task);
                            }
                        }
                    } else {
                        // Could not restart right now (e.g. podman socket not yet
                        // ready after boot). Keep the container_state record so the
                        // next prune cycle can try again. Only update task status.
                        if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                            if task.status == TaskStatus::Running {
                                task.set_exited(None);
                                let _ = db.update_task(&task);
                                pruned += 1;
                            }
                        }
                        eprintln!(
                            "[prune] Sandbox {} could not be restarted — will retry next cycle",
                            container_name
                        );
                    }
                }
                SandboxHealth::Dead => {
                    let _ = db.delete_container_state(task_id);
                    if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                        task.set_exited(None);
                        let _ = db.update_task(&task);
                        eprintln!(
                            "[prune] Sandbox {} dead → exited task {}",
                            container_name,
                            &task_id[..8.min(task_id.len())]
                        );
                        pruned += 1;

                        // Notify: container crash is the one case the user
                        // needs to know about regardless of what they were doing.
                        if cfg.telegram.is_configured() {
                            let repo_label = std::path::Path::new(repo_path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(repo_path.as_str());
                            let msg = format!("💥 Sandbox for `{}` crashed or was killed by the OS.\nRe-spawn with: `nibble sandbox spawn {}`", repo_label, repo_path);
                            if let Ok(text) =
                                build_notification_text(db, Some(task_id), &msg, false)
                            {
                                let _ = notifications::telegram::send(&cfg.telegram, &text);
                            }
                        }
                    }
                }
                SandboxHealth::Degraded => {
                    // Container alive but exec fails — update task status only.
                    // Keep the container_state record so `nibble list` still shows
                    // it and the user can investigate or kill it explicitly.
                    if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                        if task.status == TaskStatus::Running {
                            task.set_exited(None);
                            let _ = db.update_task(&task);
                            eprintln!(
                                "[prune] Sandbox {} degraded → exited task {}",
                                container_name,
                                &task_id[..8.min(task_id.len())]
                            );
                            pruned += 1;
                        }
                    }
                }
            }
        }
    }

    Ok(pruned)
}

// ── Sandbox context injection ─────────────────────────────────────────────────

/// Detect the project toolchain from files present in the repo directory.
///
/// Returns a list of (ecosystem, install_command, run_hint) tuples for every
/// recognised manifest found so Claude can install deps and run the project
/// without guessing.
fn detect_toolchains(
    repo_path: &std::path::Path,
) -> Vec<(&'static str, &'static str, &'static str)> {
    let checks: &[(&str, &str, &str, &str)] = &[
        // (manifest file, ecosystem label, install cmd, run hint)
        (
            "package.json",
            "Node.js",
            "npm install",
            "npm start / npm test / npm run dev",
        ),
        (
            "yarn.lock",
            "Node.js",
            "yarn install",
            "yarn start / yarn test / yarn dev",
        ),
        (
            "pnpm-lock.yaml",
            "Node.js",
            "pnpm install",
            "pnpm start / pnpm test / pnpm dev",
        ),
        (
            "Cargo.toml",
            "Rust",
            "cargo build  # rustup + cargo pre-installed by .nibble/setup.sh; binary at ~/.cargo/bin/cargo",
            "cargo run / cargo test",
        ),
        (
            "go.mod",
            "Go",
            "go mod download",
            "go run . / go test ./...",
        ),
        (
            "requirements.txt",
            "Python",
            "pip install -r requirements.txt",
            "python main.py / pytest",
        ),
        (
            "pyproject.toml",
            "Python",
            "pip install -e .",
            "python -m pytest / python -m <module>",
        ),
        (
            "Pipfile",
            "Python",
            "pipenv install",
            "pipenv run python ... / pipenv run pytest",
        ),
        (
            "composer.json",
            "PHP",
            "composer install",
            "php artisan serve / php -S localhost:8000",
        ),
        (
            "Gemfile",
            "Ruby",
            "bundle install",
            "bundle exec rails s / bundle exec rspec",
        ),
        (
            "build.gradle",
            "JVM",
            "./gradlew build",
            "./gradlew run / ./gradlew test",
        ),
        (
            "pom.xml",
            "JVM",
            "mvn install -DskipTests",
            "mvn exec:java / mvn test",
        ),
        ("mix.exs", "Elixir", "mix deps.get", "mix run / mix test"),
        ("Makefile", "Make", "make", "make run / make test"),
    ];

    // Deduplicate: if yarn.lock is present package.json will also be — prefer
    // the more specific lock-file entry over the generic one.
    let mut seen_ecosystems = std::collections::HashSet::new();
    let mut results = Vec::new();

    for (manifest, ecosystem, install, run_hint) in checks {
        if repo_path.join(manifest).exists() && seen_ecosystems.insert(*ecosystem) {
            results.push((*ecosystem, *install, *run_hint));
        }
    }

    results
}

/// Build the AGENTS.md content written to `/workspace/AGENTS.md` inside the
/// container.  This is the **primary** agent instruction file — OpenCode reads
/// it natively and Claude Code reads it via the `@../AGENTS.md` import in CLAUDE.md.
///
/// The content covers sandbox environment, toolchain setup, and (when factory is
/// enabled) the full AI Factory pipeline instructions.
/// Static sandbox instruction fragments embedded at compile time.
/// Edit the .md files under `src/sandbox_instructions/` — never edit here.
mod sandbox_instructions {
    pub const BASE: &str = include_str!("sandbox_instructions/base.md");
    pub const GENERAL_PRINCIPLES: &str = include_str!("sandbox_instructions/general_principles.md");
    pub const FACTORY: &str = include_str!("sandbox_instructions/factory.md");
}

fn build_sandbox_agents_md(
    repo_name: &str,
    toolchains: &[(&str, &str, &str)],
    factory_enabled: bool,
) -> String {
    let mut out = String::new();

    // Header (repo name is dynamic, so it stays inline)
    out.push_str("# nibble Sandbox Agent Instructions\n\n");
    out.push_str(&format!(
        "You are running inside an isolated Podman sandbox managed by **nibble** for the **{}** project. \
         This file contains all instructions for how to operate inside this environment. \
         Read it fully before starting any task.\n\n",
        repo_name
    ));

    // Static: environment bullets + toolchain setup preamble
    out.push_str(sandbox_instructions::BASE);
    out.push('\n');

    // Dynamic: detected toolchain table (parametric, cannot be static)
    if toolchains.is_empty() {
        out.push_str("No recognised dependency manifest was found in the repo root.\n");
        out.push_str(
            "Inspect the project structure and install any required tools before running or testing.\n",
        );
    } else {
        out.push_str("The following dependency manifests were detected:\n\n");
        out.push_str("| Manifest | Install command | Run/test |\n");
        out.push_str("|----------|----------------|----------|\n");
        for (ecosystem, install_cmd, run_hint) in toolchains {
            out.push_str(&format!(
                "| {} | `{}` | `{}` |\n",
                ecosystem, install_cmd, run_hint
            ));
        }
        out.push('\n');
        out.push_str(
            "If a command fails due to missing system tools, install them with \
             `sudo apt-get install <package>`.\n",
        );
    }

    // Static: general working principles
    out.push('\n');
    out.push_str(sandbox_instructions::GENERAL_PRINCIPLES);

    // Static: factory pipeline instructions (only when factory is enabled)
    if factory_enabled {
        out.push('\n');
        out.push_str(sandbox_instructions::FACTORY);
    }

    out
}

/// Write nibble's sandbox instructions into `AGENTS.md` and `.claude/CLAUDE.md`
/// inside the container, **without clobbering any existing repo content**.
///
/// **AGENTS.md** (`/workspace/AGENTS.md`):
/// - Nibble's content is wrapped in sentinel comments so it can be updated
///   idempotently without touching the rest of the file:
///   ```
///   <!-- nibble-sandbox:begin -->
///   ...nibble instructions...
///   <!-- nibble-sandbox:end -->
///   ```
/// - If the file does not exist → created containing only the sentinel block.
/// - If the file exists and the sentinel block is present → the block is
///   replaced in-place; everything outside the sentinels is preserved.
/// - If the file exists but has no sentinel → the block is appended at the end;
///   existing repo content is left completely untouched.
///
/// **CLAUDE.md** (`/workspace/.claude/CLAUDE.md`):
/// - Contains `@../AGENTS.md` as the first line (safe-prepend, never overwrites).
/// - If the file already exists with `@../AGENTS.md` at line 1, it is left untouched.
/// - If `@../AGENTS.md` is missing from line 1, it is prepended — user content below is preserved.
fn inject_sandbox_claude_md(container_id: &str, agents_content: &str) -> Result<()> {
    let escaped_agents = agents_content.replace('\'', "'\\''");

    let script = format!(
        r#"set -e
mkdir -p /workspace/.claude

# ── Update AGENTS.md using sentinel block (never overwrites repo content) ──────
AGENTS_FILE=/workspace/AGENTS.md
BEGIN_SENTINEL='<!-- nibble-sandbox:begin -->'
END_SENTINEL='<!-- nibble-sandbox:end -->'
NIBBLE_BLOCK=$(printf '%s\n%s\n%s\n' "$BEGIN_SENTINEL" '{agents}' "$END_SENTINEL")

if [ ! -f "$AGENTS_FILE" ]; then
    # File does not exist — create it with just the sentinel block.
    printf '%s\n' "$NIBBLE_BLOCK" > "$AGENTS_FILE"
elif grep -qF "$BEGIN_SENTINEL" "$AGENTS_FILE" 2>/dev/null; then
    # Sentinel is present — replace the block in-place, preserving everything outside.
    # Write the new block to a temp file so awk can read it without newline-escaping issues.
    BLOCK_TMP=$(mktemp)
    printf '%s\n' "$NIBBLE_BLOCK" > "$BLOCK_TMP"
    TMP=$(mktemp)
    awk -v begin="$BEGIN_SENTINEL" -v end="$END_SENTINEL" -v blockfile="$BLOCK_TMP" '
        $0 == begin {{ in_block=1; while ((getline line < blockfile) > 0) print line; next }}
        $0 == end   {{ in_block=0; next }}
        !in_block   {{ print }}
    ' "$AGENTS_FILE" > "$TMP"
    rm -f "$BLOCK_TMP"
    mv "$TMP" "$AGENTS_FILE"
else
    # No sentinel found — append the block; existing content is untouched.
    printf '\n%s\n' "$NIBBLE_BLOCK" >> "$AGENTS_FILE"
fi

# ── Update .claude/CLAUDE.md (Claude Code entrypoint) ─────────────────────────
TARGET=/workspace/.claude/CLAUDE.md
IMPORT_LINE='@../AGENTS.md'

if [ ! -f "$TARGET" ]; then
    printf '%s\n' "$IMPORT_LINE" > "$TARGET"
else
    # Ensure @../AGENTS.md is present as the very first line.
    # Checking only the first line (via head -1) prevents a false match when
    # "@../AGENTS.md" appears in the file body (e.g. inside a comment or example).
    if ! head -1 "$TARGET" | grep -qF "$IMPORT_LINE" 2>/dev/null; then
        TMP=$(mktemp)
        printf '%s\n' "$IMPORT_LINE" > "$TMP"
        cat "$TARGET" >> "$TMP"
        mv "$TMP" "$TARGET"
    fi
fi"#,
        agents = escaped_agents,
    );

    let output = std::process::Command::new("podman")
        .args(["exec", container_id, "/bin/bash", "-c", &script])
        .output()
        .context("Failed to write AGENTS.md / CLAUDE.md into container")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Writing AGENTS.md/CLAUDE.md failed: {}", stderr.trim());
    }

    Ok(())
}

// ── Notification helpers ──────────────────────────────────────────────────────

/// Build the full notification text sent to Telegram.
///
/// When a `task_id` is supplied and found in the database a rich header is
/// prepended so the user can immediately identify which agent and repo the
/// message comes from without reading the body.
///
/// `attention` = true produces a visually distinct header for permission
/// requests and questions that require an immediate response.
pub(crate) fn build_notification_text(
    db: &Database,
    task_id: Option<&str>,
    message: &str,
    attention: bool,
) -> Result<String> {
    if let Some(id) = task_id {
        if let Some(task) = db.get_task_by_id(id)? {
            let header = format_header(&task, attention);
            let divider = "─".repeat(28);
            return Ok(format!("{}\n{}\n{}", header, divider, message));
        }
    }

    // No task context — still show the attention banner if requested.
    if attention {
        let banner = "🚨 <b>Agent needs your attention</b>";
        let divider = "─".repeat(28);
        return Ok(format!("{}\n{}\n{}", banner, divider, message));
    }

    Ok(message.to_string())
}

/// Build the rich header block for a task notification.
///
/// `attention` = true adds a prominent "needs your attention" banner above the
/// standard context lines so it is immediately obvious on a phone screen.
///
/// Normal completion example:
/// ```
/// 🤖 Claude Code
/// 📁 nibble · main
/// ⏱ 4m 32s
/// ```
///
/// Attention example:
/// ```
/// 🚨 Needs your attention
/// 🤖 Claude Code
/// 📁 nibble · main
/// ⏱ 4m 32s
/// ```
fn format_header(task: &Task, attention: bool) -> String {
    let (agent_emoji, agent_label) = agent_display(&task.agent_type);
    let elapsed = format_elapsed(task);
    let location = format_location(task);

    let mut lines: Vec<String> = Vec::new();

    if attention {
        lines.push("🚨 <b>Needs your attention</b>".to_string());
    }

    lines.push(format!("{} <b>{}</b>", agent_emoji, agent_label));
    lines.push(format!("📁 {}", location));
    lines.push(format!("⏱ {}", elapsed));

    // Append status only when it carries meaning (exited = something went wrong).
    if task.status == TaskStatus::Exited {
        let exit_str = task
            .exit_code
            .map(|c| format!(" (exit {})", c))
            .unwrap_or_default();
        lines.push(format!("⚠️ <i>session ended{}</i>", exit_str));
    }

    lines.join("\n")
}

/// Returns (emoji, display label) for an agent type string.
fn agent_display(agent_type: &str) -> (&'static str, String) {
    match agent_type {
        "claude_code" => ("🤖", "Claude Code".to_string()),
        "opencode" => ("⚡", "OpenCode".to_string()),
        "claude_web" => ("🌐", "Claude Web".to_string()),
        "gemini_web" => ("✨", "Gemini".to_string()),
        other => ("🔧", other.to_string()),
    }
}

/// Derive a human-readable location string from the task.
///
/// The wrapper sets `title` as `[repo:branch]` or `[dirname]`.  We parse that
/// to get repo + branch, and fall back to the project path from context.
fn format_location(task: &Task) -> String {
    // Try to parse [repo:branch] or [repo] from the title.
    let title = task.title.trim();
    if title.starts_with('[') && title.ends_with(']') {
        let inner = &title[1..title.len() - 1];
        if let Some((repo, branch)) = inner.split_once(':') {
            return format!("<code>{}</code> · <i>{}</i>", repo, branch);
        }
        // [dirname] — no branch info
        return format!("<code>{}</code>", inner);
    }

    // Fallback: use project_path from context, show only the last component.
    if let Some(ctx) = &task.context {
        if let Some(path) = &ctx.project_path {
            let dir = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path.as_str());
            return format!("<code>{}</code>", dir);
        }
    }

    // Last resort: show raw title.
    title.to_string()
}

/// Format the elapsed time since task creation as a human-readable string.
fn format_elapsed(task: &Task) -> String {
    let secs = (chrono::Utc::now() - task.created_at).num_seconds().max(0) as u64;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    use crate::models::{Task, TaskStatus};

    fn make_task(agent_type: &str, title: &str) -> Task {
        Task::new(
            "test-id".to_string(),
            agent_type.to_string(),
            title.to_string(),
            None,
            None,
        )
    }

    #[test]
    fn test_format_location_repo_branch() {
        let task = make_task("claude_code", "[nibble:main]");
        let loc = format_location(&task);
        assert!(loc.contains("nibble"));
        assert!(loc.contains("main"));
    }

    #[test]
    fn test_format_location_repo_only() {
        let task = make_task("claude_code", "[my-project]");
        let loc = format_location(&task);
        assert!(loc.contains("my-project"));
    }

    #[test]
    fn test_format_location_fallback_to_path() {
        use crate::models::TaskContext;
        use std::collections::HashMap;
        let mut task = make_task("opencode", "opencode (interactive)");
        task.context = Some(TaskContext {
            url: None,
            project_path: Some("/home/user/projects/my-app".to_string()),
            session_id: None,
            extra: HashMap::new(),
        });
        let loc = format_location(&task);
        assert!(loc.contains("my-app"));
    }

    #[test]
    fn test_agent_display_known_types() {
        assert_eq!(
            agent_display("claude_code"),
            ("🤖", "Claude Code".to_string())
        );
        assert_eq!(agent_display("opencode"), ("⚡", "OpenCode".to_string()));
        assert_eq!(
            agent_display("claude_web"),
            ("🌐", "Claude Web".to_string())
        );
        assert_eq!(agent_display("gemini_web"), ("✨", "Gemini".to_string()));
    }

    #[test]
    fn test_agent_display_unknown_type() {
        let (emoji, label) = agent_display("my_custom_agent");
        assert_eq!(emoji, "🔧");
        assert_eq!(label, "my_custom_agent".to_string());
    }

    #[test]
    fn test_format_elapsed_seconds() {
        let task = make_task("claude_code", "[repo:main]");
        // Task was just created so elapsed should be ~0s
        let elapsed = format_elapsed(&task);
        assert!(elapsed.ends_with('s'));
    }

    #[test]
    fn test_header_contains_agent_and_location() {
        let task = make_task("claude_code", "[myrepo:feature-x]");
        let header = format_header(&task, false);
        assert!(header.contains("Claude Code"));
        assert!(header.contains("myrepo"));
        assert!(header.contains("feature-x"));
        assert!(header.contains('⏱'));
        assert!(header.contains('📁'));
    }

    #[test]
    fn test_header_exited_task_shows_warning() {
        let mut task = make_task("opencode", "[proj:main]");
        task.status = TaskStatus::Exited;
        task.exit_code = Some(1);
        let header = format_header(&task, false);
        assert!(header.contains('⚠'));
        assert!(header.contains("exit 1"));
    }

    #[test]
    fn test_header_attention_shows_banner() {
        let task = make_task("claude_code", "[myrepo:main]");
        let header = format_header(&task, true);
        assert!(header.contains('🚨'));
        assert!(header.contains("Needs your attention"));
        // Standard fields still present
        assert!(header.contains("Claude Code"));
        assert!(header.contains("myrepo"));
    }

    #[test]
    fn test_attention_without_task_id_shows_banner() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let db = crate::db::Database::open(&db_path).unwrap();
        let text = build_notification_text(&db, None, "need permission", true).unwrap();
        assert!(text.contains('🚨'));
        assert!(text.contains("need permission"));
    }

    // ── build_sandbox_agents_md tests ─────────────────────────────────────────

    #[test]
    fn test_agents_md_factory_enabled_contains_pipeline() {
        let out = build_sandbox_agents_md("myrepo", &[], true);
        assert!(
            out.contains("AI Factory Pipeline"),
            "factory section missing when factory_enabled=true"
        );
        assert!(
            out.contains("factory-spec"),
            "stage skills should be listed"
        );
        assert!(out.contains("QA Gate"), "QA Gate mention should be present");
    }

    #[test]
    fn test_agents_md_factory_disabled_no_pipeline() {
        let out = build_sandbox_agents_md("myrepo", &[], false);
        assert!(
            !out.contains("AI Factory Pipeline"),
            "factory section must be absent when factory_enabled=false"
        );
        assert!(
            !out.contains("factory-spec"),
            "stage skills must not appear when disabled"
        );
    }

    #[test]
    fn test_agents_md_contains_repo_name() {
        let out = build_sandbox_agents_md("my-cool-project", &[], false);
        assert!(
            out.contains("my-cool-project"),
            "repo name should appear in AGENTS.md header"
        );
    }

    #[test]
    fn test_agents_md_toolchain_table_present() {
        let toolchains = [("Rust", "cargo build", "cargo test")];
        let out = build_sandbox_agents_md("proj", &toolchains, false);
        assert!(
            out.contains("cargo build"),
            "install command should be in toolchain table"
        );
        assert!(
            out.contains("cargo test"),
            "run hint should be in toolchain table"
        );
    }

    #[test]
    fn test_agents_md_no_toolchain_fallback_message() {
        let out = build_sandbox_agents_md("proj", &[], false);
        assert!(
            out.contains("No recognised dependency manifest"),
            "should show fallback message when no toolchain detected"
        );
    }

    // ── CLAUDE.md content tests ───────────────────────────────────────────────
    // CLAUDE.md is now just "@../AGENTS.md" — all content lives in AGENTS.md.
    // These tests verify that the agents_md produced for factory-enabled sandboxes
    // contains the information that used to live in the nibble delimiter block.

    #[test]
    fn test_agents_md_contains_toolchain_info() {
        // Toolchain info must be in AGENTS.md (not duplicated in a CLAUDE.md block)
        let toolchains = [("Node.js", "npm install", "npm test")];
        let out = build_sandbox_agents_md("proj", &toolchains, false);
        assert!(
            out.contains("npm install"),
            "toolchain install command must be in AGENTS.md"
        );
        assert!(
            out.contains("npm test"),
            "toolchain run hint must be in AGENTS.md"
        );
    }

    #[test]
    fn test_agents_md_is_single_source_of_truth() {
        // AGENTS.md must contain the repo name and environment info —
        // everything Claude Code needs is here, imported via @../AGENTS.md in CLAUDE.md
        let out = build_sandbox_agents_md("my-cool-project", &[], false);
        assert!(
            out.contains("my-cool-project"),
            "repo name must appear in AGENTS.md"
        );
        assert!(
            out.contains("Working directory"),
            "environment section must be present"
        );
    }

    // ── Sentinel block tests ──────────────────────────────────────────────────
    // These tests verify the shell logic in inject_sandbox_claude_md by
    // simulating the three cases in pure Rust (no container needed).

    fn apply_sentinel_logic(existing: Option<&str>, nibble_content: &str) -> String {
        let begin = "<!-- nibble-sandbox:begin -->";
        let end = "<!-- nibble-sandbox:end -->";
        let block = format!("{}\n{}\n{}", begin, nibble_content, end);

        match existing {
            None => format!("{}\n", block),
            Some(file) if file.contains(begin) => {
                // Replace in-place: keep lines outside sentinels, substitute block.
                let mut out = String::new();
                let mut in_block = false;
                let mut replaced = false;
                for line in file.lines() {
                    if line == begin {
                        in_block = true;
                        if !replaced {
                            out.push_str(&block);
                            out.push('\n');
                            replaced = true;
                        }
                        continue;
                    }
                    if line == end {
                        in_block = false;
                        continue;
                    }
                    if !in_block {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                out
            }
            Some(file) => {
                // Append: existing content untouched, block added at end.
                format!("{}\n\n{}\n", file, block)
            }
        }
    }

    #[test]
    fn test_sentinel_creates_file_when_absent() {
        let result = apply_sentinel_logic(None, "nibble instructions here");
        assert!(result.contains("<!-- nibble-sandbox:begin -->"));
        assert!(result.contains("nibble instructions here"));
        assert!(result.contains("<!-- nibble-sandbox:end -->"));
    }

    #[test]
    fn test_sentinel_replaces_block_in_place() {
        let existing = "# My Project\n\nSome docs.\n\n<!-- nibble-sandbox:begin -->\nold content\n<!-- nibble-sandbox:end -->\n\nMore docs.\n";
        let result = apply_sentinel_logic(Some(existing), "new nibble content");
        assert!(
            result.contains("# My Project"),
            "repo content before sentinel must be preserved"
        );
        assert!(
            result.contains("More docs."),
            "repo content after sentinel must be preserved"
        );
        assert!(
            result.contains("new nibble content"),
            "new nibble content must be present"
        );
        assert!(
            !result.contains("old content"),
            "old nibble content must be gone"
        );
    }

    #[test]
    fn test_sentinel_appends_when_no_sentinel_present() {
        let existing = "# My Project\n\nThis is the real AGENTS.md.\n";
        let result = apply_sentinel_logic(Some(existing), "nibble instructions");
        assert!(
            result.starts_with("# My Project"),
            "original content must come first"
        );
        assert!(
            result.contains("This is the real AGENTS.md."),
            "original content must be preserved"
        );
        assert!(
            result.contains("<!-- nibble-sandbox:begin -->"),
            "sentinel begin must be appended"
        );
        assert!(
            result.contains("nibble instructions"),
            "nibble content must be appended"
        );
    }

    #[test]
    fn test_sentinel_replace_is_idempotent() {
        let existing = "# Header\n\n<!-- nibble-sandbox:begin -->\nv1 content\n<!-- nibble-sandbox:end -->\n\nFooter\n";
        let after_first = apply_sentinel_logic(Some(existing), "v2 content");
        let after_second = apply_sentinel_logic(Some(&after_first), "v2 content");
        assert_eq!(
            after_first, after_second,
            "applying sentinel twice with same content must be idempotent"
        );
    }
}
