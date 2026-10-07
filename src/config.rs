//! Configuration loading from ~/.nibble/config.toml

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Top-level configuration structure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub hermes: HermesConfig,

    #[serde(default)]
    pub pi: PiConfig,

    #[serde(default)]
    pub claude: ClaudeConfig,

    #[serde(default)]
    pub memory: MemoryConfig,

    #[serde(default)]
    pub lm: LmConfig,

    #[serde(default)]
    pub quota_watch: QuotaWatchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LmConfig {
    /// Directories scanned for .gguf model files.
    /// Defaults to ~/workspace/llm-models.
    #[serde(default = "default_lm_model_dirs")]
    pub model_dirs: Vec<String>,

    /// Path to the llama-server systemd unit file used to detect the active model.
    #[serde(default = "default_lm_service_unit")]
    pub service_unit: String,
}

fn default_lm_model_dirs() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    vec![format!("{}/workspace/llm-models", home)]
}

/// Quota auto-continue configuration.
///
/// When a Claude Code / pi / omp session dies on a subscription quota error
/// ("usage limit reached", "You've hit your limit", out-of-credits, …), the
/// `nibble quota-watch` daemon waits for the quota window to reset and then
/// continues the task automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWatchConfig {
    /// Master switch for the quota-watch daemon.
    #[serde(default = "default_quota_watch_enabled")]
    pub enabled: bool,

    /// How often to re-scan session transcripts for new errors (seconds).
    #[serde(default = "default_quota_watch_poll_secs")]
    pub poll_secs: u64,

    /// Message sent to the agent when continuing after a quota reset.
    #[serde(default = "default_quota_watch_continue_message")]
    pub continue_message: String,

    /// Give up auto-continuing a task after this many attempts.
    #[serde(default = "default_quota_watch_max_attempts")]
    pub max_attempts: u32,

    /// Re-try interval when the error carries no parseable reset time (seconds).
    #[serde(default = "default_quota_watch_unknown_retry_secs")]
    pub unknown_retry_secs: u64,

    /// Extra delay after the stated reset time before continuing (seconds),
    /// so providers have a moment to actually lift the limit.
    #[serde(default = "default_quota_watch_reset_buffer_secs")]
    pub reset_buffer_secs: u64,
}

fn default_quota_watch_enabled() -> bool {
    true
}

fn default_quota_watch_poll_secs() -> u64 {
    60
}

fn default_quota_watch_continue_message() -> String {
    "continue".to_string()
}

fn default_quota_watch_max_attempts() -> u32 {
    8
}

fn default_quota_watch_unknown_retry_secs() -> u64 {
    1800
}

fn default_quota_watch_reset_buffer_secs() -> u64 {
    120
}

impl Default for QuotaWatchConfig {
    fn default() -> Self {
        Self {
            enabled: default_quota_watch_enabled(),
            poll_secs: default_quota_watch_poll_secs(),
            continue_message: default_quota_watch_continue_message(),
            max_attempts: default_quota_watch_max_attempts(),
            unknown_retry_secs: default_quota_watch_unknown_retry_secs(),
            reset_buffer_secs: default_quota_watch_reset_buffer_secs(),
        }
    }
}

/// Hermes Agent sandbox configuration.
///
/// Controls how the Hermes Agent is run inside a nibble sandbox.
/// Repo mounts are managed dynamically via `nibble hermes mount/unmount`
/// and stored in the `hermes_repos` database table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HermesConfig {
    /// Legacy field — silently ignored. Repos are now managed via `nibble hermes mount`.
    #[serde(default, skip_serializing)]
    pub repos: Vec<String>,

    /// Whether to start `hermes gateway` as the main process (PID 1).
    /// When true, the container stays alive as long as the gateway runs.
    /// When false, uses `sleep infinity` like standard sandboxes.
    #[serde(default = "default_hermes_gateway")]
    pub gateway: bool,

    /// Container image name for the Hermes sandbox.
    #[serde(default = "default_hermes_image")]
    pub image: String,
}

fn default_hermes_gateway() -> bool {
    true
}

fn default_hermes_image() -> String {
    "nibble-hermes:latest".to_string()
}

impl Default for HermesConfig {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            gateway: default_hermes_gateway(),
            image: default_hermes_image(),
        }
    }
}

/// Resolve a list of repo paths to (mount_name, absolute_path) pairs with de-duplicated basenames.
/// Used by both HermesConfig migration and DB-backed repo lists.
pub fn resolve_repo_mounts(
    repos: &[(String, std::path::PathBuf)],
) -> Vec<(String, std::path::PathBuf)> {
    let mut mounts = Vec::new();
    let mut seen_names = std::collections::HashMap::new();

    for (name_override, abs_path) in repos {
        let mount_name = if !name_override.is_empty() {
            name_override.clone()
        } else {
            abs_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "repo".to_string())
        };

        let count = seen_names.entry(mount_name.clone()).or_insert(0u32);
        let final_name = if *count == 0 {
            mount_name.clone()
        } else {
            format!("{}-{}", mount_name, count)
        };
        *count += 1;

        mounts.push((final_name, abs_path.clone()));
    }
    mounts
}

