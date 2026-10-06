//! Sandbox command handlers: spawn/attach/list/bash/kill/resume for
//! Podman-backed repo sandboxes, plus the resolvers they share.
//!
//! Bin-only module (`#[path]` from main.rs) — it references bin-private items
//! and must not be re-exported through `crate::sandbox` (which is shared with
//! the lib target).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::config;
use crate::db::{self, Database};
use crate::format::expand_tilde;
use crate::models::{AgentType, SandboxConfig, SandboxType, Task, TaskContext, TaskStatus};
use crate::sandbox::podman::PodmanSandbox;
use crate::sandbox::{container_working_dir, pi_session_dir_name, SandboxHealth};
use crate::session::pi::{
    discover_pi_session_in_container, ensure_agent_skills_symlink, list_pi_sessions_for_cwd,
    mount_agent_config_dir, pick_pi_session, pi_resume_command, pi_session_path_candidates,
    PiSessionPick,
};

/// Options for [`cmd_sandbox_spawn`], one field per `nibble sandbox spawn` flag.
pub(crate) struct SpawnOptions {
    pub repo_path: String,
    pub task_desc: Option<String>,
    pub image: String,
    pub fresh: bool,
    pub session_id: Option<String>,
    pub no_attach: bool,
    pub hermes: bool,
    pub pi: bool,
    pub omp: bool,
}

/// Derive a deterministic UUID v5 for a repo, keyed on its canonical host path.
///
/// Every sandbox on the same repo gets the same UUID, so re-attach always
/// resumes the right conversation. `--resume <uuid>` is a direct
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

