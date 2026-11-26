//! The judge's prompt, its answer, and the fixed thresholds its answer is held to.

use super::bundle::{Bundle, Profile};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// What every judge is told, whichever agent runs it.
pub(super) const SYSTEM_PROMPT: &str = include_str!("judge.md");

/// Past this much evidence a judge would read only part of it, so a person decides.
pub(super) const MAX_PROMPT_CHARS: usize = 400_000;

/// JSON schema of the answer, for agents that can enforce one.
pub(super) const SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["decision","impact","reversible","likelihood","summary","reasons","question","fixes"],"properties":{"decision":{"type":"string","enum":["merge","fix","ask"]},"impact":{"type":"string","enum":["low","medium","high"]},"reversible":{"type":"boolean"},"likelihood":{"type":"string","enum":["low","medium","high"]},"summary":{"type":"string"},"reasons":{"type":"array","items":{"type":"string"}},"question":{"type":"string"},"fixes":{"type":"array","items":{"type":"string"}}}}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Choice {
    Merge,
    Fix,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Level {
    Low,
    Medium,
    High,
}

impl Level {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Level::Low => "low",
            Level::Medium => "medium",
            Level::High => "high",
        }
    }
}

/// A judge's answer, exactly as it must be written.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Verdict {
    pub decision: Choice,
    pub impact: Level,
    pub reversible: bool,
    pub likelihood: Level,
    pub summary: String,
    // Models leave out what doesn't apply to their decision. A decision that
    // needs one of these is still refused without it.
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub fixes: Vec<String>,
}

impl Verdict {
    /// Reads an answer. One that can't be read is an error, unless it clearly
    /// holds the change back: a hold stays a hold however badly it is written.
    pub(super) fn parse(text: &str) -> Result<Verdict> {
        let text = unfenced(text);
        let problem = match serde_json::from_str::<Verdict>(text) {
            Ok(verdict) if verdict.is_complete() => return Ok(verdict),
            Ok(_) => None,
            Err(error) => Some(error),
        };
        if let Some(held) = Verdict::badly_written_hold(text) {
            return Ok(held);
        }
        match problem {
            Some(error) => {
                Err(error).context("the judge's answer is not the JSON object it was asked for")
            }
            None => bail!("the judge's answer is incomplete"),
        }
    }

    /// Whether a decision carries what makes it usable.
    fn is_complete(&self) -> bool {
        match self.decision {
            Choice::Merge => true,
            Choice::Ask => !self.question.trim().is_empty(),
            Choice::Fix => self.fixes.iter().any(|fix| !fix.trim().is_empty()),
        }
    }

    fn badly_written_hold(text: &str) -> Option<Verdict> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let decision = value["decision"].as_str()?;
        if decision != "ask" && decision != "fix" {
            return None;
        }
        let summary = value["summary"].as_str().unwrap_or("").trim().to_string();
        Some(Verdict {
            decision: Choice::Ask,
            impact: Level::High,
            reversible: false,
            likelihood: Level::High,
            question: format!(
                "The judge held this back without saying clearly why. Should it merge? {summary}"
            )
            .trim()
            .to_string(),
            summary,
            reasons: Vec::new(),
            fixes: Vec::new(),
        })
    }

    /// Turns a merge into an ask when the risk is above what the profile allows.
    ///
    /// The judge rates the risk and the profile decides what is acceptable, so
    /// a judge that talks itself into merging is still held to the profile.
    pub(super) fn held_to(mut self, profile: Profile) -> Verdict {
        if self.decision == Choice::Merge && above_appetite(profile, &self) {
            self.decision = Choice::Ask;
            self.question = format!(
                "Merge this? {} (impact {}, {}, likelihood of a problem {}; above what the {} profile merges unattended)",
                self.summary.trim(),
                self.impact.as_str(),
                if self.reversible { "reversible" } else { "not reversible" },
                self.likelihood.as_str(),
                profile.as_str(),
            );
        }
        self
    }
}

/// Drops a Markdown code fence around the whole answer, which some models add.
fn unfenced(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map_or(text, str::trim)
}

