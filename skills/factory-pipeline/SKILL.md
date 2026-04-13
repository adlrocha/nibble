---
name: factory-pipeline
description: AI Factory Pipeline manifest — read this first to understand the full Spec→Implement→TDD→Adversarial→Risk Score→QA Gate pipeline
---

# AI Factory — Pipeline Manifest

A structured development pipeline inspired by chip manufacturing. Every feature goes
through six stages before reaching production. The human only reviews what matters.

## Pipeline

```
 SPEC  ──▶  IMPLEMENT  ──▶   TDD    ──▶  ADVERSARIAL ──▶  RISK SCORE  ──▶  QA GATE
(Blueprint)  (Synthesis)    (DV)        (Red Team)       (Analysis)       (Tape-out)
```

| # | Stage | Artifact In | Artifact Out | Gate Criteria |
|---|-------|-------------|--------------|---------------|
| 0 | Spec | Task description | `blueprints/YYYY-MM-DD_<feature>.md` | All sections filled, invariants stated, acceptance criteria testable |
| 1 | Implement | Blueprint | Source code | Code compiles, lint passes, typecheck passes |
| 2 | TDD | Blueprint + Code | Test suite (red→green) | All acceptance criteria covered, coverage target met |
| 3 | Adversarial | Code + Tests | `reports/adversarial/YYYY-MM-DD_<feature>.md` | All attacks documented, no critical unresolved findings |
| 4 | Risk Score | Code + Adversarial report | `reports/risk/YYYY-MM-DD_<feature>.md` | Every function scored, critical sections identified |
| 5 | QA Gate | Risk report | `reports/qa/YYYY-MM-DD_<feature>.md` + human stamp | All critical sections approved by human |

## How to Use

### Automatic (recommended)

When factory is enabled, every coding task follows this pipeline. Load each stage's
skill as you enter it. No manual intervention needed until the QA Gate.

### Manual

Load a specific stage's skill at any time via the skill tool, e.g. `factory-spec`.

### Bypass

For trivial changes (typo fixes, config updates), the human can instruct the agent to skip
the factory pipeline. The agent should confirm before skipping. The agent should also be able
to evaluate when is worth running the pipeline and when to skip it.

### Retrospective Mode

When working on **existing code** (bug fix, refactor, small addition) rather than a new
feature, adapt the pipeline:

- **Spec**: Document what exists + what's changing and why. Skip sections that don't apply.
- **TDD**: Confirm existing tests still pass + add tests specifically for the change.
- **Adversarial / Risk Score**: Scope to the changed functions only, not the whole module.

The gate criteria still apply — don't skip them, just scope them to the delta.

### Scale Guidance

Match report depth to feature size. Overhead should be proportional to risk.

| Feature size | Functions changed | Report style |
|---|---|---|
| Small | 1–4 | Collapse adversarial + risk into a single brief section per function |
| Medium | 5–15 | Full templates, but skip Low-risk sections in the risk report |
| Large | 16+ | Full templates for all sections |

When in doubt, err toward brevity — a concise finding is more useful than a padded one.

## Skills

| Skill name | Stage | Purpose |
|------------|-------|---------|
| `factory-spec` | Blueprint Design | Write structured feature specifications |
| `factory-implement` | Synthesis | Implement code from spec |
| `factory-tdd` | Design Verification | Write red/green TDD tests |
| `factory-adversarial` | Red Team | Find holes in the implementation |
| `factory-risk-score` | Analysis | Score risk, flag critical sections |
| `factory-qa-gate` | Tape-out Gate | Present findings for human approval |
| `factory-lessons` | All stages | Read the continuous improvement log |

## Principles

1. **Spec is the contract** — Like a chip blueprint, the spec is the source of truth. Code
   that doesn't match the spec is wrong, even if it works.

2. **Tests prove correctness** — Tests are written from the spec, not from the
   implementation. They verify the contract, not the code.

3. **Adversarial thinking catches what tests miss** — No test suite is complete. The
   adversarial phase looks for gaps using attacker mindset.

4. **Risk scores focus human attention** — Not all code is equally important. Risk scoring
   ensures the human reviews only what matters.

5. **Every failure teaches** — When something slips through, it's documented in the
   lessons-learned log and becomes part of every future run.

6. **Portability over lock-in** — Skills are plain markdown. Any coding agent can use them.
   No proprietary formats, no tool-specific syntax.

## Artifact Locations

```
.nibble/
  factory/
    .gitignore               # ignores adversarial/ and risk/ (process artifacts)
    blueprints/              # COMMITTED — design decisions, long-term value
      YYYY-MM-DD_<feature>.md
    reports/
      adversarial/           # NOT committed — stale after findings are fixed
        YYYY-MM-DD_<feature>.md
      risk/                  # NOT committed — scores are pre-fix, stale after merge
        YYYY-MM-DD_<feature>.md
      qa/                    # COMMITTED — audit trail of human approval decisions
        YYYY-MM-DD_<feature>.md
```

Use `YYYY-MM-DD` = the date the pipeline run started. Keep the same date across all
artifacts for one feature run so they sort together.

## Continuous Learning

After every factory run, load `factory-lessons` and check:

- Did the adversarial phase find something the spec missed? → Add to Spec Gaps
- Did a bug survive testing? → Add to Implementation Bugs
- Did the QA gate catch something? → Add to QA Catches
- Was a stage redundant or missing? → Add to Process Improvements
