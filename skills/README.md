# Skills — AI Factory Pipeline

This directory contains the skill files for the **AI Factory pipeline** — a structured,
chip-design-inspired development workflow built into every nibble sandbox.

## What is the AI Factory?

The AI Factory is a mandatory multi-stage pipeline that coding agents follow for every
non-trivial task. It is inspired by chip manufacturing: like silicon, code that skips
verification steps ships with hidden defects. The pipeline forces the agent to plan
before coding, test from the spec (not the implementation), red-team its own work, score
risk objectively, and present only what matters to the human for final approval.

The human only intervenes at the **QA Gate** — and only for sections scored as Critical
or High risk.

## Pipeline stages

```
 SPEC  ──▶  IMPLEMENT  ──▶   TDD    ──▶  ADVERSARIAL ──▶  RISK SCORE  ──▶  QA GATE
(Blueprint)  (Synthesis)    (DV)        (Red Team)       (Analysis)       (Tape-out)
```

| Stage | Skill | What the agent does |
|-------|-------|---------------------|
| 0 — Spec | `factory-spec` | Writes a blueprint: interfaces, invariants, acceptance criteria. No coding yet. |
| 1 — Implement | `factory-implement` | Codes from the blueprint bottom-up with runtime assertions for every invariant. |
| 2 — TDD | `factory-tdd` | Writes tests **from the blueprint**, not from the code. Red→green discipline. |
| 3 — Adversarial | `factory-adversarial` | Red-teams the implementation across 7 attack vectors. Produces a findings report. |
| 4 — Risk Score | `factory-risk-score` | Scores every changed function on 5 dimensions (C+D+S+E+T). Flags critical sections. |
| 5 — QA Gate | `factory-qa-gate` | Presents Critical/High items to the human one at a time for approve/reject/changes. |

There is also a **lessons-learned log** (`factory-lessons`) that every stage reads at
startup and appends to when something slips through.

## Directory layout

```
skills/
  factory-pipeline/SKILL.md      # Pipeline manifest — read first
  factory-spec/SKILL.md          # Stage 0
  factory-implement/SKILL.md     # Stage 1
  factory-tdd/SKILL.md           # Stage 2
  factory-adversarial/SKILL.md   # Stage 3
  factory-risk-score/SKILL.md    # Stage 4
  factory-qa-gate/SKILL.md       # Stage 5
  factory-lessons/SKILL.md       # Continuous improvement log
```

Each file is a plain markdown document with a YAML frontmatter header (`name`,
`description`) that agents use to discover and load it via the `skill` tool.

## How agents use skills

Skills are loaded on-demand via the agent's `skill` tool:

```
load skill `factory-spec`
```

The agent reads the skill file and follows the instructions for that stage before
proceeding to the next one. Skills are plain markdown — no proprietary format, no
tool-specific syntax. Any agent (Claude Code, OpenCode, or future agents) can use them.

## Installation

`install.sh` copies the skills to `~/.claude/skills/factory-<name>/SKILL.md`, which is
the path both Claude Code and OpenCode (in Claude Code compat mode) scan automatically.

A global `~/.config/opencode/AGENTS.md` is also written, giving OpenCode the factory
pipeline instructions as part of its global system prompt.

## Artifact locations (inside sandbox repos)

The pipeline writes artifacts into `.nibble/factory/` in the project repo:

```
.nibble/factory/
  blueprints/      # Spec blueprints (Stage 0 output)
  reports/
    adversarial/   # Red team findings (Stage 3 output)
    risk/          # Risk score tables (Stage 4 output)
    qa/            # QA gate decisions (Stage 5 output)
```

## When to skip the pipeline

The pipeline is mandatory for non-trivial changes. It may be skipped (with human
confirmation) for:

- Typo or comment fixes
- Single-value config tweaks
- Pure formatting / linting changes
- Documentation-only changes

## Continuous improvement

After every pipeline run, new lessons are appended to `factory-lessons/SKILL.md` in the
appropriate category (Spec Gaps, Implementation Bugs, Testing Gaps, Adversarial Blind
Spots, QA Catches, Risk Scoring Misses, Process Improvements). This log is read at the
start of every stage so past mistakes prevent future ones.