fn above_appetite(profile: Profile, verdict: &Verdict) -> bool {
    let recoverable = verdict.reversible && verdict.likelihood == Level::Low;
    match (profile, verdict.impact) {
        (_, Level::Low) => verdict.likelihood == Level::High && profile == Profile::Critical,
        (Profile::SideProject, Level::Medium) => false,
        (Profile::Standard, Level::Medium) => !verdict.reversible,
        (Profile::Critical, Level::Medium) => !recoverable,
        (Profile::SideProject, Level::High) => !verdict.reversible,
        (Profile::Standard, Level::High) => !recoverable,
        (Profile::Critical, Level::High) => true,
    }
}

/// Asks again after an answer that couldn't be read. It doesn't quote the
/// answer, which evidence can influence.
pub(super) fn retry(prompt: &str) -> String {
    format!(
        "{prompt}\nYour previous answer could not be read. Reply with the complete JSON object only.\n"
    )
}

/// Builds the judge's input, or `None` when there is too much of it to judge.
///
/// `nonce` fences each section, so text inside one can't pretend to end it.
pub(super) fn prompt(bundle: &Bundle, history: &[String], nonce: &str) -> Result<Option<String>> {
    let Some(diff) = bundle.diff.as_deref() else {
        return Ok(None);
    };
    let pr = &bundle.pull_request;
    let mut out = String::new();
    let mut section = |name: &str, body: &str| {
        let body = if body.trim().is_empty() {
            "(none)"
        } else {
            body
        };
        // Writing to a String can't fail.
        let _ = write!(out, "<{name} {nonce}>\n{body}\n</{name} {nonce}>\n\n");
    };

    section(
        "POLICY",
        &serde_json::to_string_pretty(&bundle.policy).context("failed to render the policy")?,
    );
    section("RISK MAP", bundle.risk_map.as_deref().unwrap_or(""));
    section("HISTORY", &bullets(history));
    section(
        "PULL REQUEST",
        &format!(
            "{} #{} by {} into {}\n\n{}\n\n{}",
            pr.repository, pr.number, pr.author, pr.base, pr.title, pr.body
        ),
    );
    for issue in &bundle.issues {
        section(
            "LINKED ISSUE",
            &format!("#{} {}\n\n{}", issue.number, issue.title, issue.body),
        );
    }
    let files: Vec<String> = bundle
        .files
        .iter()
        .map(|file| {
            format!(
                "{} {} +{} -{}",
                file.status, file.filename, file.additions, file.deletions
            )
        })
        .collect();
    section("CHANGED FILES", &files.join("\n"));
    section("DIFF", diff);
    section(
        "SIGNALS FROM FIXED RULES",
        &bullets(&bundle.decision.signals),
    );
    for review in &bundle.reviews {
        section(
            "REVIEW",
            &format!(
                "{} {} {}\n\n{}",
                review.author, review.state, review.at, review.body
            ),
        );
    }
    for thread in &bundle.threads {
        let comments: Vec<String> = thread
            .comments
            .iter()
            .map(|comment| format!("{}: {}", comment.author, comment.body))
            .collect();
        section(
            "REVIEW THREAD",
            &format!(
                "{} | {} | {}\n\n{}",
                thread.path.as_deref().unwrap_or("(no file)"),
                if thread.resolved { "resolved" } else { "open" },
                if thread.code_changed {
                    "the code changed afterwards"
                } else {
                    "the code did not change afterwards"
                },
                comments.join("\n\n")
            ),
        );
    }
    for comment in &bundle.comments {
        section(
            "COMMENT",
            &format!(
                "{} ({}) {}\n\n{}",
                comment.author, comment.association, comment.at, comment.body
            ),
        );
    }
    out.push_str("Judge this pull request. Reply with the JSON object only.\n");
    // An agent may read a file named as `@path`, so no `@` stands before a name.
    let out = out.replace('@', "@\u{200b}");
    Ok((out.chars().count() <= MAX_PROMPT_CHARS).then_some(out))
}

fn bullets(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