/// Spawn a sandboxed Claude Code agent.  Returns the new task_id on success.
pub(crate) fn cmd_sandbox_spawn(db: &Database, opts: SpawnOptions) -> Result<String> {
    let SpawnOptions {
        repo_path,
        task_desc,
        image,
        fresh,
        session_id,
        no_attach,
        hermes,
        pi,
        omp,
    } = opts;

    let repo = PathBuf::from(&repo_path);
    if !repo.exists() {
        anyhow::bail!("Repository path does not exist: {}", repo_path);
    }

    let sandbox = PodmanSandbox::new();
    if !sandbox.is_available()? {
        anyhow::bail!("Podman is not installed. Run ./install.sh to set it up.");
    }

    // Resolve the pi-family implementation for this spawn. --pi and --omp are
    // explicit and independent; None = no pi-family agent was requested.
    let pi_impl: Option<PiImplementation> = if omp {
        Some(PiImplementation::Omp)
    } else if pi {
        Some(PiImplementation::Pi)
    } else {
        None
    };
    let pi_family = pi_impl.is_some();
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
                        omp,
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
                            cmd_sandbox_attach(db, tid.clone(), fresh, false, hermes, pi, omp, None)?;
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
        // Additional providers understood by omp (oh-my-pi); harmless for
        // other agents since they are only set when present on the host.
        "GEMINI_API_KEY",
        "OPENROUTER_API_KEY",
        "XAI_API_KEY",
        "MISTRAL_API_KEY",
        "GROQ_API_KEY",
        "DEEPSEEK_API_KEY",
    ] {
        if let Ok(val) = std::env::var(key) {
            env_vars.insert(key.to_string(), val);
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

    // Mount pi-family config dirs (always — skills/extensions/sessions are
    // needed regardless of the agent type; sandboxes created without --pi/--omp
    // would otherwise miss the mounts and pi/omp sessions would have no config).
    {
        let home_dir = dirs::home_dir().context("Failed to get home directory")?;
        for dir_name in [".pi", ".omp"] {
            mount_agent_config_dir(&home_dir, dir_name, &mut extra_volumes)?;
            ensure_agent_skills_symlink(&home_dir, dir_name);
        }
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

    let repo_name = repo
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| repo_path.clone());

    // Post-spawn setup (skip for Hermes)
    // Pi-family setup: install the agent, run setup.sh, inject AGENTS.md
    if !hermes {
        install_spawn_agent(&info.id, pi_impl);
        restore_claude_credentials(&info.id);
        run_repo_setup_script(&repo, &info.id)?;
        inject_sandbox_context(&repo, &repo_name, &info.id);
    }

    let title = task_desc.unwrap_or_else(|| format!("[{}:sandbox]", repo_name));

    let agent_type = if hermes {
        AgentType::Hermes
    } else if pi_family {
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
    //   All sandboxes on the same repo share a session, so every attach reaches
    //   the right conversation context without starting a new session.
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
    // The sandbox row mirrors the container: idle from spawn, retired by
    // prune/kill. Live per-session status belongs to window tasks.
    task.status = TaskStatus::Completed;
    task.completed_at = Some(chrono::Utc::now());
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
        } else {
            match pi_impl {
                Some(PiImplementation::Omp) => "omp",
                Some(PiImplementation::Pi) => "Pi",
                None => "Claude",
            }
        };
        let agent_flag = if hermes {
            " --hermes"
        } else if omp {
            " --omp"
        } else if pi_family {
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

        // For pi-family sandboxes spawned without attach, run a background discovery so
        // the session is linked for `nibble session list --sandbox` even before the
        // first attach.
        if let Some(implementation) = pi_impl {
            let dir_name = match implementation {
                PiImplementation::Omp => ".omp",
                PiImplementation::Pi => ".pi",
            };
            let cid = info.id.clone();
            let tid = task_id.clone();
            let db_path = db::default_db_path();
            let pi_slug = pi_session_dir_name(&container_working_dir(&repo));
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(5));
                if let Some(host_path) = discover_pi_session_in_container(&cid, &pi_slug, dir_name) {
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
        } else {
            match pi_impl {
                Some(PiImplementation::Omp) => "omp",
                Some(PiImplementation::Pi) => "Pi",
                None => "Claude",
            }
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
        cmd_sandbox_attach(db, task_id.clone(), fresh, false, hermes, pi, omp, None)?;
    }

    Ok(task_id)
}

/// Install the selected agent inside a freshly spawned container (non-fatal).
///
/// omp is installed via its standalone installer (a self-contained binary — the
/// omp npm package requires bun, which the sandbox image does not ship); upstream
/// pi via npm. With no pi-family agent selected, refreshes the baked-in Claude
/// install instead.
fn install_spawn_agent(container_id: &str, pi_impl: Option<PiImplementation>) {
    if let Some(implementation) = pi_impl {
        // omp stores auth in ~/.omp/agent/agent.db on the host, which is
        // mounted rw into every sandbox. When it's absent, each sandbox
        // starts unauthenticated and omp asks for /login again — the exact
        //per-sandbox setup pain the mount exists to avoid. Warn loudly:
        // one login (host or inside any sandbox) persists for all future
        // sandboxes via the mount.
        if implementation == PiImplementation::Omp {
            let omp_agent_db = dirs::home_dir()
                .map(|h| h.join(".omp").join("agent").join("agent.db"))
                .unwrap_or_default();
            if !omp_agent_db.exists() {
                eprintln!("  Auth:      ⚠️  no omp login on host (~/.omp/agent/agent.db missing)");
                eprintln!("             Run `omp` → /login once (on the host or inside this");
                eprintln!("             sandbox) — ~/.omp is mounted rw, so it persists for");
                eprintln!("             every future sandbox. Coming from pi? Run");
                eprintln!("             scripts/migrate-pi-to-omp.sh on the host first.");
            }
        }
        let pi_cfg = config::load().unwrap_or_default().pi;
        if pi_cfg.install_on_spawn {
            let core_ok = match implementation {
                PiImplementation::Omp => {
                    let status = std::process::Command::new("podman")
                        .args([
                            "exec",
                            "--user",
                            "node",
                            container_id,
                            "/bin/bash",
                            "-lc",
                            "command -v omp >/dev/null 2>&1 || curl -fsSL https://omp.sh/install | sh",
                        ])
                        .status();
                    match &status {
                        Ok(s) if s.success() => {
                            println!("  Tools:     omp (oh-my-pi) installed")
                        }
                        Ok(_) => eprintln!(
                            "  Tools:     ⚠️  omp installer exited non-zero (install manually inside)"
                        ),
                        Err(e) => {
                            eprintln!("  Tools:     ⚠️  omp install failed to run: {e}")
                        }
                    }
                    matches!(status, Ok(s) if s.success())
                }
                PiImplementation::Pi => {
                    let status = std::process::Command::new("podman")
                        .args([
                            "exec",
                            container_id,
                            "sudo",
                            "npm",
                            "install",
                            "-g",
                            "@earendil-works/pi-coding-agent",
                        ])
                        .status();
                    match &status {
                        Ok(s) if s.success() => {
                            println!("  Tools:     @earendil-works/pi-coding-agent installed")
                        }
                        Ok(_) => eprintln!(
                            "  Tools:     ⚠️  pi npm install exited non-zero (install manually inside)"
                        ),
                        Err(e) => {
                            eprintln!("  Tools:     ⚠️  pi npm install failed to run: {e}")
                        }
                    }
                    matches!(status, Ok(s) if s.success())
                }
            };

            // Install configured pi extensions (e.g. pi-dynamic-workflows) so
            // they are available in every pi-family sandbox like any other
            // extension. Non-fatal — runs as the `node` user that owns the
            // mounted config dir.
            if core_ok {
                let ext_cmd = match implementation {
                    PiImplementation::Omp => "omp",
                    PiImplementation::Pi => "pi",
                };
                for ext in &pi_cfg.extensions {
                    let ext_status = std::process::Command::new("podman")
                        .args([
                            "exec",
                            "--user",
                            "node",
                            container_id,
                            "/bin/bash",
                            "-lc",
                            &format!("{ext_cmd} install {ext}"),
                        ])
                        .status();
                    match ext_status {
                        Ok(s) if s.success() => {
                            println!("  Tools:     {ext_cmd} extension installed: {ext}")
                        }
                        Ok(_) => {
                            if implementation == PiImplementation::Omp {
                                // `omp install npm:…` shells out to bun, which the
                                // sandbox image does not ship. After a host-side
                                // pi→omp migration the extension is already in the
                                // mounted ~/.omp/agent/extensions, so this is benign.
                                eprintln!("  Tools:     ⚠️  omp install {ext} exited non-zero (needs bun in the sandbox; extensions already in ~/.omp/agent/extensions on the host are available via the mount)");
                            } else {
                                eprintln!("  Tools:     ⚠️  {ext_cmd} install {ext} exited non-zero");
                            }
                        }
                        Err(e) => eprintln!(
                            "  Tools:     ⚠️  {ext_cmd} install {ext} failed to run: {e}"
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
                    container_id,
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
}

/// Propagate the host Claude login into the container so the user doesn't
/// have to re-authenticate inside the sandbox.
fn restore_claude_credentials(container_id: &str) {
    let restore_result = std::process::Command::new("podman")
        .args([
            "exec", container_id,
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
}

/// Run .nibble/setup.sh if present, otherwise warn the user.
fn run_repo_setup_script(repo: &std::path::Path, container_id: &str) -> Result<()> {
    let setup_script = repo.join(".nibble").join("setup.sh");
    let container_cwd = container_working_dir(repo);
    if setup_script.exists() {
        println!("  Setup:     running .nibble/setup.sh …");
        let setup_path = format!("{}/.nibble/setup.sh", container_cwd);
        let status = std::process::Command::new("podman")
            .args(["exec", "--user", "node", container_id, "/bin/bash", &setup_path])
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
    Ok(())
}

/// Detect project toolchains and write AGENTS.md + CLAUDE.md into the container.
fn inject_sandbox_context(repo: &std::path::Path, repo_name: &str, container_id: &str) {
    let toolchains = crate::sandbox::context::detect_toolchains(repo);
    let agents_md = crate::sandbox::context::build_sandbox_agents_md(repo_name, &toolchains);
    let container_cwd = container_working_dir(repo);
    match crate::sandbox::context::inject_sandbox_claude_md(container_id, &container_cwd, &agents_md)
    {
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


/// List all tracked sandboxes, auto-cleaning gone entries.
pub(crate) fn cmd_sandbox_list(db: &Database) -> Result<()> {
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
pub(crate) fn resolve_sandbox_repo_path(input: &str) -> Result<String> {
    let expanded = expand_tilde(input)?;
    let canonical = std::fs::canonicalize(&expanded)
        .with_context(|| format!("Cannot resolve path: {}", input))?;
    Ok(canonical.to_string_lossy().to_string())
}

pub(crate) fn resolve_sandbox_id(db: &Database, input: &str) -> Result<String> {
    // Heuristic: treat as a path if it looks like one or actually exists on disk.
    let looks_like_path = input.starts_with('.')
        || input.starts_with('/')
        || input.starts_with('~')
        || input.contains('/')
        || std::path::Path::new(input).exists();

    if looks_like_path {
        let expanded = expand_tilde(input)?;

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

/// Attach to a running sandbox with an interactive bash shell.
/// Useful for debugging agent installations, inspecting the container, etc.
pub(crate) fn cmd_sandbox_bash(db: &Database, task_id: String) -> Result<()> {
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
        crate::sandbox::ContainerStatus::Running => {}
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

/// Kill a sandbox container and mark its task as exited.
pub(crate) fn cmd_sandbox_kill(db: &Database, task_id: String) -> Result<()> {
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
pub(crate) fn cmd_sandbox_kill_all(db: &Database) -> Result<()> {
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
pub(crate) fn cmd_sandbox_resume(db: &Database, all: bool) -> Result<()> {
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
    if resumed > 0 {
        println!("Re-attach to a sandbox with:  nibble sandbox attach <repo-or-task-id>");
        println!("If attach resumes the wrong conversation, pick the right one with:");
        println!("  nibble session list         (shows which session each task is linked to)");
        println!("  nibble sandbox attach <repo> --session <id>");
        println!("See docs/session-recovery.md for the full runbook.");
    }
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SelectedAgent {
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

/// Which pi-family agent a spawn installs and an attach runs. Selected
/// explicitly per invocation: `--pi` (upstream pi) or `--omp` (oh-my-pi).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PiImplementation {
    Omp,
    Pi,
}

/// Determine which agent to attach to, and validate flag combinations.
///
/// Rules:
/// - Plain sandboxes (non-hermes) default to Claude; hermes sandboxes default to hermes.
/// - `--pi` selects upstream pi; `--omp` selects omp (oh-my-pi). The two flags
///   are explicit and independent — no config indirection.
/// - `--hermes` is only valid on hermes sandboxes (different image/binary).
/// - Claude-only option (`--btw`) is rejected for other agents.
/// - At most one agent flag can be specified per invocation.
fn resolve_attach_agent(
    stored_type: &AgentType,
    hermes: bool,
    pi: bool,
    omp: bool,
    btw: bool,
) -> Result<SelectedAgent> {
    let explicit_count = [hermes, pi, omp].iter().filter(|&&f| f).count();
    if explicit_count > 1 {
        let mut flags = Vec::new();
        if hermes {
            flags.push("--hermes");
        }
        if pi {
            flags.push("--pi");
        }
        if omp {
            flags.push("--omp");
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
    } else if omp {
        if is_hermes_sandbox {
            anyhow::bail!("--omp is not supported on hermes sandboxes (omp is not installed)");
        }
        SelectedAgent::Omp
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

    if btw
        && agent != SelectedAgent::Claude
        && agent != SelectedAgent::Pi
        && agent != SelectedAgent::Omp
    {
        anyhow::bail!(
            "--btw is not supported with {} (side sessions require Claude Code or a pi-family agent)",
            agent
        );
    }

    Ok(agent)
}


/// Whether this task is a `--btw` side-session window.
pub(crate) fn is_btw_window(task: &Task) -> bool {
    task.context
        .as_ref()
        .and_then(|c| c.extra.get("btw"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Child window tasks of a sandbox task (any status), newest first.
pub(crate) fn child_window_tasks(db: &Database, parent_id: &str) -> Result<Vec<Task>> {
    let mut tasks = db.list_tasks()?;
    tasks.retain(|t| {
        t.context
            .as_ref()
            .and_then(|c| c.extra.get("parent_task_id"))
            .and_then(|v| v.as_str())
            == Some(parent_id)
    });
    Ok(tasks) // list_tasks is already updated_at DESC (newest first)
}

/// The most recent main (non-btw) child window's value, so a new attach
/// resumes the conversation the last main window was in (INV-4: btw
/// sessions never feed this).
pub(crate) fn latest_main_child_value(
    db: &Database,
    parent_id: &str,
    get: impl Fn(&TaskContext) -> Option<String>,
) -> Result<Option<String>> {
    Ok(child_window_tasks(db, parent_id)?
        .into_iter()
        .filter(|t| !is_btw_window(t))
        .find_map(|t| t.context.as_ref().and_then(&get)))
}

/// Build the per-window task for one attach invocation. Every window —
/// including `--btw` side sessions — gets its own sidebar row; the pid is
/// this process, which `exec`s into podman and therefore lives exactly as
/// long as the window, letting the sidebar's liveness reconcile retire the
/// row when the window closes (INV-1, INV-2).
pub(crate) fn build_window_task(
    parent: &Task,
    agent: SelectedAgent,
    pane_id: Option<u32>,
    pid: i32,
    window_num: usize,
    btw: bool,
) -> Task {
    let agent_type = match agent {
        SelectedAgent::Claude => AgentType::ClaudeCode,
        SelectedAgent::Pi => AgentType::Pi,
        SelectedAgent::Omp => AgentType::Unknown("omp".to_string()),
        SelectedAgent::Hermes => AgentType::Hermes,
    };
    let mut extra = HashMap::new();
    extra.insert(
        "parent_task_id".to_string(),
        serde_json::Value::String(parent.task_id.clone()),
    );
    extra.insert("window".to_string(), serde_json::Value::Bool(true));
    if let Some(pane) = pane_id {
        extra.insert(
            "zellij_pane_id".to_string(),
            serde_json::Value::Number(pane.into()),
        );
    }
    if btw {
        extra.insert("btw".to_string(), serde_json::Value::Bool(true));
    }
    let mut task = Task::new(
        Uuid::new_v4().to_string(),
        agent_type,
        format!("{} · {} #{}", parent.title, agent, window_num),
        Some(pid),
        None,
    );
    task.repo_path = parent.repo_path.clone();
    task.context = Some(TaskContext {
        url: None,
        project_path: parent
            .context
            .as_ref()
            .and_then(|c| c.project_path.clone()),
        session_id: None,
        claude_session_id: None,
        extra,
    });
    task
}

/// Attach to the Claude session inside a running sandbox.
///
/// Resumes the session UUID stored on the task (derived deterministically from the repo
/// path at spawn, then updated by the Stop hook after each session). The UUID is stable
/// for the lifetime of the sandbox — `--fresh` backs up the conversation history for that
/// UUID (rename to .jsonl.bak, never deleted) rather than minting a new one, so
/// re-attaches always use the same ID.
pub(crate) fn cmd_sandbox_attach(
    db: &Database,
    task_id: String,
    fresh: bool,
    btw: bool,
    hermes: bool,
    pi: bool,
    omp: bool,
    session_id: Option<String>,
) -> Result<()> {
    let mut task = db
        .get_task_by_id(&task_id)?
        .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

    if task.sandbox_type != SandboxType::Podman {
        anyhow::bail!("Task {} is not a sandbox task", task_id);
    }

    // Remember which zellij pane this agent is attached in, so
    // `nibble status --watch` / `nibble goto` can jump back to it. The
    // pane is also stamped on the per-window task below.
    let pane_id = std::env::var("ZELLIJ_PANE_ID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok());
    if let Some(pane) = pane_id {
        if let Some(ctx) = task.context.as_mut() {
            ctx.extra.insert(
                "zellij_pane_id".to_string(),
                serde_json::Value::Number(pane.into()),
            );
            let _ = db.update_task(&task);
        }
    }


    let container_id = task
        .container_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Task {} has no container_id", task_id))?;

    let sandbox = PodmanSandbox::new();
    match sandbox.status(&container_id)? {
        crate::sandbox::ContainerStatus::Running => {}
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
    let (hermes, pi, omp, agent_override) = if let Some(ref sesh) = override_session {
        let derived = crate::session::derive_agent_flags_from_session(hermes, pi, omp, sesh);
        if [hermes, pi, omp].iter().filter(|&&f| f).count() > 0 {
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
        (derived.hermes, derived.pi, derived.omp, derived.overridden)
    } else {
        (hermes, pi, omp, false)
    };

    let agent = resolve_attach_agent(&task.agent_type, hermes, pi, omp, btw)?;

    // Per-window task: every attach invocation gets its own sidebar row, so
    // concurrent windows on one sandbox no longer share (and flap) the
    // sandbox task's status. AGENT_TASK_ID below points at the window, not
    // the sandbox (INV-1).
    let children = child_window_tasks(db, &task.task_id)?;
    let window_task = build_window_task(
        &task,
        agent,
        pane_id,
        std::process::id() as i32,
        children.len() + 1,
        btw,
    );
    db.insert_task(&window_task)?;

    // Name the pane after the window so it's recognisable in the frame
    // border (no-op outside zellij).
    crate::status::rename_current_pane(&window_task.title);

    // Resolve the per-agent session IDs stored for this task.
    // Each agent writes its own field so they never clobber each other.
    // Legacy rows (pre-split) may only have the generic `session_id`; fall back to
    // that when the typed field is absent so existing sandboxes keep working.
    // Hooks inside the window write session IDs to the *window* task now, so
    // when the parent has nothing stored, resume from the most recent main
    // child window (INV-4: btw windows are excluded).
    let claude_session_id: Option<String> = if let Some(sesh) = &override_session {
        if sesh.agent == "claude" {
            Some(sesh.session_id.clone())
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
        let stored = task.context.as_ref().and_then(|c| {
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
            raw.filter(|id| !id.starts_with("ses_")).map(str::to_string)
        });
        match stored {
            Some(id) => Some(id),
            None => {
                latest_main_child_value(db, &task.task_id, |c| c.claude_session_id.clone())?
                    .filter(|id| !id.starts_with("ses_"))
            }
        }
    };

    let shell_cmd = match agent {
        SelectedAgent::Hermes => {
            if fresh {
                "hermes".to_string()
            } else {
                "hermes --continue".to_string()
            }
        }
        SelectedAgent::Pi | SelectedAgent::Omp => {
            let is_omp = agent == SelectedAgent::Omp;
            let bin = if is_omp { "omp" } else { "pi" };
            let install = if is_omp {
                "command -v omp >/dev/null 2>&1 || curl -fsSL https://omp.sh/install | sh"
            } else {
                "command -v pi >/dev/null 2>&1 || sudo npm install -g @earendil-works/pi-coding-agent"
            };
            // omp is a fork of pi with an identical session format. `omp --resume`
            // accepts a session ID or file path, so ~/.pi sessions (also mounted)
            // stay resumable for continuity after migrating from pi to omp.
            let resume_flag = if is_omp { "--resume" } else { "--session" };
            let roots: &[&str] = if is_omp { &[".omp", ".pi"] } else { &[".pi"] };
            if fresh || btw {
                // Plain invocation always starts a brand-new session. Previous sessions are
                // never deleted — they stay on disk and remain resumable via --session.
                format!("{install}; {bin}")
            } else if let Some(ref sesh) = override_session {
                // --session override: look up the session path from the session ID
                if sesh.agent == "pi" || sesh.agent == "omp" {
                    let host_path = sesh.path.clone();
                    let home = dirs::home_dir().unwrap_or_default();
                    let cp = if host_path.starts_with(&home) {
                        std::path::PathBuf::from("/home/node")
                            .join(host_path.strip_prefix(&home).unwrap_or(&host_path))
                    } else {
                        host_path
                    };
                    eprintln!(
                        "  Session:   resuming {} session {} (override)",
                        sesh.agent,
                        cp.display()
                    );
                    pi_resume_command(install, bin, resume_flag, &cp)
                } else {
                    eprintln!(
                        "  Warning:   requested session is for {}, not {bin}",
                        sesh.agent
                    );
                    format!("{install}; {bin}")
                }
            } else {
                // Prefer a stored session path from a previous attach, otherwise discover
                // the most recent session inside this specific container.
                // Using container-specific discovery avoids grabbing a session from a
                // different sandbox when multiple pi-family sandboxes are active.
                let stored_path: Option<String> = task
                    .context
                    .as_ref()
                    .and_then(|c| {
                        c.extra
                            .get("pi_session_path")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .or_else(|| {
                        // Windows report their session path to their own
                        // task; fall back to the latest main window's path
                        // when the parent never stored one.
                        latest_main_child_value(db, &task.task_id, |c| {
                            c.extra
                                .get("pi_session_path")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                        })
                        .ok()
                        .flatten()
                    });

                // Candidate container paths for the stored session, ordered by the
                // implementation's config roots (omp prefers its ~/.omp twin of a
                // stored ~/.pi session, falling back to the original path).
                let candidates: Vec<std::path::PathBuf> = stored_path
                    .as_deref()
                    .map(|p| pi_session_path_candidates(p, roots))
                    .unwrap_or_default();

                let home = dirs::home_dir().unwrap_or_default();
                let maybe_cp = candidates.into_iter().find(|cp| {
                    let host_path = if cp.starts_with("/home/node/") {
                        home.join(cp.strip_prefix("/home/node/").unwrap_or(cp.as_path()))
                    } else {
                        cp.clone()
                    };
                    host_path.exists()
                });

                if stored_path.is_some() && maybe_cp.is_none() {
                    eprintln!("  Session:   stored {bin} session gone, re-discovering...");
                }

                let pi_cmd = if let Some(cp) = maybe_cp {
                    // Stored path exists — use it, but still refresh the DB record
                    // so the link survives if the user switched sessions.
                    eprintln!("  Session:   resuming stored {bin} session {}", cp.display());
                    let mut updated_task = task.clone();
                    if let Some(ref mut ctx) = updated_task.context {
                        ctx.extra.insert(
                            "pi_session_path".to_string(),
                            serde_json::Value::String(cp.to_string_lossy().to_string()),
                        );
                    }
                    let _ = db.update_task(&updated_task);
                    pi_resume_command(install, bin, resume_flag, &cp)
                } else {
                    // No stored mapping (or it vanished): enumerate every session
                    // for this repo across the implementation's config roots
                    // (omp: ~/.omp first, then ~/.pi for pre-migration sessions).
                    // With more than one candidate and a terminal attached, let
                    // the user pick — after a reboot/crash the newest file is
                    // often a --btw side session, not the conversation they want
                    // back. Non-interactive callers keep newest-wins.
                    let mut candidates: Vec<(std::path::PathBuf, std::time::SystemTime)> =
                        Vec::new();
                    for root in roots {
                        candidates.extend(list_pi_sessions_for_cwd(&container_dir, root));
                    }
                    candidates.sort_by(|a, b| b.1.cmp(&a.1));
                    use std::io::IsTerminal;
                    let mut user_chose_fresh = false;
                    let host_path = if candidates.len() > 1 && std::io::stdin().is_terminal() {
                        match pick_pi_session(&candidates, bin) {
                            PiSessionPick::Session(p) => Some(p),
                            PiSessionPick::Fresh => {
                                user_chose_fresh = true;
                                None
                            }
                        }
                    } else {
                        candidates.first().map(|(p, _)| p.clone())
                    };
                    if let Some(host_path) = host_path {
                        let cp = if host_path.starts_with(&home) {
                            std::path::PathBuf::from("/home/node")
                                .join(host_path.strip_prefix(&home).unwrap_or(&host_path))
                        } else {
                            host_path
                        };
                        eprintln!("  Session:   resuming {bin} session {}", cp.display());
                        let mut updated_task = task.clone();
                        if let Some(ref mut ctx) = updated_task.context {
                            ctx.extra.insert(
                                "pi_session_path".to_string(),
                                serde_json::Value::String(cp.to_string_lossy().to_string()),
                            );
                        }
                        let _ = db.update_task(&updated_task);
                        pi_resume_command(install, bin, resume_flag, &cp)
                    } else if user_chose_fresh {
                        eprintln!("  Session:   starting a fresh {bin} session");
                        format!("{install}; {bin}")
                    } else {
                        eprintln!(
                            "  Session:   no {bin} session found for {container_dir}, starting fresh"
                        );
                        format!("{install}; {bin}")
                    }
                };
                pi_cmd
            }
        }
        SelectedAgent::Claude => {
            let claude = "/home/node/.local/bin/claude --dangerously-skip-permissions";
            if fresh && !btw {
                if let Some(sid) = claude_session_id.as_deref() {
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
            } else if let Some(sid) = claude_session_id.as_deref() {
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

    // Every window — including --btw side sessions — reports as its own
    // task; nothing inside the container writes the sandbox row any more,
    // so side sessions can no longer clobber the main session mapping.
    podman_args.extend([
        "-e".into(),
        format!("AGENT_TASK_ID={}", window_task.task_id),
    ]);

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
        SelectedAgent::Pi | SelectedAgent::Omp => {
            let bin = if agent == SelectedAgent::Omp {
                "omp"
            } else {
                "pi"
            };
            if btw {
                eprintln!(
                    "Attaching to sandbox {} ({}) [{bin}, btw — side session]…",
                    task.title, container_id
                );
                eprintln!("(Independent session — main history untouched. Exit to close.)");
            } else {
                eprintln!(
                    "Attaching to sandbox {} ({}) [{bin}]…",
                    task.title, container_id
                );
                eprintln!(
                    "(Exit {bin} or press Ctrl+C to detach — the container keeps running)"
                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn parent() -> Task {
        let mut t = Task::new(
            "parent-1".to_string(),
            AgentType::Pi,
            "[demo:sandbox]".to_string(),
            None,
            None,
        );
        t.container_name = Some("nibble-x".to_string());
        t.context = Some(TaskContext {
            url: None,
            project_path: Some("/repos/demo".to_string()),
            session_id: None,
            claude_session_id: None,
            extra: HashMap::new(),
        });
        t
    }

    fn window(parent_id: &str, extra_pairs: &[(&str, serde_json::Value)], updated: i64) -> Task {
        let mut extra = HashMap::new();
        extra.insert(
            "parent_task_id".to_string(),
            serde_json::Value::String(parent_id.to_string()),
        );
        for (k, v) in extra_pairs {
            extra.insert(k.to_string(), v.clone());
        }
        let mut t = Task::new(
            format!("w-{updated}"),
            AgentType::Unknown("omp".to_string()),
            "w".to_string(),
            None,
            None,
        );
        t.context = Some(TaskContext {
            url: None,
            project_path: None,
            session_id: None,
            claude_session_id: None,
            extra,
        });
        t.updated_at = chrono::DateTime::from_timestamp(updated, 0).unwrap();
        t
    }

    #[test]
    fn build_window_task_maps_fields() {
        let p = parent();
        let w = build_window_task(&p, SelectedAgent::Omp, Some(7), 42, 2, false);
        assert_eq!(w.agent_type, AgentType::Unknown("omp".to_string()));
        assert_eq!(w.pid, Some(42));
        assert_eq!(w.container_name, None, "window rows keep host-pid reconcile");
        assert_eq!(w.repo_path, p.repo_path);
        let ctx = w.context.as_ref().unwrap();
        assert_eq!(
            ctx.extra.get("parent_task_id").unwrap(),
            &serde_json::Value::String("parent-1".to_string())
        );
        assert_eq!(ctx.extra.get("window"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(ctx.extra.get("btw"), None, "main windows are not btw");
        assert_eq!(
            ctx.extra.get("zellij_pane_id"),
            Some(&serde_json::Value::Number(7.into()))
        );
        assert_eq!(w.title, "[demo:sandbox] · omp #2");
        assert!(!w.task_id.is_empty() && w.task_id != "parent-1");
    }

    #[test]
    fn build_window_task_btw_marker() {
        let w = build_window_task(&parent(), SelectedAgent::Claude, None, 1, 1, true);
        assert_eq!(
            w.context.as_ref().unwrap().extra.get("btw"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn child_windows_lookup_and_btw_exclusion() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = Database::open(tmp.path().join("t.db")).unwrap();
        db.insert_task(&parent()).unwrap();
        // newest-first input order: newest main window has the session id
        let mut newest = window(
            "parent-1",
            &[("pi_session_path", serde_json::json!("/h/.omp/s2.jsonl"))],
            200,
        );
        newest.context.as_mut().unwrap().claude_session_id = Some("sid-new".to_string());
        let btw = window(
            "parent-1",
            &[
                ("btw", serde_json::Value::Bool(true)),
                ("pi_session_path", serde_json::json!("/h/.omp/btw.jsonl")),
            ],
            300,
        );
        db.insert_task(&newest).unwrap();
        db.insert_task(&btw).unwrap();
        db.insert_task(&window("other", &[], 400)).unwrap();

        let children = child_window_tasks(&db, "parent-1").unwrap();
        assert_eq!(children.len(), 2, "only this parent's windows");
        assert!(is_btw_window(&children[0]) || is_btw_window(&children[1]));

        let claude = latest_main_child_value(&db, "parent-1", |c| c.claude_session_id.clone())
            .unwrap()
            .expect("main window session id found");
        assert_eq!(claude, "sid-new", "btw never supplies resume state");
        let pi = latest_main_child_value(&db, "parent-1", |c| {
            c.extra.get("pi_session_path").and_then(|v| v.as_str()).map(String::from)
        })
        .unwrap()
        .expect("main window session path found");
        assert_eq!(pi, "/h/.omp/s2.jsonl", "btw session path excluded");
    }
}
