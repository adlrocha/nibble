//! Git operations for the memory directory.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Initialize a git repo in the memory directory.
pub fn init_repo(dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("init")
        .arg(dir)
        .output()
        .context("Failed to run git init")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git init failed: {stderr}");
    }

    // Make an initial commit so the repo is valid
    let _output = Command::new("git")
        .args(["-C", &dir.to_string_lossy()])
        .args(["add", "-A"])
        .output()?;
    let _ = Command::new("git")
        .args(["-C", &dir.to_string_lossy()])
        .args(["-c", "user.name=nibble"])
        .args(["-c", "user.email=nibble@local"])
        .args([
            "commit",
            "-m",
            "memory: initialize memory store",
            "--allow-empty",
        ])
        .output();

    Ok(())
}

/// Stage all changes and commit.
pub fn commit(dir: &Path, message: &str, author_name: &str, author_email: &str) -> Result<bool> {
    // git add -A
    let output = Command::new("git")
        .args(["-C", &dir.to_string_lossy()])
        .args(["add", "-A"])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git add failed: {stderr}");
    }

    // Check if there's anything to commit
    let output = Command::new("git")
        .args(["-C", &dir.to_string_lossy()])
        .args(["diff", "--cached", "--quiet"])
        .output();
    match output {
        Ok(o) if o.status.success() => return Ok(false), // nothing to commit
        _ => {}                                          // there are changes
    }

    // git commit
    let output = Command::new("git")
        .args(["-C", &dir.to_string_lossy()])
        .args(["-c", &format!("user.name={}", author_name)])
        .args(["-c", &format!("user.email={}", author_email)])
        .args(["commit", "-m", message])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git commit failed: {stderr}");
    }

    Ok(true)
}

/// Pull from remote.
pub fn pull(dir: &Path) -> Result<()> {
    // Check if there's a remote configured
    let output = Command::new("git")
        .args(["-C", &dir.to_string_lossy()])
        .args(["remote"])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(()); // no remote configured, skip
    }

    let output = Command::new("git")
        .args(["-C", &dir.to_string_lossy()])
        .args(["pull", "--rebase"])
        .output();

    match output {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("[memory] git pull warning: {stderr}");
            Ok(()) // non-fatal
        }
        Err(e) => {
            eprintln!("[memory] git pull failed: {e}");
            Ok(()) // non-fatal
        }
    }
}

/// Push to remote.
pub fn push(dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", &dir.to_string_lossy()])
        .args(["remote"])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(()); // no remote configured
    }

    let output = Command::new("git")
        .args(["-C", &dir.to_string_lossy()])
        .args(["push"])
        .output();

    match output {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("[memory] git push warning: {stderr}");
            Ok(()) // non-fatal
        }
        Err(e) => {
            eprintln!("[memory] git push failed: {e}");
            Ok(()) // non-fatal
        }
    }
}

/// Full sync: commit + pull + push.
pub fn sync(dir: &Path, message: &str, author_name: &str, author_email: &str) -> Result<()> {
    commit(dir, message, author_name, author_email)?;
    pull(dir)?;
    push(dir)?;
    Ok(())
}