/// Pi Agent sandbox configuration.
///
/// Controls how the Pi coding agent is installed inside a nibble sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiConfig {
    /// If true, npm install @earendil-works/pi-coding-agent on every spawn.
    #[serde(default = "default_pi_install_on_spawn")]
    pub install_on_spawn: bool,

    /// pi extension sources (npm:… / git:…) to `pi install` in every Pi sandbox
    /// at spawn time, so they are available like any other extension.
    #[serde(default = "default_pi_extensions")]
    pub extensions: Vec<String>,
}

fn default_pi_extensions() -> Vec<String> {
    // Single source of truth: pi-extensions/external-packages.txt.
    // install.sh also reads that file (best-effort host install); spawn installs
    // the resolved list in-container. Edit the manifest to add/remove packages.
    const MANIFEST: &str = include_str!("../pi-extensions/external-packages.txt");
    MANIFEST
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn default_pi_install_on_spawn() -> bool {
    true
}

impl Default for PiConfig {
    fn default() -> Self {
        Self {
            install_on_spawn: default_pi_install_on_spawn(),
            extensions: default_pi_extensions(),
        }
    }
}

/// Controls how Claude Code is kept up to date inside a nibble sandbox.
///
/// Claude is baked into the sandbox image at build time, so without this it
/// stays frozen at whatever version the image was built with. When enabled,
/// `claude update` runs on every spawn to pull the latest release.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeConfig {
    /// If true, run `claude update` on every Claude sandbox spawn.
    #[serde(default = "default_claude_update_on_spawn")]
    pub update_on_spawn: bool,
}

fn default_claude_update_on_spawn() -> bool {
    true
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            update_on_spawn: default_claude_update_on_spawn(),
        }
    }
}

/// Returns the path to the config file: ~/.nibble/config.toml
pub fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".nibble").join("config.toml")
}

/// Load configuration from disk. Returns a default (notifications disabled) config
/// if the file does not exist or cannot be parsed.
pub fn load() -> Result<Config> {
    let path = config_path();

    if !path.exists() {
        return Ok(Config::default());
    }

    let contents = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("Failed to read config at {}: {}", path.display(), e))?;

    let config: Config = toml::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("Failed to parse config at {}: {}", path.display(), e))?;

    Ok(config)
}

/// Write configuration to disk, creating the directory if needed.
#[allow(dead_code)]
pub fn save(config: &Config) -> Result<()> {
    let path = config_path();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let contents = toml::to_string_pretty(config)
        .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))?;

    std::fs::write(&path, contents)
        .map_err(|e| anyhow::anyhow!("Failed to write config to {}: {}", path.display(), e))?;

    Ok(())
}

// ── Memory system configuration ──────────────────────────────────────────────

/// Memory system configuration.
///
/// Controls persistent cross-session memory storage, LLM extraction,
/// and git-based sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default)]
    pub llm: MemoryLlmConfig,

    #[serde(default)]
    pub sync: MemorySyncConfig,
}

