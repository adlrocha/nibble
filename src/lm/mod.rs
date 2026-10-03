use crate::config::LmConfig;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub struct ModelEntry {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub is_active: bool,
    /// True when the filename contains an MTP marker (-MTP- or -A3B-).
    pub is_mtp: bool,
}

/// Return true if the filename pattern suggests an MTP model.
fn looks_like_mtp(name: &str) -> bool {
    let n = name.to_uppercase();
    n.contains("-MTP-") || n.contains("-A3B-")
}

/// Parse the active model path out of a systemd unit ExecStart line.
/// Looks for `-m <path>` anywhere in the file.
fn read_active_model(unit_path: &str) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(unit_path).ok()?;
    // Walk tokens; the token after `-m` is the model path.
    let tokens: Vec<&str> = contents.split_whitespace().collect();
    for i in 0..tokens.len().saturating_sub(1) {
        if tokens[i] == "-m" {
            return Some(PathBuf::from(tokens[i + 1]));
        }
    }
    None
}

/// Scan `dir` for .gguf files and return their paths sorted by name.
fn scan_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("gguf") {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    paths.sort();
    paths
}

pub fn list_models(cfg: &LmConfig) -> Result<Vec<ModelEntry>> {
    let active = read_active_model(&cfg.service_unit);

    let mut entries = Vec::new();
    for dir_str in &cfg.model_dirs {
        let dir = expand_home(dir_str);
        for path in scan_dir(&dir) {
            let size_bytes = path.metadata().map(|m| m.len()).unwrap_or(0);
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let is_mtp = looks_like_mtp(&name);
            let is_active = active
                .as_ref()
                .map(|a| a.canonicalize().ok() == path.canonicalize().ok() || *a == path)
                .unwrap_or(false);
            entries.push(ModelEntry {
                path,
                size_bytes,
                is_active,
                is_mtp,
            });
        }
    }
    Ok(entries)
}

/// Print a formatted table of model entries to stdout.
pub fn print_list(entries: &[ModelEntry]) {
    if entries.is_empty() {
        eprintln!("No .gguf models found. Check [lm] model_dirs in ~/.nibble/config.toml");
        return;
    }

    // Column widths
    let name_width = entries
        .iter()
        .map(|e| {
            e.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .len()
        })
        .max()
        .unwrap_or(10)
        .max(10);

    println!(
        "  {:<width$}  {:>8}  FLAGS",
        "MODEL",
        "SIZE",
        width = name_width
    );
    println!("  {}  --------  -----", "-".repeat(name_width));

    for e in entries {
        let name = e.path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let size = format_size(e.size_bytes);
        let mut flags = Vec::new();
        if e.is_active {
            flags.push("active");
        }
        if e.is_mtp {
            flags.push("mtp");
        }
        let flag_str = if flags.is_empty() {
            String::new()
        } else {
            format!("[{}]", flags.join(", "))
        };

        let active_marker = if e.is_active { "▶" } else { " " };
        println!(
            "{} {:<width$}  {:>8}  {}",
            active_marker,
            name,
            size,
            flag_str,
            width = name_width
        );
    }
}

fn format_size(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if bytes >= GIB {
        format!("{:.1}G", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.0}M", bytes as f64 / MIB as f64)
    } else {
        format!("{}B", bytes)
    }
}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(s)
    }
}

// ── Model profiles ────────────────────────────────────────────────────────────

/// Per-model sampling overrides loaded from `profiles.toml` in the model dir.
#[derive(Debug, Default)]
struct ModelProfile {
    mtp: Option<bool>,
    think: Option<bool>,
    temp: Option<f32>,
    top_p: Option<f32>,
    top_k: Option<u32>,
    min_p: Option<f32>,
}

/// Parse a minimal TOML profiles file.
///
/// Format:
/// ```toml
/// [gemma-4-26B]
/// temp = 1.0
/// top_k = 64
/// mtp = false
/// think = true
/// ```
/// The section name is matched case-insensitively as a substring of the filename.
///
/// Profiles are layered: the repo-shipped `scripts/llm-model-profiles.toml`
/// (committed, pulled with `git pull`) provides the defaults, and an optional
/// `profiles.toml` in the model directory overrides them field-by-field. This
/// lets the common profiles live in the nibble repo while a host can still
/// tweak sampling locally without editing tracked files.
fn load_profile(model_dir: &Path, model_name: &str) -> ModelProfile {
    let mut profile = ModelProfile::default();

    // 1. Repo-shipped defaults (lowest priority).
    if let Ok(repo_profiles) = locate_repo_profiles() {
        if let Ok(src) = std::fs::read_to_string(&repo_profiles) {
            apply_profile(&mut profile, &src, model_name);
        }
    }

    // 2. Per-model-dir overrides (highest priority).
    if let Ok(src) = std::fs::read_to_string(model_dir.join("profiles.toml")) {
        apply_profile(&mut profile, &src, model_name);
    }

    profile
}

