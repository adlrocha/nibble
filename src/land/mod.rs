//! `nibble land`: decide whether a finished pull request merges without its
//! owner, and merge it or say who has to act.
//!
//! Fixed rules come first. They are nibble-review's, read from GitHub and run
//! at the commit the repository's default branch pins, so CI's `## Risk`
//! report and this decision share one implementation. Then a judge, a fresh
//! headless run of the same agent that is asking, reads the change. It can
//! hold a change back but never clear a rule. Every failure leaves the pull
//! request unmerged.

mod agent;
mod bundle;
mod github;
mod history;
mod holds;
mod judge;
mod record;
mod run;

pub(crate) use agent::AgentKind;
pub(crate) use record::Flag;

use crate::cli::LandAction;
use agent::Agent;
use anyhow::{bail, Context, Result};
use bundle::{Bundle, Mode, Outcome};
use chrono::DateTime;
use github::GitHub;
use holds::Holds;
use judge::{Choice, Verdict};
use record::{Decided, Record};
use run::RealRunner;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// Exit codes the calling agent can act on.
const DONE: i32 = 0;
const CALL_HUMAN: i32 = 10;
const KEEP_WORKING: i32 = 11;

/// Posting a decision is worth a few tries: an unposted one is easy to miss.
const POST_TRIES: u32 = 3;

/// GitHub computes mergeability after it is first asked for.
const MERGEABLE_TRIES: u32 = 5;
const MERGEABLE_WAIT: Duration = Duration::from_secs(4);
const STILL_COMPUTING: &str = "still computing";

/// Entry point for `nibble land`; returns the process exit code.
pub(crate) fn cmd_land(action: LandAction) -> Result<i32> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let cache = dirs::cache_dir()
        .context("no cache directory on this system")?
        .join("nibble/land");
    let runner = RealRunner;
    let land = Land {
        holds: Holds {
            dir: cache.join("holds"),
        },
        github: GitHub {
            runner: &runner,
            cache: cache.join("rules"),
            env: &env,
        },
        env: &env,
        wait: MERGEABLE_WAIT,
    };
    land.run(action, &mut std::io::stdout())
}

struct Land<'a> {
    github: GitHub<'a>,
    holds: Holds,
    env: &'a BTreeMap<String, String>,
    wait: Duration,
}

/// What a backtest tries instead of the repository's own files.
struct Trial<'a> {
    policy: Option<&'a Path>,
    risk_map: Option<&'a str>,
}

/// What a decision came to, with everything needed to explain it.
enum Decision {
    /// The agent has work left. Nobody is asked.
    NotReady(Vec<String>),
    /// A fixed rule needs a person.
    Rule(Vec<String>),
    /// A judge answered.
    Judged(Verdict),
    /// Already decided for this exact commit, so it isn't asked again.
    Standing(Decided),
}

