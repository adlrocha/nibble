//! Git worktree management for `nibble sandbox spawn --branch`.
//!
//! Worktrees are placed next to the repo at
//! `<repo_parent>/<repo_name>--<branch-slug>` so a sandbox can work on a
//! branch without touching the main checkout.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Map a branch name to a filesystem-safe slug: alphanumeric, `-` and `_`
/// are kept as-is, everything else becomes `-`.
pub fn branch_slug(branch: &str) -> String {
    branch
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Create a git worktree for `branch` next to `repo_path`, returning the worktree path.
///
/// The worktree is placed at `<repo_parent>/<repo_name>--<branch-slug>`, where the
/// branch slug replaces `/` and non-alphanumeric chars with `-`.
/// If the branch doesn't exist it is auto-created from the repo's current HEAD.
pub fn create_worktree(repo_path: &Path, branch: &str) -> Result<PathBuf> {
    let abs_repo = repo_path
        .canonicalize()
        .with_context(|| format!("Cannot resolve repo path: {}", repo_path.display()))?;

    let repo_name = abs_repo
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("Cannot determine repo name from path"))?;

    let branch_slug = branch_slug(branch);

    let parent = abs_repo
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Repo path has no parent directory"))?;

    let worktree_path = parent.join(format!("{}--{}", repo_name, branch_slug));

    if worktree_path.exists() {
        anyhow::bail!(
            "Worktree path already exists: {}. \
             Remove it first or use a different branch name.",
            worktree_path.display()
        );
    }

    // Check if branch already exists in the repo.
    let branch_exists = std::process::Command::new("git")
        .args([
            "-C",
            abs_repo.to_str().unwrap_or(""),
            "rev-parse",
            "--verify",
            branch,
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if branch_exists {
        // Check out the existing branch into the new worktree.
        let out = std::process::Command::new("git")
            .args([
                "-C",
                abs_repo.to_str().unwrap_or(""),
                "worktree",
                "add",
                worktree_path.to_str().unwrap_or(""),
                branch,
            ])
            .output()
            .context("Failed to run git worktree add")?;
        if !out.status.success() {
            anyhow::bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    } else {
        // Auto-create the branch from current HEAD.
        let out = std::process::Command::new("git")
            .args([
                "-C",
                abs_repo.to_str().unwrap_or(""),
                "worktree",
                "add",
                "-b",
                branch,
                worktree_path.to_str().unwrap_or(""),
            ])
            .output()
            .context("Failed to run git worktree add -b")?;
        if !out.status.success() {
            anyhow::bail!(
                "git worktree add -b failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        println!("  Branch:    created '{}' from HEAD", branch);
    }

    println!("  Worktree:  {} → {}", branch, worktree_path.display());
    Ok(worktree_path)
}

/// Remove a git worktree directory.
///
/// Returns `true` if removed, `false` if skipped by the user.
/// `force` skips the dirty-check prompt (but still prints a warning).
pub fn remove_worktree(worktree_path: &Path, force: bool) -> Result<bool> {
    use std::io::{BufRead, Write};

    if !worktree_path.exists() {
        return Ok(true); // already gone, nothing to do
    }

    // Detect uncommitted changes inside the worktree.
    let dirty = std::process::Command::new("git")
        .args([
            "-C",
            worktree_path.to_str().unwrap_or(""),
            "status",
            "--porcelain",
        ])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if dirty {
        if force {
            eprintln!(
                "⚠️  Warning: worktree {} has uncommitted changes — removing anyway (--force).",
                worktree_path.display()
            );
        } else {
            eprint!(
                "⚠️  Worktree {} has uncommitted changes. Remove it anyway? [y/N] ",
                worktree_path.display()
            );
            std::io::stderr().flush().ok();
            let mut input = String::new();
            std::io::BufReader::new(std::io::stdin())
                .read_line(&mut input)
                .ok();
            if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
                println!("Aborted. Worktree kept.");
                return Ok(false);
            }
        }
    }

    // `git worktree remove --force` removes the directory and unregisters from .git/worktrees.
    let out = std::process::Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            worktree_path.to_str().unwrap_or(""),
        ])
        .output()
        .context("Failed to run git worktree remove")?;

    if out.status.success() {
        println!("Removed worktree: {}", worktree_path.display());
    } else {
        // Fall back to plain directory removal if git worktree remove fails
        // (e.g. the .git link is already broken).
        eprintln!(
            "git worktree remove failed ({}), falling back to rm -rf",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        std::fs::remove_dir_all(worktree_path)
            .with_context(|| format!("Failed to remove {}", worktree_path.display()))?;
        println!("Removed worktree directory: {}", worktree_path.display());
    }

    Ok(true)
}
