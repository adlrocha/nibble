//! Pi-family (pi/omp) session helpers: config-dir mounting, session discovery,
//! interactive picking, and resume command construction.

use anyhow::{Context, Result};

use crate::sandbox::pi_session_dir_name;

/// Mount a pi-family config dir (`~/.pi` or `~/.omp`) into the sandbox.
///
/// Handles the case where `~/<dir>/agent` is a symlink (e.g. into a dotfiles
/// repo managed with stow): the symlink target is mounted over the agent path
/// so the container sees the real contents. Creates the standard subdirs
/// (skills/sessions/extensions) on the host first so rootless podman never
/// creates root-owned dirs.
pub(crate) fn mount_agent_config_dir(
    home_dir: &std::path::Path,
    dir_name: &str,
    extra_volumes: &mut Vec<String>,
) -> Result<()> {
    let pi_dir = home_dir.join(dir_name);
    let agent_dir = pi_dir.join("agent");

    if agent_dir.is_symlink() {
        let resolved = agent_dir
            .canonicalize()
            .with_context(|| format!("Failed to resolve ~/{dir_name}/agent symlink"))?;
        std::fs::create_dir_all(&resolved)
            .with_context(|| format!("Failed to create resolved ~/{dir_name}/agent target"))?;
        std::fs::create_dir_all(resolved.join("skills"))
            .with_context(|| format!("Failed to create ~/{dir_name}/agent/skills"))?;
        std::fs::create_dir_all(resolved.join("sessions"))
            .with_context(|| format!("Failed to create ~/{dir_name}/agent/sessions"))?;
        std::fs::create_dir_all(resolved.join("extensions"))
            .with_context(|| format!("Failed to create ~/{dir_name}/agent/extensions"))?;
        extra_volumes.push(format!("{}:/home/node/{dir_name}:rw", pi_dir.display()));
        extra_volumes.push(format!(
            "{}:/home/node/{dir_name}/agent:rw",
            resolved.display()
        ));
    } else {
        if !agent_dir.exists() {
            std::fs::create_dir_all(&agent_dir)
                .with_context(|| format!("Failed to create ~/{dir_name}/agent"))?;
        }
        std::fs::create_dir_all(agent_dir.join("skills"))
            .with_context(|| format!("Failed to create ~/{dir_name}/agent/skills"))?;
        std::fs::create_dir_all(agent_dir.join("sessions"))
            .with_context(|| format!("Failed to create ~/{dir_name}/agent/sessions"))?;
        std::fs::create_dir_all(agent_dir.join("extensions"))
            .with_context(|| format!("Failed to create ~/{dir_name}/agent/extensions"))?;
        extra_volumes.push(format!("{}:/home/node/{dir_name}:rw", pi_dir.display()));
    }
    mount_sessions_overlay(home_dir, dir_name, extra_volumes);
    Ok(())
}

/// Session transcripts may live behind a symlink (the consolidated
/// `~/.nibble/sessions/<dir>` layout keeps the corpus in nibble's data dir
/// with symlinks back at the agent paths). A symlink resolves against its
/// *physical* parent, which can differ between host and container (e.g. a
/// stow-managed `~/.pi/agent` lives deeper in a dotfiles repo than the
/// container's `/home/node/.pi/agent`), so the link can dangle in the
/// sandbox even when it resolves on the host. Overlay the resolved target
/// over the container's sessions path — same pattern as the agent-dir
/// overlay above — so transcripts are reachable from both namespaces.
fn mount_sessions_overlay(
    home_dir: &std::path::Path,
    dir_name: &str,
    extra_volumes: &mut Vec<String>,
) {
    let sessions = home_dir.join(dir_name).join("agent").join("sessions");
    if sessions.is_symlink() {
        if let Ok(resolved) = sessions.canonicalize() {
            extra_volumes.push(format!(
                "{}:/home/node/{dir_name}/agent/sessions:rw",
                resolved.display()
            ));
        }
    }
}

