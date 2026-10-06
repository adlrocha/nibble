mod backup;
mod cli;
#[path = "sandbox/commands.rs"]
mod commands;
mod config;
mod db;
mod format;
#[path = "sandbox/hermes.rs"]
mod hermes;
mod memory;
mod models;
mod agent_input;
mod cron;
mod lm;
mod privacy_filter;
mod quota_watch;
mod sandbox;
mod session;
mod status;
mod usage;
mod web;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands, HermesAction, ReportAction, SandboxAction};
use db::Database;
use models::{AgentType, Task, TaskContext};
use sandbox::podman::PodmanSandbox;
use sandbox::worktree::{branch_slug, create_worktree, remove_worktree};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Ensure data directory exists
    db::ensure_data_dir()?;

    // Open database
    let db_path = db::default_db_path();
    let db = Database::open(&db_path).context("Failed to open database")?;

    match cli.command {
        Commands::Prune => {
            commands::prune_stale_tasks(&db)?;
        }
        Commands::Status {
            watch,
            json,
            all,
            clear,
            scratch,
        } => {
            if scratch {
                commands::cmd_sandbox_kill_all(&db)?;
                let n = db.delete_all_tasks()?;
                println!("Removed {n} task row(s). Sandboxes stopped.");
            } else if clear {
                let n = db.delete_exited_tasks()?;
                println!("Removed {n} exited task row(s).");
            } else {
                status::cmd_status(&db, watch, json, all)?;
            }
        }
        Commands::Sidebar {
            install,
            uninstall,
            focus,
            close,
        } => {
            if install || uninstall {
                status::cmd_sidebar(install, uninstall)?;
            } else if close {
                status::close_sidebar_pane()?;
            } else {
                // focus (or bare `nibble sidebar`): open + focus the pane
                status::open_sidebar_pane()?;
                status::focus_sidebar_pane()?;
            }
        }
        Commands::Goto { task } => {
            status::cmd_goto(&db, &task)?;
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
                zellij_session,
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
                if let Some(session) = zellij_session {
                    extra.insert(
                        "zellij_session_name".to_string(),
                        serde_json::Value::String(session),
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
                // Name the zellij pane after the task so agent panes are
                // recognisable in their frame border (no-op outside zellij).
                status::rename_current_pane(&task.title);
                println!("Task started: {}", task.task_id);
            }
            ReportAction::SessionId {
                task_id,
                session_id,
            } => {
                let mut task = db.ensure_task_or_create(&task_id)?;
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
                    | AgentType::Omp
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
            ReportAction::SessionPath { task_id, path } => {
                let mut task = db.ensure_task_or_create(&task_id)?;
                let ctx = task.context.get_or_insert_with(|| TaskContext {
                    url: None,
                    project_path: None,
                    session_id: None,
                    claude_session_id: None,
                    extra: HashMap::new(),
                });
                // Authoritative mapping written by the agent extension at session
                // start; the attach flow reads the same key.
                ctx.extra.insert(
                    "pi_session_path".to_string(),
                    serde_json::Value::String(path),
                );
                db.update_task(&task)?;
            }
            ReportAction::Status {
                task_id,
                state,
                message,
            } => {
                // Self-heal: an unknown task is auto-registered so the
                // agent stays visible even if its `report start` was lost
                // to a transient failure (INV-3). `exited` is the
                // exception — reporting the death of an unknown task must
                // not create a row. An invalid *state* remains a loud
                // error (hook callers use `|| true`).
                let mut task = match db.get_task_by_id(&task_id)? {
                    Some(t) => t,
                    None if state == "exited" => {
                        eprintln!(
                            "report status: unknown task {} (exited — ignored)",
                            &task_id[..8.min(task_id.len())]
                        );
                        return Ok(());
                    }
                    None => db.ensure_task_or_create(&task_id)?,
                };
                if status::apply_status_transition(&mut task, &state, message.as_deref()) {
                    db.update_task(&task)?;
                } else {
                    anyhow::bail!(
                        "Invalid status state: '{state}' \
                         (expected running|blocked|completed|exited)"
                    );
                }
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
                agent, repo, sandbox, today, yesterday, week, month, last, limit,
            } => session::cli::cmd_session_list(
                &db, agent, repo, sandbox, today, yesterday, week, month, last, limit,
            )?,
            cli::SessionAction::Read { id, raw } => session::cli::cmd_session_read(&id, raw)?,
        },
        Commands::Backup { output, sessions } => {
            let output_path = output.map(PathBuf::from);
            let path = backup::create_backup(output_path, sessions)?;
            println!("Backup created: {}", path.display());
        }
        Commands::Import { path } => {
            let zip_path = PathBuf::from(path);
            backup::import_backup(&zip_path)?;
        }

        Commands::Inject { task_id, message } => {
            let task = db
                .get_task_by_id(&task_id)?
                .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

            if task.agent_type == AgentType::Hermes {
                anyhow::bail!("Inject is not yet supported for Hermes sandboxes. Use `nibble sandbox attach` instead.");
            }
            if matches!(task.agent_type, AgentType::Pi | AgentType::Omp) {
                anyhow::bail!(
                    "Inject is not yet supported for {} sandboxes. Use `nibble sandbox attach` instead.",
                    task.agent_type
                );
            }
            agent_input::inject(&task, &message)?;
            println!("Message injected into task {}", task_id);
        }


        Commands::QuotaWatch { once } => {
            let cfg = config::load().unwrap_or_default();
            let db_path = db::default_db_path();
            if once {
                quota_watch::run_once(cfg.quota_watch, db_path)?;
            } else {
                quota_watch::run(cfg.quota_watch, db_path)?;
            }
        }

        Commands::Lm { action } => {
            let cfg = config::load().unwrap_or_default();
            match action {
                cli::LmAction::List => {
                    let models = lm::list_models(&cfg.lm)?;
                    lm::print_list(&models);
                }
                cli::LmAction::Use { model } => {
                    lm::use_model(&cfg.lm, &model)?;
                }
            }
        }


        Commands::Cron { action } => match action {
            cli::CronAction::Add {
                repo,
                schedule,
                prompt,
                file,
                label,
                expires,
            } => {
                cron::commands::cmd_cron_add(&db, repo, schedule, prompt, file, label, expires)?;
            }
            cli::CronAction::List { repo_path } => {
                cron::commands::cmd_cron_list(&db, repo_path)?;
            }
            cli::CronAction::Edit {
                id,
                schedule,
                prompt,
                label,
                enable,
                disable,
                expires,
            } => {
                let cron_id = cron::commands::resolve_cron_id(&db, &id)?;
                cron::commands::cmd_cron_edit(
                    &db, cron_id, schedule, prompt, label, enable, disable, expires,
                )?;
            }
            cli::CronAction::Stop { id } => {
                let cron_id = cron::commands::resolve_cron_id(&db, &id)?;
                cron::commands::cmd_cron_edit(&db, cron_id, None, None, None, false, true, None)?;
            }
            cli::CronAction::Start { id } => {
                let cron_id = cron::commands::resolve_cron_id(&db, &id)?;
                cron::commands::cmd_cron_edit(&db, cron_id, None, None, None, true, false, None)?;
            }
            cli::CronAction::Kill { id } => {
                let cron_id = cron::commands::resolve_cron_id(&db, &id)?;
                let deleted = db.delete_cron_job(cron_id)?;
                if deleted {
                    println!("Deleted cron job {}", id);
                } else {
                    println!("Cron job {} not found", id);
                }
            }
            cli::CronAction::Run { id } => {
                let cron_id = cron::commands::resolve_cron_id(&db, &id)?;
                cron::commands::cmd_cron_run(&db, cron_id)?;
            }
        },
        // ── Sandbox subcommands ────────────────────────────────────────────
        Commands::Sandbox { action } => match action {
            SandboxAction::Spawn {
                repo_path,
                task,
                image,
                session_id,
                agent,
            } => {
                let effective_repo_path = if let Some(branch_name) = &agent.branch {
                    let worktree = create_worktree(std::path::Path::new(&repo_path), branch_name)?;
                    worktree.to_string_lossy().to_string()
                } else {
                    repo_path
                };
                commands::cmd_sandbox_spawn(
                    &db,
                    commands::SpawnOptions {
                        repo_path: effective_repo_path,
                        task_desc: task,
                        image,
                        fresh: agent.fresh,
                        session_id,
                        no_attach: false,
                        hermes: agent.hermes,
                        pi: agent.pi,
                        omp: agent.omp,
                    },
                )?;
            }
            SandboxAction::List => {
                commands::cmd_sandbox_list(&db)?;
            }
            SandboxAction::Bash { container_or_path } => {
                let task_id = commands::resolve_sandbox_id(&db, &container_or_path)?;
                commands::cmd_sandbox_bash(&db, task_id)?;
            }
            SandboxAction::Attach {
                container_or_path,
                btw,
                session,
                agent,
            } => {
                // If --branch is given, resolve the worktree path (creating it if needed)
                // and use that as the effective target instead of the original repo.
                let effective_path = if let Some(branch_name) = &agent.branch {
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
                let running_task_id = match commands::resolve_sandbox_id(&db, &effective_path) {
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
                            commands::cmd_sandbox_attach(
                                &db,
                                task_id,
                                agent.fresh,
                                btw,
                                agent.hermes,
                                agent.pi,
                                agent.omp,
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
                        commands::cmd_sandbox_spawn(
                            &db,
                            commands::SpawnOptions {
                                repo_path: effective_path,
                                task_desc: None,
                                image: "nibble-sandbox:latest".to_string(),
                                fresh: agent.fresh,
                                session_id: None,
                                no_attach: true,
                                hermes: agent.hermes,
                                pi: agent.pi,
                                omp: agent.omp,
                            },
                        )?
                    }
                };

                commands::cmd_sandbox_attach(&db, task_id, agent.fresh, btw, agent.hermes, agent.pi, agent.omp, session.clone())?;
            }
            SandboxAction::Kill {
                container_or_path,
                all,
                worktree,
                force,
                branch,
            } => {
                if all {
                    commands::cmd_sandbox_kill_all(&db)?;
                } else {
                    let raw_input = container_or_path.ok_or_else(|| {
                        anyhow::anyhow!("Provide a repo path, container name, or --all")
                    })?;

                    // --branch <name> derives the worktree path from the repo + branch slug,
                    // exactly mirroring how `spawn --branch` and `attach --branch` create it.
                    let input = if let Some(branch_name) = &branch {
                        let branch_slug = branch_slug(branch_name);
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
                    match commands::resolve_sandbox_id(&db, &input) {
                        Ok(id) => {
                            // Check for worktree *before* killing (task record is deleted after).
                            let wt_path = if remove_worktree_flag {
                                db.get_task_by_id(&id)?.and_then(|t| t.worktree_path)
                            } else {
                                None
                            };
                            commands::cmd_sandbox_kill(&db, id)?;
                            if let Some(wt) = wt_path {
                                let wt_pb = std::path::PathBuf::from(&wt);
                                let _ = remove_worktree(&wt_pb, force)?;
                            } else if remove_worktree_flag {
                                eprintln!("Note: no worktree recorded for this sandbox.");
                            }
                        }
                        Err(_) if remove_worktree_flag => {
                            // No sandbox running, but user asked to remove the worktree directory.
                            let abs = std::fs::canonicalize(&input)
                                .unwrap_or_else(|_| std::path::PathBuf::from(&input));
                            let _ = remove_worktree(&abs, force)?;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            SandboxAction::Resume { all } => {
                commands::cmd_sandbox_resume(&db, all)?;
            }
            SandboxAction::Build { image, rebuild } => {
                let sandbox = PodmanSandbox::new();
                sandbox.ensure_image_with_opts(&image, rebuild)?;
                println!("Sandbox image ready.");
            }
        },
        Commands::Hermes { action } => match action {
            HermesAction::Init => {
                hermes::cmd_hermes_init(&db)?;
            }
            HermesAction::Attach { fresh } => {
                hermes::cmd_hermes_attach(&db, fresh)?;
            }
            HermesAction::Mount {
                repo_path,
                name,
                yes,
            } => {
                hermes::cmd_hermes_mount(&db, &repo_path, name.as_deref(), yes)?;
            }
            HermesAction::Unmount { repo_path, yes } => {
                hermes::cmd_hermes_unmount(&db, &repo_path, yes)?;
            }
            HermesAction::List => {
                hermes::cmd_hermes_list(&db)?;
            }
            HermesAction::Kill => {
                hermes::cmd_hermes_kill(&db)?;
            }
        },
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
        Commands::Web { host, port, token } => {
            let cfg = web::WebConfig {
                host,
                port,
                token,
                sessions_roots: usage::pi_log::sessions_roots(),
                db_path,
            };
            web::serve(cfg)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod session_recovery_tests {
    use super::*;
    use crate::models::TaskContext;
    use std::collections::HashMap;

    #[test]
    fn report_session_path_stores_pi_session_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::open(tmp.path().join("test.db")).unwrap();
        let mut task = Task::new(
            "task-pi".to_string(),
            AgentType::from_str("pi").unwrap(),
            "Test task".to_string(),
            None,
            None,
        );
        db.insert_task(&task).unwrap();
        let ctx = task.context.get_or_insert_with(|| TaskContext {
            url: None,
            project_path: None,
            session_id: None,
            claude_session_id: None,
            extra: HashMap::new(),
        });
        ctx.extra.insert(
            "pi_session_path".to_string(),
            serde_json::Value::String(
                "/home/node/.pi/agent/sessions/--nibble--/s.jsonl".to_string(),
            ),
        );
        db.update_task(&task).unwrap();

        let reloaded = db.get_task_by_id(&task.task_id).unwrap().unwrap();
        let stored = reloaded
            .context
            .as_ref()
            .and_then(|c| c.extra.get("pi_session_path"))
            .and_then(|v| v.as_str());
        assert_eq!(
            stored,
            Some("/home/node/.pi/agent/sessions/--nibble--/s.jsonl")
        );
    }
}
