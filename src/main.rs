mod agent_input;
mod cli;
mod config;
mod db;
mod display;
mod models;
mod monitor;
mod notifications;
mod sandbox;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands, ReportAction};
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
                let status = TaskStatus::from_str(&status_str)
                    .map_err(|e| anyhow::anyhow!(e))?;
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
            ReportAction::SessionId { task_id, session_id } => {
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
        Some(Commands::Notify { message, task_id, attention }) => {
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
        // ── Internal sandbox subcommands (invoked by agent-sandbox script) ────

        Some(Commands::SandboxSpawn { repo_path, task, image, fresh, session_id }) => {
            cmd_sandbox_spawn(&db, repo_path, task, image, fresh, session_id, false, false)?;
        }
        Some(Commands::SandboxList) => {
            cmd_sandbox_list(&db)?;
        }
        Some(Commands::SandboxAttach { task_id_or_path, fresh, kimi }) => {
            match resolve_sandbox_id(&db, &task_id_or_path) {
                Ok(task_id) => {
                    cmd_sandbox_attach(&db, task_id, fresh, kimi)?;
                }
                Err(e) => {
                    // If the input looks like a repo path and no sandbox exists,
                    // spawn one and then attach to it.
                    let looks_like_path = task_id_or_path.starts_with('.')
                        || task_id_or_path.starts_with('/')
                        || task_id_or_path.starts_with('~')
                        || task_id_or_path.contains('/')
                        || std::path::Path::new(&task_id_or_path).exists();

                    if looks_like_path {
                        eprintln!("No sandbox found for '{}', spawning one...", task_id_or_path);
                        let task_id = cmd_sandbox_spawn(
                            &db,
                            task_id_or_path,
                            None, // task_desc
                            "agent-inbox-sandbox:latest".to_string(),
                            fresh,
                            None, // session_id
                            false, // no_attach - we will attach below
                            kimi, // pass through kimi flag
                        )?;
                        cmd_sandbox_attach(&db, task_id, fresh, kimi)?;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        Some(Commands::SandboxKill { task_id_or_path, all }) => {
            if all {
                cmd_sandbox_kill_all(&db)?;
            } else {
                let input = task_id_or_path.ok_or_else(|| anyhow::anyhow!("Provide a task ID, repo path, or --all"))?;
                let id = resolve_sandbox_id(&db, &input)?;
                cmd_sandbox_kill(&db, id)?;
            }
        }
        Some(Commands::SandboxRestart) => {
            cmd_sandbox_resume(&db, true)?;
        }
        Some(Commands::SandboxResume { all }) => {
            cmd_sandbox_resume(&db, all)?;
        }
        Some(Commands::SandboxBuild { image, rebuild }) => {
            let sandbox = PodmanSandbox::new();
            sandbox.ensure_image_with_opts(&image, rebuild)?;
            println!("Sandbox image ready.");
        }
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
                anyhow::bail!(
                    "Telegram is not configured. Run scripts/setup-telegram.sh first."
                );
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

/// Find the most recent Claude Code session for a given repo path.
///
/// First checks the database for any task with this repo path that has a stored
/// session_id (enables repo-level conversation continuity across sandboxes).
/// Falls back to scanning ~/.claude/projects/<url-encoded-path>/ for host-side
/// sessions if no DB record exists.
///
/// This ensures repo-level isolation: sessions from repo A will never bleed into
/// repo B, even though all sandboxes store sessions under the same "-workspace"
/// Derive a deterministic UUID v5 for a repo path.
///
/// Every sandbox mounting the same repo will always get the same session UUID,
/// so `claude --session-id <uuid>` resumes the right conversation regardless of
/// which container the repo is mounted in or what its in-container path is.
///
/// Passing `--fresh` at spawn generates a new random v4 UUID instead, replacing
/// the stored ID so that subsequent attaches start from the new session.
fn repo_session_id(repo_path: &str) -> Uuid {
    // Canonicalise so /home/user/myrepo and ./myrepo resolve to the same UUID.
    let canonical = std::fs::canonicalize(repo_path)
        .unwrap_or_else(|_| std::path::PathBuf::from(repo_path));
    let key = canonical.to_string_lossy();
    // UUID v5: SHA-1 hash of the path in the OID namespace — stable across runs.
    Uuid::new_v5(&Uuid::NAMESPACE_OID, key.as_bytes())
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
    if let Some((existing_task_id, _)) = db.get_container_state_by_repo_path(&abs_repo_path_early)? {
        if let Some(task) = db.get_task_by_id(&existing_task_id)? {
            if let Some(ref cid) = task.container_id {
                if let Ok(crate::sandbox::ContainerStatus::Running) = sandbox.status(cid) {
                    eprintln!(
                        "⚠️  A sandbox for '{}' already exists (task {}).",
                        abs_repo_path_early,
                        &existing_task_id[..existing_task_id.len().min(8)]
                    );
                    eprintln!("   Attaching to the existing sandbox instead of spawning a new one.");
                    eprintln!();
                    if no_attach {
                        eprintln!("Attach with:");
                        eprintln!("  agent-sandbox attach {}", abs_repo_path_early);
                    } else {
                        cmd_sandbox_attach(db, existing_task_id.clone(), fresh, kimi)?;
                    }
                    return Ok(existing_task_id);
                }
            }
        }
    }

    sandbox.ensure_image_with_opts(&image, false)?;

    let task_id = uuid::Uuid::new_v4().to_string();

    let mut env_vars = HashMap::new();
    for key in &["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "HOME", "CLAUDE_CONFIG_DIR"] {
        if let Ok(val) = std::env::var(key) {
            env_vars.insert(key.to_string(), val);
        }
    }

    let config = SandboxConfig { image, env_vars, ..SandboxConfig::default() };

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

    let repo_name = repo
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| repo_path.clone());

    // Detect project toolchains and write a CLAUDE.md into the container so
    // Claude knows it is in a sandbox and how to install/run the project.
    let toolchains = detect_toolchains(&repo);
    let claude_md = build_sandbox_claude_md(&repo_name, &toolchains);
    match inject_sandbox_claude_md(&info.id, &claude_md) {
        Ok(()) => {
            if toolchains.is_empty() {
                println!("  Context:   CLAUDE.md written (no toolchain detected)");
            } else {
                let names: Vec<&str> = toolchains.iter().map(|(e, _, _)| *e).collect();
                println!("  Context:   CLAUDE.md written (detected: {})", names.join(", "));
            }
        }
        Err(e) => eprintln!("  Warning:   Could not write CLAUDE.md: {e:#}"),
    }

    let title = task_desc.unwrap_or_else(|| format!("[{}:sandbox]", repo_name));

    let mut task = Task::new(task_id.clone(), "claude_code".to_string(), title, None, None);
    task.sandbox_type = SandboxType::Podman;
    let abs_repo_path = repo
        .canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(repo_path);

    // Determine the session UUID for this sandbox.
    // - Normal spawn: derive a deterministic UUID v5 from the canonical repo path.
    //   Every sandbox on the same repo gets the same UUID, so `claude --session-id`
    //   always resumes the right conversation regardless of in-container path (/workspace).
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
        println!("  Session:   {} (deterministic for this repo)", &det_sid[..8]);
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

    db.upsert_container_state(&task_id, &info.name, &abs_repo_path)?;

    let short_id = &task_id[..task_id.len().min(8)];
    println!("\nSandbox started:");
    println!("  Task ID:   {} ({})", short_id, task_id);
    println!("  Container: {}", info.name);
    println!("  Repo:      {}", abs_repo_path);
    println!();

    if no_attach {
        println!("Attach to the Claude session:");
        println!("  agent-sandbox attach {}          (by repo path)", abs_repo_path);
        println!("  agent-sandbox attach {}   (by task ID)", short_id);
        println!("  agent-sandbox attach {} --fresh  (start a new conversation)", short_id);
        println!("  (The container keeps running after you exit — re-attach any time)");
        println!();
        println!("After a system reboot, restart stopped containers with:");
        println!("  agent-sandbox resume --all");
    } else {
        println!("Attaching to Claude session (exit to detach — container keeps running)…");
        println!("  Re-attach later: agent-sandbox attach {}  (or by path: {})", short_id, abs_repo_path);
        println!();
        cmd_sandbox_attach(db, task_id.clone(), fresh, kimi)?;
    }

    Ok(task_id)
}

/// List all tracked sandboxes, auto-cleaning gone entries.
fn cmd_sandbox_list(db: &Database) -> Result<()> {
    let states = db.list_container_states()?;

    if states.is_empty() {
        println!("No sandbox containers found.");
        println!("Start one with:  agent-sandbox <repo_path>");
        return Ok(());
    }

    let sandbox = PodmanSandbox::new();

    println!("{:<20} {:<18} {:<12} {}", "TASK ID", "STARTED", "STATUS", "REPO");
    println!("{}", "─".repeat(82));

    let mut any_gone = false;
    for (task_id, container_name, repo_path, _created) in &states {
        let health = sandbox.health_check(container_name);

        let status = match health {
            SandboxHealth::Healthy  => "healthy",
            SandboxHealth::Degraded => "degraded",
            SandboxHealth::Dead => {
                // Container is gone — prune state silently
                any_gone = true;
                let _ = db.delete_container_state(task_id);
                continue;
            }
        };

        // Parse timestamp from name: agent-inbox-YYYYMMDD-HHMM-shortid
        let started = container_name
            .strip_prefix("agent-inbox-")
            .and_then(|s| {
                let parts: Vec<&str> = s.splitn(3, '-').collect();
                if parts.len() >= 2 {
                    let date = parts[0];
                    let time = parts[1];
                    if date.len() == 8 && time.len() == 4 {
                        return Some(format!(
                            "{}-{}-{} {}:{}",
                            &date[..4], &date[4..6], &date[6..8],
                            &time[..2], &time[2..4]
                        ));
                    }
                }
                None
            })
            .unwrap_or_else(|| container_name.chars().take(17).collect());

        let short_id = &task_id[..task_id.len().min(8)];
        println!("{:<20} {:<18} {:<12} {}", short_id, started, status, repo_path);
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
             Start one with:  agent-sandbox {}",
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
        .filter(|(tid, _, _, _)| tid.starts_with(input))
        .collect();

    match matches.len() {
        0 => anyhow::bail!("No sandbox found with ID or path: {}", input),
        1 => Ok(matches[0].0.clone()),
        _ => {
            let ids: Vec<&str> = matches.iter().map(|(tid, _, _, _)| tid.as_str()).collect();
            anyhow::bail!(
                "Ambiguous prefix '{}' matches multiple sandboxes:\n  {}",
                input,
                ids.join("\n  ")
            )
        }
    }
}

/// Attach to the Claude session inside a running sandbox.
///
/// Uses `--session-id` to resume the deterministic session stored on the task.
/// Pass `fresh = true` to generate a new random session UUID and persist it,
/// so subsequent attaches continue from this new session.
fn cmd_sandbox_attach(db: &Database, task_id: String, fresh: bool, kimi: bool) -> Result<()> {
    let task = db
        .get_task_by_id(&task_id)?
        .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

    if task.sandbox_type != SandboxType::Podman {
        anyhow::bail!("Task {} is not a sandbox task", task_id);
    }

    let container_id = task
        .container_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Task {} has no container_id", task_id))?;

    let sandbox = PodmanSandbox::new();
    match sandbox.status(container_id)? {
        sandbox::ContainerStatus::Running => {}
        _ => anyhow::bail!("Container {} is not running", container_id),
    }

    let claude = "/home/node/.local/bin/claude --dangerously-skip-permissions";

    // Build the shell command that runs inside the container.
    let shell_cmd = if fresh {
        format!("cd /workspace && {claude}")
    } else {
        // Use the session ID stored on the task (set at spawn time from the host-path
        // session lookup, or updated later by the Stop hook).
        let session_id = task.context.as_ref().and_then(|c| c.session_id.as_deref());
        if let Some(sid) = session_id {
            // Try to resume the stored session; fall back to a fresh start if it fails.
            format!("cd /workspace && {claude} --resume {sid} 2>&1 || {claude}")
        } else {
            // No session stored — use --continue to resume last conversation, or fresh.
            format!("cd /workspace && {claude} --continue 2>&1 || {claude}")
        }
    };

    // Build podman exec args, injecting Kimi credentials if requested.
    // KIMI_BASE_URL and KIMI_API_KEY must be set in the host environment
    // (e.g. via the claude-kimi alias definition in ~/.zshrc).
    let mut podman_args: Vec<String> = vec![
        "exec".into(), "-it".into(),
        "-e".into(), "TERM=xterm-256color".into(),
        "-e".into(), "PATH=/home/node/.local/bin:/usr/local/bin:/usr/bin:/bin".into(),
        "-e".into(), "CLAUDE_CONFIG_DIR=/home/node/.claude".into(),
    ];

    if kimi {
        let base_url = std::env::var("KIMI_BASE_URL")
            .context("--kimi requires KIMI_BASE_URL to be set in the host environment")?;
        let api_key = std::env::var("KIMI_API_KEY")
            .context("--kimi requires KIMI_API_KEY to be set in the host environment")?;
        eprintln!("Using Kimi backend ({})", base_url);
        podman_args.extend([
            "-e".into(), format!("ANTHROPIC_BASE_URL={}", base_url),
            "-e".into(), format!("ANTHROPIC_API_KEY={}", api_key),
            "-e".into(), "ENABLE_TOOL_SEARCH=FALSE".into(),
        ]);
    }

    podman_args.extend([
        "-w".into(), "/workspace".into(),
        container_id.into(),
        "/bin/bash".into(), "-c".into(), shell_cmd,
    ]);

    eprintln!("Attaching to sandbox {} ({})…", task.title, container_id);
    eprintln!("(Exit Claude or press Ctrl+C to detach — the container keeps running)");

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

/// Kill all running sandbox containers.
fn cmd_sandbox_kill_all(db: &Database) -> Result<()> {
    let sandbox = PodmanSandbox::new();
    let states = db.list_container_states()?;

    if states.is_empty() {
        println!("No sandbox agents to kill.");
        return Ok(());
    }

    let mut killed = 0;
    for (task_id, container_name, _, _) in &states {
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

    for (task_id, container_name, repo_path, _) in &states {
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
                println!("  Degraded: {} (container up, Claude session gone, repo: {})", container_name, repo_path);
                stale += 1;
            }
            SandboxHealth::Dead => {
                // Container may be stopped (reboot) rather than truly gone.
                // Try to start it; if that succeeds re-check health.
                match sandbox.start(container_name) {
                    Ok(()) => {
                        match sandbox.health_check(container_name) {
                            SandboxHealth::Healthy => {
                                if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                                    if task.status != TaskStatus::Running {
                                        task.set_running();
                                        let _ = db.update_task(&task);
                                    }
                                }
                                println!("  Restarted: {} ({})", container_name, task_id);
                                resumed += 1;
                            }
                            _ => {
                                if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                                    task.set_exited(None);
                                    let _ = db.update_task(&task);
                                }
                                let _ = db.delete_container_state(task_id);
                                println!("  Cleaned: {} (start failed health check, repo: {})", container_name, repo_path);
                                stale += 1;
                            }
                        }
                    }
                    Err(_) => {
                        // Container doesn't exist at all — clean up.
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
                    eprintln!("[prune] Task {} (pid {}) is dead → exited", &task.task_id[..8.min(task.task_id.len())], pid);
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
        for (task_id, container_name, repo_path, _) in &states {
            match sandbox.health_check(container_name) {
                SandboxHealth::Healthy => {}
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
                            let msg = format!("💥 Sandbox for `{}` crashed or was killed by the OS.\nRe-spawn with: `agent-sandbox {}`", repo_label, repo_path);
                            if let Ok(text) = build_notification_text(db, Some(task_id), &msg, false) {
                                let _ = notifications::telegram::send(&cfg.telegram, &text);
                            }
                        }
                    }
                }
                SandboxHealth::Degraded => {
                    // Container alive but exec fails — update DB only, no notification.
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
fn detect_toolchains(repo_path: &std::path::Path) -> Vec<(&'static str, &'static str, &'static str)> {
    let checks: &[(&str, &str, &str, &str)] = &[
        // (manifest file, ecosystem label, install cmd, run hint)
        ("package.json",    "Node.js",  "npm install",          "npm start / npm test / npm run dev"),
        ("yarn.lock",       "Node.js",  "yarn install",         "yarn start / yarn test / yarn dev"),
        ("pnpm-lock.yaml",  "Node.js",  "pnpm install",         "pnpm start / pnpm test / pnpm dev"),
        ("Cargo.toml",      "Rust",     "cargo build",          "cargo run / cargo test"),
        ("go.mod",          "Go",       "go mod download",      "go run . / go test ./..."),
        ("requirements.txt","Python",   "pip install -r requirements.txt", "python main.py / pytest"),
        ("pyproject.toml",  "Python",   "pip install -e .",     "python -m pytest / python -m <module>"),
        ("Pipfile",         "Python",   "pipenv install",       "pipenv run python ... / pipenv run pytest"),
        ("composer.json",   "PHP",      "composer install",     "php artisan serve / php -S localhost:8000"),
        ("Gemfile",         "Ruby",     "bundle install",       "bundle exec rails s / bundle exec rspec"),
        ("build.gradle",    "JVM",      "./gradlew build",      "./gradlew run / ./gradlew test"),
        ("pom.xml",         "JVM",      "mvn install -DskipTests", "mvn exec:java / mvn test"),
        ("mix.exs",         "Elixir",   "mix deps.get",         "mix run / mix test"),
        ("Makefile",        "Make",     "make",                 "make run / make test"),
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

const CLAUDE_MD_BEGIN: &str = "<!-- agent-inbox:begin -->";
const CLAUDE_MD_END: &str = "<!-- agent-inbox:end -->";

/// Build the sandbox CLAUDE.md section that tells Claude it is running inside
/// an isolated container and how to set up the project toolchain.
///
/// The returned string is wrapped in delimiters so `inject_sandbox_claude_md`
/// can replace it in-place without touching any user-written content.
fn build_sandbox_claude_md(repo_name: &str, toolchains: &[(&str, &str, &str)]) -> String {
    let mut lines = vec![
        "# Agent Inbox Sandbox".to_string(),
        String::new(),
        format!(
            "You are running inside an isolated Podman sandbox container for the **{}** project.",
            repo_name
        ),
        String::new(),
        "## Environment".to_string(),
        String::new(),
        "- Working directory: `/workspace` (the project repo, mounted read-write)".to_string(),
        "- You have full `sudo` access — install any system package with `apt-get install`".to_string(),
        "- Ports are forwarded to the host: services on `localhost:3000`, `:8080`, etc. are reachable from outside".to_string(),
        "- Internet access is available".to_string(),
        "- Git is configured with the host user's identity and SSH keys".to_string(),
        String::new(),
    ];

    if toolchains.is_empty() {
        lines.push("## Toolchain".to_string());
        lines.push(String::new());
        lines.push("No recognised dependency manifest was found in the repo root.".to_string());
        lines.push("Inspect the project structure and install any required tools before running or testing.".to_string());
    } else {
        lines.push("## Toolchain".to_string());
        lines.push(String::new());
        lines.push(
            "The following dependency manifests were detected. \
             Install dependencies before running or testing the project:"
                .to_string(),
        );
        lines.push(String::new());
        for (ecosystem, install_cmd, run_hint) in toolchains {
            lines.push(format!("### {}", ecosystem));
            lines.push(format!("- **Install:** `{}`", install_cmd));
            lines.push(format!("- **Run/test:** `{}`", run_hint));
            lines.push(String::new());
        }
        lines.push(
            "Always install dependencies before attempting to build, run, or test the project. \
             If a command fails due to missing tools, install them with `sudo apt-get install <package>` \
             or the appropriate package manager."
                .to_string(),
        );
    }

    lines.push(String::new());
    lines.push("## Important notes".to_string());
    lines.push(String::new());
    lines.push("- Prefer making small, focused changes and running tests after each one".to_string());
    lines.push("- The container persists between sessions — installed packages and build artifacts are retained".to_string());
    lines.push("- When you finish a task, summarise what you did clearly so the notification sent to the user is informative".to_string());

    format!("{}\n{}\n{}", CLAUDE_MD_BEGIN, lines.join("\n"), CLAUDE_MD_END)
}

/// Write the sandbox section of `.claude/CLAUDE.md` inside the container.
///
/// - If the file does not exist, it is created with just the generated block.
/// - If the file exists and already contains the agent-inbox delimiters, only
///   the block between them is replaced — any user-written content outside the
///   markers is preserved verbatim.
/// - If the file exists but has no delimiters (user wrote it by hand), the
///   generated block is appended so Claude sees both.
fn inject_sandbox_claude_md(container_id: &str, content: &str) -> Result<()> {
    // Write the new content to a temp file first, then use a small python
    // script inside the container to do the marker-aware merge.  Python is
    // available in the node:20-slim image (via apt), but we can't rely on it.
    // Instead we do everything with bash + awk, which is always present.
    //
    // The awk script removes everything between the markers (inclusive) and
    // inserts the new block in their place.  If no markers exist it appends.
    let escaped = content.replace('\'', "'\\''"); // escape single quotes for bash

    let script = format!(
        r#"set -e
mkdir -p /workspace/.claude
TARGET=/workspace/.claude/CLAUDE.md
NEW_BLOCK='{escaped}'
BEGIN='{begin}'
END='{end}'

if [ ! -f "$TARGET" ]; then
    printf '%s\n' "$NEW_BLOCK" > "$TARGET"
else
    # Check if markers already exist in the file.
    if grep -qF "$BEGIN" "$TARGET" 2>/dev/null; then
        # Replace the block between markers (inclusive) with the new content.
        awk -v new="$NEW_BLOCK" -v begin="$BEGIN" -v end="$END" '
            BEGIN {{ skip=0 }}
            index($0, begin) {{ skip=1; print new; next }}
            skip && index($0, end) {{ skip=0; next }}
            !skip {{ print }}
        ' "$TARGET" > "$TARGET.tmp" && mv "$TARGET.tmp" "$TARGET"
    else
        # No markers — append the new block separated by a blank line.
        printf '\n%s\n' "$NEW_BLOCK" >> "$TARGET"
    fi
fi"#,
        escaped = escaped,
        begin = CLAUDE_MD_BEGIN,
        end = CLAUDE_MD_END,
    );

    let output = std::process::Command::new("podman")
        .args(["exec", container_id, "/bin/bash", "-c", &script])
        .output()
        .context("Failed to write CLAUDE.md into container")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Writing CLAUDE.md failed: {}", stderr.trim());
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
/// 📁 agent-inbox · main
/// ⏱ 4m 32s
/// ```
///
/// Attention example:
/// ```
/// 🚨 Needs your attention
/// 🤖 Claude Code
/// 📁 agent-inbox · main
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
        "opencode"    => ("⚡", "OpenCode".to_string()),
        "claude_web"  => ("🌐", "Claude Web".to_string()),
        "gemini_web"  => ("✨", "Gemini".to_string()),
        other         => ("🔧", other.to_string()),
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
        let task = make_task("claude_code", "[agent-inbox:main]");
        let loc = format_location(&task);
        assert!(loc.contains("agent-inbox"));
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
        assert_eq!(agent_display("claude_code"), ("🤖", "Claude Code".to_string()));
        assert_eq!(agent_display("opencode"),    ("⚡", "OpenCode".to_string()));
        assert_eq!(agent_display("claude_web"),  ("🌐", "Claude Web".to_string()));
        assert_eq!(agent_display("gemini_web"),  ("✨", "Gemini".to_string()));
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
}
