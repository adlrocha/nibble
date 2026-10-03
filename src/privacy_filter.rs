//! Privacy Filter proxy management.
//!
//! The LLM Privacy Proxy is a Python service that intercepts Anthropic and
//! OpenAI API calls from sandboxed agents, scans prompts for PII/secrets
//! using OpenAI's privacy-filter model, and redacts or blocks them before
//! they leave the host.
//!
//! Since sandboxes use `--network host`, `127.0.0.1:8474` inside the
//! container routes to the host's proxy.

use crate::config::PrivacyFilterConfig;
use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const PROXY_PID_FILE: &str = "privacy-proxy.pid";
const PROXY_SCRIPT: &str = "scripts/privacy-proxy.py";

/// Return the path to the proxy PID file (~/.nibble/privacy-proxy.pid).
fn pid_file_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let path = home.join(".nibble").join(PROXY_PID_FILE);
    Ok(path)
}

/// Return the path to the proxy Python script inside the nibble repo.
fn proxy_script_path() -> Result<PathBuf> {
    // Try a few locations: installed copy first, then repo-relative
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let installed = home.join(".nibble").join("privacy-proxy.py");
    if installed.exists() {
        return Ok(installed);
    }

    // Fallback: repo-relative (works when running from source)
    let exe = std::env::current_exe()?;
    let repo_relative = exe
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(PROXY_SCRIPT));
    if let Some(ref p) = repo_relative {
        if p.exists() {
            return Ok(p.clone());
        }
    }

    // Last resort: check if nibble binary is in ~/.local/bin and repo is alongside
    let local_bin = home.join(".local/bin");
    if exe.starts_with(&local_bin) {
        let guess = home.join("nibble").join(PROXY_SCRIPT);
        if guess.exists() {
            return Ok(guess);
        }
    }

    anyhow::bail!(
        "Cannot find privacy-proxy.py. Expected at {} or repo/scripts/privacy-proxy.py",
        installed.display()
    )
}

/// Check whether the proxy process is alive.
pub fn is_proxy_running() -> bool {
    match pid_file_path() {
        Ok(path) => {
            if let Ok(pid_str) = std::fs::read_to_string(&path) {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    // Check if process exists (sends signal 0)
                    let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
                    if !alive {
                        // Stale PID file — clean it up
                        let _ = std::fs::remove_file(&path);
                    }
                    return alive;
                }
            }
            false
        }
        Err(_) => false,
    }
}

/// Check proxy health via HTTP.
pub fn health_check(port: u16) -> Result<bool> {
    let url = format!("http://127.0.0.1:{}/health", port);
    let resp = ureq::get(&url).set("Accept", "application/json").call();
    match resp {
        Ok(r) => {
            let json: serde_json::Value = r.into_json().unwrap_or_default();
            let status = json.get("status").and_then(|v| v.as_str()).unwrap_or("");
            Ok(status == "healthy")
        }
        Err(_) => Ok(false),
    }
}

