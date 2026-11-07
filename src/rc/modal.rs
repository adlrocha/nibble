//! The "new session" modal: where the session runs (host or sandbox), which
//! agent, which repo or directory, and the first prompt.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use super::store::{Location, SpawnAgent};

/// `callback_id` of the modal.
pub(crate) const CALLBACK_ID: &str = "rc_new_session";
/// Slack's cap on a static select's options.
const MAX_OPTIONS: usize = 100;
/// Slack caps option text at 75 characters and values at 150.
const OPTION_TEXT_MAX: usize = 75;
const OPTION_VALUE_MAX: usize = 150;

/// Where the session's thread will be: an existing DM thread (started from
/// a message) or a new root message in `channel`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Origin {
    pub channel: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
}

/// A validated new-session request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NewSession {
    pub origin: Origin,
    pub location: Location,
    pub agent: SpawnAgent,
    pub cwd: PathBuf,
    pub prompt: String,
}

fn plain(text: &str) -> Value {
    json!({ "type": "plain_text", "text": text })
}

fn option(text: &str, value: &str) -> Value {
    let text: String = text.chars().take(OPTION_TEXT_MAX).collect();
    json!({ "text": plain(&text), "value": value })
}

/// The modal view. `repos` become the repo picker's options.
pub(crate) fn view(origin: &Origin, repos: &[PathBuf], prompt: &str, home: Option<&Path>) -> Value {
    let repo_options: Vec<Value> = repos
        .iter()
        .filter_map(|p| p.to_str().map(|s| (p, s)))
        .filter(|(_, s)| s.len() <= OPTION_VALUE_MAX)
        .take(MAX_OPTIONS)
        .map(|(p, s)| option(&display_path(p, home), s))
        .collect();
    let mut blocks = vec![
        json!({
            "type": "input", "block_id": "location", "label": plain("Run on"),
            "element": {
                "type": "radio_buttons", "action_id": "value",
                "initial_option": option("Host", "host"),
                "options": [option("Host", "host"), option("Sandbox", "sandbox")]
            }
        }),
        json!({
            "type": "input", "block_id": "agent", "label": plain("Agent"),
            "element": {
                "type": "static_select", "action_id": "value",
                "initial_option": option("Claude Code", "claude"),
                "options": [option("Claude Code", "claude"), option("pi", "pi")]
            }
        }),
    ];
    if !repo_options.is_empty() {
        blocks.push(json!({
            "type": "input", "block_id": "repo", "optional": true, "label": plain("Repo"),
            "element": {
                "type": "static_select", "action_id": "value",
                "placeholder": plain("Pick a repo"), "options": repo_options
            }
        }));
    }
    blocks.push(json!({
        "type": "input", "block_id": "path", "optional": true, "label": plain("Or a directory"),
        "hint": plain("Used instead of the repo picker when set. ~/ is your home."),
        "element": { "type": "plain_text_input", "action_id": "value", "placeholder": plain("~/workspace/project") }
    }));
    let mut prompt_el = json!({
        "type": "plain_text_input", "action_id": "value", "multiline": true,
        "placeholder": plain("What should the agent do?")
    });
    if !prompt.trim().is_empty() {
        prompt_el["initial_value"] = json!(prompt.trim());
    }
    blocks.push(json!({
        "type": "input", "block_id": "prompt", "label": plain("First message"), "element": prompt_el
    }));
    json!({
        "type": "modal",
        "callback_id": CALLBACK_ID,
        "private_metadata": serde_json::to_string(origin).unwrap_or_default(),
        "title": plain("New nibble session"),
        "submit": plain("Start"),
        "close": plain("Cancel"),
        "blocks": blocks
    })
}

fn display_path(p: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| p.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

/// Expand a leading `~/` (or a bare `~`).
pub(crate) fn expand_home(raw: &str, home: Option<&Path>) -> PathBuf {
    let raw = raw.trim();
    match (raw.strip_prefix("~/"), raw == "~", home) {
        (Some(rest), _, Some(h)) => h.join(rest),
        (None, true, Some(h)) => h.to_path_buf(),
        _ => PathBuf::from(raw),
    }
}

fn state_value<'a>(values: &'a Value, block: &str) -> Option<&'a Value> {
    values.get(block)?.get("value")
}

fn selected(values: &Value, block: &str) -> Option<String> {
    state_value(values, block)?["selected_option"]["value"]
        .as_str()
        .map(str::to_string)
}

