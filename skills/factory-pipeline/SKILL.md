---
name: factory-pipeline
description: AI Factory — 3-tier development pipeline. Load this first to classify the task and determine which stages to run.
---

# AI Factory Pipeline

## Startup

1. Load `factory-lessons` and read the lessons log.
2. Classify the task (Quick / Standard / Full) using the criteria below.
3. State your classification to the human. They can override.
4. Execute stages in order per tier.

## Tier Classification

| Criterion | Quick | Standard | Full |
|-----------|-------|----------|------|
| Functions changed | ≤ 3 | 4–15 | 16+ |
| Security-sensitive? | No | — | Yes |
| Public API / interface change? | No | — | Yes |
| New state machine / complex flow? | No | — | Yes |
| Database schema change? | No | — | Yes |

**Rules:** If ANY criterion hits Full → run Full. If majority Quick and none Full → run Quick. Otherwise → Standard.

## Pipeline by Tier

```
Quick:     SPEC ──▶ IMPLEMENT ──▶ VERIFY ──────────────────────── done*
Standard:  SPEC ──▶ IMPLEMENT ──▶ VERIFY ──▶ AUDIT ───────────── done*
Full:      SPEC ──▶ IMPLEMENT ──▶ VERIFY ──▶ AUDIT ──▶ QA GATE ─ done

* QA Gate fires for ANY tier if Audit finds unfixed Critical/High findings.
```

| Stage | Quick | Standard | Full |
|-------|-------|----------|------|
| Spec | Brief template | Standard template | Full template |
| Implement | Code + compile + lint | + Invariant assertions (INV-N) | + DFT: injectable deps, pure logic separated |
| Verify | Existing tests + targeted tests for the change | AC/INV/boundary/error path tests, ≥80% coverage | Full TDD, strict coverage targets |
| Audit | Skip | Adversarial scan: explore attack surface, fix findings inline, no formal report | Full adversarial report + risk scoring |
| QA Gate | Only if Critical/High finding | Only if Critical/High finding | Always (formal gate) |

## Stage Transitions

- Load the appropriate skill at each stage: `factory-spec`, `factory-verify`, `factory-qa-gate`.
- Implementation guidance is inline below (no separate skill needed).

## Implementation Rules (all tiers)

- Encode every blueprint invariant as an assertion: `assert!(condition, "INV-N: description")`
- Follow existing codebase conventions (naming, error handling, module structure)
- Pure functions preferred — separate business logic from side effects
- Dependencies injected, not hardcoded (for Standard/Full tiers)
- Code must compile, lint passes, typecheck passes before proceeding

## Retrospective Mode

For changes to **existing code** (bug fix, refactor): scope each stage to the delta only. Don't re-spec unchanged code. Confirm existing tests still pass. Scope audit to changed functions.

## Scale Guidance for Reports

| Functions changed | Report depth |
|-------------------|-------------|
| 1–4 | Findings-only: list issues found, skip the "no issues found" sections |
| 5–15 | Standard templates, skip Low-risk sections |
| 16+ | Full templates for all sections |

## Artifacts

```
.nibble/factory/
  blueprints/YYYY-MM-DD_<slug>.md    # COMMITTED
  reports/
    audit/YYYY-MM-DD_<slug>.md       # NOT committed (adversarial + risk merged for Full tier)
    qa/YYYY-MM-DD_<slug>.md          # COMMITTED
```

## Lessons

Load `factory-lessons` once at pipeline start. At pipeline end, append new lessons if something slipped through a stage.

## Principles

1. **Spec is the contract** — code that doesn't match the spec is wrong.
2. **Tests prove correctness** — written from the spec, not the implementation.
3. **Adversarial thinking catches what tests miss** — attacker mindset finds gaps.
4. **Risk scores focus human attention** — only review what matters.
5. **Every failure teaches** — lessons log prevents repeat mistakes.
