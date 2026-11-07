//! Which transcript file belongs to a nibble task. Shared by quota-watch
//! (scanning for quota errors) and `nibble rc` (posting replies).

use std::path::{Path, PathBuf};

use crate::models::{AgentType, Task};

/// Resolve the host-side transcript file for a task, if one is linked and
/// exists. pi/omp archive cold sessions as `.jsonl.gz` — those are not live
/// and are skipped.
pub(crate) fn session_file_for(task: &Task) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let ctx = task.context.as_ref()?;
    match &task.agent_type {
        AgentType::ClaudeCode => {
            // Legacy rows may only carry the generic session_id.
            let sid = ctx
                .claude_session_id
                .as_deref()
                .or(ctx.session_id.as_deref())?;
            if sid.starts_with("ses_") {
                return None; // legacy non-claude id
            }
            claude_session_file(sid, &home)
        }
        AgentType::Pi | AgentType::Omp => {
            let stored = ctx.extra.get("pi_session_path")?.as_str()?;
            pi_session_file(stored, &home)
        }
        _ => None,
    }
}

/// Find `~/.claude/projects/<any-slug>/<sid>.jsonl`. Session UUIDs are unique,
/// so a single walk over the project dirs is unambiguous.
fn claude_session_file(sid: &str, home: &Path) -> Option<PathBuf> {
    let projects = home.join(".claude").join("projects");
    let wanted = format!("{sid}.jsonl");
    for entry in std::fs::read_dir(&projects).ok()? {
        let dir = entry.ok()?.path();
        if !dir.is_dir() {
            continue;
        }
        let candidate = dir.join(&wanted);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Map a stored pi/omp session path to the host-side file. Sandbox sessions
/// store the container path (`/home/node/...`); the same directory is
/// bind-mounted from the host home, so rewriting the prefix recovers it.
fn pi_session_file(stored: &str, home: &Path) -> Option<PathBuf> {
    let home_str = home.to_string_lossy();
    let candidates: Vec<PathBuf> = if let Some(rest) = stored.strip_prefix("/home/node/") {
        vec![PathBuf::from(&*home_str).join(rest), PathBuf::from(stored)]
    } else if let Some(rest) = stored.strip_prefix("~/") {
        vec![PathBuf::from(&*home_str).join(rest), PathBuf::from(stored)]
    } else {
        vec![PathBuf::from(stored)]
    };
    candidates
        .into_iter()
        .find(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) != Some("gz"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_container_pi_path_to_host() {
        let dir = std::env::temp_dir().join("qw-test-pathmap");
        let host_file =
            dir.join(".omp/agent/sessions/--repo--/2026-09-18T11-09-41-646Z_019x.jsonl");
        std::fs::create_dir_all(host_file.parent().unwrap()).unwrap();
        std::fs::write(&host_file, b"{}").unwrap();
        let p = pi_session_file(
            "/home/node/.omp/agent/sessions/--repo--/2026-09-18T11-09-41-646Z_019x.jsonl",
            &dir,
        );
        assert_eq!(p, Some(host_file));
    }

    #[test]
    fn skips_archived_pi_sessions() {
        let dir = std::env::temp_dir().join("qw-test-gz");
        let _ = std::fs::create_dir_all(&dir);
        let gz = dir.join("s.jsonl.gz");
        std::fs::write(&gz, b"x").unwrap();
        assert!(pi_session_file(gz.to_str().unwrap(), Path::new("/home/x")).is_none());
    }
}
