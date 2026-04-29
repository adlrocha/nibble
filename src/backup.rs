//! Backup and restore nibble state.
//!
//! `nibble backup` creates a zip of `~/.nibble/` plus a manifest.
//! `nibble import <zip>` restores `~/.nibble/` from a zip.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const MANIFEST_NAME: &str = "nibble-backup-manifest.json";

/// Directories inside ~/.nibble that are reconstructible caches and should
/// not be backed up (they can be huge: rustup toolchains, cargo registry, etc.).
const SKIP_DIRS: &[&str] = &["cache"];

/// Metadata stored inside every backup zip.
#[derive(Debug, Serialize, Deserialize)]
pub struct BackupManifest {
    pub version: String,
    pub created_at: String,
    pub hostname: String,
    pub nibble_version: String,
    pub source_dir: String,
}

impl BackupManifest {
    pub fn new(source_dir: PathBuf) -> Self {
        let hostname = std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string());

        Self {
            version: "1".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            hostname,
            nibble_version: env!("CARGO_PKG_VERSION").to_string(),
            source_dir: source_dir.to_string_lossy().to_string(),
        }
    }
}

/// Return the path to `~/.nibble`.
fn nibble_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    Ok(home.join(".nibble"))
}

/// Check whether a path relative to ~/.nibble should be skipped.
fn should_skip(rel: &Path) -> bool {
    for component in rel.components() {
        if let Some(name) = component.as_os_str().to_str() {
            if SKIP_DIRS.contains(&name) {
                return true;
            }
        }
    }
    false
}

/// Recursively copy a directory.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(src)?;
        if should_skip(rel) {
            continue;
        }
        let dest = dst.join(rel);
        if path.is_file() {
            if let Some(parent) = dest.parent() {
                if !parent.exists() {
                    fs::create_dir_all(parent)?;
                }
            }
            fs::copy(path, dest)?;
        } else if path.is_dir() {
            fs::create_dir_all(dest)?;
        }
    }
    Ok(())
}

/// Remove all contents of a directory without removing the directory itself.
fn clear_dir_contents(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            fs::remove_file(&path)?;
        } else if path.is_dir() {
            fs::remove_dir_all(&path)?;
        }
    }
    Ok(())
}

/// Create a timestamped backup zip.
///
/// If `output` is `Some`, use that exact path. Otherwise generate
/// `nibble-backup-<timestamp>.zip` in the current directory.
pub fn create_backup(output: Option<PathBuf>) -> Result<PathBuf> {
    let source = nibble_dir()?;

    if !source.exists() {
        anyhow::bail!(
            "Nibble data directory does not exist: {}. Nothing to backup.",
            source.display()
        );
    }

    let dest = output.unwrap_or_else(|| {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        PathBuf::from(format!("nibble-backup-{}.zip", ts))
    });

    if dest.exists() {
        anyhow::bail!("Output file already exists: {}", dest.display());
    }

    let file = fs::File::create(&dest)
        .with_context(|| format!("Cannot create backup file: {}", dest.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let dir_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    // Write manifest first.
    let manifest = BackupManifest::new(source.clone());
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    zip.start_file(MANIFEST_NAME, options)?;
    zip.write_all(manifest_json.as_bytes())?;

    let mut file_count: usize = 0;
    let mut byte_count: u64 = 0;

    println!("Backing up {} ...", source.display());

    // Walk ~/.nibble and add every file / empty directory.
    // Use into_iter() so we can skip whole subtrees (e.g. cache/).
    let mut walker = WalkDir::new(&source).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(&source).with_context(|| {
            format!(
                "Failed to strip prefix {} from {}",
                source.display(),
                path.display()
            )
        })?;

        if should_skip(rel) {
            if path.is_dir() {
                walker.skip_current_dir();
            }
            continue;
        }

        let name_in_zip = Path::new(".nibble").join(rel);
        let name_str = name_in_zip.to_string_lossy();

        if path.is_file() {
            let meta = entry.metadata()?;
            let mut perms = options;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                perms = perms.unix_permissions(meta.permissions().mode());
            }
            zip.start_file(&name_str, perms)?;
            let mut f = fs::File::open(path)?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            zip.write_all(&buf)?;

            file_count += 1;
            byte_count += buf.len() as u64;
            eprint!(
                "  + {} ({} files, {} bytes)\r",
                name_str, file_count, byte_count
            );
        } else if path.is_dir() {
            // zip directories by adding a trailing-slash entry so empty dirs are preserved.
            let mut dir_name = name_str.to_string();
            if !dir_name.ends_with('/') {
                dir_name.push('/');
            }
            zip.add_directory(&dir_name, dir_options)?;
        }
    }

    // Finish the archive and flush to disk.
    let mut file = zip.finish().context("Failed to finalise zip archive")?;
    file.flush().context("Failed to flush zip file to disk")?;
    drop(file);

    let size = fs::metadata(&dest)?.len();
    println!(
        "Backup complete: {} files, {} bytes -> {} ({} bytes zipped)",
        file_count,
        byte_count,
        dest.display(),
        size
    );
    Ok(dest)
}

