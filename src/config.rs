//! Configuration loading from ~/.nibble/config.toml

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Top-level configuration structure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub telegram: TelegramConfig,

    #[serde(default)]
    pub factory: FactoryConfig,

    #[serde(default)]
    pub hermes: HermesConfig,

    #[serde(default)]
    pub pi: PiConfig,
}

/// AI Factory pipeline configuration.
///
/// When enabled, every sandboxed agent follows the structured development pipeline:
/// Spec → Implement → TDD → Adversarial → Risk Score → QA Gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactoryConfig {
    /// Whether the AI Factory pipeline is enabled for new sandboxes.
    #[serde(default = "default_factory_enabled")]
    pub enabled: bool,
}

fn default_factory_enabled() -> bool {
    true
}

impl Default for FactoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_factory_enabled(),
        }
    }
}

/// Hermes Agent sandbox configuration.
///
/// Controls how the Hermes Agent is installed and run inside a nibble sandbox,
/// including which repos are mounted and whether the gateway daemon is started.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HermesConfig {
    /// Repo paths to mount into the Hermes sandbox (mounted at /repos/<basename>).
    #[serde(default)]
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

impl HermesConfig {
    /// Resolve repo paths to absolute paths, filtering out non-existent ones.
    /// Returns (mount_point_name, absolute_path) pairs with de-duplicated basenames.
    pub fn resolve_repo_mounts(&self) -> Vec<(String, std::path::PathBuf)> {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut mounts = Vec::new();
        let mut seen_names = std::collections::HashMap::new();

        for repo in &self.repos {
            let expanded = if repo.starts_with('~') {
                repo.replacen('~', &home, 1)
            } else {
                repo.clone()
            };
            let path = std::path::PathBuf::from(&expanded);
            let abs = match path.canonicalize() {
                Ok(p) => p,
                Err(_) => {
                    eprintln!(
                        "  Warning: Hermes repo path '{}' does not exist, skipping",
                        repo
                    );
                    continue;
                }
            };
            let basename = abs
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "repo".to_string());

            // INV-3: Handle duplicate basenames by appending a suffix
            let count = seen_names.entry(basename.clone()).or_insert(0u32);
            let mount_name = if *count == 0 {
                basename.clone()
            } else {
                format!("{}-{}", basename, count)
            };
            *count += 1;

            mounts.push((mount_name, abs));
        }
        mounts
    }
}

/// Pi Agent sandbox configuration.
///
/// Controls how the Pi coding agent is installed inside a nibble sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiConfig {
    /// If true, npm install @mariozechner/pi-coding-agent on every spawn.
    #[serde(default = "default_pi_install_on_spawn")]
    pub install_on_spawn: bool,
}

fn default_pi_install_on_spawn() -> bool {
    true
}

impl Default for PiConfig {
    fn default() -> Self {
        Self {
            install_on_spawn: default_pi_install_on_spawn(),
        }
    }
}

/// Telegram bot notification settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Whether Telegram notifications are enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Bot token from @BotFather (e.g. "123456:ABC-DEF...").
    #[serde(default)]
    pub bot_token: String,

    /// Chat ID to send notifications to (user or group chat).
    #[serde(default)]
    pub chat_id: String,

    /// Telegram username (without @) that is allowed to interact with the bot.
    /// When set, the listener rejects any message whose sender username does not
    /// match, providing a second layer of protection on top of the chat_id check.
    #[serde(default)]
    pub allowed_username: String,
}

impl TelegramConfig {
    /// Returns true when the config is complete enough to use.
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.bot_token.is_empty() && !self.chat_id.is_empty()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_disabled() {
        let cfg = Config::default();
        assert!(!cfg.telegram.is_configured());
    }

    #[test]
    fn test_telegram_config_is_configured() {
        let cfg = TelegramConfig {
            enabled: true,
            bot_token: "token".to_string(),
            chat_id: "123".to_string(),
            allowed_username: String::new(),
        };
        assert!(cfg.is_configured());
    }

    #[test]
    fn test_telegram_config_not_configured_when_disabled() {
        let cfg = TelegramConfig {
            enabled: false,
            bot_token: "token".to_string(),
            chat_id: "123".to_string(),
            allowed_username: String::new(),
        };
        assert!(!cfg.is_configured());
    }

    #[test]
    fn test_telegram_config_not_configured_when_empty_token() {
        let cfg = TelegramConfig {
            enabled: true,
            bot_token: String::new(),
            chat_id: "123".to_string(),
            allowed_username: String::new(),
        };
        assert!(!cfg.is_configured());
    }

