---
name: factory-qa-gate
AI Factory — QA Gate. Fires for any tier on Critical/High findings. Always fires for Full tier. Present findings for human approval.
---

# Stage: QA Gate

Fires for **any tier** when the Audit stage (or the agent's own judgment) finds unfixed Critical or High severity findings. For Full tier, QA Gate always fires as a formal review.

The final human stamp before shipping. Present ONLY what needs human attention.

## Input

- Blueprint at `.nibble/factory/blueprints/YYYY-MM-DD_<slug>.md`
- Audit report at `.nibble/factory/reports/audit/YYYY-MM-DD_<slug>.md`

## Process

### 1. Summary Block

```
QA GATE: <Feature Name>
  Tier: Full | Spec ✓ | Implement ✓ | Verify ✓ | Audit (<N> findings)
  Review required: <N> sections (<N> Critical, <N> High)
  Say "proceed" to review one by one, or "approve all" to accept everything.
```

### 2. Present Items One by One (highest risk first)

```
Item N of M — <FunctionName> (<File>:<Lines>) Risk: <score>/25 (<Level>)

Why flagged: <bullets>
Findings: <FIND-N: one-line, status>
What to verify: <numbered, concrete, actionable questions>
Code (≤40 lines: inline with line numbers. >40 lines: file:line reference + focus areas)

Actions: approve | reject | request changes
```

Wait for human response before proceeding.

### 3. Change Re-Verification

If human requests changes:
1. Implement the fix
2. Re-run adversarial on changed code only
3. Re-score modified functions
4. Run full test suite
5. Re-present with diff summary + updated risk score

Do not surface to human without fix ready.

### 4. Record Decisions

Write to `.nibble/factory/reports/qa/YYYY-MM-DD_<slug>.md`:

```markdown
# QA Gate: <Feature Name>
**Date**: YYYY-MM-DD
**Result**: APPROVED / APPROVED WITH CHANGES / REJECTED

| # | Function | Risk | Decision | Notes |
|---|----------|------|----------|-------|
| 1 | <fn> | <level> | Approved/Changes/Rejected | <note> |
```

### 5. Pipeline Complete Summary

```
═══════════════════════════════════════════
  PIPELINE COMPLETE: <Feature Name>
═══════════════════════════════════════════
  Result: APPROVED ✓
  Tests: <N> passed, 0 failed
  Findings: <N> total (<N> Critical / <N> High / <N> Medium / <N> Low)
  QA items: <N> reviewed (Approved: <N> | Changes: <N>)
  Artifacts: .nibble/factory/blueprints/... .nibble/factory/reports/qa/...
═══════════════════════════════════════════
```