fn typed(values: &Value, block: &str) -> Option<String> {
    state_value(values, block)?["value"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Validate a `view_submission` payload. `Err` carries Slack's per-block
/// error messages, shown in the modal.
pub(crate) fn parse_submission(
    payload: &Value,
    home: Option<&Path>,
    is_dir: &dyn Fn(&Path) -> bool,
) -> std::result::Result<NewSession, BTreeMap<String, String>> {
    let view = &payload["view"];
    let values = &view["state"]["values"];
    let mut errors = BTreeMap::new();

    let origin: Option<Origin> = view["private_metadata"]
        .as_str()
        .and_then(|m| serde_json::from_str(m).ok());
    let location = match selected(values, "location").as_deref() {
        Some("sandbox") => Location::Sandbox,
        _ => Location::Host,
    };
    let agent = match selected(values, "agent").as_deref() {
        Some("pi") => SpawnAgent::Pi,
        _ => SpawnAgent::Claude,
    };
    let dir = typed(values, "path")
        .map(|p| expand_home(&p, home))
        .or_else(|| selected(values, "repo").map(PathBuf::from));
    let cwd = match dir {
        None => {
            errors.insert(
                "path".to_string(),
                "Pick a repo or type a directory.".to_string(),
            );
            None
        }
        Some(d) if !d.is_absolute() => {
            errors.insert(
                "path".to_string(),
                "Use an absolute path or ~/…".to_string(),
            );
            None
        }
        Some(d) if !is_dir(&d) => {
            errors.insert(
                "path".to_string(),
                format!("{} is not a directory.", d.display()),
            );
            None
        }
        Some(d) => Some(d),
    };
    let prompt = typed(values, "prompt");
    if prompt.is_none() {
        errors.insert(
            "prompt".to_string(),
            "Say what the agent should do.".to_string(),
        );
    }
    match (origin, cwd, prompt) {
        (Some(origin), Some(cwd), Some(prompt)) if errors.is_empty() => Ok(NewSession {
            origin,
            location,
            agent,
            cwd,
            prompt,
        }),
        (None, _, _) => {
            errors.insert(
                "prompt".to_string(),
                "This form expired; open it again.".to_string(),
            );
            Err(errors)
        }
        _ => Err(errors),
    }
}

/// The Socket Mode ack payload that shows `errors` in the modal.
pub(crate) fn errors_response(errors: &BTreeMap<String, String>) -> Value {
    json!({ "response_action": "errors", "errors": errors })
}

const SKIP_DIRS: &[&str] = &["node_modules", "target", "vendor", "dist", "build"];

/// Git repos under `roots` (up to `depth` levels down), not descending into
/// repos or hidden/build directories.
pub(crate) fn find_repos(roots: &[PathBuf], depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        let mut walker = walkdir::WalkDir::new(root)
            .min_depth(1)
            .max_depth(depth)
            .sort_by_file_name()
            .into_iter();
        while let Some(entry) = walker.next() {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                walker.skip_current_dir();
                continue;
            }
            if entry.path().join(".git").exists() {
                out.push(entry.path().to_path_buf());
                walker.skip_current_dir();
            }
        }
    }
    out
}

