---
name: nibble-sdlc
description: How code changes ship in any repository that contains `.nibble/sdlc.toml`. Use for every feature, fix, refactor, or spec in such a repository, and whenever asked to drive a PR to LGTM with nibble-review. Covers checking the spec, building, a fresh review before pushing, looping on review findings, and letting `nibble land` merge or call the user.
---

# Nibble SDLC

Take a spec to a merged pull request without a person in the middle. Call the user only when this skill or `nibble land` says to. Where the repository's `AGENTS.md` is stricter, it wins.

A repository uses this flow when `.nibble/sdlc.toml` is on its default branch; `nibble sdlc install` puts it there, and nothing is added to `AGENTS.md`. When the user asks how the flow works or how to set it up, run `nibble sdlc runbook` and answer from it. Read the file: `[land] mode` says whether land merges (`enforce`), only reports (`shadow`), or is off.

## 1. Check the spec

Read the spec and the code it touches, then state what you will build, what you will not, and how you will know it works.

If a requirement is missing or can be read two ways, and the answer changes what you build, stop and ask. Otherwise go ahead and record your assumptions in the PR.

Keep the spec where the reviewer can check against it: a GitHub issue the PR closes, or the PR body. The reviewer can't see other trackers, so copy the acceptance criteria across.

## 2. Build

Use one branch per change, and keep the change small enough to review. Follow the repository's `AGENTS.md`. Add tests with the code, and run its lint, format, and test commands before pushing.

## 3. Review it fresh before pushing

Ask a sub-agent with no knowledge of this session to review the diff against the spec. Fix what it finds. The session that wrote the code is a poor judge of it, and each issue caught here saves a CI review round.

## 4. Open the PR and loop

Open the PR ready for review, because drafts skip the review. Then repeat until nibble-review LGTMs the current head and required checks are green:

- Read every finding and resolve each one, by fixing it or by replying with evidence that it doesn't apply. The reviewer can be wrong, so a well-supported reply is a valid answer.
- Resolve each thread on GitHub once it is settled. Land doesn't merge over an open thread.
- A push triggers a re-review. A reply alone doesn't, so after a reply-only resolution post `@nibble-review re-review` and wait for the result.

There is no round limit, so don't stop on round count alone. Do look at what the rounds are telling you. When findings keep coming back in the same place, or each fix opens the next problem, stop patching and ask a sub-agent with no knowledge of this session whether they share one cause in the design:

- If a better design still meets the spec, build it and carry on. That needs no one.
- If it would change the spec, a public contract, or an earlier decision, call the user with the problem, the options, and the one you recommend.

For a finding you believe is wrong, reply with your evidence and carry on. Land's judge reads both sides.

## 5. Land

Run `nibble land pr <number>` from the repository and do what it says:

| It says | Do |
| --- | --- |
| `MERGED` | Report it and stop |
| `NOT READY` or `FIX FIRST` | Do the listed work, get a new LGTM, and run it again. If the judge is wrong, ask the user with both sides and stop |
| `CALL THE HUMAN` | Ask the user exactly what it lists, with the PR link, and stop |
| `WOULD MERGE` | Nothing merged. Report it and stop |
| An error | Check whether the PR is still open, then run it once more. If it fails again, tell the user and stop |

It runs a judge with the same agent and model as you. Don't pick another with `--agent` or `--model`, run it with `--dry-run`, brief it, or push an empty change to get a second opinion: a commit it has held stays held until the user merges it. Don't merge any other way, enable auto-merge, edit `.nibble/sdlc.toml` to get past it, or use `nibble land flag`. Flags are the user's.
