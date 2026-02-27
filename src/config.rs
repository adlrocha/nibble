//! Configuration loading from ~/.agent-tasks/config.toml

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Top-level configuration structure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub telegram: TelegramConfig,
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

/// Returns the path to the config file: ~/.agent-tasks/config.toml
pub fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".agent-tasks").join("config.toml")
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
}