/// Merge any sections of `src` that match `model_name` onto `profile`.
/// Later matching sections (and later sources) override earlier fields.
fn apply_profile(profile: &mut ModelProfile, src: &str, model_name: &str) {
    let lower_name = model_name.to_lowercase();
    let mut in_section = false;

    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let key = &trimmed[1..trimmed.len() - 1];
            in_section = lower_name.contains(&key.to_lowercase());
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "mtp" => profile.mtp = v.parse().ok().or(profile.mtp),
            "think" => profile.think = v.parse().ok().or(profile.think),
            "temp" => profile.temp = v.parse().ok().or(profile.temp),
            "top_p" => profile.top_p = v.parse().ok().or(profile.top_p),
            "top_k" => profile.top_k = v.parse().ok().or(profile.top_k),
            "min_p" => profile.min_p = v.parse().ok().or(profile.min_p),
            _ => {}
        }
    }
}

/// Locate the shipped model profiles. Prefers the install-time copy in
/// `~/.nibble/` (staged by install.sh), then falls back to the file next to
/// the setup script in the repo (for running directly from a checkout).
fn locate_repo_profiles() -> Result<PathBuf> {
    let installed = expand_home("~/.nibble/llm-model-profiles.toml");
    if installed.exists() {
        return Ok(installed);
    }
    let script = locate_setup_script()?;
    let dir = script.parent().context("setup script has no parent dir")?;
    let candidate = dir.join("llm-model-profiles.toml");
    if candidate.exists() {
        return Ok(candidate);
    }
    bail!(
        "No model profiles found ({} or {})",
        installed.display(),
        candidate.display()
    );
}

// ── Switch active model ───────────────────────────────────────────────────────

/// Find a model by partial name match, update the systemd service, and restart it.
pub fn use_model(cfg: &LmConfig, query: &str) -> Result<()> {
    let models = list_models(cfg)?;
    if models.is_empty() {
        bail!("No .gguf models found. Check [lm] model_dirs in ~/.nibble/config.toml");
    }

    let lower_query = query.to_lowercase();
    let matches: Vec<&ModelEntry> = models
        .iter()
        .filter(|e| {
            e.path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_lowercase().contains(&lower_query))
                .unwrap_or(false)
        })
        .collect();

    let entry = match matches.len() {
        0 => {
            eprintln!("No model matching {:?}. Available models:", query);
            print_list(&models);
            bail!("No match found");
        }
        1 => matches[0],
        _ => {
            eprintln!("Ambiguous query {:?} — matched:", query);
            for m in &matches {
                eprintln!(
                    "  {}",
                    m.path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                );
            }
            bail!("Narrow your query to match exactly one model");
        }
    };

    let model_path = entry.path.to_string_lossy();
    let model_name = entry
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let model_dir = entry.path.parent().unwrap_or(Path::new("."));

    // Load per-model profile (sampling overrides).
    let profile = load_profile(model_dir, model_name);

    // MTP: profile wins, else auto-detect from filename.
    let is_mtp = profile.mtp.unwrap_or(entry.is_mtp);
    // Thinking mode: profile wins, else off (script default).
    let think = profile.think.unwrap_or(false);

    let script = locate_setup_script()?;

    println!("Switching to: {}", model_name);
    println!(
        "  MTP: {}  think: {}  temp: {}  top_p: {}  top_k: {}  min_p: {}",
        is_mtp,
        think,
        profile.temp.unwrap_or(0.6),
        profile.top_p.unwrap_or(0.95),
        profile.top_k.unwrap_or(40),
        profile.min_p.unwrap_or(0.05),
    );

    let status = std::process::Command::new("bash")
        .arg(&script)
        .env("LLAMA_MODEL", model_path.as_ref())
        .env("LLAMA_MTP", if is_mtp { "true" } else { "false" })
        .env("LLAMA_THINK", if think { "true" } else { "false" })
        .env("LLAMA_TEMP", profile.temp.unwrap_or(0.6).to_string())
        .env("LLAMA_TOP_P", profile.top_p.unwrap_or(0.95).to_string())
        .env("LLAMA_TOP_K", profile.top_k.unwrap_or(40).to_string())
        .env("LLAMA_MIN_P", profile.min_p.unwrap_or(0.05).to_string())
        .status()
        .with_context(|| format!("Failed to run {}", script.display()))?;

    if !status.success() {
        bail!("setup-llama-server.sh exited with status {}", status);
    }
    Ok(())
}

/// Locate setup-llama-server.sh relative to the nibble repo or PATH.
fn locate_setup_script() -> Result<PathBuf> {
    // Prefer the install-time copy staged by install.sh — the installed binary
    // lives in ~/.local/bin, outside the repo, so the repo-relative walk below
    // won't find it on a normal install.
    let installed = expand_home("~/.nibble/setup-llama-server.sh");
    if installed.exists() {
        return Ok(installed);
    }
    // Try relative to the nibble repo (the binary lives in target/…/nibble).
    // Walk up from the binary location looking for scripts/setup-llama-server.sh.
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent();
        while let Some(d) = dir {
            let candidate = d.join("scripts/setup-llama-server.sh");
            if candidate.exists() {
                return Ok(candidate);
            }
            dir = d.parent();
        }
    }
    // Fallback: check a well-known install path.
    let fallback = PathBuf::from("/nibble/scripts/setup-llama-server.sh");
    if fallback.exists() {
        return Ok(fallback);
    }
    bail!(
        "setup-llama-server.sh not found. \
         Run from the nibble repo or ensure /nibble/scripts/setup-llama-server.sh exists."
    );
}