/// Import `~/.nibble` from a zip created by `create_backup`.
///
/// The current `~/.nibble` is renamed to `~/.nibble.backup.<timestamp>` before
/// the new data is extracted.
pub fn import_backup(zip_path: &Path) -> Result<()> {
    if !zip_path.exists() {
        anyhow::bail!("Backup file not found: {}", zip_path.display());
    }

    let file = fs::File::open(zip_path)
        .with_context(|| format!("Cannot open backup file: {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("Cannot read zip archive: {}", zip_path.display()))?;

    // Validate manifest.
    {
        let mut manifest_file = archive.by_name(MANIFEST_NAME).with_context(|| {
            format!("Backup is missing {} — not a nibble backup?", MANIFEST_NAME)
        })?;
        let mut manifest_json = String::new();
        manifest_file.read_to_string(&mut manifest_json)?;
        let manifest: BackupManifest = serde_json::from_str(&manifest_json)
            .with_context(|| format!("Invalid {} in backup", MANIFEST_NAME))?;

        println!("Backup created: {}", manifest.created_at);
        println!("  Host:       {}", manifest.hostname);
        println!("  Source:     {}", manifest.source_dir);
        println!("  Version:    {}", manifest.nibble_version);
    }

    let target = nibble_dir()?;

    // Rename existing ~/.nibble out of the way.
    if target.exists() {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let backup_name = format!(".nibble.backup.{}", ts);
        let backup_path = target.parent().unwrap().join(&backup_name);

        // Try a fast rename first; fall back to recursive copy if the directory
        // is a mount point or otherwise busy.
        if fs::rename(&target, &backup_path).is_ok() {
            println!("Existing nibble data moved to: {}", backup_path.display());
        } else {
            copy_dir_all(&target, &backup_path)?;
            println!("Existing nibble data copied to: {}", backup_path.display());
            // Clean target in-place so we can extract into it.
            clear_dir_contents(&target)?;
        }
    } else {
        fs::create_dir_all(&target)?;
    }

    // Extract files.
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let outpath = match file.enclosed_name() {
            Some(path) => path,
            None => continue, // skip paths with `..` etc.
        };

        // Skip the manifest itself.
        if outpath.file_name() == Some(std::ffi::OsStr::new(MANIFEST_NAME)) {
            continue;
        }

        // Strip the leading ".nibble/" prefix so we extract into ~/.nibble/ directly.
        let rel = outpath.strip_prefix(".nibble").unwrap_or(&outpath);
        let dest_path = target.join(rel);

        if file.name().ends_with('/') {
            fs::create_dir_all(&dest_path)?;
        } else {
            if let Some(parent) = dest_path.parent() {
                if !parent.exists() {
                    fs::create_dir_all(parent)?;
                }
            }
            let mut out = fs::File::create(&dest_path)?;
            std::io::copy(&mut file, &mut out)?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = file.unix_mode() {
                    fs::set_permissions(&dest_path, fs::Permissions::from_mode(mode))?;
                }
            }
        }
    }

    println!("Nibble state restored to: {}", target.display());
    println!("Run `./install.sh` to reinstall wrappers, hooks and sandbox images if needed.");
    Ok(())
}
