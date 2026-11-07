//! Slack credentials for `nibble rc`.
//!
//! They live in `$XDG_CONFIG_HOME/nibble/slack.env` (default
//! `~/.config/nibble/slack.env`, mode 0600), deliberately
//! outside `~/.nibble`: every sandbox bind-mounts `~/.nibble` read-write, and
//! a sandboxed agent must never be able to read the bot token. Environment
//! variables of the same name override the file (handy for testing).
//!
//! ```text
//! SLACK_BOT_TOKEN=xoxb-…
//! SLACK_APP_TOKEN=xapp-…
//! NIBBLE_SLACK_USER=U0123456789
//! ```

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const BOT_KEY: &str = "SLACK_BOT_TOKEN";
const APP_KEY: &str = "SLACK_APP_TOKEN";
const USER_KEY: &str = "NIBBLE_SLACK_USER";

/// The three values the daemon needs to talk to Slack as you.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Secrets {
    /// Bot token (`xoxb-`), for the Web API.
    pub bot_token: String,
    /// App-level token (`xapp-`, `connections:write`), for Socket Mode.
    pub app_token: String,
    /// Your Slack member ID: the only user the bot listens to.
    pub user_id: String,
}

/// `~/.config/nibble/slack.env` (honours `XDG_CONFIG_HOME`).
pub(crate) fn secrets_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        });
    base.join("nibble").join("slack.env")
}

/// Parse `KEY=value` lines; `#` comments, blank lines and surrounding
/// quotes are allowed.
fn parse_env(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            (k.trim().to_string(), v.to_string())
        })
        .collect()
}

/// Check each value's shape so a token pasted into the wrong slot fails at
/// setup, not as an opaque Slack error later.
pub(crate) fn validate(s: &Secrets) -> Result<()> {
    if !s.bot_token.starts_with("xoxb-") {
        bail!("{BOT_KEY} must be a bot token starting with xoxb-");
    }
    if !s.app_token.starts_with("xapp-") {
        bail!("{APP_KEY} must be an app-level token starting with xapp-");
    }
    let id_ok = s.user_id.len() >= 9
        && s.user_id.starts_with(['U', 'W'])
        && s.user_id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if !id_ok {
        bail!(
            "{USER_KEY} must be your Slack member ID (like U0123456789; \
             Slack profile → ⋮ → Copy member ID)"
        );
    }
    Ok(())
}

/// Build secrets from file contents plus environment overrides.
fn resolve(
    file: &HashMap<String, String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Secrets> {
    let get = |key: &str| -> Result<String> {
        env(key)
            .filter(|v| !v.is_empty())
            .or_else(|| file.get(key).cloned().filter(|v| !v.is_empty()))
            .with_context(|| format!("{key} is not set (run `nibble rc setup`)"))
    };
    let secrets = Secrets {
        bot_token: get(BOT_KEY)?,
        app_token: get(APP_KEY)?,
        user_id: get(USER_KEY)?,
    };
    validate(&secrets)?;
    Ok(secrets)
}

/// Load the credentials, refusing a secrets file other users can read.
pub(crate) fn load() -> Result<Secrets> {
    let path = secrets_path();
    let file = match std::fs::read_to_string(&path) {
        Ok(text) => {
            check_private(&path)?;
            parse_env(&text)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    resolve(&file, &|k| std::env::var(k).ok())
}

fn check_private(path: &Path) -> Result<()> {
    let mode = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} is readable by other users (mode {:o}); run: chmod 600 {}",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    Ok(())
}

/// Validate and write the credentials to [`secrets_path`] with mode 0600.
pub(crate) fn save(secrets: &Secrets) -> Result<PathBuf> {
    let path = secrets_path();
    save_to(&path, secrets)?;
    Ok(path)
}

fn save_to(path: &Path, secrets: &Secrets) -> Result<()> {
    validate(secrets)?;
    let dir = path.parent().context("secrets path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let tmp = path.with_extension("env.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("failed to create {}", tmp.display()))?;
    writeln!(
        f,
        "# nibble rc Slack credentials (keep private)\n{BOT_KEY}={}\n{APP_KEY}={}\n{USER_KEY}={}",
        secrets.bot_token, secrets.app_token, secrets.user_id
    )
    .context("failed to write Slack credentials")?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> HashMap<String, String> {
        parse_env(
            "# comment\nSLACK_BOT_TOKEN=\"xoxb-1\"\n\nSLACK_APP_TOKEN='xapp-2'\nNIBBLE_SLACK_USER=U0123456789\n",
        )
    }

    #[test]
    fn parses_quoted_values_and_comments() {
        let s = resolve(&good(), &|_| None).unwrap();
        assert_eq!(s.bot_token, "xoxb-1");
        assert_eq!(s.app_token, "xapp-2");
        assert_eq!(s.user_id, "U0123456789");
    }

    #[test]
    fn environment_overrides_the_file() {
        let s = resolve(&good(), &|k| (k == BOT_KEY).then(|| "xoxb-env".to_string())).unwrap();
        assert_eq!(s.bot_token, "xoxb-env");
    }

    #[test]
    fn missing_and_misplaced_values_are_rejected() {
        let mut f = good();
        f.remove(APP_KEY);
        let err = resolve(&f, &|_| None).unwrap_err().to_string();
        assert!(err.contains("SLACK_APP_TOKEN is not set"), "{err}");

        let mut f = good();
        f.insert(BOT_KEY.into(), "xapp-swapped".into());
        assert!(resolve(&f, &|_| None).is_err());

        let mut f = good();
        f.insert(USER_KEY.into(), "@alfonso".into());
        assert!(resolve(&f, &|_| None).is_err());
    }

    #[test]
    fn save_writes_a_private_file_that_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slack.env");
        let secrets = Secrets {
            bot_token: "xoxb-a".into(),
            app_token: "xapp-b".into(),
            user_id: "U0123456789".into(),
        };
        save_to(&path, &secrets).unwrap();
        check_private(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_private(&path).is_err(), "0644 must be refused");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let loaded = resolve(
            &parse_env(&std::fs::read_to_string(&path).unwrap()),
            &|_| None,
        );
        assert_eq!(loaded.unwrap(), secrets);
    }
}