fn default_true() -> bool {
    true
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            llm: MemoryLlmConfig::default(),
            sync: MemorySyncConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLlmConfig {
    #[serde(default = "default_llm_provider")]
    pub provider: String,
    #[serde(default = "default_llm_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_llm_model")]
    pub model: String,
    #[serde(default = "default_llm_model")]
    pub embedding_model: String,
    #[serde(default = "default_embedding_dims")]
    pub embedding_dims: usize,
}

fn default_llm_provider() -> String {
    "openai_compatible".to_string()
}
fn default_llm_base_url() -> String {
    "http://localhost:6969/v1".to_string()
}
fn default_llm_model() -> String {
    "default".to_string()
}
fn default_embedding_dims() -> usize {
    768
}

impl Default for MemoryLlmConfig {
    fn default() -> Self {
        Self {
            provider: default_llm_provider(),
            base_url: default_llm_base_url(),
            api_key: String::new(),
            model: default_llm_model(),
            embedding_model: default_llm_model(),
            embedding_dims: default_embedding_dims(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySyncConfig {
    #[serde(default)]
    pub remote: String,
    #[serde(default)]
    pub auto_sync: bool,
    #[serde(default = "default_sync_author")]
    pub author_name: String,
    #[serde(default = "default_sync_email")]
    pub author_email: String,
}

fn default_sync_author() -> String {
    "nibble".to_string()
}
fn default_sync_email() -> String {
    "nibble@local".to_string()
}

impl Default for MemorySyncConfig {
    fn default() -> Self {
        Self {
            remote: String::new(),
            auto_sync: false,
            author_name: default_sync_author(),
            author_email: default_sync_email(),
        }
    }
}

/// Returns the path to the memory directory: ~/.nibble/memory/
pub fn memory_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".nibble").join("memory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pi_extensions_default_from_manifest() {
        // Defaults are sourced from pi-extensions/external-packages.txt.
        let ext = Config::default().pi.extensions;
        assert!(
            ext.iter()
                .any(|e| e == "npm:@quintinshaw/pi-dynamic-workflows"),
            "manifest default should include pi-dynamic-workflows, got {ext:?}"
        );
        // Comments and blank lines must not leak into the parsed list.
        for e in &ext {
            assert!(!e.is_empty(), "no empty entries: {ext:?}");
            assert!(!e.starts_with('#'), "no comment entries: {ext:?}");
        }
    }

    // ── Hermes config tests (from hermes-agent-sandbox blueprint) ──────────────

    /// AC-5 / defaults: HermesConfig defaults have gateway=true, correct image
    #[test]
    fn test_hermes_config_defaults() {
        let cfg = HermesConfig::default();
        assert!(cfg.gateway);
        assert_eq!(cfg.image, "nibble-hermes:latest");
    }

    /// AC-5: Config without [hermes] section gets defaults
    #[test]
    fn test_hermes_config_absent_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.hermes.gateway);
        assert_eq!(config.hermes.image, "nibble-hermes:latest");
    }

    /// AC-5: Parse [hermes] section with gateway and image overrides
    #[test]
    fn test_hermes_config_parse_overrides() {
        let toml_str = r#"
[hermes]
gateway = false
image = "my-hermes:v2"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.hermes.gateway);
        assert_eq!(config.hermes.image, "my-hermes:v2");
    }

    /// Backward compat: legacy [hermes] repos field is silently ignored
    #[test]
    fn test_hermes_config_legacy_repos_ignored() {
        let toml_str = r#"
[hermes]
repos = ["/home/user/project-a", "~/project-b"]
gateway = false
image = "my-hermes:v2"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.hermes.gateway);
        assert_eq!(config.hermes.image, "my-hermes:v2");
    }

    /// INV-3: resolve_repo_mounts deduplicates basenames with suffixes
    #[test]
    fn test_hermes_inv3_resolve_dedup_basenames() {
        let repos: Vec<(String, std::path::PathBuf)> = vec![
            ("".to_string(), std::path::PathBuf::from("/tmp")),
            ("".to_string(), std::path::PathBuf::from("/tmp")),
        ];
        let mounts = resolve_repo_mounts(&repos);
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].0, "tmp");
        assert_eq!(mounts[1].0, "tmp-1");
    }

    /// resolve_repo_mounts with empty list
    #[test]
    fn test_hermes_resolve_empty_repos() {
        let mounts = resolve_repo_mounts(&[]);
        assert!(mounts.is_empty());
    }

    /// resolve_repo_mounts uses name override when provided
    #[test]
    fn test_hermes_resolve_name_override() {
        let repos: Vec<(String, std::path::PathBuf)> = vec![(
            "my-custom-name".to_string(),
            std::path::PathBuf::from("/tmp"),
        )];
        let mounts = resolve_repo_mounts(&repos);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].0, "my-custom-name");
    }

    // ── PiConfig tests (from pi-agent-sandbox blueprint) ──────────────────────

    /// AC-3: PiConfig::default() has install_on_spawn = true
    #[test]
    fn test_pi_config_default() {
        let cfg = PiConfig::default();
        assert!(cfg.install_on_spawn);
    }

    /// AC-4: Parsing config with [pi] install_on_spawn = false
    #[test]
    fn test_pi_config_parse_disabled() {
        let toml_str = r#"
[pi]
install_on_spawn = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.pi.install_on_spawn);
    }

    /// AC-5: Parsing config without [pi] section yields default
    #[test]
    fn test_pi_config_absent_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.pi.install_on_spawn);
    }

    // ── ClaudeConfig tests ────────────────────────────────────────────────────

    #[test]
    fn test_claude_config_default() {
        let cfg = ClaudeConfig::default();
        assert!(cfg.update_on_spawn);
    }

    #[test]
    fn test_claude_config_parse_disabled() {
        let toml_str = r#"
[claude]
update_on_spawn = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.claude.update_on_spawn);
    }

    #[test]
    fn test_claude_config_absent_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.claude.update_on_spawn);
    }
}

fn default_lm_service_unit() -> String {
    "/etc/systemd/system/llama-server.service".to_string()
}

impl Default for LmConfig {
    fn default() -> Self {
        Self {
            model_dirs: default_lm_model_dirs(),
            service_unit: default_lm_service_unit(),
        }
    }
}
