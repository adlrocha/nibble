mod agent_input;
mod backup;
mod cli;
mod config;
mod cron;
mod db;
mod lm;
mod memory;
mod models;
mod notifications;
mod privacy_filter;
mod sandbox;
mod session;
mod usage;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands, CronAction, HermesAction, LmAction, ReportAction, SandboxAction};
use db::Database;
use models::{AgentType, SandboxConfig, SandboxType, Task, TaskContext, TaskStatus};
use sandbox::podman::PodmanSandbox;
use sandbox::{container_working_dir, pi_session_dir_name, Sandbox, SandboxHealth};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use uuid::Uuid;

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Ensure data directory exists
    db::ensure_data_dir()?;

    // Open database
    let db_path = db::default_db_path();
    let db = Database::open(&db_path).context("Failed to open database")?;

    match cli.command {
        Commands::Notify {
            message,
            task_id,
            attention,
        } => {
            let cfg = config::load().unwrap_or_default();

            if !cfg.telegram.is_configured() {
                eprintln!(
                    "Telegram notifications are not configured. \
                     Run scripts/setup-telegram.sh to set them up."
                );
                // Exit cleanly — missing config is not a fatal error for hooks.
                return Ok(());
            }

            if !cfg.telegram.notifications {
                // Agent-triggered notifications are disabled by the user.
                // Telegram listener messages (injection completions, heartbeats,
                // cron alerts) bypass this and call the API directly.
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
        Commands::Report { action } => match action {
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
                let mut task = Task::new(
                    task_id,
                    AgentType::from_str(&agent_type).unwrap(), // infallible
                    title,
                    pid,
                    ppid,
                );
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
                    claude_session_id: None,
                    extra,
                });
                db.insert_task(&task)?;
                println!("Task started: {}", task.task_id);
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
                    claude_session_id: None,
                    extra: HashMap::new(),
                });
                // Route to the agent-specific field.
                // All sandbox tasks write claude_session_id when the ID is a UUID.
                // ses_ prefix (legacy) is ignored.
                match task.agent_type {
                    AgentType::ClaudeCode
                    | AgentType::Hermes
                    | AgentType::Pi
                    | AgentType::Unknown(_) => {
                        if session_id.starts_with("ses_") {
                            // Legacy session ID (ses_ prefix) — ignore
                        } else {
                            ctx.claude_session_id = Some(session_id);
                        }
                    }
                }
                db.update_task(&task)?;
            }
        },
        Commands::Memory { action } => {
            // Ensure memory directory exists
            crate::memory::init_memory_dir()?;
            match action {
                cli::MemoryAction::Search {
                    query,
                    project,
                    r#type,
                    limit,
                    semantic,
                } => {
                    memory::cli::handle_search(
                        &query,
                        project.as_deref(),
                        r#type.as_deref(),
                        Some(limit),
                        semantic,
                    )?;
                }
                cli::MemoryAction::List {
                    project,
                    r#type,
                    since,
                    limit,
                } => {
                    memory::cli::handle_list(
                        project.as_deref(),
                        r#type.as_deref(),
                        since.as_deref(),
                        Some(limit),
                    )?;
                }
                cli::MemoryAction::Show { id, with_session } => {
                    let entry = memory::cli::handle_show(&id)?;
                    if with_session {
                        if let Some(mem) = entry {
                            if let Some(ref sid) = mem.session_id {
                                println!("\n═══════════════════════════════════════════════════════════════════════════════\n");
                                println!("# Session Transcript: {}\n", sid);
                                match session::read_session(sid) {
                                    Ok(transcript) => println!("{}", transcript),
                                    Err(e) => {
                                        eprintln!("Could not read live session transcript: {}", e);
                                        eprintln!(
                                            "The agent may have garbage-collected its local copy."
                                        );
                                        eprintln!();
                                        eprintln!("Archived copy (if summarized): ~/.nibble/memory/archive/<agent>/{}.jsonl", sid);
                                        eprintln!("Capture file (if captured):     ~/.nibble/memory/capture/*/{sid}.jsonl");
                                    }
                                }
                            } else {
                                println!("\n(No session ID linked to this memory.)");
                            }
                        }
                    }
                }
                cli::MemoryAction::Write {
                    content,
                    r#type,
                    project,
                    tags,
                    update,
                    title,
                } => {
                    memory::cli::handle_write(
                        &content,
                        &r#type,
                        project.as_deref(),
                        tags.as_deref(),
                        update.as_deref(),
                        title.as_deref(),
                    )?;
                }
                cli::MemoryAction::BySession { session_id } => {
                    memory::cli::handle_by_session(&session_id)?;
                }
                cli::MemoryAction::Context {
                    query,
                    project,
                    limit,
                } => {
                    memory::cli::handle_context(&query, project.as_deref(), limit)?;
                }
                cli::MemoryAction::Forget { id } => {
                    memory::cli::handle_forget(&id)?;
                }
                cli::MemoryAction::Stats { project } => {
                    memory::cli::handle_stats(project.as_deref())?;
                }
                cli::MemoryAction::Capture {
                    task_id,
                    role,
                    content,
                    tool_name,
                    tool_input,
                    tool_output,
                } => {
                    memory::cli::handle_capture(
                        &task_id,
                        &role,
                        &content,
                        tool_name.as_deref(),
                        tool_input.as_deref(),
                        tool_output.as_deref(),
                    )?;
                }
                cli::MemoryAction::Lessons {
                    context,
                    status,
                    severity,
                    limit,
                } => {
                    memory::cli::handle_lessons(
                        context.as_deref(),
                        Some(&status),
                        severity.as_deref(),
                        Some(limit),
                    )?;
                }
                cli::MemoryAction::LessonAdd {
                    content,
                    category,
                    severity,
                    prevention,
                    project,
                    tags,
                } => {
                    memory::cli::handle_lesson_add(
                        &content,
                        &category,
                        &severity,
                        &prevention,
                        project.as_deref(),
                        tags.as_deref(),
                    )?;
                }
                cli::MemoryAction::LessonResolve { id, note } => {
                    memory::cli::handle_lesson_resolve(&id, note.as_deref())?;
                }
                cli::MemoryAction::Inspect { project } => {
                    memory::cli::handle_inspect(project.as_deref())?;
                }
                cli::MemoryAction::Reindex => {
                    memory::cli::handle_reindex()?;
                }
                cli::MemoryAction::Config { setup } => {
                    if setup {
                        memory::cli::handle_setup()?;
                    } else {
                        memory::cli::handle_config()?;
                    }
                }
                cli::MemoryAction::Sync => {
                    memory::cli::handle_sync()?;
                }
                cli::MemoryAction::Dedup { yes } => {
                    memory::cli::handle_dedup(yes)?;
                }
                cli::MemoryAction::Archive { task_id } => {
                    memory::cli::handle_archive(&task_id)?;
                }
                cli::MemoryAction::Summarize {
                    task_id,
                    force,
                    from_pi_session,
                } => {
                    memory::cli::handle_summarize(&task_id, force, from_pi_session.as_deref())?;
                }
            }
        }
        Commands::Session { action } => match action {
            cli::SessionAction::List {
                agent,
                repo,
                sandbox,
                today,
                yesterday,
                week,
                month,
                last,
                limit,
            } => {
                // Resolve --sandbox path using the same semantics as attach/spawn/kill.
                let sandbox_repo = if let Some(ref input) = sandbox {
                    Some(resolve_sandbox_repo_path(input)?)
                } else {
                    None
                };
                // Compute date filter
                let date_range = if today {
                    let local_now = chrono::Local::now();
                    let start_of_day = local_now
                        .date_naive()
                        .and_hms_opt(0, 0, 0)
                        .unwrap()
                        .and_local_timezone(chrono::Local)
                        .unwrap();
                    let tomorrow = start_of_day + chrono::Duration::days(1);
                    Some(session::DateRange {
                        since: Some(start_of_day.with_timezone(&chrono::Utc)),
                        until: Some(tomorrow.with_timezone(&chrono::Utc)),
                    })
                } else if yesterday {
                    let local_now = chrono::Local::now();
                    let start_of_today = local_now
                        .date_naive()
                        .and_hms_opt(0, 0, 0)
                        .unwrap()
                        .and_local_timezone(chrono::Local)
                        .unwrap();
                    let start_of_yesterday = start_of_today - chrono::Duration::days(1);
                    Some(session::DateRange {
                        since: Some(start_of_yesterday.with_timezone(&chrono::Utc)),
                        until: Some(start_of_today.with_timezone(&chrono::Utc)),
                    })
                } else if week {
                    Some(session::DateRange {
                        since: Some(chrono::Utc::now() - chrono::Duration::days(7)),
                        until: None,
                    })
                } else if month {
                    Some(session::DateRange {
                        since: Some(chrono::Utc::now() - chrono::Duration::days(30)),
                        until: None,
                    })
                } else {
                    None
                };

                let effective_limit = last.unwrap_or(limit);

                let groups = session::list_sessions_grouped(
                    agent.as_deref(),
                    repo.as_deref(),
                    date_range,
                    effective_limit,
                )?;

                if groups.is_empty() || groups.iter().all(|g| g.sessions.is_empty()) {
                    println!("No sessions found.");
                    return Ok(());
                }

                // Build memory count lookup per session_id
                let memories =
                    memory::store::list_memories(None, None, None, None).unwrap_or_default();
                let mut memory_counts: HashMap<String, usize> = HashMap::new();
                for m in &memories {
                    if let Some(ref sid) = m.session_id {
                        *memory_counts.entry(sid.clone()).or_insert(0) += 1;
                    }
                }

                // Build lookup maps from session identifiers → project_path
                let tasks = db.list_tasks().unwrap_or_default();
                let mut claude_task_repo: HashMap<String, String> = HashMap::new();
                let mut pi_task_repo: HashMap<String, String> = HashMap::new();
                // Reverse maps: agent session_id → task_id (for memory badge linkage)
                let mut claude_session_to_task: HashMap<String, String> = HashMap::new();
                let mut pi_path_to_task: HashMap<String, String> = HashMap::new();
                let home_dir = dirs::home_dir();
                for task in &tasks {
                    if let Some(ref ctx) = task.context {
                        if let Some(ref path) = ctx.project_path {
                            if let Some(ref sid) = ctx.claude_session_id {
                                claude_task_repo.insert(sid.clone(), path.clone());
                                claude_session_to_task.insert(sid.clone(), task.task_id.clone());
                            }
                            if let Some(serde_json::Value::String(ref p)) =
                                ctx.extra.get("pi_session_path")
                            {
                                // pi_session_path is stored as a container path when the
                                // sandbox attach logic discovers it. Convert it back to a
                                // host path so it matches the paths returned by session
                                // discovery (which scans ~/.pi on the host).
                                let host_path = if let Some(ref home) = home_dir {
                                    let home_str = home.to_string_lossy();
                                    if let Some(rest) = p.strip_prefix("/home/node/") {
                                        format!("{}/{}", home_str, rest)
                                    } else {
                                        p.clone()
                                    }
                                } else {
                                    p.clone()
                                };
                                pi_task_repo.insert(host_path.clone(), path.clone());
                                pi_path_to_task.insert(host_path, task.task_id.clone());
                            }
                        }
                    }
                }

                // When --sandbox is given, filter groups to exact repo matches.
                let groups = if let Some(ref repo) = sandbox_repo {
                    let mut filtered = groups;
                    for group in &mut filtered {
                        group.sessions.retain(|s| {
                            let resolved_repo = match s.agent.as_str() {
                                "claude" => claude_task_repo.get(&s.session_id).cloned(),
                                "pi" => pi_task_repo
                                    .get(&s.path.to_string_lossy().to_string())
                                    .cloned(),
                                _ => None,
                            };
                            resolved_repo.as_deref() == Some(repo)
                        });
                    }
                    filtered.retain(|g| !g.sessions.is_empty());
                    filtered
                } else {
                    groups
                };

                // Compute max title width for alignment
                let mut max_title_width = 40;
                for group in &groups {
                    for s in &group.sessions {
                        let title = session::get_session_title(s);
                        max_title_width = max_title_width.max(title.len().min(60));
                    }
                }
                max_title_width = max_title_width.max(20);

                for group in &groups {
                    println!("\n{} ({})", group.label, group.sessions.len());
                    println!("{}", "─".repeat(max_title_width + 58));
                    for s in &group.sessions {
                        let time = session::format_time(s.modified);
                        let title = session::get_session_title(s);
                        let sid = &s.session_id[..s.session_id.len().min(8)];

                        // Resolve workspace: prefer task project_path for sandbox sessions
                        let resolved_ws = match s.agent.as_str() {
                            "claude" => claude_task_repo.get(&s.session_id).cloned(),
                            "pi" => pi_task_repo
                                .get(&s.path.to_string_lossy().to_string())
                                .cloned(),
                            _ => None,
                        };
                        let ws = session::format_workspace(
                            resolved_ws.as_deref().or(s.workspace.as_deref()),
                        );

                        let agent_short = memory::format::agent_short_name(&s.agent);
                        // Resolve task_id for memory badge lookup
                        let task_id_for_mem = match s.agent.as_str() {
                            "claude" => claude_session_to_task.get(&s.session_id).cloned(),
                            "pi" => pi_path_to_task
                                .get(&s.path.to_string_lossy().to_string())
                                .cloned(),
                            _ => None,
                        };
                        let mem_count = task_id_for_mem
                            .and_then(|tid| memory_counts.get(&tid).copied())
                            .unwrap_or(0);
                        let mem_badge = if mem_count > 0 {
                            format!("M:{}", mem_count)
                        } else {
                            "   ".to_string()
                        };
                        println!(
                            "  {:<6} {:<10} {:<8}  {:<width$}  {:<12} {:>5} {}",
                            time,
                            agent_short,
                            sid,
                            title,
                            ws,
                            mem_badge,
                            session::format_size(s.size_bytes),
                            width = max_title_width.min(60)
                        );
                    }
                }
                println!();
            }
            cli::SessionAction::Read { id, raw } => {
                if raw {
                    let content = session::read_session_raw(&id)?;
                    println!("{}", content);
                } else {
                    let content = session::read_session(&id)?;
                    // Use pager for formatted output
                    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
                    let mut child = std::process::Command::new(&pager)
                        .stdin(std::process::Stdio::piped())
                        .spawn()
                        .unwrap_or_else(|_| {
                            std::process::Command::new("cat")
                                .stdin(std::process::Stdio::piped())
                                .spawn()
                                .expect("cat should always work")
                        });
                    if let Some(mut stdin) = child.stdin.take() {
                        use std::io::Write;
                        let _ = stdin.write_all(content.as_bytes());
                    }
                    let _ = child.wait();
                }
            }
        },
        Commands::Backup { output } => {
            let output_path = output.map(PathBuf::from);
            let path = backup::create_backup(output_path)?;
            println!("Backup created: {}", path.display());
        }
        Commands::Import { path } => {
            let zip_path = PathBuf::from(path);
            backup::import_backup(&zip_path)?;
        }
        Commands::Proxy { action } => match action {
            cli::ProxyAction::Start => {
                let cfg = config::load().unwrap_or_default();
                privacy_filter::start_proxy(&cfg.privacy_filter)?;
            }
            cli::ProxyAction::Stop => {
                privacy_filter::stop_proxy()?;
            }
            cli::ProxyAction::Status => {
                let cfg = config::load().unwrap_or_default();
                privacy_filter::proxy_status(cfg.privacy_filter.proxy_port);
            }
        },
        Commands::Cron { action } => match action {
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
            CronAction::Stop { id } => {
                let cron_id = resolve_cron_id(&db, &id)?;
                cmd_cron_edit(&db, cron_id, None, None, None, false, true, None)?;
            }
            CronAction::Start { id } => {
                let cron_id = resolve_cron_id(&db, &id)?;
                cmd_cron_edit(&db, cron_id, None, None, None, true, false, None)?;
            }
            CronAction::Kill { id } => {
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
        Commands::Sandbox { action } => match action {
            SandboxAction::Spawn {
                repo_path,
                task,
                image,
                fresh,
                session_id,
                branch,
                factory,
                hermes,
                pi,
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
                    false,
                    factory_enabled,
                    hermes,
                    pi,
                )?;
            }
            SandboxAction::List => {
                cmd_sandbox_list(&db)?;
            }
            SandboxAction::Bash { container_or_path } => {
                let task_id = resolve_sandbox_id(&db, &container_or_path)?;
                cmd_sandbox_bash(&db, task_id)?;
            }
            SandboxAction::Attach {
                container_or_path,
                fresh,
                btw,
                hermes,
                pi,
                session,
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

                let looks_like_path = effective_path.starts_with('.')
                    || effective_path.starts_with('/')
                    || effective_path.starts_with('~')
                    || effective_path.contains('/')
                    || std::path::Path::new(&effective_path).exists();

                // Determine whether a usable (running) sandbox already exists for the
                // target. A resolved task whose container is dead — e.g. after `kill` or
                // `kill --all`, which retain the task record but stop the container — must
                // not be attached to; for path inputs we transparently re-spawn instead.
                let running_task_id = match resolve_sandbox_id(&db, &effective_path) {
                    Ok(task_id) => {
                        let container_alive = db
                            .get_task_by_id(&task_id)
                            .ok()
                            .flatten()
                            .and_then(|t| t.container_id)
                            .map(|cid| {
                                matches!(
                                    PodmanSandbox::new().status(&cid),
                                    Ok(sandbox::ContainerStatus::Running)
                                )
                            })
                            .unwrap_or(false);

                        if container_alive {
                            Some(task_id)
                        } else if looks_like_path {
                            None
                        } else {
                            // Non-path target (task ID / container) with a dead container:
                            // surface the original "not running" error from attach.
                            cmd_sandbox_attach(
                                &db,
                                task_id,
                                fresh,
                                btw,
                                hermes,
                                pi,
                                session.clone(),
                            )?;
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        if looks_like_path {
                            None
                        } else {
                            return Err(e);
                        }
                    }
                };

                let task_id = match running_task_id {
                    Some(id) => id,
                    None => {
                        eprintln!(
                            "No running sandbox for '{}', spawning one...",
                            effective_path
                        );
                        let cfg = config::load().unwrap_or_default();
                        cmd_sandbox_spawn(
                            &db,
                            effective_path,
                            None,
                            "nibble-sandbox:latest".to_string(),
                            fresh,
                            None,
                            true,
                            cfg.factory.enabled,
                            hermes,
                            pi,
                        )?
                    }
                };

                cmd_sandbox_attach(&db, task_id, fresh, btw, hermes, pi, session.clone())?;
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
                                db.get_task_by_id(&id)?.and_then(|t| t.worktree_path)
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
        },
        Commands::Hermes { action } => match action {
            HermesAction::Init => {
                cmd_hermes_init(&db)?;
            }
            HermesAction::Attach { fresh } => {
                cmd_hermes_attach(&db, fresh)?;
            }
            HermesAction::Mount {
                repo_path,
                name,
                yes,
            } => {
                cmd_hermes_mount(&db, &repo_path, name.as_deref(), yes)?;
            }
            HermesAction::Unmount { repo_path, yes } => {
                cmd_hermes_unmount(&db, &repo_path, yes)?;
            }
            HermesAction::List => {
                cmd_hermes_list(&db)?;
            }
            HermesAction::Kill => {
                cmd_hermes_kill(&db)?;
            }
        },
        Commands::Inject { task_id, message } => {
            let task = db
                .get_task_by_id(&task_id)?
                .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

            let _hermes = task.agent_type == AgentType::Hermes;
            if _hermes {
                anyhow::bail!("Inject is not yet supported for Hermes sandboxes. Use `nibble sandbox attach` instead.");
            }
            let _pi = task.agent_type == AgentType::Pi;
            if _pi {
                anyhow::bail!("Inject is not yet supported for Pi sandboxes. Use `nibble sandbox attach` instead.");
            }
            agent_input::inject(&task, &message)?;
            println!("Message injected into task {}", task_id);
        }

        Commands::Listen => {
            let cfg = config::load().unwrap_or_default();

            if !cfg.telegram.is_configured() {
                anyhow::bail!("Telegram is not configured. Run scripts/setup-telegram.sh first.");
            }

            // Run an initial prune before entering the listener loop so stale
            // tasks from a previous crash or reboot are cleaned up immediately.
            let _ = prune_stale_tasks(&db);

            notifications::telegram_listener::run(&db, &cfg.telegram)?;
        }

        Commands::Lm { action } => {
            let cfg = config::load().unwrap_or_default();
            match action {
                LmAction::List => {
                    let models = lm::list_models(&cfg.lm)?;
                    lm::print_list(&models);
                }
                LmAction::Use { model } => {
                    lm::use_model(&cfg.lm, &model)?;
                }
            }
        }
        Commands::Usage { action } => {
            let pricing = usage::PricingTable::load().context("Failed to load pricing table")?;
            match action {
                cli::UsageAction::Scan { report } => {
                    let stats = usage::scan_all(&db, &pricing)?;
                    eprintln!(
                        "claude: {}/{} new (seen/inserted),  pi: {}/{} new",
                        stats.claude_seen, stats.claude_inserted, stats.pi_seen, stats.pi_inserted,
                    );
                    if report {
                        usage::print_report(&db, "model", None, usage::ReportFormat::Table)?;
                    }
                }
                cli::UsageAction::Report { since, by, json } => {
                    let fmt = if json {
                        usage::ReportFormat::Json
                    } else {
                        usage::ReportFormat::Table
                    };
                    usage::print_report(&db, &by, since.as_deref(), fmt)?;
                }
                cli::UsageAction::Pricing => {
                    usage::print_pricing(&pricing)?;
                }
            }
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
/// is invisible to Claude (wrong extension) but kept forever — nibble never deletes
/// session data, so it stays recoverable on disk.
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

// ── Pi helper functions ────────────────────────────────────────────────────────

/// Ensure `~/.pi/agent/skills` is a symlink pointing to `~/.claude/skills/`.
///
/// Called at spawn time so the AI Factory pipeline skills are available to pi
/// without duplicating files.  Non-fatal on failure — logs a warning and continues.
fn ensure_pi_skills_symlink(home_dir: &std::path::Path) {
    let pi_skills = home_dir.join(".pi").join("agent").join("skills");
    let claude_skills = home_dir.join(".claude").join("skills");

    // Already a symlink — no-op
    if pi_skills.is_symlink() {
        return;
    }

    // Exists as a real directory — warn and skip (don't clobber user data)
    if pi_skills.is_dir() {
        eprintln!(
            "  Warning: {} exists as a directory, skipping skills symlink",
            pi_skills.display()
        );
        return;
    }

    if let Err(e) = std::os::unix::fs::symlink(&claude_skills, &pi_skills) {
        eprintln!(
            "  Warning: failed to create skills symlink {} → {}: {}",
            pi_skills.display(),
            claude_skills.display(),
            e
        );
    }
}

/// Discover the most recent Pi session file inside a specific running container.
///
/// This is more reliable than `find_pi_session_for_cwd` when multiple Pi sandboxes
/// are active, because it looks at the session files *inside the container* rather
/// than scanning the host's ~/.pi which is shared across all sandboxes.
fn discover_pi_session_in_container(
    container_id: &str,
    pi_slug: &str,
) -> Option<std::path::PathBuf> {
    let cmd = format!(
        "ls -t /home/node/.pi/agent/sessions/{}/{{*.jsonl,**/*.jsonl}} 2>/dev/null | head -1",
        pi_slug
    );
    let output = std::process::Command::new("podman")
        .args(["exec", container_id, "sh", "-c", &cmd])
        .output()
        .ok()?;
    let container_path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if container_path.is_empty() {
        return None;
    }
    // Convert container path back to host path
    let home = dirs::home_dir()?;
    let host_path = container_path.replacen("/home/node/", &format!("{}/", home.display()), 1);
    let path = std::path::PathBuf::from(host_path);
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// Find the most recent Pi session file whose `cwd` matches the given path.
///
/// Pi session files start with a JSON line like:
///   {"type":"session","cwd":"/nibble",...}
///
/// Returns the absolute path to the matching session file, or None if no match.
/// Find the most recent Pi session file for a given container working directory.
///
/// With repo-specific mount points (e.g. `/nibble`), Pi stores sessions in a
/// dedicated directory (`--nibble--`). We look there directly instead of scanning
/// all sessions and grepping for a `cwd` field.
///
/// Falls back to the legacy `--workspace--` directory so old sessions are still
/// discoverable.
fn find_pi_session_for_cwd(container_dir: &str) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let slug = pi_session_dir_name(container_dir);
    let sessions_dir = home.join(".pi").join("agent").join("sessions");

    // Helper: find newest .jsonl in a specific slug directory.
    let find_newest = |slug_name: &str| -> Option<(std::path::PathBuf, std::time::SystemTime)> {
        let slug_dir = sessions_dir.join(slug_name);
        if !slug_dir.exists() {
            return None;
        }
        let mut newest: Option<(std::path::PathBuf, std::time::SystemTime)> = None;
        for file in std::fs::read_dir(&slug_dir).ok()?.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let meta = file.metadata().ok()?;
            let mtime = meta.modified().ok()?;
            if newest.as_ref().is_none_or(|(_, t)| mtime > *t) {
                newest = Some((path, mtime));
            }
        }
        newest
    };

    // Try the repo-specific directory first, then fall back to legacy.
    find_newest(&slug)
        .or_else(|| find_newest("--workspace--"))
        .map(|(path, _mtime)| path)
}

// ── Hermes command handlers ──────────────────────────────────────────────────

/// Sentinel repo_path for the hermes sandbox.
const HERMES_REPO_PATH: &str = "__hermes__";

/// Find the running hermes sandbox task, if any.
fn find_hermes_sandbox(db: &Database) -> Result<Option<Task>> {
    let sandbox = PodmanSandbox::new();
    for task in db.list_sandbox_tasks()? {
        if task.agent_type == AgentType::Hermes {
            if let Some(ref cid) = task.container_id {
                if let Ok(crate::sandbox::ContainerStatus::Running) = sandbox.status(cid) {
                    return Ok(Some(task));
                }
            }
        }
    }
    Ok(None)
}

/// Spawn the singleton Hermes sandbox. Returns the task_id.
fn cmd_hermes_spawn_internal(db: &Database) -> Result<String> {
    // INV-1: Only one hermes sandbox at a time.
    if let Some(existing) = find_hermes_sandbox(db)? {
        let short = &existing.task_id[..8.min(existing.task_id.len())];
        eprintln!("Hermes sandbox already running (task {})", short);
        return Ok(existing.task_id.clone());
    }

    let sandbox = PodmanSandbox::new();
    if !sandbox.is_available()? {
        anyhow::bail!("Podman is not installed. Run ./install.sh to set it up.");
    }

    let cfg = config::load().unwrap_or_default();
    let hcfg = &cfg.hermes;
    let image = hcfg.image.clone();
    sandbox.ensure_image_with_opts(&image, false)?;

    let task_id = uuid::Uuid::new_v4().to_string();

    let mut env_vars = HashMap::new();
    for key in &[
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "HOME",
        "CLAUDE_CONFIG_DIR",
        "KIMI_API_KEY",
        "ZAI_API_KEY",
    ] {
        if let Ok(val) = std::env::var(key) {
            env_vars.insert(key.to_string(), val);
        }
    }

    // Redirect LLM API traffic through the privacy filter proxy if enabled.
    {
        let pf_cfg = config::load().unwrap_or_default().privacy_filter;
        if pf_cfg.enabled {
            let proxy_url = privacy_filter::proxy_endpoint(pf_cfg.proxy_port);
            if std::env::var("ANTHROPIC_BASE_URL").is_err() {
                env_vars.insert("ANTHROPIC_BASE_URL".to_string(), proxy_url.clone());
            }
            if std::env::var("OPENAI_BASE_URL").is_err() {
                env_vars.insert("OPENAI_BASE_URL".to_string(), proxy_url.clone());
            }
            env_vars.insert("BASE_URL".to_string(), proxy_url);
        }
    }

    let mut extra_volumes = Vec::new();

    // INV-5: Always mount ~/.hermes/ so sessions/memories persist
    let home_dir = dirs::home_dir().context("Failed to get home directory")?;
    let hermes_dir = home_dir.join(".hermes");
    if !hermes_dir.exists() {
        eprintln!("  Warning: ~/.hermes/ does not exist. Creating minimal structure.");
        eprintln!("           Run `hermes setup` to configure your LLM provider.");
        std::fs::create_dir_all(hermes_dir.join("sessions"))?;
        std::fs::create_dir_all(hermes_dir.join("memories"))?;
        std::fs::create_dir_all(hermes_dir.join("skills"))?;
        std::fs::create_dir_all(hermes_dir.join("cron"))?;
        std::fs::create_dir_all(hermes_dir.join("logs"))?;
    }
    extra_volumes.push(format!("{}:/home/node/.hermes:rw", hermes_dir.display()));

    // Seed repos from config.toml on first init (INV-9)
    let legacy_repos = &hcfg.repos;
    if !legacy_repos.is_empty() {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut resolved = Vec::new();
        for repo in legacy_repos {
            let expanded = if repo.starts_with('~') {
                repo.replacen('~', &home, 1)
            } else {
                repo.clone()
            };
            let path = std::path::PathBuf::from(&expanded);
            if let Ok(abs) = path.canonicalize() {
                resolved.push((String::new(), abs));
            }
        }
        if !resolved.is_empty() {
            let mounts = config::resolve_repo_mounts(&resolved);
            let seed_data: Vec<(String, std::path::PathBuf)> = mounts.into_iter().collect();
            let seeded = db.seed_hermes_repos_from_config(&seed_data)?;
            if seeded > 0 {
                println!("  Seeded {} repo(s) from config.toml", seeded);
            }
        }
    }

    // Mount all repos from hermes_repos table (INV-3, INV-4)
    let repo_mounts = db.list_hermes_repos()?;
    let mut mount_entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    for (repo_path, mount_name) in &repo_mounts {
        let abs = std::path::PathBuf::from(repo_path);
        if abs.exists() {
            mount_entries.push((mount_name.clone(), abs));
        } else {
            eprintln!(
                "  Warning: mounted repo '{}' no longer exists, skipping",
                repo_path
            );
        }
    }
    let resolved_mounts = config::resolve_repo_mounts(&mount_entries);
    for (mount_name, abs_path) in &resolved_mounts {
        extra_volumes.push(format!("{}:/repos/{}:rw", abs_path.display(), mount_name));
    }
    if !resolved_mounts.is_empty() {
        let names: Vec<&str> = resolved_mounts.iter().map(|(n, _)| n.as_str()).collect();
        println!(
            "  Repos:     mounted {} repo(s): {}",
            resolved_mounts.len(),
            names.join(", ")
        );
    }

    // INV-2: Gateway as PID 1 if configured
    // Check for updates before starting the gateway so the running container
    // is always on the latest hermes-agent release.
    let entrypoint = if hcfg.gateway {
        let update_cmd = "uv pip install --upgrade hermes-agent \
            --python /home/node/.hermes-agent/venv/bin/python \
            2>&1 | grep -v '^Resolved\\|^Audited' || true";
        vec![
            "/bin/bash".to_string(),
            "-lc".to_string(),
            format!("{} && hermes gateway", update_cmd),
        ]
    } else {
        vec![]
    };

    let sb_config = SandboxConfig {
        image,
        env_vars,
        extra_volumes,
        entrypoint,
        ..SandboxConfig::default()
    };

    // INV-7: No primary repo. Create a persistent empty dir under ~/.nibble/hermes/
    // that gets reused across restarts (no tempdir accumulation).
    let workspace_dir = home_dir.join(".nibble").join("hermes").join("workspace");
    std::fs::create_dir_all(&workspace_dir)?;
    println!("Spawning Hermes sandbox…");
    let info = sandbox.spawn(&task_id, &workspace_dir, &sb_config)?;

    // Ensure privacy filter proxy is running if enabled.
    {
        let pf_cfg = config::load().unwrap_or_default().privacy_filter;
        if pf_cfg.enabled {
            if let Err(e) = privacy_filter::ensure_proxy_running(&pf_cfg) {
                eprintln!("  Privacy:   ⚠️  Could not start privacy filter proxy: {e}");
                if !pf_cfg.fail_open {
                    anyhow::bail!(
                        "Privacy filter is enabled but proxy failed to start and fail_open=false"
                    );
                }
            } else {
                println!(
                    "  Privacy:   LLM privacy proxy active on port {}",
                    pf_cfg.proxy_port
                );
            }
        }
    }

    // Health check: wait a moment and verify the container is still running.
    // If the gateway crashes immediately (e.g. hermes binary not found),
    // podman will still report "Created" briefly before the process exits.
    if hcfg.gateway {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        match sandbox.status(&info.id)? {
            crate::sandbox::ContainerStatus::Running => {}
            _ => {
                let logs = std::process::Command::new("podman")
                    .args(["logs", "--tail", "20", &info.id])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                    .unwrap_or_default();
                let mut task = Task::new(
                    task_id.clone(),
                    AgentType::Hermes,
                    "[hermes:sandbox]".to_string(),
                    None,
                    None,
                );
                task.sandbox_type = SandboxType::Podman;
                task.container_id = Some(info.id.clone());
                task.container_name = Some(info.name.clone());
                task.repo_path = Some(HERMES_REPO_PATH.to_string());
                task.set_exited(None);
                db.insert_task(&task)?;

                anyhow::bail!(
                    "Hermes container crashed immediately. Gateway logs:\n{}\n\
                     Try rebuilding the image:\n  nibble sandbox build --image nibble-hermes:latest --rebuild",
                    logs.trim_end()
                );
            }
        }
    }

    let mut task = Task::new(
        task_id.clone(),
        AgentType::Hermes,
        "[hermes:sandbox]".to_string(),
        None,
        None,
    );
    task.sandbox_type = SandboxType::Podman;
    task.container_id = Some(info.id.clone());
    task.container_name = Some(info.name.clone());
    task.repo_path = Some(HERMES_REPO_PATH.to_string());
    task.sandbox_config = Some(sb_config);
    task.context = Some(TaskContext {
        url: None,
        project_path: None,
        session_id: None,
        claude_session_id: None,
        extra: HashMap::new(),
    });
    db.insert_task(&task)?;

    let short_id = &task_id[..task_id.len().min(8)];
    println!("\nHermes sandbox started:");
    println!("  Task ID:   {} ({})", short_id, task_id);
    println!("  Container: {}", info.name);
    println!(
        "  Gateway:   {}",
        if hcfg.gateway {
            "running (PID 1)"
        } else {
            "disabled"
        }
    );
    if !resolved_mounts.is_empty() {
        println!("  Mounted:   {} repo(s)", resolved_mounts.len());
    }
    println!();

    Ok(task_id)
}

fn cmd_hermes_init(db: &Database) -> Result<()> {
    let _task_id = cmd_hermes_spawn_internal(db)?;
    println!("Hermes sandbox ready.");
    println!("  Attach:    nibble hermes attach");
    println!("  Add repo:  nibble hermes mount /path/to/repo");
    println!("  List:      nibble hermes list");
    println!();
    Ok(())
}

fn cmd_hermes_attach(db: &Database, fresh: bool) -> Result<()> {
    // Auto-spawn if no sandbox exists (same pattern as sandbox attach)
    if find_hermes_sandbox(db)?.is_none() {
        eprintln!("No Hermes sandbox found. Spawning one…");
        cmd_hermes_spawn_internal(db)?;
    }

    let task =
        find_hermes_sandbox(db)?.ok_or_else(|| anyhow::anyhow!("No Hermes sandbox running"))?;
    let container_id = task
        .container_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Hermes task has no container_id"))?;

    let shell_cmd = if fresh {
        "hermes".to_string()
    } else {
        "hermes --continue 2>/dev/null || hermes".to_string()
    };

    let podman_args: Vec<String> = vec![
        "exec".into(),
        "-it".into(),
        "-e".into(),
        "TERM=xterm-256color".into(),
        "-e".into(),
        "PATH=/home/node/.local/bin:/home/node/.hermes-agent/venv/bin:/home/node/.cargo/bin:/usr/local/bin:/usr/bin:/bin".into(),
        "-e".into(),
        "CLAUDE_CONFIG_DIR=/home/node/.claude".into(),
        "-e".into(),
        format!("AGENT_TASK_ID={}", task.task_id),
        "-w".into(),
        "/home/node".into(),
        container_id.clone(),
        "/bin/bash".into(),
        "-lc".into(),
        shell_cmd,
    ];

    eprintln!("Attaching to Hermes sandbox ({})…", container_id);
    eprintln!("(Exit hermes or press Ctrl+C to detach — the container keeps running)");

    let err = std::os::unix::process::CommandExt::exec(
        std::process::Command::new("podman").args(&podman_args),
    );
    anyhow::bail!("Failed to exec podman: {}", err)
}

fn cmd_hermes_mount(
    db: &Database,
    repo_path: &str,
    name: Option<&str>,
    skip_confirm: bool,
) -> Result<()> {
    // Validate path exists
    let expanded = if repo_path.starts_with('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        repo_path.replacen('~', &home, 1)
    } else {
        repo_path.to_string()
    };
    let abs = std::path::PathBuf::from(&expanded)
        .canonicalize()
        .with_context(|| format!("Path '{}' does not exist", repo_path))?;
    let canonical = abs.to_string_lossy().to_string();

    // Check if already mounted
    if let Some((id, existing_name)) = db.get_hermes_repo(&canonical)? {
        anyhow::bail!(
            "Repo '{}' is already mounted as '/repos/{}' (id {})",
            canonical,
            existing_name,
            id
        );
    }

    // Determine mount name
    let mount_name = match name {
        Some(n) => n.to_string(),
        None => abs
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string()),
    };

    // Check mount_name uniqueness
    if db.hermes_mount_name_exists(&mount_name)? {
        anyhow::bail!(
            "Mount name '{}' is already in use. Use --name to specify a different name.",
            mount_name
        );
    }

    // Insert into DB
    db.insert_hermes_repo(&canonical, &mount_name)?;
    println!("Added repo '{}' as '/repos/{}'", canonical, mount_name);

    let needs_restart = find_hermes_sandbox(db)?.is_some();
    if needs_restart {
        eprintln!();
        eprintln!("⚠️  This will restart the Hermes sandbox, interrupting any in-progress tasks.");
        if !skip_confirm {
            eprint!("   Proceed? [y/N] ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!(
                    "Aborted. Repo '{}' added to mount list but sandbox not restarted.",
                    canonical
                );
                println!("         Run `nibble hermes kill && nibble hermes init` to apply.");
                return Ok(());
            }
        }
        eprintln!();
        cmd_hermes_kill_internal(db)?;
        cmd_hermes_spawn_internal(db)?;
        println!("Hermes sandbox restarted with updated mounts.");
    }

    Ok(())
}

fn cmd_hermes_unmount(db: &Database, repo_path: &str, skip_confirm: bool) -> Result<()> {
    let expanded = if repo_path.starts_with('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        repo_path.replacen('~', &home, 1)
    } else {
        repo_path.to_string()
    };
    let abs = std::path::PathBuf::from(&expanded)
        .canonicalize()
        .with_context(|| format!("Path '{}' does not exist or cannot be resolved", repo_path))?;
    let canonical = abs.to_string_lossy().to_string();

    let deleted = db.delete_hermes_repo(&canonical)?;
    if !deleted {
        anyhow::bail!("Repo '{}' is not currently mounted", canonical);
    }
    println!("Removed repo '{}'", canonical);

    let needs_restart = find_hermes_sandbox(db)?.is_some();
    if needs_restart {
        eprintln!();
        eprintln!("⚠️  This will restart the Hermes sandbox, interrupting any in-progress tasks.");
        if !skip_confirm {
            eprint!("   Proceed? [y/N] ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!(
                    "Aborted. Repo '{}' removed from mount list but sandbox not restarted.",
                    canonical
                );
                println!("         Run `nibble hermes kill && nibble hermes init` to apply.");
                return Ok(());
            }
        }
        eprintln!();
        cmd_hermes_kill_internal(db)?;
        cmd_hermes_spawn_internal(db)?;
        println!("Hermes sandbox restarted with updated mounts.");
    }

    Ok(())
}

