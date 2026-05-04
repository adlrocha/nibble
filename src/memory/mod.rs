//! Persistent memory system for cross-session knowledge.
//!
//! Stores memories and lessons as Markdown files with YAML frontmatter
//! in ~/.nibble/memory/, git-tracked for versioning and sync.

pub mod archive;
pub mod cli;
pub mod format;
pub mod git;
pub mod index;
pub mod llm;
pub mod models;
pub mod search;
pub mod store;
pub mod summarize;

#[cfg(test)]
mod tests;

use crate::config::memory_dir;
use anyhow::Result;
use std::fs;
use std::path::PathBuf;

/// Ensure the memory directory structure exists and is git-initialized.
pub fn init_memory_dir() -> Result<PathBuf> {
    let base = memory_dir();
    fs::create_dir_all(base.join("memories"))?;
    fs::create_dir_all(base.join("lessons"))?;
    fs::create_dir_all(base.join("sessions"))?;
    fs::create_dir_all(base.join("capture"))?;
    fs::create_dir_all(base.join("archive"))?;

    // Write .gitignore if it doesn't exist
    let gitignore = base.join(".gitignore");
    if !gitignore.exists() {
        fs::write(&gitignore, ".index.json\n.embeddings.json\n*.tmp\n")?;
    }

    // Initialize git repo if needed
    if !base.join(".git").is_dir() {
        git::init_repo(&base)?;
    }

    Ok(base)
}
