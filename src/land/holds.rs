//! Commits a judge held back, remembered on this machine.
//!
//! The comment on the pull request is the lasting record. This covers what a
//! comment can't: a dry run, which posts nothing, and a post that failed.

use super::record::Decided;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;

pub(super) struct Holds {
    pub dir: PathBuf,
}

impl Holds {
    fn path(&self, repo: &str, sha: &str) -> Result<PathBuf> {
        if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("unexpected commit {sha}");
        }
        Ok(self.dir.join(repo.replace('/', "__")).join(sha))
    }

    pub(super) fn put(&self, repo: &str, sha: &str, decided: Decided) -> Result<()> {
        let path = self.path(repo, sha)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = serde_json::to_string(&decided).context("failed to write the hold")?;
        fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))
    }

    /// The hold on a commit. One that can't be read still holds.
    pub(super) fn get(&self, repo: &str, sha: &str) -> Result<Option<Decided>> {
        let path = self.path(repo, sha)?;
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text).unwrap_or(Decided::Ask))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }
}