/// Ensure `~/<dir>/agent/skills` is a symlink pointing to `~/.claude/skills/`.
///
/// Called at spawn time so the installed skills are available to
/// pi-family agents without duplicating files. Non-fatal on failure — logs a
/// warning and continues.
pub(crate) fn ensure_agent_skills_symlink(home_dir: &std::path::Path, dir_name: &str) {
    let pi_skills = home_dir.join(dir_name).join("agent").join("skills");
    let claude_skills = home_dir.join(".claude").join("skills");

    // Already a symlink — no-op
    if pi_skills.is_symlink() {
        return;
    }

    // Exists as a real directory — warn and skip (don't clobber user data)
    if pi_skills.is_dir() {
        eprintln!(
            "  Warning: {} exists as a directory, skipping skills symlink",
            pi_skills.display()
        );
        return;
    }

    if let Err(e) = std::os::unix::fs::symlink(&claude_skills, &pi_skills) {
        eprintln!(
            "  Warning: failed to create skills symlink {} → {}: {}",
            pi_skills.display(),
            claude_skills.display(),
            e
        );
    }
}

/// Discover the most recent pi-family session file inside a specific running
/// container. `dir_name` is the config root (".pi" or ".omp").
///
/// This is more reliable than `find_pi_session_for_cwd` when multiple sandboxes
/// are active, because it looks at the session files *inside the container*
/// rather than scanning the host's config dir which is shared across sandboxes.
pub(crate) fn discover_pi_session_in_container(
    container_id: &str,
    pi_slug: &str,
    dir_name: &str,
) -> Option<std::path::PathBuf> {
    let cmd = format!(
        "ls -t /home/node/{dir_name}/agent/sessions/{pi_slug}/{{*.jsonl,**/*.jsonl}} 2>/dev/null | head -1",
    );
    let output = std::process::Command::new("podman")
        .args(["exec", container_id, "sh", "-c", &cmd])
        .output()
        .ok()?;
    let container_path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if container_path.is_empty() {
        return None;
    }
    // Convert container path back to host path
    let home = dirs::home_dir()?;
    let host_path = container_path.replacen("/home/node/", &format!("{}/", home.display()), 1);
    let path = std::path::PathBuf::from(host_path);
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// List every pi session file for a container working directory, newest first.
///
/// Uses the repo-specific slug directory under `dir_name` (".pi" or ".omp");
/// only when it is empty does the legacy `--workspace--` directory take over.
pub(crate) fn list_pi_sessions_for_cwd(
    container_dir: &str,
    dir_name: &str,
) -> Vec<(std::path::PathBuf, std::time::SystemTime)> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    list_pi_sessions_for_cwd_with_home(&home, container_dir, dir_name)
}

pub(crate) fn list_pi_sessions_for_cwd_with_home(
    home: &std::path::Path,
    container_dir: &str,
    dir_name: &str,
) -> Vec<(std::path::PathBuf, std::time::SystemTime)> {
    let slug = pi_session_dir_name(container_dir);
    let sessions_dir = home.join(dir_name).join("agent").join("sessions");

    let collect = |slug_name: &str| -> Vec<(std::path::PathBuf, std::time::SystemTime)> {
        let slug_dir = sessions_dir.join(slug_name);
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&slug_dir) {
            for file in entries.flatten() {
                let path = file.path();
                if !crate::session::is_session_file(&path) {
                    continue;
                }
                if let Ok(mtime) = file.metadata().and_then(|m| m.modified()) {
                    out.push((path, mtime));
                }
            }
        }
        out
    };

    let mut sessions = collect(&slug);
    if sessions.is_empty() {
        sessions = collect("--workspace--");
    }
    sessions.sort_by(|a, b| b.1.cmp(&a.1));
    sessions
}

/// The user's choice in the interactive session picker.
pub(crate) enum PiSessionPick {
    Session(std::path::PathBuf),
    Fresh,
}

