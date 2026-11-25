//! Which coding agent is asking, and how to run the same one as a judge.

use super::run::{Env, Invocation};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Longest one judgement may take.
const JUDGE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// A coding agent nibble can run as a judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum AgentKind {
    Claude,
    Pi,
}

impl AgentKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Pi => "pi",
        }
    }
}

/// An agent with the model it is using.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Agent {
    pub kind: AgentKind,
    pub model: String,
    /// Reasoning level, in the agent's own words, when the caller has one set.
    pub effort: Option<String>,
    program: String,
}

impl Agent {
    /// The agent that called nibble, unless `kind` or `model` say otherwise.
    ///
    /// Each agent marks the commands it runs, and the innermost one wins: pi
    /// started from Claude Code overwrites `AI_AGENT` but leaves `CLAUDECODE`.
    pub(super) fn detect(
        env: &BTreeMap<String, String>,
        kind: Option<AgentKind>,
        model: Option<String>,
    ) -> Result<Agent> {
        let caller = calling_kind(env);
        let kind = kind.or(caller).ok_or_else(|| {
            anyhow!("can't tell which agent is calling; pass --agent claude or --agent pi, with --model")
        })?;
        // The caller's model and effort only mean something to the same agent.
        let same = caller == Some(kind);
        let model = match (model, kind) {
            (Some(model), _) => model,
            (None, _) if !same => bail!("pass --model with --agent {}", kind.as_str()),
            (None, AgentKind::Claude) => claude_model(env)?,
            (None, AgentKind::Pi) => pi_model(env)?,
        };
        let set = |name: &str| env.get(name).filter(|value| !value.is_empty()).cloned();
        let (effort, program) = match kind {
            AgentKind::Claude => (set("CLAUDE_EFFORT"), set("CLAUDE_CODE_EXECPATH")),
            AgentKind::Pi => (set("PI_REASONING_LEVEL"), None),
        };
        Ok(Agent {
            kind,
            model,
            effort: effort.filter(|_| same),
            program: program.unwrap_or_else(|| kind.as_str().to_string()),
        })
    }

    /// A headless run of this agent that can only read its prompt and answer.
    ///
    /// It gets no tools, no settings, no MCP servers, no project files, an
    /// empty directory, and no GitHub credentials, so nothing in the pull
    /// request can make it run or read anything.
    pub(super) fn judge(
        &self,
        env: &BTreeMap<String, String>,
        system: &str,
        schema: &str,
        prompt: String,
        dir: PathBuf,
    ) -> Invocation {
        let mut args: Vec<String> = match self.kind {
            AgentKind::Claude => [
                "-p",
                "--tools",
                "",
                "--setting-sources",
                "",
                "--strict-mcp-config",
                "--disable-slash-commands",
                "--safe-mode",
                "--no-session-persistence",
                "--output-format",
                "json",
                "--json-schema",
                schema,
            ]
            .map(String::from)
            .to_vec(),
            AgentKind::Pi => [
                "-p",
                "--no-tools",
                "--no-mcp",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-context-files",
                "--no-approve",
                "--no-session",
            ]
            .map(String::from)
            .to_vec(),
        };
        args.extend(["--system-prompt", system, "--model", &self.model].map(String::from));
        if let Some(effort) = &self.effort {
            let flag = match self.kind {
                AgentKind::Claude => "--effort",
                AgentKind::Pi => "--thinking",
            };
            args.extend([flag.to_string(), effort.clone()]);
        }
        Invocation {
            program: self.program.clone(),
            args,
            stdin: Some(prompt),
            env: Env::Only(judge_env(env, self.kind)),
            dir: Some(dir),
            timeout: JUDGE_TIMEOUT,
        }
    }

