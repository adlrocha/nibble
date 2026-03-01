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
use sandbox::Sandbox;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

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
        Some(Commands::Sandbox { args }) => {
            // Proxy to agent-sandbox so `agent-inbox sandbox *` and `agent-sandbox *` are identical.
            let err = std::os::unix::process::CommandExt::exec(
                std::process::Command::new("agent-sandbox").args(&args),
            );
            anyhow::bail!("Failed to exec agent-sandbox: {}", err);
        }

        // ── Internal sandbox subcommands (invoked by agent-sandbox script) ────

        Some(Commands::SandboxSpawn { repo_path, task, image, fresh, session_id }) => {
            cmd_sandbox_spawn(&db, repo_path, task, image, fresh, session_id)?;
        }
        Some(Commands::SandboxList) => {
            cmd_sandbox_list(&db)?;
        }
        Some(Commands::SandboxAttach { task_id, fresh, kimi }) => {
            cmd_sandbox_attach(&db, task_id, fresh, kimi)?;
        }
        Some(Commands::SandboxKill { task_id, all }) => {
            if all {
                cmd_sandbox_kill_all(&db)?;
            } else {
                let id = task_id.ok_or_else(|| anyhow::anyhow!("Provide a task ID or --all"))?;
                cmd_sandbox_kill(&db, id)?;
            }
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

            notifications::telegram_listener::run(&db, &cfg.telegram)?;
        }
    }

    Ok(())
}

// ── Sandbox command handlers ──────────────────────────────────────────────────

/// Find the most recent Claude Code session for a given repo path.
///
/// Claude stores sessions under ~/.claude/projects/<url-encoded-path>/*.jsonl.
/// We look for the file with the most recent modification time and return its
/// stem (filename without extension) as the session ID.
///
/// Both host sessions and previous sandbox sessions are stored here because all
/// sandboxes mount the same ~/.claude directory read-write.
fn find_latest_session_for_repo(repo_path: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    let projects_dir = home.join(".claude").join("projects");

    // Claude encodes the absolute path with each '/' replaced by '-'
    // and the leading '/' dropped, e.g. /home/user/myapp → -home-user-myapp
    // Try to canonicalize the repo path first for a reliable match.
    let abs_path = std::fs::canonicalize(repo_path)
        .unwrap_or_else(|_| std::path::PathBuf::from(repo_path));
    let encoded = abs_path.to_string_lossy().replace('/', "-");

    let session_dir = projects_dir.join(&encoded);
    if !session_dir.exists() {
        return None;
    }

    // Find the most recently modified .jsonl file in that directory.
    std::fs::read_dir(&session_dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()?.to_str()? != "jsonl" {
                return None;
            }
            let mtime = entry.metadata().ok()?.modified().ok()?;
            let stem = path.file_stem()?.to_string_lossy().to_string();
            Some((mtime, stem))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, stem)| stem)
}

