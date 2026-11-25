use super::judge::Level;
use super::run::{Env, Invocation, Runner};
use super::*;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Matches only a run of the rules script.
const RULES: &str = "dist/cli/land.js";
const PIN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

fn pi_vars() -> BTreeMap<String, String> {
    vars(&[
        ("AI_AGENT", "pi"),
        ("PI_PROVIDER", "opencode-go"),
        ("PI_MODEL", "glm"),
        ("PI_REASONING_LEVEL", "high"),
        ("PATH", "/bin"),
        ("HOME", "/home/x"),
        ("GITHUB_TOKEN", "ghs_secret"),
        ("GH_TOKEN", "ghs_secret"),
        ("OPENCODE_API_KEY", "key"),
    ])
}

fn answer(decision: &str) -> Value {
    json!({
        "decision": decision, "impact": "low", "reversible": true, "likelihood": "low",
        "summary": "Adds a flag.", "reasons": ["small"],
        "question": if decision == "ask" { "Ship it?" } else { "" },
        "fixes": if decision == "fix" { json!(["handle the error"]) } else { json!([]) },
    })
}

fn bundle() -> Value {
    json!({
        "version": 1, "mode": "enforce",
        "policy": {"profile": "side-project", "ask_when": {"mail": "Emails people"}},
        "pullRequest": {"repository": "o/r", "number": 7, "title": "feat: x", "body": "the spec",
            "author": "owner", "base": "main", "headSha": SHA},
        "decision": {"outcome": "ready", "blockers": [], "asks": [], "signals": ["test removed"],
            "countedLines": 3, "rounds": 1},
        "files": [{"filename": "src/a.rs", "status": "modified", "additions": 2, "deletions": 1}],
        "diff": "+let x = 1;", "riskMap": "- merging deploys", "issues": [{"number": 2, "title": "t", "body": "crit"}],
        "reviews": [{"author": "bot", "state": "COMMENTED", "at": "t", "body": "LGTM body"}],
        "threads": [{"path": "src/a.rs", "resolved": true, "codeChanged": false,
            "comments": [{"author": "bot", "body": "a finding"}, {"author": "owner", "body": "a reply"}]},
            {"path": null, "resolved": false, "codeChanged": true, "comments": []}],
        "comments": []
    })
}

fn history(prs: Value) -> Value {
    json!({"data": {"repository": {"pullRequests": {"nodes": prs}}}})
}

#[derive(Clone)]
struct Answer {
    key: String,
    value: Result<String, String>,
    once: bool,
}

/// Answers programs by the newest matching text, and records every call.
struct Fake {
    answers: RefCell<Vec<Answer>>,
    calls: RefCell<Vec<Invocation>>,
}

impl Fake {
    fn new() -> Fake {
        let fake = Fake {
            answers: RefCell::new(Vec::new()),
            calls: RefCell::new(Vec::new()),
        };
        fake.always(
            "contents/.github/workflows",
            &format!("    - uses: adlrocha/nibble-review@{PIN} # v1"),
        );
        fake.always("auth token", "gh_token\n");
        fake.always(RULES, &bundle().to_string());
        fake.always("graphql", &history(json!([])).to_string());
        fake.always(
            "viewerDefaultMergeMethod",
            r#"{"viewerDefaultMergeMethod":"SQUASH"}"#,
        );
        fake.always("--json state", r#"{"state":"MERGED"}"#);
        fake
    }

    fn set(&self, key: &str, value: Result<&str, &str>, once: bool) {
        let value = value.map(String::from).map_err(String::from);
        self.answers.borrow_mut().push(Answer {
            key: key.to_string(),
            value,
            once,
        });
    }

    /// Later answers win over earlier ones.
    fn always(&self, key: &str, value: &str) {
        self.set(key, Ok(value), false);
    }

    /// Used once, before the standing answer.
    fn once(&self, key: &str, value: &str) {
        self.set(key, Ok(value), true);
    }

    fn fails(&self, key: &str, message: &str) {
        self.set(key, Err(message), false);
    }

    fn judged(self, decision: &str) -> Fake {
        self.always("pi -p", &answer(decision).to_string());
        self
    }

    fn lines(&self) -> Vec<String> {
        self.calls.borrow().iter().map(line).collect()
    }

    fn ran(&self, wanted: &str) -> bool {
        self.lines().iter().any(|call| call.contains(wanted))
    }

    fn call(&self, wanted: &str) -> Invocation {
        self.calls
            .borrow()
            .iter()
            .find(|call| line(call).contains(wanted))
            .cloned()
            .unwrap_or_else(|| panic!("no call matched {wanted}"))
    }
}

fn line(call: &Invocation) -> String {
    format!("{} {}", call.program, call.args.join(" "))
}

impl Runner for Fake {
    fn run(&self, invocation: &Invocation) -> Result<String> {
        self.calls.borrow_mut().push(invocation.clone());
        let call = line(invocation);
        // A build leaves the script behind, as npm would.
        if call == "npm run build" {
            let dist = invocation.dir.clone().unwrap().join("dist/cli");
            fs::create_dir_all(&dist)?;
            fs::write(dist.join("land.js"), "")?;
        }
        let mut answers = self.answers.borrow_mut();
        let Some(index) = answers
            .iter()
            .rposition(|answer| call.contains(answer.key.as_str()))
        else {
            return Ok(String::new());
        };
        let Answer { value, once, .. } = answers[index].clone();
        if once {
            answers.remove(index);
        }
        value.map_err(|message| anyhow::anyhow!("{message}"))
    }
}

struct Setup {
    fake: Fake,
    cache: tempfile::TempDir,
    env: BTreeMap<String, String>,
}

impl Setup {
    fn new(fake: Fake) -> Setup {
        Setup {
            fake,
            cache: tempfile::tempdir().unwrap(),
            env: pi_vars(),
        }
    }

    fn run(&self, action: LandAction) -> Result<(i32, String)> {
        let land = Land {
            github: GitHub {
                runner: &self.fake,
                cache: self.cache.path().join("land"),
                env: &self.env,
            },
            env: &self.env,
            wait: Duration::ZERO,
        };
        let mut out = Vec::new();
        let code = land.run(action, &mut out)?;
        Ok((code, String::from_utf8(out).unwrap()))
    }