    /// The answer inside what the agent printed.
    pub(super) fn answer(&self, stdout: &str) -> Result<String> {
        match self.kind {
            AgentKind::Pi => Ok(stdout.to_string()),
            AgentKind::Claude => {
                let result: Value = serde_json::from_str(stdout.trim())
                    .context("claude did not print a JSON result")?;
                if result["is_error"].as_bool() != Some(false) {
                    bail!(
                        "claude reported an error: {}",
                        result["result"].as_str().unwrap_or("no details")
                    );
                }
                match &result["structured_output"] {
                    Value::Null => bail!("claude returned no structured answer"),
                    output => Ok(output.to_string()),
                }
            }
        }
    }
}

fn calling_kind(env: &BTreeMap<String, String>) -> Option<AgentKind> {
    match env.get("AI_AGENT").map(String::as_str) {
        Some("pi") => return Some(AgentKind::Pi),
        Some(name) if name.starts_with("claude") => return Some(AgentKind::Claude),
        // Another agent: its own variables may sit beside an outer agent's.
        Some(name) if !name.is_empty() => return None,
        _ => {}
    }
    if env.contains_key("PI_CODING_AGENT") {
        Some(AgentKind::Pi)
    } else if env.contains_key("CLAUDECODE") {
        Some(AgentKind::Claude)
    } else {
        None
    }
}

fn pi_model(env: &BTreeMap<String, String>) -> Result<String> {
    match (env.get("PI_PROVIDER"), env.get("PI_MODEL")) {
        (Some(provider), Some(model)) if !provider.is_empty() && !model.is_empty() => {
            Ok(format!("{provider}/{model}"))
        }
        _ => bail!("pi did not say which model it is using; pass --model provider/id"),
    }
}

/// The model of the calling Claude Code session, from its transcript.
fn claude_model(env: &BTreeMap<String, String>) -> Result<String> {
    let missing = || anyhow!("can't tell which model Claude Code is using; pass --model");
    let session = env
        .get("CLAUDE_CODE_SESSION_ID")
        .filter(|id| id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !id.is_empty())
        .ok_or_else(missing)?;
    let config = match (env.get("CLAUDE_CONFIG_DIR"), env.get("HOME")) {
        (Some(dir), _) if !dir.is_empty() => PathBuf::from(dir),
        (_, Some(home)) => Path::new(home).join(".claude"),
        _ => return Err(missing()),
    };
    let projects = fs::read_dir(config.join("projects")).map_err(|_| missing())?;
    projects
        .filter_map(|entry| Some(entry.ok()?.path().join(format!("{session}.jsonl"))))
        .find(|path| path.is_file())
        .and_then(|path| last_model(&path))
        .ok_or_else(missing)
}

/// The model of the last real reply in a transcript.
fn last_model(path: &Path) -> Option<String> {
    BufReader::new(fs::File::open(path).ok()?)
        .lines()
        .map_while(Result::ok)
        .filter(|line| line.contains("\"assistant\""))
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter(|entry| entry["type"] == "assistant")
        .filter_map(|entry| entry["message"]["model"].as_str().map(String::from))
        // Claude Code writes placeholder replies of its own, such as after an API error.
        .filter(|model| model != "<synthetic>")
        .last()
}

/// The variables an agent needs to reach its model, and nothing else.
fn judge_env(env: &BTreeMap<String, String>, kind: AgentKind) -> BTreeMap<String, String> {
    const SHARED: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
    ];
    let wanted = |name: &str| {
        SHARED.contains(&name)
            || match kind {
                AgentKind::Claude => {
                    name.starts_with("ANTHROPIC_")
                        || name.starts_with("CLAUDE_CODE_USE_")
                        || name == "CLAUDE_CONFIG_DIR"
                        || name == "CLAUDE_CODE_OAUTH_TOKEN"
                }
                // pi looks for the key of whichever provider the model belongs to.
                AgentKind::Pi => name.ends_with("_API_KEY") || name == "PI_CODING_AGENT_DIR",
            }
    };
    env.iter()
        .filter(|(name, _)| wanted(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}
