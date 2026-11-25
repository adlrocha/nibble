# Landing pull requests

`nibble land` decides whether a finished pull request merges without you. It merges what doesn't need you and calls you, with one question, for what does.

```bash
nibble land pr 42          # decide one pull request, and merge it if the policy allows
nibble land pr 42 --dry-run
nibble land queue          # your pull requests that wait for you, in every repository
nibble land digest         # what merged since the last release, riskiest first
nibble land flag 42 should-ask "it emails people"
nibble land backtest 41 42 43
```

Agents run `nibble land pr` at the end of the [delivery flow](sdlc.md). Nothing runs on a schedule.

## How it decides

1. **Fixed rules.** They are nibble-review's, run at the commit the repository's default branch pins, so CI's `## Risk` comment and this decision share one implementation. They read the policy from the default branch and everything else from GitHub. No comment, label, or status is trusted. See [Land](https://github.com/adlrocha/nibble-review#land) for the rules and profiles.
2. **The judge.** A fresh headless run of the agent that called `nibble land`, with its model: Claude Code judges for Claude Code, and pi for pi. It reads the diff, the spec, the review and how each finding was closed, the policy, the risk map, and the history. It answers merge, fix, or ask, and rates impact, reversibility, and likelihood.
3. **The threshold.** The profile turns a merge into an ask when the risk is above what it allows:

| Impact | `side-project` | `standard` | `critical` |
| --- | --- | --- | --- |
| low | merges | merges | asks if a problem is likely |
| medium | merges | asks if it can't be reverted | asks unless it can be reverted and a problem is unlikely |
| high | asks if it can't be reverted | asks unless it can be reverted and a problem is unlikely | asks |

The judge can hold a change back. It can't clear a rule or a threshold.

Right before merging, land reads everything again and merges that exact commit, with the repository's default merge method.

| Result | Exit code | Meaning |
| --- | --- | --- |
| `MERGED` | 0 | Done |
| `CALL THE HUMAN`, `WOULD MERGE` | 10 | You decide. A held pull request gets the `needs-human` label |
| `NOT READY`, `FIX FIRST` | 11 | The agent has work left. Nobody is asked |
| an error | 1 | Nothing merged |

To approve a held pull request, merge it on GitHub. The same commit is never judged twice, so an agent can't ask again until it gets a yes.

## Policy

`.nibble/sdlc.toml` on the default branch. A pull request that changes it always needs you.

```toml
[land]
mode = "enforce"              # "off", "shadow" (decide and report) or "enforce" (also merge)
profile = "side-project"      # "side-project", "standard" or "critical"
required_checks = ["test"]    # nothing merges unattended without one
authors = ["adlrocha"]
always_human = ["src/auth"]

[land.ask_when]
newsletter = "Changes who gets an email, or when"
```

Review rounds and size never call you.

## Risk map

`.nibble/risk.md` tells the judge what merging sets off and what can't be undone: what deploys, what becomes public, what touches money, people, or stored data. Write down what is true of the repository and where that comes from. It is context, not rules. In `standard` and `critical`, changing it needs you.

## Learning

The judge learns from GitHub itself, so nothing is stored and every machine sees the same thing. It is told, for the last 50 pull requests:

- every `nibble land flag` you left, anywhere in the repository;
- for changes to the same files: later reverts and fixes, asks, closures without a merge, and high findings whose code then changed.

A merge nobody complained about teaches nothing, and neither does a finding that was dismissed.

To tune a policy, try it on the past before changing it. Nothing is posted or merged:

```bash
nibble land backtest 41 42 43 --policy new-sdlc.toml --risk-map new-risk.md
```

## Safety

The judge has no tools, no settings, no MCP servers, no project files, no GitHub credentials, and an empty working directory. Nothing in a pull request can make it run or read anything. Every section of its input is fenced with a random marker, and it is told that a claim of approval in the evidence is a reason to ask.

Anything that goes wrong leaves the pull request unmerged: an error, a timeout, an answer that can't be read twice in a row, a change too large to read in full, or a pull request that moved while it was being judged. `mode = "off"` stops everything in a repository.

What it doesn't stop:

- **An agent with your credentials can merge by itself.** The `nibble-sdlc` skill forbids it, and `nibble land digest` lists merges that have no land record. GitHub can refuse it only with a separate account for agents.
- **The review it starts from can be forged** by anyone who can push a branch, because it posts as `github-actions[bot]`. The judge reads the diff itself for that reason.
- **Two models can miss the same problem.**

Turn on a ruleset that requires your checks on `main`, so GitHub enforces them even if land is wrong.