    #[test]
    fn test_parse_valid_toml() {
        let toml_str = r#"
[telegram]
enabled = true
bot_token = "123:ABC"
chat_id = "456789"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.telegram.is_configured());
        assert_eq!(config.telegram.bot_token, "123:ABC");
        assert_eq!(config.telegram.chat_id, "456789");
        // allowed_username is optional — defaults to empty string
        assert_eq!(config.telegram.allowed_username, "");
    }

    #[test]
    fn test_parse_toml_with_username() {
        let toml_str = r#"
[telegram]
enabled = true
bot_token = "123:ABC"
chat_id = "456789"
allowed_username = "adlrocha"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.telegram.allowed_username, "adlrocha");
    }

    #[test]
    fn test_parse_empty_toml() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.telegram.is_configured());
    }

    #[test]
    fn test_factory_default_enabled() {
        let config = Config::default();
        assert!(config.factory.enabled);
    }

    #[test]
    fn test_parse_toml_with_factory_enabled() {
        let toml_str = r#"
[factory]
enabled = true
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.factory.enabled);
    }

    #[test]
    fn test_parse_toml_with_factory_disabled() {
        let toml_str = r#"
[factory]
enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.factory.enabled);
    }

    #[test]
    fn test_parse_toml_factory_absent_defaults_enabled() {
        let toml_str = r#"
[telegram]
enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.factory.enabled);
    }

    // ── Hermes config tests (from hermes-agent-sandbox blueprint) ──────────────

    /// AC-5 / defaults: HermesConfig defaults have empty repos, gateway=true, correct image
    #[test]
    fn test_hermes_config_defaults() {
        let cfg = HermesConfig::default();
        assert!(cfg.repos.is_empty());
        assert!(cfg.gateway);
        assert_eq!(cfg.image, "nibble-hermes:latest");
    }

    /// AC-5: Config without [hermes] section gets defaults
    #[test]
    fn test_hermes_config_absent_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.hermes.repos.is_empty());
        assert!(config.hermes.gateway);
        assert_eq!(config.hermes.image, "nibble-hermes:latest");
    }

    /// AC-5: Parse [hermes] section with repos
    #[test]
    fn test_hermes_config_parse_repos() {
        let toml_str = r#"
[hermes]
repos = ["/home/user/project-a", "~/project-b"]
gateway = false
image = "my-hermes:v2"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.hermes.repos.len(), 2);
        assert!(!config.hermes.gateway);
        assert_eq!(config.hermes.image, "my-hermes:v2");
    }

    /// INV-3: resolve_repo_mounts skips non-existent paths
    #[test]
    fn test_hermes_inv3_resolve_skips_missing() {
        let cfg = HermesConfig {
            repos: vec!["/nonexistent/path/abc123".to_string()],
            ..Default::default()
        };
        let mounts = cfg.resolve_repo_mounts();
        assert!(mounts.is_empty(), "non-existent paths should be skipped");
    }

    /// INV-3: resolve_repo_mounts resolves existing paths
    #[test]
    fn test_hermes_inv3_resolve_existing_path() {
        // /tmp always exists on Linux
        let cfg = HermesConfig {
            repos: vec!["/tmp".to_string()],
            ..Default::default()
        };
        let mounts = cfg.resolve_repo_mounts();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].0, "tmp");
    }

    /// INV-3: resolve_repo_mounts deduplicates basenames with suffixes
    #[test]
    fn test_hermes_inv3_resolve_dedup_basenames() {
        // Create two temp dirs with the same basename
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        // Both tempdir basenames are unique, so let's use /tmp twice to test dedup
        let cfg = HermesConfig {
            repos: vec!["/tmp".to_string(), "/tmp".to_string()],
            ..Default::default()
        };
        let mounts = cfg.resolve_repo_mounts();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].0, "tmp");
        assert_eq!(mounts[1].0, "tmp-1");
        drop(dir1);
        drop(dir2);
    }

    /// Boundary: resolve_repo_mounts with empty repos list
    #[test]
    fn test_hermes_resolve_empty_repos() {
        let cfg = HermesConfig::default();
        let mounts = cfg.resolve_repo_mounts();
        assert!(mounts.is_empty());
    }

    /// Boundary: resolve_repo_mounts expands tilde
    #[test]
    fn test_hermes_resolve_tilde_expansion() {
        // This tests that ~ is replaced with $HOME
        let cfg = HermesConfig {
            repos: vec!["~/nonexistent_dir_xyz".to_string()],
            ..Default::default()
        };
        let mounts = cfg.resolve_repo_mounts();
        // Should be skipped because the dir doesn't exist, but tilde should be expanded
        // (the path won't canonicalize). This just verifies no panic.
        assert!(mounts.is_empty());
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
}