/// Interactively pick one of several pi sessions for a repo.
///
/// Shown when attach has no stored session mapping and more than one session
/// exists — after a reboot/crash the newest file is often a `--btw` side
/// session, not the conversation the user wants back.
/// Empty input (or anything unparseable) selects the newest session; `n`
/// starts a fresh one.
pub(crate) fn pick_pi_session(
    candidates: &[(std::path::PathBuf, std::time::SystemTime)],
    bin: &str,
) -> PiSessionPick {
    use std::io::Write;

    let show = candidates.len().min(8);
    eprintln!("  Session:   multiple {bin} sessions found for this repo:");
    for (i, (path, mtime)) in candidates.iter().take(show).enumerate() {
        let info = crate::session::SessionInfo {
            agent: bin.to_string(),
            session_id: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string(),
            workspace: None,
            path: path.clone(),
            modified: Some(*mtime),
            size_bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        };
        let title = crate::session::get_session_title(&info);
        let dt: chrono::DateTime<chrono::Local> = (*mtime).into();
        eprintln!(
            "    {}. [{}] {} ({})",
            i + 1,
            dt.format("%b %d %H:%M"),
            title,
            crate::session::format_size(info.size_bytes)
        );
    }
    eprintln!("    n. Start a new session");
    eprint!("  Choose [1-{show}, n] (default 1): ");
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return PiSessionPick::Session(candidates[0].0.clone());
    }
    let line = line.trim();
    if line.eq_ignore_ascii_case("n") {
        return PiSessionPick::Fresh;
    }
    if let Ok(n) = line.parse::<usize>() {
        if (1..=show).contains(&n) {
            return PiSessionPick::Session(candidates[n - 1].0.clone());
        }
    }
    PiSessionPick::Session(candidates[0].0.clone())
}

/// Build the shell command that resumes a pi-family session file.
///
/// omp cannot resume gzipped archives by path (`--resume <path>` requires a
/// plain-text header), but it resolves them fine by session ID — so omp
/// `.jsonl.gz` archives are resumed by ID instead.
pub(crate) fn pi_resume_command(
    install: &str,
    bin: &str,
    resume_flag: &str,
    cp: &std::path::Path,
) -> String {
    if bin == "omp" && cp.extension().and_then(|e| e.to_str()) == Some("gz") {
        let id = crate::session::session_id_from_filename(cp);
        format!("{install}; {bin} {resume_flag} '{id}'")
    } else {
        format!("{install}; {bin} {resume_flag} '{cp}'", cp = cp.display())
    }
}

/// Expand a stored pi-family session path into candidate container paths,
/// ordered by the implementation's config roots.
///
/// The stored path may be a container path (`/home/node/...`) or a host path
/// (`~/.pi/...` / `~/.omp/...`). For omp (`roots = [".omp", ".pi"]`) a stored
/// `.pi` session also yields its `.omp` twin first so post-migration sessions
/// win; for upstream pi only `.pi` is tried.
pub(crate) fn pi_session_path_candidates(stored: &str, roots: &[&str]) -> Vec<std::path::PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let stored_path = std::path::Path::new(stored);
    // Normalize to a path relative to the config home (as seen in the container).
    let rel: &std::path::Path = if let Ok(r) = stored_path.strip_prefix("/home/node/") {
        r
    } else if let Ok(r) = stored_path.strip_prefix(&home) {
        r
    } else {
        return vec![stored_path.to_path_buf()];
    };
    let first = rel.components().next().and_then(|c| match c {
        std::path::Component::Normal(s) => s.to_str().map(|s| s.to_string()),
        _ => None,
    });
    let container_home = std::path::Path::new("/home/node");
    match first.as_deref() {
        Some(".pi") | Some(".omp") => roots
            .iter()
            .map(|root| {
                container_home.join(root).join(
                    rel.components()
                        .skip(1)
                        .collect::<std::path::PathBuf>(),
                )
            })
            .collect(),
        _ => vec![container_home.join(rel)],
    }
}

#[cfg(test)]
mod tests {
    use super::{list_pi_sessions_for_cwd_with_home, pi_session_path_candidates};
    use super::mount_agent_config_dir;
    use std::path::PathBuf;

    #[test]
    fn sessions_symlink_gets_container_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let corpus = home.join(".nibble/sessions/omp");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::create_dir_all(home.join(".omp/agent")).unwrap();
        std::os::unix::fs::symlink(&corpus, home.join(".omp/agent/sessions")).unwrap();

        let mut volumes = Vec::new();
        mount_agent_config_dir(&PathBuf::from(home), ".omp", &mut volumes).unwrap();