impl Land<'_> {
    fn run(&self, action: LandAction, out: &mut dyn Write) -> Result<i32> {
        match action {
            LandAction::Pr {
                number,
                repo,
                dry_run,
                agent,
                model,
            } => {
                let repo = self.github.repository(repo)?;
                let agent = Agent::detect(self.env, agent, model)?;
                self.land(&repo, number, &agent, dry_run, out)
            }
            LandAction::Flag {
                number,
                flag,
                why,
                repo,
            } => {
                let repo = self.github.repository(repo)?;
                let body = record::flag_comment(flag, &why);
                self.github.comment(&repo, number, &body)?;
                writeln!(out, "Flagged {repo}#{number} as {}.", flag.as_str())?;
                Ok(DONE)
            }
            LandAction::Backtest {
                numbers,
                repo,
                policy,
                risk_map,
                agent,
                model,
            } => {
                let repo = self.github.repository(repo)?;
                let agent = Agent::detect(self.env, agent, model)?;
                let risk_map = risk_map
                    .map(|path| {
                        fs::read_to_string(&path)
                            .with_context(|| format!("failed to read {}", path.display()))
                    })
                    .transpose()?;
                let trial = Trial {
                    policy: policy.as_deref(),
                    risk_map: risk_map.as_deref(),
                };
                self.backtest(&repo, &numbers, &trial, &agent, out)
            }
            LandAction::Queue => {
                let waiting = self.github.waiting()?;
                if waiting.is_empty() {
                    writeln!(out, "Nothing is waiting for you.")?;
                }
                for pr in waiting {
                    writeln!(out, "{}\n  {}", pr.title, pr.url)?;
                }
                Ok(DONE)
            }
            LandAction::Digest { repo, since } => {
                let repo = self.github.repository(repo)?;
                self.digest(&repo, since, out)
            }
        }
    }

    /// Decides one pull request and acts on it.
    fn land(
        &self,
        repo: &str,
        number: u64,
        agent: &Agent,
        dry_run: bool,
        out: &mut dyn Write,
    ) -> Result<i32> {
        match self.github.state(repo, number)?.as_str() {
            "OPEN" => {}
            "MERGED" => {
                writeln!(out, "MERGED: {repo}#{number} is already merged.")?;
                return Ok(DONE);
            }
            _ => {
                writeln!(
                    out,
                    "CALL THE HUMAN: {repo}#{number} was closed without merging. Don't reopen it; tell the user and stop."
                )?;
                return Ok(CALL_HUMAN);
            }
        }
        let Some(bundle) = self.settled_bundle(repo, number)? else {
            writeln!(out, "Land is off in {repo}. Leave the merge to the user.")?;
            return Ok(CALL_HUMAN);
        };
        let decision = self.decide(&bundle, agent)?;
        let sha = bundle.pull_request.head_sha.as_str();
        match &decision {
            Decision::NotReady(blockers) => {
                writeln!(
                    out,
                    "NOT READY: {repo}#{number} at {sha}. Finish this, then run `nibble land pr {number}` again:\n{}",
                    bullets(blockers)
                )?;
                Ok(KEEP_WORKING)
            }
            Decision::Rule(asks) => {
                writeln!(
                    out,
                    "CALL THE HUMAN: {repo}#{number} at {sha} needs its owner. Tell them this and stop:\n{}",
                    bullets(asks)
                )?;
                // A rule asks every time it is run, and saying so once is enough.
                let said = bundle.comments.iter().any(|comment| {
                    record::trusted(&comment.association)
                        && record::parse(&comment.body).is_some_and(|record| {
                            record.sha == sha && !record.judged && record.decided == Decided::Ask
                        })
                });
                if !dry_run && !said {
                    let report = Report {
                        bundle: &bundle,
                        agent,
                        verdict: None,
                        decided: Decided::Ask,
                        merged: false,
                        text: format!("A fixed rule needs you:\n\n{}", bullets(asks)),
                    };
                    self.publish(&report, out)?;
                }
                Ok(CALL_HUMAN)
            }
            Decision::Standing(Decided::Fix) => {
                writeln!(
                    out,
                    "FIX FIRST: the judge already asked for fixes on {sha}. Push them and run this again. If you believe it is wrong, ask the user with both sides and stop."
                )?;
                Ok(KEEP_WORKING)
            }
            Decision::Standing(_) => {
                writeln!(
                    out,
                    "CALL THE HUMAN: {sha} was already held for its owner, see the comments on {repo}#{number}. Only they can merge this commit."
                )?;
                if !dry_run {
                    self.label(repo, number, true, out)?;
                }
                Ok(CALL_HUMAN)
            }
            Decision::Judged(verdict) => self.act(&bundle, verdict, agent, dry_run, out),
        }
    }

    /// Carries out a judge's answer.
    fn act(
        &self,
        bundle: &Bundle,
        verdict: &Verdict,
        agent: &Agent,
        dry_run: bool,
        out: &mut dyn Write,
    ) -> Result<i32> {
        let pr = &bundle.pull_request;
        let (repo, number, sha) = (pr.repository.as_str(), pr.number, pr.head_sha.as_str());
        let report = |decided, merged| Report {
            bundle,
            agent,
            verdict: Some(verdict),
            decided,
            merged,
            text: explain(verdict),
        };
        let (decided, code) = match verdict.decision {
            Choice::Fix => (Decided::Fix, KEEP_WORKING),
            Choice::Ask => (Decided::Ask, CALL_HUMAN),
            Choice::Merge if dry_run || bundle.mode != Mode::Enforce => {
                writeln!(
                    out,
                    "WOULD MERGE: {repo}#{number} at {sha}. Nothing was merged ({}). Tell the user.",
                    if dry_run { "dry run" } else { "shadow mode" }
                )?;
                if !dry_run {
                    self.publish(&report(Decided::Merge, false), out)?;
                }
                return Ok(CALL_HUMAN);
            }
            Choice::Merge => {
                self.confirm(bundle)?;
                self.github.merge(repo, number, sha)?;
                writeln!(out, "MERGED: {repo}#{number} at {sha}.")?;
                self.publish(&report(Decided::Merge, true), out)?;
                return Ok(DONE);
            }
        };
        // Remembered first, and on a dry run too, so nothing that fails from
        // here on gives this commit another chance.
        self.holds.put(repo, sha, decided)?;
        if decided == Decided::Fix {
            writeln!(
                out,
                "FIX FIRST: {repo}#{number} at {sha}. Fix this, get a new LGTM, then run this again. If you believe it is wrong, ask the user with both sides and stop.\n{}",
                bullets(&verdict.fixes)
            )?;
        } else {
            writeln!(
                out,
                "CALL THE HUMAN: {repo}#{number} at {sha}. Ask them this and stop:\n{}",
                verdict.question.trim()
            )?;
        }
        if !dry_run {
            self.publish(&report(decided, false), out)?;
        }
        Ok(code)
    }

    /// Posts a decision and sets the label. The decision already stands, so a
    /// failure here is reported and changes nothing.
    fn publish(&self, report: &Report, out: &mut dyn Write) -> Result<()> {
        let pr = &report.bundle.pull_request;
        let decided = report.decided;
        let body = report.comment()?;
        let mut attempt = self.github.comment(&pr.repository, pr.number, &body);
        for _ in 1..POST_TRIES {
            if attempt.is_ok() {
                break;
            }
            thread::sleep(self.wait);
            attempt = self.github.comment(&pr.repository, pr.number, &body);
        }
        if let Err(error) = attempt {
            writeln!(
                out,
                "Could not post this on the pull request, so tell the user: {error:#}"
            )?;
        }
        self.label(&pr.repository, pr.number, decided == Decided::Ask, out)
    }

    fn label(&self, repo: &str, number: u64, wanted: bool, out: &mut dyn Write) -> Result<()> {
        let changed = if wanted {
            self.github.add_label(repo, number)
        } else {
            self.github.remove_label(repo, number)
        };
        if let Err(error) = changed {
            writeln!(
                out,
                "Could not change the {} label: {error:#}",
                github::LABEL
            )?;
        }
        Ok(())
    }

    /// Reads the bundle, giving GitHub time to work out whether it merges cleanly.
    fn settled_bundle(&self, repo: &str, number: u64) -> Result<Option<Bundle>> {
        let mut bundle = self.github.bundle(repo, number, None)?;
        for _ in 1..MERGEABLE_TRIES {
            let waiting = bundle.as_ref().is_some_and(|bundle| {
                bundle
                    .decision
                    .blockers
                    .iter()
                    .any(|blocker| blocker.contains(STILL_COMPUTING))
            });
            if !waiting {
                break;
            }
            thread::sleep(self.wait);
            bundle = self.github.bundle(repo, number, None)?;
        }
        Ok(bundle)
    }

    /// Applies the fixed rules, then any standing decision, then the judge.
    fn decide(&self, bundle: &Bundle, agent: &Agent) -> Result<Decision> {
        match bundle.decision.outcome {
            Outcome::NotReady => return Ok(Decision::NotReady(bundle.decision.blockers.clone())),
            Outcome::Ask => return Ok(Decision::Rule(bundle.decision.asks.clone())),
            Outcome::Ready => {}
        }
        // Asking a judge again about the same commit until it says yes would
        // defeat it, so a held commit stays held.
        if let Some(decided) = self.standing(bundle)? {
            return Ok(Decision::Standing(decided));
        }
        self.ask_judge(bundle, agent, false)
    }

    /// The decision that already holds this exact commit, here or on GitHub.
    fn standing(&self, bundle: &Bundle) -> Result<Option<Decided>> {
        let pr = &bundle.pull_request;
        Ok(self
            .holds
            .get(&pr.repository, &pr.head_sha)?
            .or_else(|| held(bundle)))
    }

    /// Has the judge read the change. `replayed` hides how it turned out.
    fn ask_judge(&self, bundle: &Bundle, agent: &Agent, replayed: bool) -> Result<Decision> {
        let pr = &bundle.pull_request;
        let files: Vec<&str> = bundle
            .files
            .iter()
            .map(|file| file.filename.as_str())
            .collect();
        let history = self.github.history(&pr.repository)?;
        let lessons = history::lessons(&history, pr.number, &files, replayed);
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let Some(prompt) = judge::prompt(bundle, &lessons, &nonce)? else {
            return Ok(Decision::Rule(vec![
                "the change is too large for a judge to read in full".to_string(),
            ]));
        };
        let verdict = match self.answer(agent, prompt.clone())? {
            Ok(verdict) => verdict,
            // Only an unreadable answer gets a second try, never a hold.
            Err(_) => self.answer(agent, judge::retry(&prompt))??,
        };
        Ok(Decision::Judged(verdict.held_to(bundle.policy.profile)))
    }

    /// Runs the judge once. The inner result says whether its answer could be read.
    fn answer(&self, agent: &Agent, prompt: String) -> Result<Result<Verdict>> {
        let dir = tempfile::tempdir().context("failed to create an empty folder")?;
        let printed = self.github.runner.run(&agent.judge(
            self.env,
            judge::SYSTEM_PROMPT,
            judge::SCHEMA,
            prompt,
            dir.path().to_path_buf(),
        ))?;
        Ok(Verdict::parse(&agent.answer(&printed)?))
    }

    /// Checks everything again right before merging. Judging takes minutes, and
    /// the judgement only covers what the judge read.
    fn confirm(&self, judged: &Bundle) -> Result<()> {
        let pr = &judged.pull_request;
        let Some(now) = self.settled_bundle(&pr.repository, pr.number)? else {
            bail!("land was turned off while the judge was reading");
        };
        if self.standing(&now)?.is_some() {
            bail!("another run held this commit while the judge was reading; run this again");
        }
        if now.decision.outcome != Outcome::Ready {
            bail!(
                "the pull request stopped being ready while the judge was reading; run this again"
            );
        }
        if !judged.same_evidence(&now) {
            bail!("the pull request or its policy changed while the judge was reading; run this again");
        }
        Ok(())
    }

    /// Judges past pull requests without posting or merging anything.
    fn backtest(
        &self,
        repo: &str,
        numbers: &[u64],
        trial: &Trial,
        agent: &Agent,
        out: &mut dyn Write,
    ) -> Result<i32> {
        for number in numbers {
            let line = match self.replay(repo, *number, trial, agent) {
                Ok(line) => line,
                // One bad pull request shouldn't hide the others.
                Err(error) => format!("ERROR  {error:#}"),
            };
            writeln!(out, "#{number}  {line}")?;
            out.flush()?;
        }
        Ok(DONE)
    }

    fn replay(&self, repo: &str, number: u64, trial: &Trial, agent: &Agent) -> Result<String> {
        let Some(mut bundle) = self.github.bundle(repo, number, trial.policy)? else {
            return Ok("OFF".to_string());
        };
        if let Some(risk_map) = trial.risk_map {
            bundle.risk_map = Some(risk_map.to_string());
        }
        // What land and the owner said about it is the answer being tested.
        bundle
            .comments
            .retain(|comment| !record::is_ours(&comment.body));
        // Whether it was ready is history. What the rules and the judge say isn't.
        let rules = bundle.decision.asks.join("; ");
        let judged = match self.ask_judge(&bundle, agent, true)? {
            Decision::Judged(verdict) => {
                let detail = match verdict.decision {
                    Choice::Merge => verdict.summary.clone(),
                    Choice::Ask => verdict.question.clone(),
                    Choice::Fix => verdict.fixes.join("; "),
                };
                format!(
                    "judge {:?} (impact {}, {}, likelihood {}): {detail}",
                    verdict.decision,
                    verdict.impact.as_str(),
                    if verdict.reversible {
                        "reversible"
                    } else {
                        "not reversible"
                    },
                    verdict.likelihood.as_str(),
                )
            }
            Decision::Rule(asks) => asks.join("; "),
            Decision::NotReady(_) | Decision::Standing(_) => String::new(),
        };
        let rule = if rules.is_empty() {
            "rules pass".to_string()
        } else {
            format!("rules ask ({rules})")
        };
        Ok(format!("{}  [{rule}] {judged}", bundle.pull_request.title))
    }

    /// Lists what merged since the last release, riskiest first.
    fn digest(&self, repo: &str, since: Option<String>, out: &mut dyn Write) -> Result<i32> {
        let since = match since {
            Some(since) => since,
            None => self
                .github
                .last_release(repo)?
                .with_context(|| format!("{repo} has no release; pass --since"))?,
        };
        if DateTime::parse_from_rfc3339(&since).is_err()
            && chrono::NaiveDate::parse_from_str(&since, "%Y-%m-%d").is_err()
        {
            bail!("--since needs a date such as 2026-11-25, got `{since}`");
        }
        let merged = self.github.merged_since(repo, &since)?;
        let mut entries: Vec<(Option<Record>, &github::Merged)> = merged
            .iter()
            .map(|pr| {
                // Only a record of the commit that merged says anything about it.
                let mut records = pr
                    .comments
                    .iter()
                    .filter(|comment| record::trusted(&comment.author_association))
                    .filter_map(|comment| record::parse(&comment.body))
                    .filter(|record| record.sha == pr.head_ref_oid)
                    .collect::<Vec<_>>();
                let merge = records.iter().rposition(|record| record.merged);
                let found = match merge {
                    Some(at) => Some(records.remove(at)),
                    None => records.pop(),
                };
                (found, pr)
            })
            .collect();
        // What land didn't merge comes first: it may have gone around land.
        entries.sort_by_key(|(record, _)| match record {
            Some(record) if record.merged => (
                1 + usize::from(record.reversible != Some(false)),
                match record.impact {
                    Some(judge::Level::High) => 0,
                    Some(judge::Level::Medium) | None => 1,
                    Some(judge::Level::Low) => 2,
                },
            ),
            _ => (0, usize::from(record.is_none())),
        });
        writeln!(out, "{} merged in {repo} since {since}", entries.len())?;
        if entries.len() >= github::DIGEST_LIMIT {
            writeln!(out, "Only the newest {} are listed.", github::DIGEST_LIMIT)?;
        }
        for (record, pr) in entries {
            let risk = match &record {
                Some(record) if record.merged => format!(
                    "merged by land, impact {}, {}",
                    record.impact.map_or("unknown", judge::Level::as_str),
                    if record.reversible == Some(false) {
                        "not reversible"
                    } else {
                        "reversible"
                    }
                ),
                Some(record) if record.decided == Decided::Merge => {
                    "not merged by land, which would have merged it".to_string()
                }
                Some(_) => "not merged by land, which held it".to_string(),
                None => "not merged by land, which has no record of it".to_string(),
            };
            writeln!(out, "#{} {} ({risk})\n  {}", pr.number, pr.title, pr.url)?;
        }
        Ok(DONE)
    }
}

