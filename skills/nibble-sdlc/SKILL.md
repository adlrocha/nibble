---
name: nibble-sdlc
description: Deliver a spec as a reviewed pull request. Use when asked to build a feature, fix, or spec in a repository that has the nibble sdlc flow installed, or when asked to drive a PR to LGTM with nibble-review. Covers checking the spec, building, a fresh review before pushing, and looping on review findings.
---

# Nibble SDLC

Take a spec to a pull request that nibble-review has approved, without a person in the middle. Stop only where this skill says to. Where the repository's `AGENTS.md` is stricter, it wins.

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
- A push triggers a re-review. A reply alone doesn't, so after a reply-only resolution post `@nibble-review` and wait for the result.

**Stop after 5 review rounds** and hand off with what is still unresolved. More rounds usually mean the change or the spec needs rethinking. A critical finding you believe is wrong also goes to the user: reply with your evidence, then stop.

## 5. Hand off

Read the PR's `## Risk` comment from `land`. Report its result, the verdict, the CI state, and anything the user must decide.

Land is in shadow mode, so it only reports. Don't merge, enable auto-merge, or post `@nibble-review land`. Merging belongs to the user unless the task says otherwise.