/// Recently used directories first (most recent first), then discovered
/// repos; deduplicated.
pub(crate) fn order_repos(recent: Vec<PathBuf>, found: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    recent
        .into_iter()
        .chain(found)
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// The repo list for the modal: directories of recent nibble tasks, then
/// repos found under the configured roots.
pub(crate) fn repo_choices(
    db: &crate::db::Database,
    cfg: &crate::config::RcConfig,
) -> Result<Vec<PathBuf>> {
    let home = dirs::home_dir();
    let mut tasks = db.list_tasks().context("failed to list tasks")?;
    tasks.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
    let recent: Vec<PathBuf> = tasks
        .iter()
        .filter_map(|t| t.context.as_ref()?.project_path.clone())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .collect();
    let roots: Vec<PathBuf> = cfg
        .repo_roots
        .iter()
        .map(|r| expand_home(r, home.as_deref()))
        .collect();
    Ok(order_repos(recent, find_repos(&roots, cfg.repo_depth)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submission(values: Value, meta: &str) -> Value {
        json!({ "view": { "private_metadata": meta, "state": { "values": values } } })
    }

    fn meta() -> String {
        serde_json::to_string(&Origin {
            channel: "D1".into(),
            thread_ts: Some("1.2".into()),
        })
        .unwrap()
    }

    #[test]
    fn view_lists_repos_and_prefills_the_prompt() {
        let home = PathBuf::from("/home/u");
        let v = view(
            &Origin {
                channel: "D1".into(),
                thread_ts: None,
            },
            &[PathBuf::from("/home/u/workspace/nibble")],
            "fix ci",
            Some(&home),
        );
        let s = v.to_string();
        assert!(s.contains("~/workspace/nibble"));
        assert!(s.contains("\"initial_value\":\"fix ci\""));
        assert_eq!(v["callback_id"], CALLBACK_ID);
        let origin: Origin = serde_json::from_str(v["private_metadata"].as_str().unwrap()).unwrap();
        assert_eq!(origin.channel, "D1");
    }

    #[test]
    fn the_form_offers_only_agents_mux_can_restore() {
        let v = view(
            &Origin {
                channel: "D1".into(),
                thread_ts: None,
            },
            &[],
            "",
            None,
        )
        .to_string();
        assert!(v.contains("\"value\":\"claude\"") && v.contains("\"value\":\"pi\""));
        assert!(!v.contains("\"value\":\"omp\""));
    }

    #[test]
    fn view_without_repos_has_no_empty_select() {
        let v = view(
            &Origin {
                channel: "D1".into(),
                thread_ts: None,
            },
            &[],
            "",
            None,
        );
        assert!(!v.to_string().contains("\"block_id\":\"repo\""));
    }

    #[test]
    fn typed_directory_wins_over_the_picker() {
        let values = json!({
            "location": { "value": { "selected_option": { "value": "sandbox" } } },
            "agent": { "value": { "selected_option": { "value": "pi" } } },
            "repo": { "value": { "selected_option": { "value": "/repo/a" } } },
            "path": { "value": { "value": "~/b" } },
            "prompt": { "value": { "value": " do it " } }
        });
        let home = PathBuf::from("/home/u");
        let s = parse_submission(&submission(values, &meta()), Some(&home), &|_| true).unwrap();
        assert_eq!(s.location, Location::Sandbox);
        assert_eq!(s.agent, SpawnAgent::Pi);
        assert_eq!(s.cwd, PathBuf::from("/home/u/b"));
        assert_eq!(s.prompt, "do it");
        assert_eq!(s.origin.thread_ts.as_deref(), Some("1.2"));
    }

    #[test]
    fn picker_and_defaults() {
        let values = json!({
            "repo": { "value": { "selected_option": { "value": "/repo/a" } } },
            "prompt": { "value": { "value": "go" } }
        });
        let s = parse_submission(&submission(values, &meta()), None, &|_| true).unwrap();
        assert_eq!((s.location, s.agent), (Location::Host, SpawnAgent::Claude));
        assert_eq!(s.cwd, PathBuf::from("/repo/a"));
    }

    #[test]
    fn invalid_submissions_report_per_block_errors() {
        let values = json!({ "path": { "value": { "value": "relative/dir" } } });
        let errs = parse_submission(&submission(values, &meta()), None, &|_| true).unwrap_err();
        assert!(errs["path"].contains("absolute"));
        assert!(errs.contains_key("prompt"));

        let values = json!({ "path": { "value": { "value": "/nope" } }, "prompt": { "value": { "value": "x" } } });
        let errs = parse_submission(&submission(values, &meta()), None, &|_| false).unwrap_err();
        assert!(errs["path"].contains("not a directory"));

        let values = json!({ "prompt": { "value": { "value": "x" } } });
        let errs = parse_submission(&submission(values, &meta()), None, &|_| true).unwrap_err();
        assert!(errs["path"].contains("Pick a repo"));

        let values = json!({ "path": { "value": { "value": "/x" } }, "prompt": { "value": { "value": "x" } } });
        let errs = parse_submission(&submission(values, "garbage"), None, &|_| true).unwrap_err();
        assert!(errs["prompt"].contains("expired"));
    }

    #[test]
    fn home_expansion() {
        let h = PathBuf::from("/h");
        assert_eq!(expand_home("~/x", Some(&h)), PathBuf::from("/h/x"));
        assert_eq!(expand_home("~", Some(&h)), PathBuf::from("/h"));
        assert_eq!(expand_home("/abs", Some(&h)), PathBuf::from("/abs"));
    }

    #[test]
    fn finds_repos_without_descending_into_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for p in [
            "a/.git",
            "a/inner/.git",
            "group/b/.git",
            "node_modules/c/.git",
            ".hidden/d/.git",
            "plain",
        ] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        let found = find_repos(&[root.to_path_buf()], 3);
        assert_eq!(found, vec![root.join("a"), root.join("group/b")]);
    }

    #[test]
    fn recent_dirs_come_first_without_duplicates() {
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        let c = PathBuf::from("/c");
        assert_eq!(
            order_repos(
                vec![b.clone(), b.clone()],
                vec![a.clone(), b.clone(), c.clone()]
            ),
            vec![b, a, c]
        );
    }
}