/// The hold recorded on GitHub for this exact commit. A later record of
/// anything else doesn't lift it.
fn held(bundle: &Bundle) -> Option<Decided> {
    bundle
        .comments
        .iter()
        .filter(|comment| record::trusted(&comment.association))
        .filter_map(|comment| record::parse(&comment.body))
        .filter(|record| record.judged && record.sha == bundle.pull_request.head_sha)
        .map(|record| record.decided)
        .rfind(|decided| *decided != Decided::Merge)
}

fn bullets(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("- {}", line.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A judge's answer, in words for the pull request.
fn explain(verdict: &Verdict) -> String {
    let mut parts = vec![verdict.summary.trim().to_string()];
    match verdict.decision {
        Choice::Ask => parts.push(format!(
            "**What you need to decide:** {}",
            verdict.question.trim()
        )),
        Choice::Fix => parts.push(format!(
            "**What to fix first:**\n\n{}",
            bullets(&verdict.fixes)
        )),
        Choice::Merge => {}
    }
    parts.push(format!(
        "Impact {}, {}, likelihood of a problem {}.",
        verdict.impact.as_str(),
        if verdict.reversible {
            "reversible"
        } else {
            "not reversible"
        },
        verdict.likelihood.as_str()
    ));
    if !verdict.reasons.is_empty() {
        parts.push(bullets(&verdict.reasons));
    }
    parts.join("\n\n")
}

/// One decision to post on its pull request.
struct Report<'a> {
    bundle: &'a Bundle,
    agent: &'a Agent,
    /// Missing when a fixed rule decided.
    verdict: Option<&'a Verdict>,
    decided: Decided,
    merged: bool,
    text: String,
}

impl Report<'_> {
    /// The comment that explains the decision and records it.
    fn comment(&self) -> Result<String> {
        let sha = &self.bundle.pull_request.head_sha;
        let marker = record::marker(&Record {
            sha: sha.clone(),
            decided: self.decided,
            judged: self.verdict.is_some(),
            merged: self.merged,
            impact: self.verdict.map(|verdict| verdict.impact),
            reversible: self.verdict.map(|verdict| verdict.reversible),
        })
        .context("failed to write the land record")?;
        let heading = match self.decided {
            Decided::Merge if self.merged => "Land: merged",
            Decided::Merge => "Land: would merge",
            Decided::Fix => "Land: fix first",
            Decided::Ask => "Land: needs you",
        };
        let by = match self.verdict {
            Some(_) => format!(
                "Judged by {} with {}",
                self.agent.kind.as_str(),
                self.agent.model
            ),
            None => "Decided by a fixed rule".to_string(),
        };
        Ok(format!(
            "{marker}\n## {heading}\n\n{}\n\n{} at {sha}.",
            record::plain(&self.text),
            record::plain(&by)
        ))
    }
}

#[cfg(test)]
mod tests;
