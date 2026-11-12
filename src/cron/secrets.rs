//! Values cron runs take from the environment, such as a read-only log API
//! token, kept per repo in `$XDG_CONFIG_HOME/nibble/cron-secrets.toml`
//! (default `~/.config/nibble/cron-secrets.toml`, mode 0600):
//!
//! ```toml
//! ["/home/me/workspace/shop"]
//! NORTHFLANK_API_TOKEN = "nf-…"
//! ```
//!
//! A job file only names what it needs (`env = ["NORTHFLANK_API_TOKEN"]`).
//! The value comes from this repo's table and no other, so a repo's job
//! file (which its sandbox can edit) cannot ask for another repo's token.
//! The file is outside `~/.nibble`, which every sandbox mounts.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Repo path → name → value.
pub type Table = BTreeMap<String, BTreeMap<String, String>>;

/// `~/.config/nibble/cron-secrets.toml`, next to `nibble rc`'s Slack file.
pub fn path() -> PathBuf {
    crate::rc::secrets::secrets_path().with_file_name("cron-secrets.toml")
}

/// The whole file; empty when it does not exist. Refuses a file other
/// users can read.
pub fn load_from(path: &Path) -> Result<Table> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Table::new()),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} is readable by other users (mode {:o}); run: chmod 600 {}",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    toml::from_str(&text).with_context(|| format!("{} is not valid TOML", path.display()))
}

/// Write the file atomically with mode 0600.
pub fn save_to(path: &Path, table: &Table) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string(table).context("failed to encode the secrets")?;
    let tmp = path.with_extension("toml.tmp");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    // A leftover temp file may predate the mode above.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_round_trip_and_permissions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("nibble/cron-secrets.toml");
        assert!(load_from(&p).unwrap().is_empty(), "missing file is empty");
        let mut t = Table::new();
        t.entry("/home/me/repo".into())
            .or_default()
            .insert("TOKEN".into(), "quote\" and \\".into());
        save_to(&p, &t).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(load_from(&p).unwrap(), t);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_from(&p).unwrap_err().to_string().contains("chmod 600"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&p, "not = [toml").unwrap();
        assert!(load_from(&p).is_err());
    }
}
