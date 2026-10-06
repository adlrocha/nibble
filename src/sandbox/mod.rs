//! Sandbox module for isolated agent execution.
//!
//! Provides containerized execution environments for Claude Code agents
//! using rootless Podman.

use anyhow::Result;
use std::path::PathBuf;

pub mod context;
pub mod podman;
pub mod worktree;

/// Information about a running container
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub status: ContainerStatus,
    pub image: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub ports: Vec<String>,
}

/// Container runtime status
#[derive(Debug, Clone, PartialEq)]
pub enum ContainerStatus {
    Running,
    Stopped,
    Paused,
    Unknown,
}

/// Sandbox health — a richer view on top of ContainerStatus.
///
/// `ContainerStatus::Running` only tells us the container process is alive.
/// `SandboxHealth` goes one step further and verifies that `podman exec` can
/// actually run a process inside the container (catches zombie/unresponsive
/// containers that appear running but can no longer execute commands).
#[derive(Debug, Clone, PartialEq)]
pub enum SandboxHealth {
    /// Container running and `podman exec` works — ready to attach or inject.
    Healthy,
    /// Container appears running but `podman exec` fails (zombie / OOM / etc.).
    /// The container should be killed and re-spawned.
    Degraded,
    /// Container exists but is stopped (e.g. after a host reboot).
    /// Can be restarted with `sandbox.start()`.
    Stopped,
    /// Container no longer exists in the runtime.
    Dead,
}

/// Get the base directory for nibble data
pub fn get_data_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?;
    let data_dir = home.join(".nibble");
    std::fs::create_dir_all(&data_dir)?;
    Ok(data_dir)
}

/// Get the cache directory for sandbox dependencies
pub fn get_cache_dir() -> Result<PathBuf> {
    let cache_dir = get_data_dir()?.join("cache");
    std::fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir)
}

/// Compute the container working directory from a host repo path.
///
/// The repository is mounted at `/<basename>` inside the container and
/// that path becomes the working directory. This gives each repo its own
/// session namespace naturally.
///
/// Examples:
/// - `/home/user/nibble` → `/nibble`
/// - `/home/user/nibble--feature-x` → `/nibble--feature-x`
pub fn container_working_dir(repo_path: &std::path::Path) -> String {
    let basename = repo_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    format!("/{}", basename)
}

/// Compute the Pi session directory name for a container working directory.
///
/// Pi encodes the cwd as a directory slug by replacing `/` with `--` and
/// wrapping in `--..--`. For a repo mounted at `/nibble` this yields
/// `--nibble--`.
pub fn pi_session_dir_name(container_dir: &str) -> String {
    let inner = container_dir.trim_start_matches('/');
    if inner.is_empty() {
        "--workspace--".to_string()
    } else {
        format!("--{}--", inner.replace('/', "--"))
    }
}

#[cfg(test)]
mod tests {
    use crate::models::{SandboxConfig, SandboxType};

    #[test]
    fn test_sandbox_type_serialization() {
        assert_eq!(SandboxType::None.as_str(), "none");
        assert_eq!(SandboxType::Podman.as_str(), "podman");
    }

    #[test]
    fn test_sandbox_type_deserialization() {
        use std::str::FromStr;
        assert_eq!(SandboxType::from_str("none").unwrap(), SandboxType::None);
        assert_eq!(
            SandboxType::from_str("podman").unwrap(),
            SandboxType::Podman
        );
        assert!(SandboxType::from_str("invalid").is_err());
    }

    #[test]
    fn test_sandbox_config_default() {
        let config = SandboxConfig::default();
        assert_eq!(config.image, "nibble-sandbox:latest");
        assert!(config.privileged);
        assert_eq!(config.port_ranges.len(), 2);
    }
}