/// Start the privacy proxy in the background.
///
/// Uses `python3` to run the proxy script. Writes the PID to
/// `~/.nibble/privacy-proxy.pid` so we can stop it later.
pub fn start_proxy(config: &PrivacyFilterConfig) -> Result<()> {
    if is_proxy_running() {
        if health_check(config.proxy_port)? {
            println!("Privacy proxy is already running and healthy.");
            return Ok(());
        }
        // PID file exists but health check failed — stale PID, try to kill then restart
        eprintln!("[privacy-proxy] Stale PID file detected, forcing restart…");
        let _ = stop_proxy();
    }

    let script = proxy_script_path()?;
    let python = which_python()?;

    println!(
        "Starting privacy proxy on port {} (mode={})…",
        config.proxy_port, config.mode
    );

    let mut child = Command::new(&python)
        .arg(&script)
        .env("PRIVACY_FILTER_PORT", config.proxy_port.to_string())
        .env("PRIVACY_FILTER_MODE", &config.mode)
        .env("PRIVACY_FILTER_DEVICE", &config.device)
        .env("ANTHROPIC_UPSTREAM", &config.anthropic_upstream)
        .env("OPENAI_UPSTREAM", &config.openai_upstream)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to spawn privacy proxy with {}", python.display()))?;

    let pid = child.id();

    // Write PID file
    let pid_path = pid_file_path()?;
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&pid_path, pid.to_string())
        .with_context(|| format!("Failed to write PID file to {}", pid_path.display()))?;

    // Detach stdout/stderr to log files
    let log_dir = pid_path.parent().unwrap().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let stdout_log = log_dir.join("privacy-proxy.out.log");
    let stderr_log = log_dir.join("privacy-proxy.err.log");

    let mut stdout_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_log)?;
    let mut stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_log)?;

    // Spawn threads to drain pipes into log files
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for l in reader.lines().map_while(Result::ok) {
                let _ = writeln!(stdout_file, "{}", l);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stderr);
            for l in reader.lines().map_while(Result::ok) {
                let _ = writeln!(stderr_file, "{}", l);
            }
        });
    }

    // Detach the child — move it into a background thread so it keeps
    // running after this function returns (avoids dropping the handle).
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    // Wait a moment for the proxy to start, then verify health
    std::thread::sleep(std::time::Duration::from_millis(800));
    match health_check(config.proxy_port) {
        Ok(true) => {
            println!(
                "Privacy proxy running on 127.0.0.1:{} (PID {})",
                config.proxy_port, pid
            );
            Ok(())
        }
        Ok(false) => {
            eprintln!("Warning: proxy started but health check reports not ready yet.");
            eprintln!("         Logs: {}", log_dir.display());
            Ok(())
        }
        Err(e) => {
            eprintln!("Warning: proxy started but health check failed: {}", e);
            eprintln!("         Logs: {}", log_dir.display());
            Ok(())
        }
    }
}

/// Stop the privacy proxy.
pub fn stop_proxy() -> Result<()> {
    let pid_path = pid_file_path()?;
    if !pid_path.exists() {
        println!("Privacy proxy is not running.");
        return Ok(());
    }

    let pid_str = std::fs::read_to_string(&pid_path)?;
    let pid = pid_str
        .trim()
        .parse::<u32>()
        .with_context(|| format!("Invalid PID in {}: {}", pid_path.display(), pid_str))?;

    // Try graceful SIGTERM first
    let term_ok = unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 };
    if term_ok {
        // Give it a moment to shut down
        std::thread::sleep(std::time::Duration::from_millis(500));
        let still_alive = unsafe { libc::kill(pid as i32, 0) == 0 };
        if still_alive {
            // Force kill
            let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    } else {
        // Process already gone — force kill just in case
        let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }

    let _ = std::fs::remove_file(&pid_path);
    println!("Privacy proxy stopped (PID {})", pid);
    Ok(())
}

/// Show proxy status.
pub fn proxy_status(port: u16) {
    let running = is_proxy_running();
    let healthy = health_check(port).unwrap_or(false);

    if running && healthy {
        println!("Privacy proxy: running and healthy on 127.0.0.1:{}", port);
    } else if running {
        println!(
            "Privacy proxy: process alive but health check failed on 127.0.0.1:{}",
            port
        );
    } else {
        println!("Privacy proxy: not running");
    }

    if let Ok(pid_path) = pid_file_path() {
        let log_dir = pid_path.parent().unwrap().join("logs");
        println!("  Logs: {}", log_dir.display());
    }
}

/// Ensure the proxy is running; start it if needed.
pub fn ensure_proxy_running(config: &PrivacyFilterConfig) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    if is_proxy_running() && health_check(config.proxy_port).unwrap_or(false) {
        return Ok(());
    }
    start_proxy(config)
}

/// Return the proxy endpoint URL for injection into sandboxes.
pub fn proxy_endpoint(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// Find `python3` or `python` on PATH.
fn which_python() -> Result<PathBuf> {
    for name in &["python3", "python"] {
        // Try `which` first, fall back to `command -v`
        let output = Command::new("which").arg(name).output().or_else(|_| {
            let cmd = format!("command -v {}", name);
            Command::new("sh").arg("-c").arg(&cmd).output()
        });
        if let Ok(out) = output {
            if out.status.success() {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !path.is_empty() {
                    return Ok(PathBuf::from(path));
                }
            }
        }
    }
    anyhow::bail!("python3 not found on PATH. Install Python 3 to use the privacy filter.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_endpoint_format() {
        assert_eq!(proxy_endpoint(8474), "http://127.0.0.1:8474");
        assert_eq!(proxy_endpoint(9999), "http://127.0.0.1:9999");
    }
}
