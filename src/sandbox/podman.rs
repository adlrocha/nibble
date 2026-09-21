//! Podman sandbox implementation for containerized agent execution.
//!
//! Uses rootless Podman to run Claude Code inside an isolated container with:
//! - Container PID 1 = `sleep infinity` (keeps container alive)
//! - `attach` opens an interactive Claude session via `podman exec -it`
//! - Host network mode for easy port access
//! - Privileged mode so the agent can install system packages inside the container
//! - Volume mounts for the repo and shared dependency caches

use crate::models::SandboxConfig;
use crate::sandbox::{container_working_dir, ContainerInfo, ContainerStatus};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// Base prefix for container names
const CONTAINER_NAME_PREFIX: &str = "nibble";

/// Podman sandbox implementation
pub struct PodmanSandbox;

impl Default for PodmanSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl PodmanSandbox {
    pub fn new() -> Self {
        Self
    }

    /// Build the sandbox base image if it doesn't exist.
    ///
    /// Pass `force = true` to remove any existing image and rebuild from scratch.
    pub fn ensure_image_with_opts(&self, image_name: &str, force: bool) -> Result<()> {
        if force {
            let _ = Command::new("podman")
                .args(["rmi", "-f", image_name])
                .output();
        } else {
            let output = Command::new("podman")
                .args(["image", "exists", image_name])
                .output()
                .context("Failed to check if image exists")?;
            if output.status.success() {
                return Ok(());
            }
        }

        println!("Building sandbox image '{}'...", image_name);
        self.build_image(image_name)
    }

    fn ensure_image(&self, image_name: &str) -> Result<()> {
        self.ensure_image_with_opts(image_name, false)
    }

    fn build_image(&self, image_name: &str) -> Result<()> {
        let default_hermes_image = "nibble-hermes:latest";
        let dockerfile = if image_name == default_hermes_image {
            self.generate_hermes_dockerfile()
        } else {
            self.generate_dockerfile()
        };

        let temp_dir = tempfile::tempdir().context("Failed to create temp directory")?;
        let dockerfile_path = temp_dir.path().join("Dockerfile");
        std::fs::write(&dockerfile_path, dockerfile).context("Failed to write Dockerfile")?;

        let output = Command::new("podman")
            .args([
                "build",
                "--progress=plain",
                "-t",
                image_name,
                "-f",
                dockerfile_path.to_str().unwrap(),
                temp_dir.path().to_str().unwrap(),
            ])
            .output()
            .context("Failed to build sandbox image")?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = format!("{}{}", stdout, stderr);
            bail!("Failed to build image: {}", combined);
        }