    fn pr(&self) -> Result<(i32, String)> {
        self.run(pr_action(false))
    }
}

fn pr_action(dry_run: bool) -> LandAction {
    LandAction::Pr {
        number: 7,
        repo: Some("o/r".to_string()),
        dry_run,
        agent: None,
        model: None,
    }
}

fn changed(edit: impl FnOnce(&mut Value)) -> String {
    let mut value = bundle();
    edit(&mut value);
    value.to_string()
}

fn record_comment(decided: &str, sha: &str, judged: bool, association: &str) -> Value {
    json!({"author": "owner", "association": association, "at": "t",
        "body": format!("<!-- nibble:land {{\"sha\":\"{sha}\",\"decided\":\"{decided}\",\"judged\":{judged}}} -->\n## Land")})
}

// ── Acting on a decision ─────────────────────────────────────────────────

#[test]
fn a_cleared_head_is_merged_at_exactly_the_judged_commit() {
    let setup = Setup::new(Fake::new().judged("merge"));
    let (code, out) = setup.pr().unwrap();
    assert_eq!(code, DONE);
    assert_eq!(out, format!("MERGED: o/r#7 at {SHA}.\n"));
    assert!(setup.fake.ran(&format!(
        "gh pr merge 7 --repo o/r --squash --match-head-commit {SHA}"
    )));
    let posted = setup.fake.call("pr comment").stdin.unwrap();
    assert!(posted.starts_with("<!-- nibble:land {"), "{posted}");
    assert!(posted.contains("## Land: merge"));
    assert!(posted.contains("Judged by pi with opencode-go/glm"));
    assert!(setup
        .fake
        .ran("DELETE repos/o/r/issues/7/labels/needs-human"));
}

#[test]
fn every_merge_method_is_mapped_and_an_unknown_one_stops() {
    for (method, flag) in [("MERGE", "--merge"), ("REBASE", "--rebase")] {
        let fake = Fake::new().judged("merge");
        fake.always(
            "viewerDefaultMergeMethod",
            &format!(r#"{{"viewerDefaultMergeMethod":"{method}"}}"#),
        );
        let setup = Setup::new(fake);
        setup.pr().unwrap();
        assert!(setup.fake.ran(&format!("pr merge 7 --repo o/r {flag} ")));
    }
    let fake = Fake::new().judged("merge");
    fake.always(
        "viewerDefaultMergeMethod",
        r#"{"viewerDefaultMergeMethod":"X"}"#,
    );
    let setup = Setup::new(fake);
    assert!(setup
        .pr()
        .unwrap_err()
        .to_string()
        .contains("unknown merge method"));
    assert!(!setup.fake.ran("pr merge"));
}

#[test]
fn a_merge_github_did_not_make_is_an_error() {
    let fake = Fake::new().judged("merge");
    fake.always("--json state", r#"{"state":"OPEN"}"#);
    assert!(Setup::new(fake)
        .pr()
        .unwrap_err()
        .to_string()
        .contains("is OPEN"));
}

#[test]
fn unfinished_work_goes_back_to_the_agent_without_a_judge_or_a_comment() {
    let fake = Fake::new().judged("merge");
    fake.always(
        RULES,
        &changed(|b| {
            b["decision"]["outcome"] = json!("not-ready");
            b["decision"]["blockers"] = json!(["2 unresolved review threads"]);
        }),
    );
    let setup = Setup::new(fake);
    let (code, out) = setup.pr().unwrap();
    assert_eq!(code, KEEP_WORKING);
    assert!(out.starts_with("NOT READY") && out.contains("- 2 unresolved review threads"));
    assert!(
        !setup.fake.ran("pi -p") && !setup.fake.ran("pr comment") && !setup.fake.ran("pr merge")
    );
}

#[test]
fn a_fixed_rule_calls_the_human_without_a_judge() {
    let fake = Fake::new().judged("merge");
    fake.always(
        RULES,
        &changed(|b| {
            b["decision"]["outcome"] = json!("ask");
            b["decision"]["asks"] = json!(["always-human paths: `.github/x`"]);
        }),
    );
    let setup = Setup::new(fake);
    let (code, out) = setup.pr().unwrap();
    assert_eq!(code, CALL_HUMAN);
    assert!(out.starts_with("CALL THE HUMAN") && out.contains(".github/x"));
    assert!(!setup.fake.ran("pi -p") && !setup.fake.ran("pr merge"));
    assert!(setup
        .fake
        .ran("repos/o/r/issues/7/labels -f labels[]=needs-human"));
    assert!(setup
        .fake
        .call("pr comment")
        .stdin
        .unwrap()
        .contains("Decided by a fixed rule"));
}

#[test]
fn a_judge_can_call_the_human_with_one_question() {
    let setup = Setup::new(Fake::new().judged("ask"));
    let (code, out) = setup.pr().unwrap();
    assert_eq!(code, CALL_HUMAN);
    assert!(out.ends_with("Ask them this and stop:\nShip it?\n"));
    assert!(!setup.fake.ran("pr merge"));
    assert!(setup.fake.ran("labels[]=needs-human"));
    let posted = setup.fake.call("pr comment").stdin.unwrap();
    assert!(
        posted.contains("## Land: needs you")
            && posted.contains("**What you need to decide:** Ship it?")
    );
}

#[test]
fn a_judge_can_send_the_agent_back_to_fix() {
    let setup = Setup::new(Fake::new().judged("fix"));
    let (code, out) = setup.pr().unwrap();
    assert_eq!(code, KEEP_WORKING);
    assert!(out.contains("FIX FIRST") && out.contains("- handle the error"));
    assert!(!setup.fake.ran("pr merge") && !setup.fake.ran("labels[]="));
}

#[test]
fn shadow_mode_and_dry_runs_never_merge_and_a_dry_run_posts_nothing() {
    let fake = Fake::new().judged("merge");
    fake.always(RULES, &changed(|b| b["mode"] = json!("shadow")));
    let setup = Setup::new(fake);
    let (code, out) = setup.pr().unwrap();
    assert_eq!(code, CALL_HUMAN);
    assert!(out.contains("WOULD MERGE") && out.contains("shadow mode"));
    assert!(setup.fake.ran("pr comment") && !setup.fake.ran("pr merge"));

    for decision in ["merge", "ask", "fix"] {
        let setup = Setup::new(Fake::new().judged(decision));
        setup.run(pr_action(true)).unwrap();
        for write in ["pr comment", "pr merge", "labels"] {
            assert!(!setup.fake.ran(write), "{decision} ran {write}");
        }
    }
}

#[test]
fn land_off_leaves_the_merge_to_the_user() {
    let fake = Fake::new().judged("merge");
    fake.always(RULES, r#"{"version":1,"mode":"off"}"#);
    let setup = Setup::new(fake);
    let (code, out) = setup.pr().unwrap();
    assert_eq!(code, CALL_HUMAN);
    assert!(out.contains("Land is off"));
    assert!(!setup.fake.ran("pr merge"));
}

#[test]
fn a_held_commit_is_not_judged_again_until_it_changes() {
    for (decided, code, text) in [
        ("ask", CALL_HUMAN, "already sent"),
        ("fix", KEEP_WORKING, "already asked"),
    ] {
        let fake = Fake::new().judged("merge");
        fake.always(
            RULES,
            &changed(|b| {
                b["comments"] = json!([record_comment(decided, SHA, true, "OWNER")]);
            }),
        );
        let setup = Setup::new(fake);
        let (got, out) = setup.pr().unwrap();
        assert_eq!(got, code);
        assert!(out.contains(text), "{out}");
        assert!(
            !setup.fake.ran("pi -p")
                && !setup.fake.ran("pr merge")
                && !setup.fake.ran("pr comment")
        );
    }
    // Another commit, a fixed rule's old decision, a merge, or a stranger's text hold nothing.
    for comment in [
        record_comment("ask", PIN, true, "OWNER"),
        record_comment("ask", SHA, false, "OWNER"),
        record_comment("merge", SHA, true, "OWNER"),
        record_comment("ask", SHA, true, "NONE"),
        json!({"author": "o", "association": "OWNER", "at": "t", "body": "quoting <!-- nibble:land {} -->"}),
    ] {
        let fake = Fake::new().judged("merge");
        fake.always(RULES, &changed(|b| b["comments"] = json!([comment])));
        assert_eq!(Setup::new(fake).pr().unwrap().0, DONE, "{comment}");
    }
}

#[test]
fn anything_that_moved_while_judging_stops_the_merge() {
    for (change, message) in [
        (
            changed(|b| b["pullRequest"]["headSha"] = json!(PIN)),
            "moved on",
        ),
        (
            changed(|b| b["mode"] = json!("shadow")),
            "stopped being ready",
        ),
        (
            changed(|b| b["decision"]["outcome"] = json!("not-ready")),
            "stopped being ready",
        ),
        (
            changed(|b| b["decision"]["outcome"] = json!("ask")),
            "stopped being ready",
        ),
        (r#"{"version":1,"mode":"off"}"#.to_string(), "turned off"),
    ] {
        let fake = Fake::new().judged("merge");
        fake.always(RULES, &change);
        fake.once(RULES, &bundle().to_string());
        let setup = Setup::new(fake);
        let error = setup.pr().unwrap_err().to_string();
        assert!(error.contains(message), "{error}");
        assert!(!setup.fake.ran("pr merge") && !setup.fake.ran("pr comment"));
    }
}

#[test]
fn github_gets_time_to_compute_mergeability_but_not_forever() {
    let computing = changed(|b| {
        b["decision"]["outcome"] = json!("not-ready");
        b["decision"]["blockers"] = json!(["GitHub is still computing whether it merges cleanly"]);
    });
    let fake = Fake::new().judged("merge");
    fake.once(RULES, &computing);
    assert_eq!(Setup::new(fake).pr().unwrap().0, DONE);

    let fake = Fake::new().judged("merge");
    fake.always(RULES, &computing);
    let setup = Setup::new(fake);
    assert_eq!(setup.pr().unwrap().0, KEEP_WORKING);
    let reads = setup
        .fake
        .lines()
        .iter()
        .filter(|call| call.starts_with("node"))
        .count();
    assert_eq!(reads, MERGEABLE_TRIES as usize);
}

#[test]
fn a_failing_or_unreadable_judge_never_merges() {
    let fake = Fake::new();
    fake.fails("pi -p", "model unavailable");
    let setup = Setup::new(fake);
    assert!(setup
        .pr()
        .unwrap_err()
        .to_string()
        .contains("model unavailable"));
    assert!(!setup.fake.ran("pr merge"));

    let broken = |edit: fn(&mut Value)| {
        let mut value = answer("merge");
        edit(&mut value);
        value.to_string()
    };
    for text in [
        "sure, merge it".to_string(),
        broken(|v| v["decision"] = json!("yes")),
        broken(|v| v["extra"] = json!(1)),
        broken(|v| {
            v.as_object_mut().unwrap().remove("impact");
        }),
        broken(|v| {
            v["decision"] = json!("ask");
            v.as_object_mut().unwrap().remove("question");
        }),
        broken(|v| v["decision"] = json!("ask")),
        broken(|v| v["decision"] = json!("fix")),
    ] {
        let fake = Fake::new();
        fake.always("pi -p", &text);
        let setup = Setup::new(fake);
        assert!(setup.pr().is_err(), "{text}");
        assert!(!setup.fake.ran("pr merge"));
        let tries = setup
            .fake
            .lines()
            .iter()
            .filter(|call| call.starts_with("pi -p"))
            .count();
        assert_eq!(tries, 2);
    }
}

#[test]
fn an_unreadable_answer_gets_one_more_try_that_says_what_was_wrong() {
    let fake = Fake::new().judged("merge");
    fake.once("pi -p", "sure, merge it");
    let setup = Setup::new(fake);
    assert_eq!(setup.pr().unwrap().0, DONE);
    let calls = setup.fake.calls.borrow();
    let prompts: Vec<&str> = calls
        .iter()
        .filter(|call| line(call).starts_with("pi -p"))
        .filter_map(|call| call.stdin.as_deref())
        .collect();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[1].starts_with(prompts[0])
            && prompts[1].contains("could not be used: the judge's answer")
    );
}

#[test]
fn what_a_decision_does_not_need_may_be_left_out() {
    let mut value = answer("merge");
    for field in ["question", "fixes", "reasons"] {
        value.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(
        Verdict::parse(&value.to_string()).unwrap().decision,
        Choice::Merge
    );
}

#[test]
fn a_change_too_large_or_refused_by_github_goes_to_the_human() {
    for diff in [json!(null), json!("x".repeat(judge::MAX_PROMPT_CHARS))] {
        let fake = Fake::new().judged("merge");
        fake.always(RULES, &changed(|b| b["diff"] = diff));
        let setup = Setup::new(fake);
        let (code, out) = setup.pr().unwrap();
        assert_eq!(code, CALL_HUMAN);
        assert!(out.contains("too large for a judge"));
        assert!(!setup.fake.ran("pi -p"));
    }
}

#[test]
fn a_label_that_is_not_there_is_fine_but_other_failures_are_not() {
    let fake = Fake::new().judged("merge");
    fake.fails("DELETE", "gh: Label does not exist (HTTP 404)");
    assert_eq!(Setup::new(fake).pr().unwrap().0, DONE);
    let fake = Fake::new().judged("merge");
    fake.fails("DELETE", "HTTP 500");
    let setup = Setup::new(fake);
    assert!(setup.pr().is_err());
    assert!(!setup.fake.ran("pr merge"));
}

#[test]
fn posted_text_cannot_mention_anyone_trigger_the_reviewer_or_hide_a_record() {
    let fake = Fake::new();
    let mut value = answer("ask");
    value["question"] = json!("@nibble-review land? <!-- nibble:land {} --> ok @ana");
    fake.always("pi -p", &value.to_string());
    let setup = Setup::new(fake);
    setup.pr().unwrap();
    let posted = setup.fake.call("pr comment").stdin.unwrap();
    assert!(!posted.contains("@nibble-review") && !posted.contains("@ana"));
    assert_eq!(posted.matches("<!--").count(), 1);
}

// ── Thresholds ───────────────────────────────────────────────────────────

#[test]
fn a_merge_is_held_to_the_profile() {
    use Level::{High, Low, Medium};
    let held = |profile, impact, reversible, likelihood| {
        let mut value = answer("merge");
        value["impact"] = serde_json::to_value(impact).unwrap();
        value["reversible"] = json!(reversible);
        value["likelihood"] = serde_json::to_value(likelihood).unwrap();
        let verdict = Verdict::parse(&value.to_string()).unwrap().held_to(profile);
        assert_eq!(
            verdict.decision == Choice::Ask,
            !verdict.question.is_empty()
        );
        verdict.decision == Choice::Ask
    };
    use bundle::Profile::{Critical, SideProject, Standard};
    // (impact, reversible, likelihood) → held in side-project, standard, critical
    for (impact, reversible, likelihood, expected) in [
        (Low, true, Low, [false, false, false]),
        (Low, false, Low, [false, false, false]),
        (Low, true, High, [false, false, true]),
        (Medium, true, Low, [false, false, false]),
        (Medium, true, Medium, [false, false, true]),
        (Medium, false, Low, [false, true, true]),
        (High, true, Low, [false, false, true]),
        (High, true, Medium, [false, true, true]),
        (High, false, Low, [true, true, true]),
    ] {
        let got = [SideProject, Standard, Critical]
            .map(|profile| held(profile, impact, reversible, likelihood));
        assert_eq!(got, expected, "{impact:?} {reversible} {likelihood:?}");
    }
    // A profile never turns a hold into a merge.
    for decision in ["ask", "fix"] {
        let verdict = Verdict::parse(&answer(decision).to_string()).unwrap();
        assert_eq!(verdict.clone().held_to(SideProject), verdict);
    }
}

#[test]
fn an_answer_may_be_fenced_but_not_surrounded_by_prose() {
    let text = answer("merge").to_string();
    for fenced in [
        format!("```json\n{text}\n```"),
        format!("```\n{text}\n```\n"),
        format!("  {text}\n"),
    ] {
        assert!(Verdict::parse(&fenced).is_ok());
    }
    assert!(Verdict::parse(&format!("Here: {text}")).is_err());
}

#[test]
fn the_schema_matches_the_answer_type() {
    let schema: Value = serde_json::from_str(judge::SCHEMA).unwrap();
    let mut required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    required.sort_unstable();
    let example = answer("merge");
    let mut keys: Vec<&str> = example
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(required, keys);
}

// ── The judge's input ────────────────────────────────────────────────────

#[test]
fn the_prompt_carries_all_evidence_inside_random_fences() {
    let parsed: Bundle = serde_json::from_value(bundle()).unwrap();
    let text = judge::prompt(&parsed, &["owner flag".to_string()], "N0NCE")
        .unwrap()
        .unwrap();
    for wanted in [
        "Emails people",
        "- merging deploys",
        "- owner flag",
        "the spec",
        "#2 t",
        "modified src/a.rs +2 -1",
        "+let x = 1;",
        "- test removed",
        "LGTM body",
        "a finding",
        "a reply",
        "the code did not change afterwards",
        "(no file) | open | the code changed afterwards",
    ] {
        assert!(text.contains(wanted), "missing {wanted}");
    }
    assert_eq!(text.matches("<DIFF N0NCE>").count(), 1);
    assert_eq!(text.matches("</DIFF N0NCE>").count(), 1);
    let bare: Bundle = serde_json::from_value({
        let mut value = bundle();
        value["riskMap"] = json!(null);
        value
    })
    .unwrap();
    let text = judge::prompt(&bare, &[], "N").unwrap().unwrap();
    assert!(text.contains("<RISK MAP N>\n(none)\n") && text.contains("<HISTORY N>\n(none)\n"));
}

#[test]
fn each_run_uses_its_own_fence() {
    let fence = || {
        let setup = Setup::new(Fake::new().judged("merge"));
        setup.pr().unwrap();
        let prompt = setup.fake.call("pi -p").stdin.unwrap();
        prompt.lines().next().unwrap().to_string()
    };
    assert_ne!(fence(), fence());
}

#[test]
fn comments_reach_the_judge_with_their_author_role() {
    let fake = Fake::new().judged("merge");
    fake.always(
        RULES,
        &changed(|b| {
            b["comments"] =
                json!([{"author": "ana", "association": "NONE", "at": "t", "body": "hello"}]);
        }),
    );
    let setup = Setup::new(fake);
    setup.pr().unwrap();
    assert!(setup
        .fake
        .call("pi -p")
        .stdin
        .unwrap()
        .contains("ana (NONE) t\n\nhello"));
}

// ── The agent that judges ────────────────────────────────────────────────

#[test]
fn the_innermost_agent_is_the_one_that_judges() {
    let pi = Agent::detect(&pi_vars(), None, None).unwrap();
    assert_eq!(
        (pi.kind, pi.model.as_str(), pi.effort.as_deref()),
        (AgentKind::Pi, "opencode-go/glm", Some("high"))
    );
    // pi started from Claude Code inherits its variables.
    let mut nested = pi_vars();
    nested.insert("CLAUDECODE".to_string(), "1".to_string());
    assert_eq!(
        Agent::detect(&nested, None, None).unwrap().kind,
        AgentKind::Pi
    );
    // Older versions set only their own variable.
    let mut old = pi_vars();
    old.remove("AI_AGENT");
    old.insert("PI_CODING_AGENT".to_string(), "true".to_string());
    assert_eq!(Agent::detect(&old, None, None).unwrap().kind, AgentKind::Pi);
}

#[test]
fn an_unknown_agent_or_model_must_be_named() {
    let claude = || Some(AgentKind::Claude);
    for (env, kind, model) in [
        (vars(&[]), None, None),
        (
            vars(&[("AI_AGENT", "omp"), ("CLAUDECODE", "1")]),
            None,
            None,
        ),
        (vars(&[("AI_AGENT", "pi")]), None, None),
        (
            vars(&[("AI_AGENT", "pi"), ("PI_PROVIDER", ""), ("PI_MODEL", "m")]),
            None,
            None,
        ),
        (vars(&[("CLAUDECODE", "1")]), None, None),
        (
            vars(&[
                ("CLAUDECODE", "1"),
                ("CLAUDE_CODE_SESSION_ID", "../x"),
                ("HOME", "/h"),
            ]),
            None,
            None,
        ),
        (
            vars(&[("CLAUDECODE", "1"), ("CLAUDE_CODE_SESSION_ID", "x")]),
            None,
            None,
        ),
        (pi_vars(), claude(), None),
        (vars(&[]), None, Some("m".to_string())),
    ] {
        assert!(Agent::detect(&env, kind, model).is_err(), "{env:?}");
    }
    let named = Agent::detect(&pi_vars(), claude(), Some("opus".to_string())).unwrap();
    assert_eq!(
        (named.kind, named.model.as_str(), named.effort),
        (AgentKind::Claude, "opus", None)
    );
}

fn claude_vars(home: &Path) -> BTreeMap<String, String> {
    let mut env = vars(&[
        ("AI_AGENT", "claude-code_2-1-316_agent"),
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_SESSION_ID", "abc-1"),
        ("CLAUDE_EFFORT", "medium"),
        ("CLAUDE_CODE_EXECPATH", "/opt/claude"),
        ("CLAUDE_CODE_MESSAGING_TOKEN", "secret"),
        ("GITHUB_TOKEN", "ghs_secret"),
        ("BREVO_API_KEY", "secret"),
        ("ANTHROPIC_BASE_URL", "https://x"),
        ("PATH", "/bin"),
    ]);
    env.insert("HOME".to_string(), home.display().to_string());
    env
}

#[test]
fn claudes_model_is_the_last_real_reply_of_its_session() {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join(".claude/projects/-work");
    fs::create_dir_all(&project).unwrap();
    let transcript = project.join("abc-1.jsonl");
    let reply = |model: &str| json!({"type": "assistant", "message": {"model": model}}).to_string();
    let lines = [
        "not json \"assistant\"".to_string(),
        reply("claude-old"),
        json!({"type": "user", "message": {"model": "no", "text": "\"assistant\""}}).to_string(),
        reply("claude-opus-5-5"),
        reply("<synthetic>"),
    ];
    fs::write(&transcript, lines.join("\n")).unwrap();
    let env = claude_vars(home.path());
    let agent = Agent::detect(&env, None, None).unwrap();
    assert_eq!(
        (agent.kind, agent.model.as_str()),
        (AgentKind::Claude, "claude-opus-5-5")
    );
    assert_eq!(agent.effort.as_deref(), Some("medium"));

    let mut moved = claude_vars(Path::new("/nowhere"));
    moved.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        home.path().join(".claude").display().to_string(),
    );
    assert_eq!(
        Agent::detect(&moved, None, None).unwrap().model,
        "claude-opus-5-5"
    );

    fs::write(&transcript, "").unwrap();
    assert!(Agent::detect(&env, None, None).is_err());
    fs::remove_file(&transcript).unwrap();
    assert!(Agent::detect(&env, None, None).is_err());
}

#[test]
fn a_judge_gets_no_tools_settings_project_files_or_credentials() {
    let run = |agent: &Agent, env: &BTreeMap<String, String>| {
        agent.judge(
            env,
            "SYSTEM",
            "{}",
            "PROMPT".to_string(),
            PathBuf::from("/empty"),
        )
    };
    let env = claude_vars(Path::new("/h"));
    let claude = Agent::detect(&env, None, Some("opus".to_string())).unwrap();
    let call = run(&claude, &env);
    assert_eq!(call.program, "/opt/claude");
    let joined = call.args.join("|");
    for flag in [
        "--tools||",
        "--setting-sources||",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--safe-mode",
        "--no-session-persistence",
        "--json-schema|{}",
        "--system-prompt|SYSTEM",
        "--model|opus",
        "--effort|medium",
    ] {
        assert!(joined.contains(flag), "missing {flag} in {joined}");
    }
    let Env::Only(seen) = call.env else {
        panic!("the judge inherits nothing")
    };
    let names: Vec<&str> = seen.keys().map(String::as_str).collect();
    assert_eq!(names, ["ANTHROPIC_BASE_URL", "HOME", "PATH"]);
    assert_eq!(call.dir, Some(PathBuf::from("/empty")));

    let pi = Agent::detect(&pi_vars(), None, None).unwrap();
    let call = run(&pi, &pi_vars());
    let joined = call.args.join("|");
    for flag in [
        "-p",
        "--no-tools",
        "--no-mcp",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-context-files",
        "--no-approve",
        "--no-session",
        "--model|opencode-go/glm",
        "--thinking|high",
    ] {
        assert!(joined.contains(flag), "missing {flag} in {joined}");
    }
    let Env::Only(seen) = call.env else {
        panic!("the judge inherits nothing")
    };
    let names: Vec<&str> = seen.keys().map(String::as_str).collect();
    assert_eq!(names, ["HOME", "OPENCODE_API_KEY", "PATH"]);
    assert_eq!(call.stdin.as_deref(), Some("PROMPT"));
}

#[test]
fn claudes_answer_comes_from_its_structured_output_only() {
    let claude = Agent::detect(&vars(&[]), Some(AgentKind::Claude), Some("m".to_string())).unwrap();
    let good = json!({"is_error": false, "result": "x", "structured_output": {"a": 1}}).to_string();
    assert_eq!(claude.answer(&good).unwrap(), r#"{"a":1}"#);
    for bad in [
        "not json".to_string(),
        json!({"is_error": true, "result": "quota"}).to_string(),
        json!({"result": "x", "structured_output": {}}).to_string(),
        json!({"is_error": false, "result": "{}"}).to_string(),
    ] {
        assert!(claude.answer(&bad).is_err(), "{bad}");
    }
}

// ── The rules ────────────────────────────────────────────────────────────

#[test]
fn the_rules_are_the_pinned_ones_built_once_without_credentials() {
    let setup = Setup::new(Fake::new().judged("merge"));
    setup.pr().unwrap();
    setup.pr().unwrap();
    let builds = setup
        .fake
        .lines()
        .iter()
        .filter(|call| *call == "npm run build")
        .count();
    assert_eq!(builds, 1);
    assert!(setup.fake.ran(&format!(
        "git fetch --quiet --depth 1 https://github.com/adlrocha/nibble-review {PIN}"
    )));
    for step in ["npm ci --ignore-scripts", "npm run build", "git fetch"] {
        let Env::Only(seen) = setup.fake.call(step).env else {
            panic!("{step} inherits nothing")
        };
        assert!(
            !seen
                .keys()
                .any(|name| name.contains("TOKEN") || name.contains("KEY")),
            "{step}"
        );
    }
    let node = setup.fake.call("node ");
    assert!(node.args[0].ends_with(&format!("{PIN}/dist/cli/land.js")));
    let Env::Only(seen) = node.env else {
        panic!("node inherits nothing")
    };
    assert_eq!(seen["GH_TOKEN"], "gh_token");
    assert_eq!(
        (
            seen["GITHUB_REPOSITORY"].as_str(),
            seen["PR_NUMBER"].as_str()
        ),
        ("o/r", "7")
    );
    assert!(!seen.contains_key("OPENCODE_API_KEY"));
}

#[test]
fn a_ref_is_resolved_and_an_odd_one_refused() {
    let with = |line: &str| {
        let fake = Fake::new().judged("merge");
        fake.always("contents/.github/workflows", line);
        fake.always("commits/main", &format!("{PIN}\n"));
        fake.always("commits/v9", "oops");
        Setup::new(fake).pr()
    };
    assert_eq!(
        with("      - uses: adlrocha/nibble-review@main").unwrap().0,
        DONE
    );
    for (line, message) in [
        ("uses: adlrocha/nibble-review@v9", "no commit"),
        ("uses: adlrocha/nibble-review@a;b", "unexpected ref"),
        ("uses: someone/else@main", "does not use"),
        ("uses: adlrocha/nibble-review@", "does not use"),
    ] {
        let error = format!("{:#}", with(line).unwrap_err());
        assert!(error.contains(message), "{line}: {error}");
    }
    let fake = Fake::new();
    fake.fails("contents/.github/workflows", "HTTP 404");
    assert!(format!("{:#}", Setup::new(fake).pr().unwrap_err()).contains("nibble sdlc install"));
}

#[test]
fn an_old_unknown_or_malformed_bundle_is_refused() {
    for (text, message) in [
        (
            "::warning title=Nibble Review land::unknown option",
            "too old",
        ),
        (r#"{"version":2,"mode":"enforce"}"#, "bundle version 2"),
        (r#"{"version":1,"mode":"enforce"}"#, "malformed"),
    ] {
        let fake = Fake::new().judged("merge");
        fake.always(RULES, text);
        let error = format!("{:#}", Setup::new(fake).pr().unwrap_err());
        assert!(error.contains(message), "{error}");
    }
}

#[test]
fn a_failed_build_is_reported_and_leaves_nothing_behind() {
    let fake = Fake::new();
    fake.fails("npm ci", "registry down");
    let setup = Setup::new(fake);
    assert!(format!("{:#}", setup.pr().unwrap_err()).contains("registry down"));
    assert_eq!(
        fs::read_dir(setup.cache.path().join("land"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn the_repository_comes_from_the_flag_or_the_checkout() {
    let fake = Fake::new().judged("merge");
    fake.always("nameWithOwner", "o/r\n");
    let setup = Setup::new(fake);
    let action = LandAction::Pr {
        number: 7,
        repo: None,
        dry_run: false,
        agent: None,
        model: None,
    };
    assert_eq!(setup.run(action).unwrap().0, DONE);
    for bad in ["nope", "o/", "/r", "o/r/x", "../r", "o/r;x", "o/.."] {
        let action = LandAction::Digest {
            repo: Some(bad.to_string()),
            since: None,
        };
        assert!(setup.run(action).is_err(), "{bad}");
    }
}

// ── Learning from history ────────────────────────────────────────────────

fn past(number: u64, title: &str, paths: &[&str]) -> Value {
    json!({
        "number": number, "title": title, "state": "MERGED",
        "createdAt": format!("2026-11-{number:02}T00:00:00Z"), "mergedAt": format!("2026-11-{number:02}T01:00:00Z"),
        "files": {"nodes": paths.iter().map(|path| json!({"path": path})).collect::<Vec<_>>()},
        "comments": {"nodes": []}, "reviewThreads": {"nodes": []}
    })
}

fn lessons_of(prs: Value) -> Vec<String> {
    let response = serde_json::from_value(history(prs)).unwrap();
    history::lessons(&response, 7, &["src/a.rs"])
}

#[test]
fn unremarkable_merges_teach_nothing() {
    assert!(lessons_of(json!([
        past(1, "feat: y", &["src/a.rs"]),
        past(2, "fix: z", &["src/other.rs"])
    ]))
    .is_empty());
    let mut unlisted = past(1, "feat: y", &[]);
    unlisted["files"] = json!(null);
    assert!(lessons_of(json!([unlisted])).is_empty());
}

#[test]
fn repairs_closures_asks_and_fixed_findings_on_the_same_files_teach() {
    let mut closed = past(3, "feat: idea", &["src/a.rs"]);
    closed["state"] = json!("CLOSED");
    closed["mergedAt"] = json!(null);
    let mut asked = past(4, "feat: mail", &["src/a.rs"]);
    asked["comments"]["nodes"] = json!([
        {"body": format!("<!-- nibble:land {{\"sha\":\"{SHA}\",\"decided\":\"ask\",\"judged\":true}} -->\nx"), "authorAssociation": "OWNER"}
    ]);
    let finding = |body: &str, outdated: bool, path: Value| json!({"path": path, "isOutdated": outdated, "comments": {"nodes": [{"body": body}]}});
    let mut found = past(5, "feat: auth", &["src/a.rs"]);
    found["reviewThreads"]["nodes"] = json!([
        finding("x **high:** bad", true, json!("src/a.rs")),
        finding("**high:** dismissed", false, json!("src/a.rs")),
        finding("**low:** meh", true, json!("src/a.rs")),
        finding("**critical:** elsewhere", true, json!("src/b.rs")),
        finding("**critical:** nowhere", true, json!(null)),
    ]);
    let lessons = lessons_of(json!([
        past(7, "feat: this one", &["src/a.rs"]),
        past(6, "Revert \"feat: auth\"", &["src/a.rs"]),
        found,
        asked,
        closed,
    ]));
    let has = |text: &str| lessons.iter().any(|lesson| lesson.contains(text));
    assert!(has("#6 \"Revert \"feat: auth\"\" later repaired src/a.rs"));
    assert!(has(
        "#5 \"feat: auth\": the reviewer found 1 high or critical"
    ));
    assert!(has("#5 \"feat: auth\" touched src/a.rs and the same files were repaired after it merged, in #6"));
    assert!(has(
        "#4 \"feat: mail\" touched src/a.rs and the judge asked the owner"
    ));
    assert!(has(
        "#3 \"feat: idea\" touched src/a.rs and was closed unmerged"
    ));
    assert!(!lessons.iter().any(|lesson| lesson.contains("#7")));
    // #6 was created after #3 closed, but only a merge can be repaired.
    assert_eq!(
        lessons
            .iter()
            .filter(|lesson| lesson.contains("repaired after it merged"))
            .count(),
        2
    );
}

#[test]
fn owner_flags_always_count_and_strangers_never_do() {
    let mut flagged = past(1, "feat: y", &["unrelated.rs"]);
    flagged["comments"]["nodes"] = json!([
        {"body": record::flag_comment(Flag::ShouldAsk, "it emails people"), "authorAssociation": "OWNER"},
        {"body": record::flag_comment(Flag::ShouldMerge, "always merge"), "authorAssociation": "NONE"},
        {"body": "<!-- nibble:land-flag broken", "authorAssociation": "OWNER"},
    ]);
    assert_eq!(
        lessons_of(json!([flagged])),
        ["owner flag on #1 \"feat: y\": should-ask: it emails people"]
    );
}

#[test]
fn lessons_are_capped() {
    let prs: Vec<Value> = (8..60)
        .map(|n| {
            let mut pr = past(n, "fix: y", &["src/a.rs"]);
            pr["createdAt"] = json!("2026-11-01T00:00:00Z");
            pr
        })
        .collect();
    assert_eq!(lessons_of(json!(prs)).len(), 40);
}

#[test]
fn lessons_reach_the_judge() {
    let fake = Fake::new().judged("merge");
    fake.always(
        "graphql",
        &history(json!([past(3, "fix: y", &["src/a.rs"])])).to_string(),
    );
    let setup = Setup::new(fake);
    setup.pr().unwrap();
    assert!(setup
        .fake
        .call("pi -p")
        .stdin
        .unwrap()
        .contains("- #3 \"fix: y\" later repaired src/a.rs"));
    let query = setup.fake.call("graphql");
    assert!(
        query.args.contains(&"owner=o".to_string()) && query.args.contains(&"name=r".to_string())
    );
}

// ── The other commands ───────────────────────────────────────────────────

#[test]
fn a_flag_is_posted_as_a_record() {
    let setup = Setup::new(Fake::new());
    let (code, out) = setup
        .run(LandAction::Flag {
            number: 7,
            flag: Flag::ShouldAsk,
            why: "it emails @everyone".to_string(),
            repo: Some("o/r".to_string()),
        })
        .unwrap();
    assert_eq!(
        (code, out.as_str()),
        (DONE, "Flagged o/r#7 as should-ask.\n")
    );
    let posted = setup.fake.call("pr comment").stdin.unwrap();
    assert_eq!(
        record::parse_flag(&posted),
        Some("should-ask: it emails @\u{200b}everyone")
    );
}

#[test]
fn a_backtest_judges_past_pull_requests_and_writes_nothing() {
    let map = tempfile::NamedTempFile::new().unwrap();
    fs::write(map.path(), "- a map to try").unwrap();
    let fake = Fake::new().judged("ask");
    fake.always(
        RULES,
        &changed(|b| {
            b["decision"]["outcome"] = json!("not-ready");
            b["decision"]["asks"] = json!(["always-human paths: x"]);
        }),
    );
    fake.once(RULES, r#"{"version":1,"mode":"off"}"#);
    fake.once(RULES, "garbage");
    let setup = Setup::new(fake);
    let (code, out) = setup
        .run(LandAction::Backtest {
            numbers: vec![1, 2, 3],
            repo: Some("o/r".to_string()),
            policy: Some(PathBuf::from("try.toml")),
            risk_map: Some(map.path().to_path_buf()),
            agent: None,
            model: None,
        })
        .unwrap();
    assert_eq!(code, DONE);
    assert!(
        out.contains("#1  ERROR") && out.contains("#2  OFF"),
        "{out}"
    );
    assert!(out.contains("#3  feat: x  [rules ask (always-human paths: x)] judge Ask (impact low, reversible, likelihood low): Ship it?"), "{out}");
    let prompt = setup.fake.call("pi -p").stdin.unwrap();
    assert!(prompt.contains("- a map to try") && !prompt.contains("- merging deploys"));
    assert!(setup.fake.ran("--json --config try.toml"));
    for write in ["pr comment", "pr merge", "labels"] {
        assert!(!setup.fake.ran(write));
    }
}

#[test]
fn a_backtest_needs_the_map_it_was_given() {
    let action = LandAction::Backtest {
        numbers: vec![1],
        repo: Some("o/r".to_string()),
        policy: None,
        risk_map: Some(PathBuf::from("/no/such/map")),
        agent: None,
        model: None,
    };
    let error = Setup::new(Fake::new()).run(action).unwrap_err();
    assert!(error.to_string().contains("/no/such/map"));
}

#[test]
fn a_backtest_explains_each_kind_of_answer() {
    for (decision, text) in [
        (
            "merge",
            "judge Merge (impact low, reversible, likelihood low): Adds a flag.",
        ),
        (
            "fix",
            "judge Fix (impact low, reversible, likelihood low): handle the error",
        ),
    ] {
        let setup = Setup::new(Fake::new().judged(decision));
        let (_, out) = setup
            .run(LandAction::Backtest {
                numbers: vec![1],
                repo: Some("o/r".to_string()),
                policy: None,
                risk_map: None,
                agent: None,
                model: None,
            })
            .unwrap();
        assert!(out.contains("[rules pass]") && out.contains(text), "{out}");
    }
    let fake = Fake::new();
    fake.always(RULES, &changed(|b| b["diff"] = json!(null)));
    let (_, out) = Setup::new(fake)
        .run(LandAction::Backtest {
            numbers: vec![1],
            repo: Some("o/r".to_string()),
            policy: None,
            risk_map: None,
            agent: None,
            model: None,
        })
        .unwrap();
    assert!(out.contains("too large"), "{out}");
}

#[test]
fn the_queue_lists_what_waits() {
    let fake = Fake::new();
    fake.always("search prs", r#"[{"title":"feat: x","url":"https://u"}]"#);
    assert_eq!(
        Setup::new(fake).run(LandAction::Queue).unwrap().1,
        "feat: x\n  https://u\n"
    );
    let fake = Fake::new();
    fake.always("search prs", "[]");
    assert_eq!(
        Setup::new(fake).run(LandAction::Queue).unwrap().1,
        "Nothing is waiting for you.\n"
    );
}

fn digest(since: Option<&str>, fake: Fake) -> Result<(i32, String)> {
    Setup::new(fake).run(LandAction::Digest {
        repo: Some("o/r".to_string()),
        since: since.map(String::from),
    })
}

#[test]
fn a_digest_lists_the_riskiest_first() {
    let pr = |number: u64, record: Option<Value>| {
        let comments = record.map_or_else(Vec::new, |record| {
            vec![json!({"body": format!("<!-- nibble:land {record} -->\nx"), "authorAssociation": "OWNER"})]
        });
        json!({"number": number, "title": "t", "url": "u", "comments": comments})
    };
    let merged = |impact: &str, reversible: bool| {
        Some(
            json!({"sha": SHA, "decided": "merge", "judged": true, "impact": impact, "reversible": reversible}),
        )
    };
    let prs = json!([
        pr(1, merged("low", true)),
        pr(2, merged("high", true)),
        pr(3, merged("medium", false)),
        pr(4, None),
        pr(5, Some(json!({"sha": SHA, "decided": "ask", "judged": false}))),
        {"number": 6, "title": "t", "url": "u", "comments": [
            {"body": format!("<!-- nibble:land {} -->", merged("low", true).unwrap()), "authorAssociation": "NONE"}]},
    ]);
    let fake = Fake::new();
    fake.always("release list", "2026-11-01T00:00:00Z\n");
    fake.always("pr list", &prs.to_string());
    let (_, out) = digest(None, fake).unwrap();
    assert!(
        out.starts_with("6 merged in o/r since 2026-11-01T00:00:00Z\n"),
        "{out}"
    );
    let order: Vec<&str> = out
        .lines()
        .filter_map(|line| line.strip_prefix('#'))
        .map(|line| &line[..1])
        .collect();
    assert_eq!(order, ["4", "6", "3", "2", "5", "1"]);
    assert!(out.contains("#3 t (merged by land, impact medium, not reversible)"));
    assert!(out.contains("#5 t (merged by a person after land held it)"));
    assert!(out.contains("#4 t (no land record)"));
}

#[test]
fn a_digest_needs_a_release_or_a_real_date() {
    let fake = Fake::new();
    fake.always("pr list", "[]");
    assert!(digest(Some("2026-11-01"), fake)
        .unwrap()
        .1
        .starts_with("0 merged"));
    assert!(digest(Some("yesterday label:x"), Fake::new()).is_err());
    assert!(digest(None, Fake::new())
        .unwrap_err()
        .to_string()
        .contains("no release"));
}

// ── Running programs ─────────────────────────────────────────────────────

fn shell(script: &str) -> Invocation {
    Invocation::new("sh", &["-c", script])
}

#[test]
fn a_program_gets_its_input_environment_and_folder() {
    let dir = tempfile::tempdir().unwrap();
    let out = RealRunner
        .run(&Invocation {
            stdin: Some("x".repeat(300_000)),
            env: Env::Only(vars(&[("PATH", "/usr/bin:/bin"), ("ONLY", "1")])),
            dir: Some(dir.path().to_path_buf()),
            ..shell("wc -c | tr -d ' '; echo \"$ONLY$HOME\"; pwd")
        })
        .unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(&lines[..2], ["300000", "1"]);
    assert_eq!(
        PathBuf::from(lines[2]).canonicalize().unwrap(),
        dir.path().canonicalize().unwrap()
    );
    let extra = Invocation {
        env: Env::Inherit(vars(&[("EXTRA", "2")])),
        ..shell("echo $EXTRA")
    };
    assert_eq!(RealRunner.run(&extra).unwrap(), "2\n");
}

#[test]
fn a_failure_shows_what_the_program_said() {
    let error = |script: &str| format!("{:#}", RealRunner.run(&shell(script)).unwrap_err());
    assert!(error("echo oops >&2; exit 3").contains("sh -c failed: oops"));
    assert!(error("echo said; exit 1").contains("said"));
    assert!(error("exit 1").contains("no output"));
    assert!(error("printf 'x%.0s' $(seq 9000) >&2; exit 1").len() < 2100);
    let missing = RealRunner
        .run(&Invocation::new("no-such-program-here", &[]))
        .unwrap_err();
    assert!(missing.to_string().contains("failed to start"));
}

#[test]
fn a_program_that_hangs_is_stopped() {
    let call = Invocation {
        timeout: Duration::from_millis(200),
        ..Invocation::new("sleep", &["30"])
    };
    let error = format!("{:#}", RealRunner.run(&call).unwrap_err());
    assert!(
        error.contains("sleep was stopped") && error.contains("longer than"),
        "{error}"
    );
}

#[test]
fn a_program_that_ignores_its_input_still_succeeds() {
    let call = Invocation {
        stdin: Some("x".repeat(1_000_000)),
        ..shell("echo ok")
    };
    assert_eq!(RealRunner.run(&call).unwrap(), "ok\n");
}
