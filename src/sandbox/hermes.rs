//! Hermes command handlers: spawn/attach/mount/unmount/list/kill for the
//! singleton Hermes sandbox.
//!
//! Bin-only module (`#[path]` from main.rs) — it references bin-private items
//! and must not be re-exported through `crate::sandbox` (which is shared with
//! the lib target).

use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::config;
use crate::db::Database;
use crate::format::expand_tilde;
use crate::models::{AgentType, SandboxConfig, SandboxType, Task, TaskContext};
use crate::sandbox::podman::PodmanSandbox;

/// Sentinel repo_path for the hermes sandbox.
const HERMES_REPO_PATH: &str = "__hermes__";

/// Find the running hermes sandbox task, if any.
fn find_hermes_sandbox(db: &Database) -> Result<Option<Task>> {
    let sandbox = PodmanSandbox::new();
    for task in db.list_sandbox_tasks()? {
        if task.agent_type == AgentType::Hermes {
            if let Some(cid) = &task.container_id {
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
        let mut resolved = Vec::new();
        for repo in legacy_repos {
            let path = expand_tilde(repo)?;
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

pub(crate) fn cmd_hermes_init(db: &Database) -> Result<()> {
    let _task_id = cmd_hermes_spawn_internal(db)?;
    println!("Hermes sandbox ready.");
    println!("  Attach:    nibble hermes attach");
    println!("  Add repo:  nibble hermes mount /path/to/repo");
    println!("  List:      nibble hermes list");
    println!();
    Ok(())
}

pub(crate) fn cmd_hermes_attach(db: &Database, fresh: bool) -> Result<()> {
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

    // Per-window task: this attach gets its own sidebar row; AGENT_TASK_ID
    // points at the window, and the exec'd podman pid retires the row via
    // liveness reconcile when the window closes.
    let pane_id = std::env::var("ZELLIJ_PANE_ID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok());
    let children = crate::commands::child_window_tasks(db, &task.task_id)?;
    let window_task = crate::commands::build_window_task(
        &task,
        crate::commands::SelectedAgent::Hermes,
        pane_id,
        std::process::id() as i32,
        children.len() + 1,
        false,
    );
    db.insert_task(&window_task)?;
    crate::status::rename_current_pane(&window_task.title);
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
        format!("AGENT_TASK_ID={}", window_task.task_id),
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

pub(crate) fn cmd_hermes_mount(
    db: &Database,
    repo_path: &str,
    name: Option<&str>,
    skip_confirm: bool,
) -> Result<()> {
    // Validate path exists
    let expanded = expand_tilde(repo_path)?;
    let abs = expanded
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
        restart(db)?;
    }

    Ok(())
}

pub(crate) fn cmd_hermes_unmount(db: &Database, repo_path: &str, skip_confirm: bool) -> Result<()> {
    let expanded = expand_tilde(repo_path)?;
    let abs = expanded
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
        restart(db)?;
    }

    Ok(())
}

/// Kill and respawn the hermes sandbox so it picks up updated repo mounts.
fn restart(db: &Database) -> Result<()> {
    eprintln!();
    cmd_hermes_kill_internal(db)?;
    cmd_hermes_spawn_internal(db)?;
    println!("Hermes sandbox restarted with updated mounts.");
    Ok(())
}

pub(crate) fn cmd_hermes_list(db: &Database) -> Result<()> {
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

pub(crate) fn cmd_hermes_kill(db: &Database) -> Result<()> {
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