fn cmd_hermes_list(db: &Database) -> Result<()> {
    match find_hermes_sandbox(db)? {
        Some(task) => {
            let container_id = task.container_id.as_deref().unwrap_or("unknown");
            let short_id = &task.task_id[..task.task_id.len().min(8)];
            println!("Hermes sandbox: running");
            println!("  Task ID:   {} ({})", short_id, task.task_id);
            println!("  Container: {}", container_id);
        }
        None => {
            println!("Hermes sandbox: not running");
        }
    }

    let repos = db.list_hermes_repos()?;
    if repos.is_empty() {
        println!("  Mounted repos: (none)");
        println!("  Add one with: nibble hermes mount /path/to/repo");
    } else {
        println!("  Mounted repos:");
        for (path, name) in &repos {
            println!("    /repos/{} ← {}", name, path);
        }
    }
    println!();
    Ok(())
}

/// Internal kill — stops the hermes sandbox without printing extra messages.
fn cmd_hermes_kill_internal(db: &Database) -> Result<()> {
    if let Some(task) = find_hermes_sandbox(db)? {
        let container_id = task
            .container_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Hermes task has no container_id"))?;
        PodmanSandbox::new().kill(&container_id)?;
        let mut task = task;
        task.set_exited(None);
        db.update_task(&task)?;
    }
    Ok(())
}

