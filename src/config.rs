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

    #[serde(default)]
    pub claude: ClaudeConfig,

    #[serde(default)]
    pub privacy_filter: PrivacyFilterConfig,

    #[serde(default)]
    pub memory: MemoryConfig,

    #[serde(default)]
    pub lm: LmConfig,
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
    // Opt-in: the full pipeline is noisy (token cost) for the common case.
    // Enable per-spawn with `nibble sandbox spawn --factory` or [factory].enabled.
    false
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

/// LLM Privacy Filter configuration.
///
/// Controls the inline proxy that scans agent API calls for PII/secrets
/// before they leave the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyFilterConfig {
    /// Whether the privacy filter proxy is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Proxy mode: "redact" replaces PII with [REDACTED: type];
    /// "block" returns a 400 error when PII is detected;
    /// "flag" redacts and forwards but adds an alert header.
    #[serde(default = "default_pf_mode")]
    pub mode: String,

    /// Port the proxy listens on (host side). Sandboxes reach it via
    /// 127.0.0.1:<port> because they use --network host.
    #[serde(default = "default_pf_port")]
    pub proxy_port: u16,

    /// Inference device for the privacy-filter model.
    #[serde(default = "default_pf_device")]
    pub device: String,

    /// Upstream Anthropic API URL (the proxy forwards here after scanning).
    #[serde(default = "default_pf_anthropic_upstream")]
    pub anthropic_upstream: String,

    /// Upstream OpenAI API URL (the proxy forwards here after scanning).
    #[serde(default = "default_pf_openai_upstream")]
    pub openai_upstream: String,

    /// If the proxy is unreachable, allow the request through (true) or
    /// block it (false).
    #[serde(default = "default_true")]
    pub fail_open: bool,
}

fn default_pf_mode() -> String {
    "redact".to_string()
}

fn default_pf_port() -> u16 {
    8474
}

fn default_pf_device() -> String {
    "cpu".to_string()
}

fn default_pf_anthropic_upstream() -> String {
    "https://api.anthropic.com".to_string()
}

fn default_pf_openai_upstream() -> String {
    "https://api.openai.com".to_string()
}

impl Default for PrivacyFilterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: default_pf_mode(),
            proxy_port: default_pf_port(),
            device: default_pf_device(),
            anthropic_upstream: default_pf_anthropic_upstream(),
            openai_upstream: default_pf_openai_upstream(),
            fail_open: true,
        }
    }
}

/// Telegram bot notification settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Whether Telegram notifications are enabled at all.
    #[serde(default)]
    pub enabled: bool,

    /// Whether agent-triggered notifications (Claude Stop hook, etc.) are sent.
    /// When false, `nibble notify` is a no-op, but Telegram listener messages
    /// (injection completions, heartbeats, cron alerts) are still sent.
    #[serde(default = "default_true")]
    pub notifications: bool,

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

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            notifications: true,
            bot_token: String::new(),
            chat_id: String::new(),
            allowed_username: String::new(),
        }
    }
}

impl TelegramConfig {
    /// Returns true when the config is complete enough to use.
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.bot_token.is_empty() && !self.chat_id.is_empty()
    }
}

/// Local LLM model management configuration.
///
/// Controls where `nibble lm list` scans for model files and which systemd
/// service unit is inspected to determine the currently active model.
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
    fn test_default_config_is_disabled() {
        let cfg = Config::default();
        assert!(!cfg.telegram.is_configured());
    }

    #[test]
    fn test_telegram_config_is_configured() {
        let cfg = TelegramConfig {
            enabled: true,
            notifications: true,
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
            notifications: true,
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
            notifications: true,
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
        assert!(config.telegram.notifications);
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
        assert!(config.telegram.notifications);
        assert_eq!(config.telegram.allowed_username, "adlrocha");
    }

    #[test]
    fn test_parse_empty_toml() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.telegram.is_configured());
        assert!(config.telegram.notifications); // default true
    }

    #[test]
    fn test_telegram_notifications_disabled_in_toml() {
        let toml_str = r#"
[telegram]
enabled = true
notifications = false
bot_token = "123:ABC"
chat_id = "456789"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.telegram.is_configured());
        assert!(!config.telegram.notifications);
    }

    #[test]
    fn test_factory_default_disabled() {
        // Factory is opt-in by default to keep sandboxes low-noise.
        let config = Config::default();
        assert!(!config.factory.enabled);
    }

    #[test]
    fn test_pi_extensions_default_from_manifest() {
        // Defaults are sourced from pi-extensions/external-packages.txt.
        let ext = Config::default().pi.extensions;
        assert!(
            ext.iter().any(|e| e == "npm:@quintinshaw/pi-dynamic-workflows"),
            "manifest default should include pi-dynamic-workflows, got {ext:?}"
        );
        // Comments and blank lines must not leak into the parsed list.
        for e in &ext {
            assert!(!e.is_empty(), "no empty entries: {ext:?}");
            assert!(!e.starts_with('#'), "no comment entries: {ext:?}");
        }
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
    fn test_parse_toml_factory_absent_defaults_disabled() {
        let toml_str = r#"
[telegram]
enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.factory.enabled);
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

    // ── PrivacyFilterConfig tests ───────────────────────────────────────────

    #[test]
    fn test_pf_config_defaults() {
        let cfg = PrivacyFilterConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.mode, "redact");
        assert_eq!(cfg.proxy_port, 8474);
        assert_eq!(cfg.device, "cpu");
        assert_eq!(cfg.anthropic_upstream, "https://api.anthropic.com");
        assert_eq!(cfg.openai_upstream, "https://api.openai.com");
        assert!(cfg.fail_open);
    }

    #[test]
    fn test_pf_config_parse_overrides() {
        let toml_str = r#"
[privacy_filter]
enabled = true
mode = "block"
proxy_port = 9999
device = "cuda"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.privacy_filter.enabled);
        assert_eq!(config.privacy_filter.mode, "block");
        assert_eq!(config.privacy_filter.proxy_port, 9999);
        assert_eq!(config.privacy_filter.device, "cuda");
    }

    #[test]
    fn test_pf_config_absent_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.privacy_filter.enabled);
        assert_eq!(config.privacy_filter.proxy_port, 8474);
    }
}
