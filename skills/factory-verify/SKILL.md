---
name: factory-verify
description: AI Factory — Verification stage. Testing, adversarial analysis, and risk scoring. Covers Verify and Audit stages.
---

# Stages: Verify + Audit

Checklists below are **non-exhaustive starting points**. The agent should explore any additional attack vectors, edge cases, or quality concerns relevant to the specific project, language, framework, or domain. Use judgment — a web API has different risks than a CLI tool or an embedded system.

## Verify (all tiers)

Tests are written from the **spec, not the implementation**.

### Quick Tier
- Run existing tests (must still pass)
- Add targeted tests for the change: at least 1 test per AC, 1 per INV
- No coverage target required

### Standard/Full Tier
- **AC coverage**: ≥1 test per acceptance criterion, named `AC-N: description`
- **INV coverage**: ≥1 test per invariant, named `INV-N: description`
- **Boundaries**: empty, min, max, off-by-one, null/undefined, large input
- **Error paths**: every error case from the spec's interface table
- **State transitions** (if applicable): valid, invalid, missing, recovery transitions
- **Coverage target**: ≥80% line coverage on new code (Full tier: ≥90%)

### Process
1. Map each AC to a test.
2. Map each INV to a test that tries to violate it.
3. Write boundary and error path tests.
4. Run all tests. Fix failures:
   - Bug in code → fix code, keep test
   - Bug in test → fix test
   - Bug in spec → document discrepancy, fix whichever is wrong
5. Optional smoke test: exercise the feature end-to-end as a user would.

### Gate Criteria
- Every AC-N has ≥1 test
- Every INV-N has ≥1 test
- All tests green
- No skipped tests

---

## Audit (Standard and Full tiers only)

**Standard tier**: Run the 7 attack vectors below. Fix Critical/High findings inline. List findings in a brief summary (no formal report). Only escalate to human if unfixed Critical/High remains.

**Full tier**: Run the 7 attack vectors. Produce a formal audit report at `.nibble/factory/reports/audit/YYYY-MM-DD_<slug>.md` with findings, severity, and fix suggestions. Score each function on 5 dimensions for risk. Present Critical/High to human at QA Gate.

### Attack Surface Exploration (non-exhaustive)

These are starting points, not a ceiling. Add vectors based on the project's domain.

1. **Spec Compliance** — Every interface implemented? Missing error cases? Extra behavior not in spec?
2. **Invariant Violations** — Can you craft input to break INV-N? Concurrent access? Resource exhaustion?
3. **Boundary & Input Attacks** — Integer overflow, string attacks (empty/long/unicode/null bytes), off-by-one, race conditions.
4. **Security** — Injection, auth bypass, data exposure, DoS, input validation. (Skip if feature has no trust boundaries.)
5. **Error Recovery** — Unexpected exception types? Hung network calls? Partial transactions? Resource cleanup?
6. **Edge Cases** — Idempotency, out-of-order operations, unexpected state, empty collections, circular refs.
7. **Mutation Testing** — If you changed `<` to `<=`, removed an error check, or swapped two operations — would a test fail?
8. **Domain-Specific** — Whatever else matters for this project (e.g., concurrency models, serialization edge cases, protocol compliance, resource leaks, compatibility constraints).

### Findings Format (Standard tier — brief)

```
## Audit Findings
- FIND-1: [Critical/High/Medium/Low] <description> → Fixed / Accepted
- FIND-2: ...
```

### Full Tier: Audit Report + Risk Scoring

Produce report at `.nibble/factory/reports/audit/YYYY-MM-DD_<slug>.md`:

```markdown
# Audit Report: <Name>
## Findings
### FIND-N: <Title>
- **Severity**: Critical/High/Medium/Low
- **Category**: Spec Violation / Invariant Break / Security / Edge Case / Test Gap
- **Description**: ...
- **Fix**: ...
- **Status**: Resolved / Unresolved / Accepted Risk

## Risk Scoring
| Function | File:Lines | C | D | S | E | T | Risk | Level |
|----------|-----------|---|---|---|---|---|------|-------|
```

### Risk Scoring (Full tier only)

Score each new/modified function on 5 dimensions (1-5). Anchors:
```
C=Complexity:     1=straight-line 2=simple-ifs 3=nested-ifs 4=complex-loops 5=recursive/concurrent
D=Dependency:     1=pure 2=stdlib 3=one-ext-lib 4=multiple-libs/services 5=critical-external-dep
S=Security:       1=none 2=indirect-input 3=direct-input 4=auth/crypto 5=secrets/financial
E=ErrorHandling:  1=none 2=simple-return 3=multiple-recovery 4=rollback 5=distributed-failure
T=TestGap:        1=100% 2=>90% 3=70-90% 4=50-70% 5=<50%
Risk = C+D+S+E+T  |  5-10=Low  11-15=Medium  16-20=High  21-25=Critical
```
Increase S/E/T by +1 per unresolved adversarial finding (cap at 5). Present Critical/High at QA Gate.

### Gate Criteria (Audit)
- All 7 vectors checked (or explicitly scoped out with reason)
- No Critical findings Unresolved
- High findings resolved or Accepted Risk with justification

---

## Test File Organization

Follow the project's existing test structure. If none exists:
```
tests/
  unit/<feature>.test.<ext>
  integration/<feature>.test.<ext>
  fixtures/<feature>.<ext>
```