        let overlay = format!("{}:/home/node/.omp/agent/sessions:rw", corpus.display());
        assert!(
            volumes.contains(&overlay),
            "symlinked sessions must be overlaid in the container, got {volumes:?}"
        );
    }

    #[test]
    fn plain_sessions_dir_gets_no_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".omp/agent/sessions")).unwrap();

        let mut volumes = Vec::new();
        mount_agent_config_dir(&PathBuf::from(home), ".omp", &mut volumes).unwrap();

        assert!(
            !volumes.iter().any(|v| v.ends_with("/agent/sessions:rw")),
            "real sessions dir needs no overlay, got {volumes:?}"
        );
    }

    #[test]
    fn candidates_pi_root_returns_pi_path() {
        let cands = pi_session_path_candidates(
            "/home/node/.pi/agent/sessions/--nibble--/s.jsonl",
            &[".pi"],
        );
        assert_eq!(
            cands,
            vec![std::path::PathBuf::from(
                "/home/node/.pi/agent/sessions/--nibble--/s.jsonl"
            )]
        );
    }

    #[test]
    fn candidates_omp_prefers_omp_twin_then_pi_original() {
        let cands = pi_session_path_candidates(
            "/home/node/.pi/agent/sessions/--nibble--/s.jsonl",
            &[".omp", ".pi"],
        );
        assert_eq!(
            cands,
            vec![
                std::path::PathBuf::from("/home/node/.omp/agent/sessions/--nibble--/s.jsonl"),
                std::path::PathBuf::from("/home/node/.pi/agent/sessions/--nibble--/s.jsonl"),
            ]
        );
    }

    #[test]
    fn candidates_omp_keeps_omp_path_first() {
        let cands = pi_session_path_candidates(
            "/home/node/.omp/agent/sessions/--nibble--/s.jsonl",
            &[".omp", ".pi"],
        );
        assert_eq!(
            cands[0],
            std::path::PathBuf::from("/home/node/.omp/agent/sessions/--nibble--/s.jsonl")
        );
    }

    #[test]
    fn candidates_host_path_normalizes_to_container() {
        let home = dirs::home_dir().unwrap();
        let stored = home.join(".pi/agent/sessions/--nibble--/s.jsonl");
        let cands = pi_session_path_candidates(stored.to_str().unwrap(), &[".omp", ".pi"]);
        assert_eq!(
            cands[0],
            std::path::PathBuf::from("/home/node/.omp/agent/sessions/--nibble--/s.jsonl")
        );
    }

    #[test]
    fn candidates_unrelated_path_passes_through() {
        let cands = pi_session_path_candidates("/opt/elsewhere/s.jsonl", &[".omp", ".pi"]);
        assert_eq!(cands, vec![std::path::PathBuf::from("/opt/elsewhere/s.jsonl")]);
    }

    #[test]
    fn list_pi_sessions_for_cwd_sorts_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let slug_dir = home.join(".pi/agent/sessions/--nibble--");
        std::fs::create_dir_all(&slug_dir).unwrap();
        let now = chrono::Utc::now().timestamp();
        for (name, age_secs) in [
            ("old.jsonl", 7200i64),
            ("new.jsonl", 60i64),
            ("mid.jsonl", 3600i64),
        ] {
            let p = slug_dir.join(name);
            std::fs::write(&p, "{}\n").unwrap();
            filetime::set_file_mtime(&p, filetime::FileTime::from_unix_time(now - age_secs, 0))
                .unwrap();
        }
        // Non-jsonl files are ignored.
        std::fs::write(slug_dir.join("notes.txt"), "x").unwrap();

        let sessions = list_pi_sessions_for_cwd_with_home(home, "/nibble", ".pi");
        let names: Vec<&str> = sessions
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, vec!["new.jsonl", "mid.jsonl", "old.jsonl"]);
    }

    #[test]
    fn list_pi_sessions_for_cwd_falls_back_to_legacy_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let legacy = home.join(".pi/agent/sessions/--workspace--");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("legacy.jsonl"), "{}\n").unwrap();

        let sessions = list_pi_sessions_for_cwd_with_home(home, "/nibble", ".pi");
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].0.ends_with("legacy.jsonl"));
    }
}