        println!("Sandbox image '{}' built successfully", image_name);
        Ok(())
    }

    fn generate_dockerfile(&self) -> String {
        self.generate_standard_dockerfile()
    }

    /// Standard Dockerfile for Claude Code sandboxes.
    fn generate_standard_dockerfile(&self) -> String {
        r#"FROM node:22-slim

# Install system dependencies.
# node:22-slim already has a 'node' user (uid 1000) — we reuse it.
RUN apt-get update && apt-get install -y \
    git \
    curl \
    procps \
    sudo \
    jq \
    gnupg \
    && rm -rf /var/lib/apt/lists/*

# Install GitHub CLI (gh) from the official apt repository.
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" | tee /etc/apt/sources.list.d/github-cli.list > /dev/null \
    && apt-get update && apt-get install -y -qq gh \
    && rm -rf /var/lib/apt/lists/*

# Give the existing 'node' user passwordless sudo so it can install
# system packages inside the container.
RUN usermod -aG sudo node \
    && echo "node ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/node \
    && chmod 0440 /etc/sudoers.d/node

# Create workspace directory owned by node
RUN mkdir -p /workspace && chown node:node /workspace
WORKDIR /workspace

# Switch to node user — the official installer must run as the user who will use claude
USER node

# Install Claude Code via the official installer (installs to ~/.local/bin/claude)
RUN curl -fsSL https://claude.ai/install.sh | bash

# Add ~/.local/bin (claude) to PATH
ENV PATH=/home/node/.local/bin:/usr/local/bin:$PATH

CMD ["bash"]
"#
        .to_string()
    }

    /// Dockerfile for Hermes Agent sandboxes.
    /// INV-6: Separate image from standard nibble-sandbox to avoid bloating it with Python.
    fn generate_hermes_dockerfile(&self) -> String {
        r#"FROM node:22-slim

# Install system dependencies.
# node:22-slim already has a 'node' user (uid 1000) — we reuse it.
RUN apt-get update && apt-get install -y \
    git \
    curl \
    procps \
    sudo \
    jq \
    gnupg \
    python3 \
    python3-pip \
    python3-venv \
    ripgrep \
    ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# Install GitHub CLI (gh) from the official apt repository.
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" | tee /etc/apt/sources.list.d/github-cli.list > /dev/null \
    && apt-get update && apt-get install -y -qq gh \
    && rm -rf /var/lib/apt/lists/*

# Give the existing 'node' user passwordless sudo so it can install
# system packages inside the container.
RUN usermod -aG sudo node \
    && echo "node ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/node \
    && chmod 0440 /etc/sudoers.d/node

# Create workspace and repos directories owned by node
RUN mkdir -p /workspace /repos && chown node:node /workspace /repos
WORKDIR /workspace

# Switch to node user
USER node

# Install uv (fast Python package manager)
RUN curl -LsSf https://astral.sh/uv/install.sh | bash

# Add uv and local bin to PATH
ENV PATH=/home/node/.local/bin:/home/node/.cargo/bin:/usr/local/bin:$PATH

# Install Hermes Agent to /opt/hermes-agent (outside ~/.hermes/) so the host's
# ~/.hermes/ mount at runtime doesn't shadow the installation directory.
# The install script uses HERMES_HOME for config/data and INSTALL_DIR for the code.
# We set INSTALL_DIR to /opt/hermes-agent to keep code separate from config.
RUN curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh \
    | bash -s -- --skip-setup --dir /home/node/.hermes-agent

# Ensure hermes venv is on PATH
ENV PATH=/home/node/.local/bin:/home/node/.hermes-agent/venv/bin:/usr/local/bin:$PATH

CMD ["bash"]
"#
        .to_string()
    }

    /// Generate container name with timestamp for easy chronological sorting.
    /// Format: nibble-YYYYMMDD-HHMM-<short-id>
    fn container_name(&self, task_id: &str) -> String {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M");
        let short_id = &task_id[..task_id.len().min(8)];
        format!("{}-{}-{}", CONTAINER_NAME_PREFIX, ts, short_id)
    }

    /// Get host cache directories to bind-mount for dependency persistence.
    fn get_cache_volumes(&self) -> Result<Vec<(String, String)>> {
        let cache_base = super::get_cache_dir()?;

        let caches = vec![
            ("npm", "/home/node/.npm"),
            ("npm-global", "/home/node/.npm-global"),
            ("cargo-registry", "/home/node/.cargo/registry"),
            ("cargo-git", "/home/node/.cargo/git"),
            ("rustup", "/home/node/.rustup"),
        ];

        let mut volumes = Vec::new();
        for (name, container_path) in caches {
            let host_path = cache_base.join(name);
            std::fs::create_dir_all(&host_path)?;
            volumes.push((
                host_path.to_string_lossy().to_string(),
                container_path.to_string(),
            ));
        }

        Ok(volumes)
    }

    fn parse_container_status(&self, json_str: &str) -> Result<ContainerStatus> {
        let json: serde_json::Value =
            serde_json::from_str(json_str).context("Failed to parse container inspect output")?;

        let state = json
            .get(0)
            .and_then(|v| v.get("State"))
            .and_then(|v| v.get("Status"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        match state {
            "running" => Ok(ContainerStatus::Running),
            "exited" | "stopped" => Ok(ContainerStatus::Stopped),
            "paused" => Ok(ContainerStatus::Paused),
            _ => Ok(ContainerStatus::Unknown),
        }
    }

    fn parse_container_info(&self, json_str: &str) -> Result<ContainerInfo> {
        let json: serde_json::Value =
            serde_json::from_str(json_str).context("Failed to parse container inspect output")?;

        let container = json.get(0).context("Empty inspect output")?;

        let id = container
            .get("Id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let name = container
            .get("Name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let image = container
            .get("ImageName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let created = container
            .get("Created")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let created_at = chrono::DateTime::parse_from_rfc3339(created)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());

        let status = self.parse_container_status(json_str)?;

        let mut ports = Vec::new();
        if let Some(network_settings) = container.get("NetworkSettings") {
            if let Some(port_bindings) = network_settings.get("Ports") {
                if let Some(bindings) = port_bindings.as_object() {
                    for (port, _) in bindings {
                        ports.push(port.clone());
                    }
                }
            }
        }

        Ok(ContainerInfo {
            id,
            name,
            status,
            image,
            created_at,
            ports,
        })
    }
}

impl PodmanSandbox {
    pub fn is_available(&self) -> Result<bool> {
        match Command::new("podman").arg("--version").output() {
            Ok(output) => Ok(output.status.success()),
            Err(_) => Ok(false),
        }
    }

    pub fn spawn(
        &self,
        task_id: &str,
        repo_path: &Path,
        config: &SandboxConfig,
    ) -> Result<ContainerInfo> {
        self.ensure_image(&config.image)?;

        let container_name = self.container_name(task_id);
        let repo_abs = repo_path
            .canonicalize()
            .context("Failed to resolve repo path")?;

        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            container_name.clone(),
            "--hostname".to_string(),
            format!("agent-{}", &task_id[..8.min(task_id.len())]),
            "--network".to_string(),
            "host".to_string(),
            "-w".to_string(),
            container_working_dir(&repo_abs),
            // Map host uid/gid into the container so volume-mounted files
            // owned by the host user (e.g. ~/.claude) are accessible as node.
            "--userns=keep-id".to_string(),
        ];

        if config.privileged {
            args.push("--privileged".to_string());
        }

        // Restart policy:
        // - Default entrypoint (sleep infinity): --restart=always is safe because
        //   sleep only exits if killed (OOM, signal); restarting it is desirable
        //   after a host reboot.
        // - Custom entrypoint (e.g. hermes gateway): --restart=always creates a
        //   crash loop if PID 1 exits repeatedly (e.g. hermes not installed, OOM).
        //   Use --restart=on-failure:3 to limit restart attempts and avoid
        //   consuming host resources in a tight loop that triggers the OOM killer.
        if config.entrypoint.is_empty() {
            args.push("--restart=always".to_string());
        } else {
            args.push("--restart=on-failure:3".to_string());
        }

        if let Some(cpu) = &config.cpu_limit {
            args.push("--cpus".to_string());
            args.push(cpu.clone());
        }
        if let Some(memory) = &config.memory_limit {
            args.push("--memory".to_string());
            args.push(memory.clone());
        }

        // User-supplied env vars from config
        for (key, value) in &config.env_vars {
            args.push("-e".to_string());
            args.push(format!("{}={}", key, value));
        }

        // Standard env vars
        args.push("-e".to_string());
        args.push(format!("AGENT_TASK_ID={}", task_id));
        args.push("-e".to_string());
        args.push("AGENT_INBOX_VERSION=1".to_string());
        args.push("-e".to_string());
        args.push("HOME=/home/node".to_string());

        // Forward Anthropic API configuration from host if set
        if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            args.push("-e".to_string());
            args.push(format!("ANTHROPIC_API_KEY={}", api_key));
        }
        if let Ok(base_url) = std::env::var("ANTHROPIC_BASE_URL") {
            // Only forward host base URL if the sandbox config hasn't already
            // set one (e.g. via the privacy filter proxy).
            if !config.env_vars.contains_key("ANTHROPIC_BASE_URL") {
                args.push("-e".to_string());
                args.push(format!("ANTHROPIC_BASE_URL={}", base_url));
            }
        }

        // Forward GitHub token from host so gh CLI is authenticated inside
        // the container. Set GITHUB_TOKEN in your host environment, e.g.
        //   export GITHUB_TOKEN=$(gh auth token)
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            args.push("-e".to_string());
            args.push(format!("GITHUB_TOKEN={}", token));
        }

        let cwd = container_working_dir(&repo_abs);

        // Repo mount
        args.push("-v".to_string());
        args.push(format!("{}:{}:rw", repo_abs.display(), cwd));

        // Shared dependency caches
        for (host_path, container_path) in self.get_cache_volumes()? {
            args.push("-v".to_string());
            args.push(format!("{}:{}", host_path, container_path));
        }

        // Extra volumes from config
        for volume in &config.extra_volumes {
            args.push("-v".to_string());
            args.push(volume.clone());
        }

        // Mount host's ~/.claude so the container shares auth, config, and hooks
        let home_dir = dirs::home_dir().context("Failed to get home directory")?;

        // If the repo has a project-level .claude/settings.json, shadow it inside
        // the container with an empty file.  This prevents hooks defined there from
        // duplicating the global hooks already in ~/.claude/settings.json.
        let repo_claude_settings = repo_abs.join(".claude").join("settings.json");
        if repo_claude_settings.exists() {
            let empty_settings_path = home_dir
                .join(".nibble")
                .join("cache")
                .join("empty-claude-settings.json");
            if let Some(parent) = empty_settings_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if !empty_settings_path.exists() {
                std::fs::write(&empty_settings_path, b"{}").ok();
            }
            eprintln!("[sandbox] Shadowing repo .claude/settings.json to prevent hook duplication");
            args.push("-v".to_string());
            args.push(format!(
                "{}:{}/.claude/settings.json:ro",
                empty_settings_path.display(),
                cwd
            ));
        }

        let claude_dir = home_dir.join(".claude");
        if claude_dir.exists() {
            args.push("-v".to_string());
            args.push(format!("{}:/home/node/.claude:rw", claude_dir.display()));
            args.push("-e".to_string());
            args.push("CLAUDE_CONFIG_DIR=/home/node/.claude".to_string());
        }

        // Mount the nibble binary so hooks inside the container can call it.
        // Prefer the musl (statically linked) build so it works regardless of
        // the container's glibc version. Fall back to the host binary.
        let musl_bin = home_dir.join(".local/bin/nibble-musl");
        let host_bin = home_dir.join(".local/bin/nibble");
        let nibble_bin = if musl_bin.exists() {
            musl_bin
        } else {
            host_bin
        };
        if nibble_bin.exists() {
            args.push("-v".to_string());
            args.push(format!("{}:/usr/local/bin/nibble:ro", nibble_bin.display()));
        }

        // Mount nibble config so in-container hooks can read settings (memory, etc.).
        let nibble_dir = home_dir.join(".nibble");
        if nibble_dir.exists() {
            args.push("-v".to_string());
            args.push(format!("{}:/home/node/.nibble:rw", nibble_dir.display()));
        }

        // Mount host gitconfig read-only so git identity and settings carry over.
        let gitconfig = home_dir.join(".gitconfig");
        if gitconfig.exists() {
            args.push("-v".to_string());
            args.push(format!("{}:/home/node/.gitconfig:ro", gitconfig.display()));
        }

        args.push(config.image.clone());

        // Container PID 1: use custom entrypoint if provided, otherwise sleep infinity.
        // INV-1: For Hermes sandboxes, the gateway is PID 1 so the container stops if it crashes.
        if config.entrypoint.is_empty() {
            args.push("sleep".to_string());
            args.push("infinity".to_string());
        } else {
            args.extend(config.entrypoint.iter().cloned());
        }

        let output = Command::new("podman")
            .args(&args)
            .output()
            .context("Failed to spawn container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to spawn container: {}", stderr);
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

        let inspect_output = Command::new("podman")
            .args(["inspect", &container_name])
            .output()
            .context("Failed to inspect container")?;

        let info =
            self.parse_container_info(String::from_utf8_lossy(&inspect_output.stdout).as_ref())?;

        println!(
            "Container '{}' started (ID: {})",
            container_name,
            &container_id[..12.min(container_id.len())]
        );

        Ok(info)
    }

    pub fn start(&self, container_id: &str) -> Result<()> {
        let output = Command::new("podman")
            .args(["start", container_id])
            .output()
            .context("Failed to start container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to start container: {}", stderr);
        }

        Ok(())
    }

    pub fn kill(&self, container_id: &str) -> Result<()> {
        // Only send SIGKILL if the container is actually running
        let status = self.status(container_id)?;
        if status == ContainerStatus::Running || status == ContainerStatus::Paused {
            let output = Command::new("podman")
                .args(["kill", container_id])
                .output()
                .context("Failed to kill container")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.contains("is not running") && !stderr.contains("state exited") {
                    bail!("Failed to kill container: {}", stderr);
                }
            }
        }

        // Remove the container regardless of state
        let _ = Command::new("podman")
            .args(["rm", "-f", container_id])
            .output();

        Ok(())
    }

    pub fn status(&self, container_id: &str) -> Result<ContainerStatus> {
        let output = Command::new("podman")
            .args(["inspect", container_id])
            .output()
            .context("Failed to inspect container")?;

        if !output.status.success() {
            return Ok(ContainerStatus::Unknown);
        }

        self.parse_container_status(String::from_utf8_lossy(&output.stdout).as_ref())
    }

    pub fn list(&self) -> Result<Vec<ContainerInfo>> {
        let output = Command::new("podman")
            .args([
                "ps",
                "-a",
                "--filter",
                "name=nibble-",
                "--format",
                "{{.Names}}",
            ])
            .output()
            .context("Failed to list containers")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to list containers: {}", stderr);
        }

        let names = String::from_utf8_lossy(&output.stdout);
        let mut containers = Vec::new();

        for name in names.lines() {
            if name.is_empty() {
                continue;
            }
            let inspect_output = Command::new("podman").args(["inspect", name]).output()?;
            if inspect_output.status.success() {
                if let Ok(info) = self
                    .parse_container_info(String::from_utf8_lossy(&inspect_output.stdout).as_ref())
                {
                    containers.push(info);
                }
            }
        }

        Ok(containers)
    }

    pub fn exec(&self, container_id: &str, command: &[&str]) -> Result<String> {
        let mut args = vec!["exec".to_string(), container_id.to_string()];
        args.extend(command.iter().map(|s| s.to_string()));

        let output = Command::new("podman")
            .args(&args)
            .output()
            .context("Failed to execute command in container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Command failed: {}", stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Check whether the container is running and can execute processes.
    ///
    /// Returns `SandboxHealth::Healthy` if the container is running and
    /// `podman exec` succeeds, `SandboxHealth::Degraded` if the container
    /// appears running but exec fails (zombie/OOM/etc.),
    /// `SandboxHealth::Stopped` if the container exists but is stopped (reboot),
    /// or `SandboxHealth::Dead` if the container no longer exists.
    pub fn health_check(&self, container_id: &str) -> super::SandboxHealth {
        match self.status(container_id) {
            Ok(ContainerStatus::Running) => {}
            Ok(ContainerStatus::Stopped) => return super::SandboxHealth::Stopped,
            _ => return super::SandboxHealth::Dead,
        }

        // Try running a trivial command to confirm exec capability.
        let can_exec = self.exec(container_id, &["true"]).is_ok();

        if can_exec {
            super::SandboxHealth::Healthy
        } else {
            super::SandboxHealth::Degraded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_container_name_format() {
        let sandbox = PodmanSandbox::new();
        let name = sandbox.container_name("abcd1234-5678-90ef");
        // Format: nibble-YYYYMMDD-HHMM-<short8>
        assert!(name.starts_with("nibble-"));
        assert!(name.contains("abcd1234"));
        // Should have 4 dash-separated segments after the "nibble" prefix
        let parts: Vec<&str> = name.splitn(4, '-').collect();
        assert_eq!(parts.len(), 4, "expected nibble-DATE-TIME-SHORTID");
    }

    #[test]
    fn test_parse_container_status() {
        let sandbox = PodmanSandbox::new();

        let running_json = r#"[{"State": {"Status": "running"}}]"#;
        assert_eq!(
            sandbox.parse_container_status(running_json).unwrap(),
            ContainerStatus::Running
        );

        let stopped_json = r#"[{"State": {"Status": "exited"}}]"#;
        assert_eq!(
            sandbox.parse_container_status(stopped_json).unwrap(),
            ContainerStatus::Stopped
        );
    }

    /// INV-6: Hermes Dockerfile is separate and contains Python + Hermes install
    #[test]
    fn test_hermes_inv6_dockerfile_contains_python() {
        let sandbox = PodmanSandbox::new();
        let df = sandbox.generate_hermes_dockerfile();
        assert!(df.contains("python3"), "Dockerfile must install python3");
        assert!(
            df.contains("hermes-agent"),
            "Dockerfile must install hermes-agent"
        );
        assert!(df.contains("uv"), "Dockerfile must install uv");
        assert!(df.contains("/repos"), "Dockerfile must create /repos dir");
    }

    /// INV-6: Hermes Dockerfile runs as non-root user (node)
    #[test]
    fn test_hermes_inv6_dockerfile_non_root() {
        let sandbox = PodmanSandbox::new();
        let df = sandbox.generate_hermes_dockerfile();
        assert!(
            df.contains("USER node"),
            "Hermes container must run as node user"
        );
    }

    /// INV-6: Hermes Dockerfile is different from standard dockerfile
    #[test]
    fn test_hermes_inv6_dockerfile_separate_from_standard() {
        let sandbox = PodmanSandbox::new();
        let hermes_df = sandbox.generate_hermes_dockerfile();
        let standard_df = sandbox.generate_standard_dockerfile();
        assert_ne!(
            hermes_df, standard_df,
            "Hermes and standard Dockerfiles must differ"
        );
    }
}