/// Spawn a sandboxed Claude Code agent.
fn cmd_sandbox_spawn(
    db: &Database,
    repo_path: String,
    task_desc: Option<String>,
    image: String,
    fresh: bool,
    session_id: Option<String>,
) -> Result<()> {
    let repo = PathBuf::from(&repo_path);
    if !repo.exists() {
        anyhow::bail!("Repository path does not exist: {}", repo_path);
    }

    let sandbox = PodmanSandbox::new();
    if !sandbox.is_available()? {
        anyhow::bail!("Podman is not installed. Run ./install.sh to set it up.");
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

    let title = task_desc.unwrap_or_else(|| format!("[{}:sandbox]", repo_name));

    // Determine which session to continue in the sandbox.
    // Priority: explicit --session-id > auto-detect from repo > none (fresh start).
    let resolved_session_id = if fresh {
        None
    } else if let Some(sid) = session_id {
        println!("  Session:   continuing {}", &sid[..sid.len().min(8)]);
        Some(sid)
    } else {
        // Auto-detect: find the most recent Claude session associated with this
        // repo path by scanning ~/.claude/projects/.
        find_latest_session_for_repo(&repo_path)
            .map(|sid| {
                println!("  Session:   auto-detected {} (use --fresh to start new)", &sid[..sid.len().min(8)]);
                sid
            })
    };

    let mut task = Task::new(task_id.clone(), "claude_code".to_string(), title, None, None);
    task.sandbox_type = SandboxType::Podman;
    task.container_id = Some(info.id.clone());
    task.sandbox_config = Some(config);
    task.context = Some(TaskContext {
        url: None,
        project_path: Some(repo_path.clone()),
        session_id: resolved_session_id,
        extra: HashMap::new(),
    });
    db.insert_task(&task)?;

    let abs_repo_path = repo
        .canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(repo_path);
    db.upsert_container_state(&task_id, &info.name, &abs_repo_path)?;

    let short_id = &task_id[..task_id.len().min(8)];
    println!("\nSandbox started:");
    println!("  Task ID:   {} ({})", short_id, task_id);
    println!("  Container: {}", info.name);
    println!("  Repo:      {}", abs_repo_path);
    println!();
    println!("Attach to the Claude session:");
    println!("  agent-sandbox attach {}", short_id);
    println!("  agent-sandbox attach {} --fresh   (start a new conversation)", short_id);
    println!("  (The container keeps running after you exit — re-attach any time)");

    Ok(())
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

    println!("{:<20} {:<18} {:<10} {}", "TASK ID", "STARTED", "STATUS", "REPO");
    println!("{}", "─".repeat(80));

    let mut any_gone = false;
    for (task_id, container_name, repo_path, _created) in &states {
        let status = match sandbox.status(container_name) {
            Ok(sandbox::ContainerStatus::Running) => "running",
            Ok(sandbox::ContainerStatus::Stopped) => "stopped",
            Ok(sandbox::ContainerStatus::Paused)  => "paused",
            _ => {
                any_gone = true;
                let _ = db.delete_container_state(task_id);
                continue; // skip gone entries silently
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
        println!("{:<20} {:<18} {:<10} {}", short_id, started, status, repo_path);
    }

    if any_gone {
        println!("\n(Gone containers were removed from tracking.)");
    }

    Ok(())
}

/// Attach to the Claude session inside a running sandbox.
///
/// By default tries `--continue` to resume the last conversation.
/// Falls back to a fresh session automatically if no conversation exists.
/// Pass `fresh = true` to always start a new session.
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
    let shell_cmd = if fresh {
        format!("cd /workspace && {claude}")
    } else {
        // Use the session ID stored on the task (set at spawn time from the host-path
        // session lookup, or updated later by the Stop hook).
        // Do NOT fall back to --continue: that resumes the last global session which
        // has no repo awareness and would bleed sessions across sandboxes.
        let session_id = task.context.as_ref().and_then(|c| c.session_id.as_deref());
        if let Some(sid) = session_id {
            format!("cd /workspace && {claude} --resume {sid} 2>&1 || {claude}")
        } else {
            format!("cd /workspace && {claude}")
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
        ]);
    }

    podman_args.extend([
        "-w".into(), "/workspace".into(),
        container_id.into(),
        "/bin/bash".into(), "-c".into(), shell_cmd,
    ]);

    eprintln!("Attaching to sandbox {}…", container_id);
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
        match sandbox.status(container_name) {
            Ok(sandbox::ContainerStatus::Running) => {
                if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                    if task.status != TaskStatus::Running {
                        task.set_running();
                        let _ = db.update_task(&task);
                    }
                }
                println!("  Running: {} ({})", container_name, task_id);
                resumed += 1;
            }
            _ => {
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

// ── Notification helpers ──────────────────────────────────────────────────────

/// Build the full notification text sent to Telegram.
///
/// When a `task_id` is supplied and found in the database a rich header is
/// prepended so the user can immediately identify which agent and repo the
/// message comes from without reading the body.
///
/// `attention` = true produces a visually distinct header for permission
/// requests and questions that require an immediate response.
fn build_notification_text(
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