fn cmd_hermes_kill(db: &Database) -> Result<()> {
    match find_hermes_sandbox(db)? {
        Some(task) => {
            let short_id = task.task_id[..task.task_id.len().min(8)].to_string();
            let container_id = task
                .container_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Hermes task has no container_id"))?;
            PodmanSandbox::new().kill(&container_id)?;
            let mut task = task;
            task.set_exited(None);
            db.update_task(&task)?;
            println!("Killed Hermes sandbox (task {})", short_id);
            println!("  Mounted repos are preserved. Run `nibble hermes init` to restart.");
        }
        None => {
            println!("No Hermes sandbox is running.");
        }
    }
    Ok(())
}

/// Spawn a sandboxed Claude Code agent.  Returns the new task_id on success.
// Each argument maps to a distinct CLI flag on `nibble sandbox spawn`; bundling
// them into a struct would only move the same surface elsewhere.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_sandbox_spawn(
    db: &Database,
    repo_path: String,
    task_desc: Option<String>,
    image: String,
    fresh: bool,
    session_id: Option<String>,
    no_attach: bool,
    factory_enabled: bool,
    hermes: bool,
    pi: bool,
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
    if let Some(task) = db.get_task_by_repo_path(&abs_repo_path_early)? {
        if let Some(ref cid) = task.container_id {
            if let Ok(crate::sandbox::ContainerStatus::Running) = sandbox.status(cid) {
                let existing_task_id = &task.task_id;
                eprintln!(
                    "⚠️  A sandbox for '{}' already exists (task {}).",
                    abs_repo_path_early,
                    &existing_task_id[..existing_task_id.len().min(8)]
                );
                eprintln!("   Attaching to the existing sandbox instead of spawning a new one.");
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
                        hermes,
                        pi,
                        None,
                    )?;
                }
                return Ok(existing_task_id.clone());
            }
        }
    }

    // INV-5: Only one Hermes sandbox at a time. Check for existing running Hermes sandboxes.
    if hermes {
        for task in db.list_sandbox_tasks()? {
            if task.agent_type == AgentType::Hermes {
                if let Some(ref cid) = task.container_id {
                    if let Ok(crate::sandbox::ContainerStatus::Running) = sandbox.status(cid) {
                        let tid = &task.task_id;
                        eprintln!(
                            "⚠️  A Hermes sandbox already exists (task {}).",
                            &tid[..tid.len().min(8)]
                        );
                        eprintln!("   Only one Hermes sandbox is supported at a time.");
                        eprintln!("   Attaching to the existing sandbox instead.");
                        eprintln!();
                        if no_attach {
                            eprintln!("Attach with:");
                            eprintln!("  nibble sandbox attach {}", tid);
                        } else {
                            cmd_sandbox_attach(db, tid.clone(), fresh, false, hermes, pi, None)?;
                        }
                        return Ok(tid.clone());
                    }
                }
            }
        }
    }

    // Determine the effective image: hermes sandboxes use their own image.
    let effective_image = if hermes {
        let cfg = config::load().unwrap_or_default();
        cfg.hermes.image.clone()
    } else {
        image
    };

    sandbox.ensure_image_with_opts(&effective_image, false)?;

    let task_id = uuid::Uuid::new_v4().to_string();

    let mut env_vars = HashMap::new();
    for key in &[
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "HOME",
        "CLAUDE_CONFIG_DIR",
        "KIMI_API_KEY",
        "ZAI_API_KEY",
    ] {
        if let Ok(val) = std::env::var(key) {
            env_vars.insert(key.to_string(), val);
        }
    }

    // Redirect LLM API traffic through the privacy filter proxy if enabled.
    {
        let pf_cfg = config::load().unwrap_or_default().privacy_filter;
        if pf_cfg.enabled {
            let proxy_url = privacy_filter::proxy_endpoint(pf_cfg.proxy_port);
            if std::env::var("ANTHROPIC_BASE_URL").is_err() {
                env_vars.insert("ANTHROPIC_BASE_URL".to_string(), proxy_url.clone());
            }
            if std::env::var("OPENAI_BASE_URL").is_err() {
                env_vars.insert("OPENAI_BASE_URL".to_string(), proxy_url.clone());
            }
            env_vars.insert("BASE_URL".to_string(), proxy_url);
        }
    }

    let mut extra_volumes = Vec::new();

    // Hermes-specific mounts: ~/.hermes/ config dir
    let hermes_cfg = if hermes {
        Some(config::load().unwrap_or_default().hermes)
    } else {
        None
    };

    if hermes {
        let home_dir = dirs::home_dir().context("Failed to get home directory")?;
        let hermes_dir = home_dir.join(".hermes");
        if !hermes_dir.exists() {
            eprintln!("  Warning: ~/.hermes/ does not exist. Creating minimal structure.");
            eprintln!("           Run `hermes setup` to configure your LLM provider.");
            std::fs::create_dir_all(hermes_dir.join("sessions"))?;
            std::fs::create_dir_all(hermes_dir.join("memories"))?;
            std::fs::create_dir_all(hermes_dir.join("skills"))?;
            std::fs::create_dir_all(hermes_dir.join("cron"))?;
            std::fs::create_dir_all(hermes_dir.join("logs"))?;
        }
        extra_volumes.push(format!("{}:/home/node/.hermes:rw", hermes_dir.display()));
    }

    // Hermes gateway as PID 1 if configured
    // Check for updates before starting the gateway so the running container
    // is always on the latest hermes-agent release.
    let entrypoint = if hermes {
        let hcfg = hermes_cfg.as_ref().unwrap();
        if hcfg.gateway {
            let update_cmd = "uv pip install --upgrade hermes-agent \
                --python /home/node/.hermes-agent/venv/bin/python \
                2>&1 | grep -v '^Resolved\\|^Audited' || true";
            vec![
                "/bin/bash".to_string(),
                "-lc".to_string(),
                format!("{} && hermes gateway", update_cmd),
            ]
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    // Mount ~/.pi config dir (always — skills/extensions/sessions are needed
    // regardless of the agent type; sandboxes created without --pi would
    // otherwise miss the .pi mount and Pi sessions would have no config).
    {
        let home_dir = dirs::home_dir().context("Failed to get home directory")?;
        let pi_dir = home_dir.join(".pi");
        let agent_dir = pi_dir.join("agent");

        if agent_dir.is_symlink() {
            let resolved = agent_dir
                .canonicalize()
                .with_context(|| "Failed to resolve ~/.pi/agent symlink".to_string())?;
            std::fs::create_dir_all(&resolved)
                .with_context(|| "Failed to create resolved ~/.pi/agent target")?;
            std::fs::create_dir_all(resolved.join("skills"))
                .with_context(|| "Failed to create ~/.pi/agent/skills")?;
            std::fs::create_dir_all(resolved.join("sessions"))
                .with_context(|| "Failed to create ~/.pi/agent/sessions")?;
            std::fs::create_dir_all(resolved.join("extensions"))
                .with_context(|| "Failed to create ~/.pi/agent/extensions")?;
            extra_volumes.push(format!("{}:/home/node/.pi:rw", pi_dir.display()));
            extra_volumes.push(format!("{}:/home/node/.pi/agent:rw", resolved.display()));
        } else {
            if !agent_dir.exists() {
                std::fs::create_dir_all(&agent_dir)
                    .with_context(|| "Failed to create ~/.pi/agent")?;
            }
            std::fs::create_dir_all(agent_dir.join("skills"))
                .with_context(|| "Failed to create ~/.pi/agent/skills")?;
            std::fs::create_dir_all(agent_dir.join("sessions"))
                .with_context(|| "Failed to create ~/.pi/agent/sessions")?;
            std::fs::create_dir_all(agent_dir.join("extensions"))
                .with_context(|| "Failed to create ~/.pi/agent/extensions")?;
            extra_volumes.push(format!("{}:/home/node/.pi:rw", pi_dir.display()));
        }
        ensure_pi_skills_symlink(&home_dir);
    }

    let config = SandboxConfig {
        image: effective_image,
        env_vars,
        extra_volumes,
        entrypoint,
        ..SandboxConfig::default()
    };

    println!("Spawning sandbox for '{}'…", repo_path);
    let info = sandbox.spawn(&task_id, &repo, &config)?;

    // Ensure privacy filter proxy is running if enabled.
    {
        let pf_cfg = config::load().unwrap_or_default().privacy_filter;
        if pf_cfg.enabled {
            if let Err(e) = privacy_filter::ensure_proxy_running(&pf_cfg) {
                eprintln!("  Privacy:   ⚠️  Could not start privacy filter proxy: {e}");
                if !pf_cfg.fail_open {
                    anyhow::bail!(
                        "Privacy filter is enabled but proxy failed to start and fail_open=false"
                    );
                }
            } else {
                println!(
                    "  Privacy:   LLM privacy proxy active on port {}",
                    pf_cfg.proxy_port
                );
            }
        }
    }

    let repo_name = repo
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| repo_path.clone());

    // Post-spawn setup (skip for Hermes)
    // Pi-specific setup: install pi via npm, run setup.sh, inject AGENTS.md
    if !hermes {
        // Pi install: npm install @earendil-works/pi-coding-agent (non-fatal)
        if pi {
            let pi_cfg = config::load().unwrap_or_default().pi;
            if pi_cfg.install_on_spawn {
                let status = std::process::Command::new("podman")
                    .args([
                        "exec",
                        &info.id,
                        "sudo",
                        "npm",
                        "install",
                        "-g",
                        "@earendil-works/pi-coding-agent",
                    ])
                    .status();
                let pi_core_ok = matches!(status, Ok(s) if s.success());
                match &status {
                    Ok(s) if s.success() => {
                        println!("  Tools:     @earendil-works/pi-coding-agent installed")
                    }
                    Ok(_) => eprintln!(
                        "  Tools:     ⚠️  pi npm install exited non-zero (install manually inside)"
                    ),
                    Err(e) => eprintln!("  Tools:     ⚠️  pi npm install failed to run: {e}"),
                }

                // Install configured pi extensions (e.g. pi-dynamic-workflows) so
                // they are available in every Pi sandbox like any other extension.
                // Non-fatal — runs as the `node` user that owns ~/.pi.
                if pi_core_ok {
                    for ext in &pi_cfg.extensions {
                        let ext_status = std::process::Command::new("podman")
                            .args([
                                "exec",
                                "--user",
                                "node",
                                &info.id,
                                "/bin/bash",
                                "-lc",
                                &format!("pi install {ext}"),
                            ])
                            .status();
                        match ext_status {
                            Ok(s) if s.success() => {
                                println!("  Tools:     pi extension installed: {ext}")
                            }
                            Ok(_) => {
                                eprintln!("  Tools:     ⚠️  pi install {ext} exited non-zero")
                            }
                            Err(e) => eprintln!(
                                "  Tools:     ⚠️  pi install {ext} failed to run: {e}"
                            ),
                        }
                    }
                }
            }
        } else {
            // Claude install: baked into the image at build time, so refresh it
            // to the latest release on spawn (non-fatal). Runs as the `node` user
            // since that is who owns the ~/.local/bin/claude installation.
            let claude_cfg = config::load().unwrap_or_default().claude;
            if claude_cfg.update_on_spawn {
                let status = std::process::Command::new("podman")
                    .args([
                        "exec",
                        "--user",
                        "node",
                        &info.id,
                        "/bin/bash",
                        "-lc",
                        "claude update",
                    ])
                    .status();
                match status {
                    Ok(s) if s.success() => {
                        println!("  Tools:     claude updated to latest")
                    }
                    Ok(_) => eprintln!(
                        "  Tools:     ⚠️  claude update exited non-zero (using image version)"
                    ),
                    Err(e) => eprintln!("  Tools:     ⚠️  claude update failed to run: {e}"),
                }
            }
        }
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

        // Run .nibble/setup.sh if present, otherwise warn the user.
        let setup_script = repo.join(".nibble").join("setup.sh");
        let container_cwd = container_working_dir(&repo);
        if setup_script.exists() {
            println!("  Setup:     running .nibble/setup.sh …");
            let setup_path = format!("{}/.nibble/setup.sh", container_cwd);
            let status = std::process::Command::new("podman")
                .args(["exec", "--user", "node", &info.id, "/bin/bash", &setup_path])
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

        // Detect project toolchains and write AGENTS.md + CLAUDE.md into the container.
        let toolchains = detect_toolchains(&repo);
        let agents_md = build_sandbox_agents_md(&repo_name, &toolchains, factory_enabled);
        let container_cwd = container_working_dir(&repo);
        match inject_sandbox_claude_md(&info.id, &container_cwd, &agents_md) {
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
    }

    let title = task_desc.unwrap_or_else(|| format!("[{}:sandbox]", repo_name));

    let agent_type = if hermes {
        AgentType::Hermes
    } else if pi {
        AgentType::Pi
    } else {
        AgentType::ClaudeCode
    };
    let mut task = Task::new(task_id.clone(), agent_type, title, None, None);
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
    task.container_name = Some(info.name.clone());
    task.repo_path = Some(abs_repo_path.clone());
    task.sandbox_config = Some(config);
    task.context = Some(TaskContext {
        url: None,
        project_path: Some(abs_repo_path.clone()),
        session_id: Some(resolved_session_id),
        claude_session_id: None,
        extra: HashMap::new(),
    });

    // Detect if this repo is a git worktree (has a `.git` file rather than a directory).
    // If so, record the worktree path so `kill --worktree` knows what to clean up.
    let worktree_marker = repo.join(".git");
    let is_worktree = worktree_marker.is_file();
    if is_worktree {
        task.worktree_path = Some(abs_repo_path.clone());
    }

    db.insert_task(&task)?;

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
        let agent_name = if hermes {
            "Hermes"
        } else if pi {
            "Pi"
        } else {
            "Claude"
        };
        let agent_flag = if hermes {
            " --hermes"
        } else if pi {
            " --pi"
        } else {
            ""
        };
        println!("Attach to the {} session:", agent_name);
        println!(
            "  nibble sandbox attach {}{}          (by repo path)",
            abs_repo_path, agent_flag
        );
        println!("  nibble sandbox attach {}   (by task ID)", short_id);
        println!(
            "  nibble sandbox attach {}{}   (by task ID)",
            short_id, agent_flag
        );
        println!(
            "  nibble sandbox attach {}{} --fresh  (start a new conversation)",
            short_id, agent_flag
        );
        println!("  (The container keeps running after you exit — re-attach any time)");
        println!();
        println!("After a system reboot, restart stopped containers with:");
        println!("  nibble sandbox resume --all");

        // For Pi sandboxes spawned without attach, run a background discovery so
        // the session is linked for `nibble session list --sandbox` even before the
        // first attach.
        if pi {
            let cid = info.id.clone();
            let tid = task_id.clone();
            let db_path = db::default_db_path();
            let pi_slug = pi_session_dir_name(&container_working_dir(&repo));
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(5));
                if let Some(host_path) = discover_pi_session_in_container(&cid, &pi_slug) {
                    let home = dirs::home_dir().unwrap_or_default();
                    let cp = if host_path.starts_with(&home) {
                        std::path::PathBuf::from("/home/node")
                            .join(host_path.strip_prefix(&home).unwrap_or(&host_path))
                    } else {
                        host_path
                    };
                    if let Ok(db) = Database::open(&db_path) {
                        if let Ok(Some(mut task)) = db.get_task_by_id(&tid) {
                            if let Some(ref mut ctx) = task.context {
                                ctx.extra.insert(
                                    "pi_session_path".to_string(),
                                    serde_json::Value::String(cp.to_string_lossy().to_string()),
                                );
                            }
                            let _ = db.update_task(&task);
                        }
                    }
                }
            });
        }
    } else {
        let agent_label = if hermes {
            "Hermes"
        } else if pi {
            "Pi"
        } else {
            "Claude"
        };
        println!(
            "Attaching to {} session (exit to detach — container keeps running)…",
            agent_label
        );
        println!(
            "  Re-attach later: nibble sandbox attach {}  (or by path: {})",
            short_id, abs_repo_path
        );
        println!();
        cmd_sandbox_attach(db, task_id.clone(), fresh, false, hermes, pi, None)?;
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
fn cmd_sandbox_list(db: &Database) -> Result<()> {
    let tasks = db.list_sandbox_tasks()?;

    if tasks.is_empty() {
        println!("No sandbox containers found.");
        println!("Start one with:  nibble sandbox spawn <repo_path>");
        return Ok(());
    }

    let sandbox = PodmanSandbox::new();

    println!("{:<20} {:<18} {:<12} REPO", "TASK ID", "STARTED", "STATUS");
    println!("{}", "─".repeat(82));

    let mut any_gone = false;
    for task in &tasks {
        let container_name = task
            .container_name
            .as_deref()
            .unwrap_or_else(|| task.container_id.as_deref().unwrap_or(""));
        let health = sandbox.health_check(container_name);

        let status = match health {
            SandboxHealth::Healthy => "healthy",
            SandboxHealth::Degraded => "degraded",
            SandboxHealth::Stopped => "stopped",
            SandboxHealth::Dead => {
                // Container is gone — prune task state silently
                any_gone = true;
                let mut t = task.clone();
                t.set_exited(None);
                let _ = db.update_task(&t);
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

        let short_id = &task.task_id[..task.task_id.len().min(8)];
        let repo_path = task.repo_path.as_deref().unwrap_or("?");

        // For worktree sandboxes, derive and display the branch name from the path suffix.
        let display_path = if task.worktree_path.is_some() {
            let branch_hint = std::path::Path::new(repo_path)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|name| name.find("--").map(|i| &name[i + 2..]))
                .unwrap_or("");
            if branch_hint.is_empty() {
                format!("{} [worktree]", repo_path)
            } else {
                format!("{} [branch: {}]", repo_path, branch_hint)
            }
        } else {
            repo_path.to_string()
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

// Resolve a user-supplied sandbox identifier to a full task_id.
//
// Accepts either:
// - A task ID or prefix (UUID hex string)
// - A repo path (starts with `.`, `/`, `~`, contains a path separator, or exists as a directory)
//
// For repo paths the canonical absolute path is looked up in the tasks table
// returning the most recently spawned sandbox for that repo.

/// Resolve a user-supplied path to its canonical absolute form.
///
/// Uses the same expansion and canonicalization rules as `resolve_sandbox_id`
/// but does not require a sandbox to exist.  Used by `nibble session list --sandbox`.
fn resolve_sandbox_repo_path(input: &str) -> Result<String> {
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
    Ok(canonical.to_string_lossy().to_string())
}

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
            .get_task_by_repo_path(&path_str)
            .with_context(|| format!("DB error looking up repo path: {}", path_str))?;

        if let Some(task) = result {
            return Ok(task.task_id);
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

    // Prefix match against sandbox tasks.
    let tasks = db.list_sandbox_tasks()?;
    let matches: Vec<_> = tasks
        .iter()
        .filter(|t| t.task_id.starts_with(input))
        .collect();

    match matches.len() {
        0 => anyhow::bail!("No sandbox found with ID or path: {}", input),
        1 => Ok(matches[0].task_id.clone()),
        _ => {
            let ids: Vec<&str> = matches.iter().map(|t| t.task_id.as_str()).collect();
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum SelectedAgent {
    Claude,
    Pi,
    Hermes,
}

impl std::fmt::Display for SelectedAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectedAgent::Claude => write!(f, "Claude Code"),
            SelectedAgent::Pi => write!(f, "pi"),
            SelectedAgent::Hermes => write!(f, "hermes"),
        }
    }
}

/// Determine which agent to attach to, and validate flag combinations.
///
/// Rules:
/// - Plain sandboxes (non-hermes) default to Claude; hermes sandboxes default to hermes.
/// - Any agent can be explicitly selected with `--pi` or `--hermes`.
/// - `--hermes` is only valid on hermes sandboxes (different image/binary).
/// - Claude-only option (`--btw`) is rejected for other agents.
/// - At most one agent flag can be specified per invocation.
fn resolve_attach_agent(
    stored_type: &AgentType,
    hermes: bool,
    pi: bool,
    btw: bool,
) -> Result<SelectedAgent> {
    let explicit_count = [hermes, pi].iter().filter(|&&f| f).count();
    if explicit_count > 1 {
        let mut flags = Vec::new();
        if hermes {
            flags.push("--hermes");
        }
        if pi {
            flags.push("--pi");
        }
        anyhow::bail!("{} are mutually exclusive", flags.join(" and "));
    }

    let is_hermes_sandbox = matches!(stored_type, AgentType::Hermes);

    let agent = if hermes {
        if !is_hermes_sandbox {
            anyhow::bail!(
                "--hermes can only be used with hermes sandboxes (this is a plain sandbox)"
            );
        }
        SelectedAgent::Hermes
    } else if pi {
        if is_hermes_sandbox {
            anyhow::bail!("--pi is not supported on hermes sandboxes (pi is not installed)");
        }
        SelectedAgent::Pi
    } else if is_hermes_sandbox {
        SelectedAgent::Hermes
    } else {
        SelectedAgent::Claude
    };

    if btw && agent != SelectedAgent::Claude && agent != SelectedAgent::Pi {
        anyhow::bail!(
            "--btw is not supported with {} (side sessions require Claude Code or pi)",
            agent
        );
    }

    Ok(agent)
}

/// Attach to a running sandbox with an interactive bash shell.
/// Useful for debugging agent installations, inspecting the container, etc.
fn cmd_sandbox_bash(db: &Database, task_id: String) -> Result<()> {
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

    eprintln!(
        "Attaching to sandbox {} ({}) [bash]…",
        task.title, container_id
    );
    eprintln!("(Type 'exit' or press Ctrl+D to detach — the container keeps running)");

    let repo_path = task
        .context
        .as_ref()
        .and_then(|c| c.project_path.as_deref())
        .unwrap_or("");
    let cwd = container_working_dir(std::path::Path::new(repo_path));

    let err =
        std::os::unix::process::CommandExt::exec(std::process::Command::new("podman").args([
            "exec",
            "-it",
            "-e",
            "TERM=xterm-256color",
            "-e",
            "PATH=/home/node/.local/bin:/usr/local/bin:/usr/bin:/bin",
            "-w",
            &cwd,
            &container_id,
            "/bin/bash",
        ]));
    anyhow::bail!("Failed to exec podman: {}", err)
}

/// Attach to the Claude session inside a running sandbox.
///
/// Resumes the session UUID stored on the task (derived deterministically from the repo
/// path at spawn, then updated by the Stop hook after each session). The UUID is stable
/// for the lifetime of the sandbox — `--fresh` backs up the conversation history for that
/// UUID (rename to .jsonl.bak, never deleted) rather than minting a new one, so Telegram
/// injection always uses the same ID.
fn cmd_sandbox_attach(
    db: &Database,
    task_id: String,
    fresh: bool,
    btw: bool,
    hermes: bool,
    pi: bool,
    session_id: Option<String>,
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

    // Derive the container working directory from the repo path.
    let container_dir = task
        .context
        .as_ref()
        .and_then(|c| c.project_path.as_deref())
        .map(std::path::Path::new)
        .map(container_working_dir)
        .unwrap_or_else(|| "/workspace".to_string());

    // If --session was passed, look it up early so we can auto-detect the agent.
    let override_session = session_id.as_ref().and_then(|sid| {
        crate::session::find_session_by_id(sid).or_else(|| {
            eprintln!("  Warning:   session {sid} not found, using stored session");
            None
        })
    });

    // Auto-derive agent flags from the requested session when the user didn't
    // explicitly specify an agent. If they did specify one but it conflicts,
    // warn and let the session's agent win.
    let (hermes, pi, agent_override) = if let Some(ref sesh) = override_session {
        let (h, pi_flag, overridden) =
            crate::session::derive_agent_flags_from_session(hermes, pi, sesh);
        if [hermes, pi].iter().filter(|&&f| f).count() > 0 {
            eprintln!(
                "  Session:   {} session {} — overriding explicit agent flag",
                sesh.agent,
                &sesh.session_id[..sesh.session_id.len().min(8)]
            );
        } else {
            eprintln!(
                "  Session:   auto-detected {} agent for session {}",
                sesh.agent,
                &sesh.session_id[..sesh.session_id.len().min(8)]
            );
        }
        (h, pi_flag, overridden)
    } else {
        (hermes, pi, false)
    };

    let agent = resolve_attach_agent(&task.agent_type, hermes, pi, btw)?;

    // Resolve the per-agent session IDs stored for this task.
    // Each agent writes its own field so they never clobber each other.
    // Legacy rows (pre-split) may only have the generic `session_id`; fall back to
    // that when the typed field is absent so existing sandboxes keep working.
    let claude_session_id: Option<&str> = if let Some(ref sesh) = override_session {
        if sesh.agent == "claude" {
            Some(&sesh.session_id)
        } else if agent_override {
            // Agent was auto-derived from the session; don't warn about mismatches
            None
        } else {
            eprintln!(
                "  Warning:   requested session is for {}, not claude",
                sesh.agent
            );
            None
        }
    } else {
        task.context.as_ref().and_then(|c| {
            let raw = c.claude_session_id.as_deref().or({
                // Legacy fallback: use generic session_id for non-hermes tasks.
                match task.agent_type {
                    AgentType::Hermes => None,
                    _ => c.session_id.as_deref(),
                }
            });
            // Guard: a ses_... value is a legacy session ID that was mistakenly stored
            // in claude_session_id by an older version of the session-id handler. Treat it
            // as absent so Claude never tries `--resume ses_...`.
            raw.filter(|id| !id.starts_with("ses_"))
        })
    };

    let shell_cmd = match agent {
        SelectedAgent::Hermes => {
            if fresh {
                "hermes".to_string()
            } else {
                "hermes --continue".to_string()
            }
        }
        SelectedAgent::Pi => {
            let pi_install =
                "command -v pi >/dev/null 2>&1 || sudo npm install -g @earendil-works/pi-coding-agent";
            if fresh || btw {
                // Plain `pi` always starts a brand-new session. Previous sessions are
                // never deleted — they stay on disk and remain resumable via --session.
                format!("{pi_install}; pi")
            } else if let Some(ref sesh) = override_session {
                // --session override: look up the session path from the session ID
                if sesh.agent == "pi" {
                    let host_path = sesh.path.clone();
                    let home = dirs::home_dir().unwrap_or_default();
                    let cp = if host_path.starts_with(&home) {
                        std::path::PathBuf::from("/home/node")
                            .join(host_path.strip_prefix(&home).unwrap_or(&host_path))
                    } else {
                        host_path
                    };
                    eprintln!(
                        "  Session:   resuming pi session {} (override)",
                        cp.display()
                    );
                    format!("{pi_install}; pi --session '{cp}'", cp = cp.display())
                } else {
                    eprintln!(
                        "  Warning:   requested session is for {}, not pi",
                        sesh.agent
                    );
                    format!("{pi_install}; pi")
                }
            } else {
                // Prefer a stored session path from a previous attach, otherwise discover
                // the most recent session inside this specific container.
                // Using container-specific discovery avoids grabbing a session from a
                // different sandbox when multiple Pi sandboxes are active.
                let stored_path = task
                    .context
                    .as_ref()
                    .and_then(|c| c.extra.get("pi_session_path").and_then(|v| v.as_str()));

                let maybe_host_path = if let Some(path_str) = stored_path {
                    let cp = std::path::PathBuf::from(path_str);
                    // The stored path may be a container path (/home/node/...) or a host path.
                    // Always convert to host path before calling .exists().
                    let home = dirs::home_dir().unwrap_or_default();
                    let host_path = if cp.starts_with("/home/node/") {
                        home.join(cp.strip_prefix("/home/node/").unwrap_or(cp.as_path()))
                    } else {
                        cp.clone()
                    };
                    if host_path.exists() {
                        Some((host_path, cp))
                    } else {
                        eprintln!("  Session:   stored pi session gone, re-discovering...");
                        None
                    }
                } else {
                    None
                };

                let pi_cmd = if let Some((_host_path, cp)) = maybe_host_path {
                    // Stored path exists — use it, but still refresh the DB record
                    // so the link survives if the user switched sessions.
                    eprintln!("  Session:   resuming stored pi session {}", cp.display());
                    let mut updated_task = task.clone();
                    if let Some(ref mut ctx) = updated_task.context {
                        ctx.extra.insert(
                            "pi_session_path".to_string(),
                            serde_json::Value::String(cp.to_string_lossy().to_string()),
                        );
                    }
                    let _ = db.update_task(&updated_task);
                    format!("{pi_install}; pi --session '{cp}'", cp = cp.display())
                } else {
                    // Try container-specific discovery first, then fall back to global scan
                    let cid = task.container_id.as_deref().unwrap_or("");
                    let pi_slug = pi_session_dir_name(&container_dir);
                    let discovered = if !cid.is_empty() {
                        discover_pi_session_in_container(cid, &pi_slug)
                    } else {
                        None
                    };
                    let host_path = if let Some(host_path) = discovered {
                        Some(host_path)
                    } else {
                        find_pi_session_for_cwd(&container_dir)
                    };
                    if let Some(host_path) = host_path {
                        let home = dirs::home_dir().unwrap_or_default();
                        let cp = if host_path.starts_with(&home) {
                            std::path::PathBuf::from("/home/node")
                                .join(host_path.strip_prefix(&home).unwrap_or(&host_path))
                        } else {
                            host_path
                        };
                        eprintln!("  Session:   resuming pi session {}", cp.display());
                        let mut updated_task = task.clone();
                        if let Some(ref mut ctx) = updated_task.context {
                            ctx.extra.insert(
                                "pi_session_path".to_string(),
                                serde_json::Value::String(cp.to_string_lossy().to_string()),
                            );
                        }
                        let _ = db.update_task(&updated_task);
                        format!("{pi_install}; pi --session '{cp}'", cp = cp.display())
                    } else {
                        eprintln!(
                            "  Session:   no pi session found for {container_dir}, starting fresh"
                        );
                        format!("{pi_install}; pi")
                    }
                };
                pi_cmd
            }
        }
        SelectedAgent::Claude => {
            let claude = "/home/node/.local/bin/claude --dangerously-skip-permissions";

            if fresh && !btw {
                if let Some(sid) = claude_session_id {
                    backup_session_file(sid);
                    eprintln!(
                        "  Session:   {} — previous history backed up, starting fresh",
                        &sid[..8.min(sid.len())]
                    );
                } else {
                    eprintln!("  Session:   no stored session, starting fresh");
                }
            }

            if btw {
                let side_id = uuid::Uuid::new_v4();
                format!("cd {container_dir} && {claude} --session-id {side_id}")
            } else if let Some(sid) = claude_session_id {
                format!(
                    "cd {container_dir} && {claude} --resume {sid} 2>&1 || {claude} --session-id {sid}"
                )
            } else {
                format!("cd {container_dir} && {claude}")
            }
        }
    };

    // Build podman exec args.
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

    // --btw sessions are side sessions: omit AGENT_TASK_ID so hooks and epilogues
    // inside the container no-op and don't overwrite the main task's stored session_id.
    if !btw {
        podman_args.extend(["-e".into(), format!("AGENT_TASK_ID={}", task_id)]);
    }

    podman_args.extend([
        "-w".into(),
        container_dir.clone(),
        container_id.clone(),
        "/bin/bash".into(),
        "-c".into(),
        shell_cmd,
    ]);

    match agent {
        SelectedAgent::Hermes => {
            eprintln!(
                "Attaching to sandbox {} ({}) [hermes]…",
                task.title, container_id
            );
            eprintln!("(Exit hermes or press Ctrl+C to detach — the container keeps running)");
        }
        SelectedAgent::Pi => {
            if btw {
                eprintln!(
                    "Attaching to sandbox {} ({}) [pi, btw — side session]…",
                    task.title, container_id
                );
                eprintln!("(Independent session — main history untouched. Exit to close.)");
            } else {
                eprintln!(
                    "Attaching to sandbox {} ({}) [pi]…",
                    task.title, container_id
                );
                eprintln!("(Exit pi or press Ctrl+C to detach — the container keeps running)");
            }
        }
        SelectedAgent::Claude => {
            if btw {
                eprintln!(
                    "Attaching to sandbox {} ({}) [btw — side session]…",
                    task.title, container_id
                );
                eprintln!("(Independent session — main history untouched. Exit to close.)");
            } else {
                eprintln!("Attaching to sandbox {} ({})…", task.title, container_id);
                eprintln!("(Exit Claude or press Ctrl+C to detach — the container keeps running)");
            }
        }
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

    println!("Killed sandbox {} (task {})", container_id, task_id);

    Ok(())
}

/// Kill all running sandbox containers.
fn cmd_sandbox_kill_all(db: &Database) -> Result<()> {
    let sandbox = PodmanSandbox::new();
    let tasks = db.list_sandbox_tasks()?;

    if tasks.is_empty() {
        println!("No sandbox agents to kill.");
        return Ok(());
    }

    let mut killed = 0;
    for task in &tasks {
        let container_name = task
            .container_name
            .as_deref()
            .unwrap_or_else(|| task.container_id.as_deref().unwrap_or(""));
        match sandbox.kill(container_name) {
            Ok(()) => {
                let mut t = task.clone();
                t.set_exited(None);
                let _ = db.update_task(&t);
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
    let tasks = db.list_sandbox_tasks()?;

    if tasks.is_empty() {
        println!("No sandbox agents to resume.");
        return Ok(());
    }

    let mut resumed = 0;
    let mut stale = 0;

    for task in &tasks {
        let container_name = task
            .container_name
            .as_deref()
            .unwrap_or_else(|| task.container_id.as_deref().unwrap_or(""));
        let repo_path = task.repo_path.as_deref().unwrap_or("?");
        match sandbox.health_check(container_name) {
            SandboxHealth::Healthy => {
                let mut t = task.clone();
                if t.status != TaskStatus::Running {
                    t.set_running();
                    let _ = db.update_task(&t);
                }
                println!("  Healthy: {} ({})", container_name, task.task_id);
                resumed += 1;
            }
            SandboxHealth::Degraded => {
                let mut t = task.clone();
                t.set_exited(None);
                let _ = db.update_task(&t);
                println!(
                    "  Degraded: {} (container up, Claude session gone, repo: {})",
                    container_name, repo_path
                );
                stale += 1;
            }
            SandboxHealth::Stopped => match sandbox.start(container_name) {
                Ok(()) => match sandbox.health_check(container_name) {
                    SandboxHealth::Healthy => {
                        let mut t = task.clone();
                        if t.status != TaskStatus::Running {
                            t.set_running();
                            let _ = db.update_task(&t);
                        }
                        println!("  Restarted: {} ({})", container_name, repo_path);
                        resumed += 1;
                    }
                    _ => {
                        let mut t = task.clone();
                        t.set_exited(None);
                        let _ = db.update_task(&t);
                        println!(
                            "  Cleaned: {} (start failed health check, repo: {})",
                            container_name, repo_path
                        );
                        stale += 1;
                    }
                },
                Err(e) => {
                    eprintln!("  Failed to restart {container_name}: {e:#}");
                    let mut t = task.clone();
                    t.set_exited(None);
                    let _ = db.update_task(&t);
                    println!(
                        "  Cleaned: {} (start error, repo: {})",
                        container_name, repo_path
                    );
                    stale += 1;
                }
            },
            SandboxHealth::Dead => {
                let mut t = task.clone();
                t.set_exited(None);
                let _ = db.update_task(&t);
                println!("  Cleaned: {} (gone, repo: {})", container_name, repo_path);
                stale += 1;
            }
        }
    }

    println!("\n{} running, {} stale cleaned up.", resumed, stale);
    Ok(())
}

// ── Stale task pruning ────────────────────────────────────────────────────────

/// Health-check sandbox containers and clean up DB state for any that have disappeared.
///
/// Returns the number of containers pruned.
pub(crate) fn prune_stale_tasks(db: &Database) -> Result<usize> {
    let mut pruned = 0;

    // Sandbox tasks: health-check each container.
    //    - Dead      → container crashed; mark task exited.
    //    - Degraded  → container running but exec fails; mark task exited.
    //    - Stopped   → try silent restart; if fails, mark task exited.
    //    - Healthy   → all good, leave it alone.
    let tasks = db.list_sandbox_tasks()?;
    if !tasks.is_empty() {
        let sandbox = PodmanSandbox::new();
        for task in &tasks {
            let container_name = task
                .container_name
                .as_deref()
                .unwrap_or_else(|| task.container_id.as_deref().unwrap_or(""));
            if container_name.is_empty() {
                continue;
            }
            match sandbox.health_check(container_name) {
                SandboxHealth::Healthy => {}
                SandboxHealth::Stopped => {
                    eprintln!(
                        "[prune] Sandbox {} stopped → attempting restart",
                        container_name
                    );
                    let restarted = match sandbox.start(container_name) {
                        Ok(()) => {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            sandbox.health_check(container_name) == SandboxHealth::Healthy
                        }
                        Err(_) => false,
                    };
                    if restarted {
                        eprintln!("[prune] Sandbox {} restarted successfully", container_name);
                        if task.status != TaskStatus::Running {
                            let mut t = task.clone();
                            t.set_running();
                            let _ = db.update_task(&t);
                        }
                    } else {
                        // Only transition Running → Exited.
                        if task.status == TaskStatus::Running {
                            let mut t = task.clone();
                            t.set_exited(None);
                            let _ = db.update_task(&t);
                            pruned += 1;
                        }
                        eprintln!(
                            "[prune] Sandbox {} could not be restarted — will retry next cycle",
                            container_name
                        );
                    }
                }
                SandboxHealth::Dead => {
                    // Container is gone for good. Remove any leftover container and
                    // delete the task record immediately — no resume is possible once
                    // the container no longer exists, so there is nothing to retain.
                    let _ = sandbox.kill(container_name);
                    match db.delete_task_by_id(&task.task_id) {
                        Ok(true) => {
                            eprintln!(
                                "[prune] Sandbox {} dead → deleted task {}",
                                container_name,
                                &task.task_id[..8.min(task.task_id.len())]
                            );
                            pruned += 1;
                        }
                        Ok(false) => {}
                        Err(e) => eprintln!(
                            "[prune] Failed to delete dead task {}: {e:#}",
                            &task.task_id[..8.min(task.task_id.len())]
                        ),
                    }
                }
                SandboxHealth::Degraded => {
                    // Only transition Running → Exited.
                    if task.status == TaskStatus::Running {
                        let mut t = task.clone();
                        t.set_exited(None);
                        let _ = db.update_task(&t);
                        eprintln!(
                            "[prune] Sandbox {} degraded → exited task {}",
                            container_name,
                            &task.task_id[..8.min(task.task_id.len())]
                        );
                        pruned += 1;
                    }
                }
            }
        }
    }

    // Lazily GC exited sandbox tasks older than 7 days.
    match db.delete_exited_sandbox_tasks_older_than(7) {
        Ok(0) => {}
        Ok(n) => eprintln!(
            "[prune] GC: deleted {} exited sandbox task(s) older than 7 days",
            n
        ),
        Err(e) => eprintln!("[prune] GC warning: {e:#}"),
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

/// Build the AGENTS.md content written to `/<repo-name>/AGENTS.md` inside the
/// container.  This is the **primary** agent instruction file — Claude reads
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
/// **AGENTS.md** (`/<repo-name>/AGENTS.md`):
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
/// **CLAUDE.md** (`/<repo-name>/.claude/CLAUDE.md`):
/// - Contains `@../AGENTS.md` as the first line (safe-prepend, never overwrites).
/// - If the file already exists with `@../AGENTS.md` at line 1, it is left untouched.
/// - If `@../AGENTS.md` is missing from line 1, it is prepended — user content below is preserved.
fn inject_sandbox_claude_md(
    container_id: &str,
    container_dir: &str,
    agents_content: &str,
) -> Result<()> {
    let escaped_agents = agents_content.replace('\'', "'\\''");

    let script = format!(
        r#"set -e
mkdir -p {dir}/.claude

# ── Update AGENTS.md using sentinel block (never overwrites repo content) ──────
AGENTS_FILE={dir}/AGENTS.md
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
TARGET={dir}/.claude/CLAUDE.md
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
        dir = container_dir,
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
fn agent_display(agent_type: &AgentType) -> (&'static str, String) {
    match agent_type {
        AgentType::ClaudeCode => ("🤖", "Claude Code".to_string()),
        AgentType::Hermes => ("🧠", "Hermes".to_string()),
        AgentType::Pi => ("🥧", "Pi".to_string()),
        AgentType::Unknown(s) => ("🔧", s.clone()),
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
            AgentType::from_str(agent_type).unwrap(),
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
        let mut task = make_task("pi", "pi (interactive)");
        task.context = Some(TaskContext {
            url: None,
            project_path: Some("/home/user/projects/my-app".to_string()),
            session_id: None,
            claude_session_id: None,
            extra: HashMap::new(),
        });
        let loc = format_location(&task);
        assert!(loc.contains("my-app"));
    }

    #[test]
    fn test_agent_display_known_types() {
        assert_eq!(
            agent_display(&AgentType::ClaudeCode),
            ("🤖", "Claude Code".to_string())
        );
        assert_eq!(
            agent_display(&AgentType::Hermes),
            ("🧠", "Hermes".to_string())
        );
    }

    #[test]
    fn test_agent_display_unknown_type() {
        let (emoji, label) = agent_display(&AgentType::Unknown("my_custom_agent".to_string()));
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
        let mut task = make_task("pi", "[proj:main]");
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
            out.contains("factory-pipeline"),
            "the factory skill should be listed"
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
            !out.contains("factory-pipeline"),
            "factory skills must not appear when disabled"
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

    // ── Per-agent session ID routing tests ────────────────────────────────────
    // These tests verify AC-5, AC-6, AC-7, INV-1, INV-2, INV-3, INV-4 from
    // .nibble/factory/blueprints/2026-04-15_per-agent-session-id.md

    fn make_db_with_task(agent_type: &str) -> (tempfile::TempDir, crate::db::Database, Task) {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let db = crate::db::Database::open(tmp.path().join("test.db")).unwrap();
        let task = Task::new(
            format!("task-{}", agent_type),
            AgentType::from_str(agent_type).unwrap(),
            "Test task".to_string(),
            None,
            None,
        );
        db.insert_task(&task).unwrap();
        (tmp, db, task)
    }

    /// AC-5 / INV-3: report session-id for a claude_code task writes claude_session_id,
    #[test]
    fn test_ac5_report_session_id_routes_to_claude_field_replacement() {
        use crate::models::TaskContext;
        use std::collections::HashMap;

        let (_tmp, db, mut task) = make_db_with_task("claude_code");
        let sid = "550e8400-e29b-41d4-a716-446655440000".to_string();
        let ctx = task.context.get_or_insert_with(|| TaskContext {
            url: None,
            project_path: None,
            session_id: None,
            claude_session_id: None,
            extra: HashMap::new(),
        });
        ctx.claude_session_id = Some(sid.clone());
        db.update_task(&task).unwrap();

        let reloaded = db.get_task_by_id(&task.task_id).unwrap().unwrap();
        let ctx = reloaded.context.as_ref().unwrap();
        assert_eq!(
            ctx.claude_session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }
}
