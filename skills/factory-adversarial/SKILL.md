---
name: factory-adversarial
description: AI Factory Stage 3 — Red Team. Switch to attacker mindset and find holes in the implementation across 7 attack vectors. Produces an adversarial report.
---

# Stage 3: Adversarial (Red Team)

You are in the **Adversarial** phase of the AI Factory pipeline. Your job is to find
holes in the implementation — the equivalent of security review and design-for-test
(DFT) analysis in chip development.

**Switch your mindset.** You are no longer the builder. You are the attacker. Your goal
is to break the code, find what was missed, and expose every assumption that doesn't hold.

**Before starting: load `factory-lessons` and read the lessons-learned log.**

## Input

- Blueprint at `.nibble/factory/blueprints/YYYY-MM-DD_<feature>.md`
- Implementation code
- Test suite
- Lessons-learned log (load `factory-lessons`)

## Attack Vectors

### 1. Spec Compliance

The implementation must match the blueprint exactly. Check:

- [ ] Every interface defined in the blueprint is implemented
- [ ] Every error case in the interface table is handled
- [ ] State machine transitions match the blueprint exactly
- [ ] No behavior exists in code that isn't in the spec (feature creep)
- [ ] No spec requirement is missing from the code

**Common violations:**

- Missing error case handling
- Extra behavior not in spec (may indicate scope creep or side-channel effects)
- Different default values than spec
- Changed interface signatures

### 2. Invariant Violations

Try to violate every invariant (INV-1, INV-2, etc.):

- Can you craft input that breaks INV-1?
- What happens under concurrent access?
- What happens under resource exhaustion (disk full, memory pressure)?
- What happens with interleaved operations?
- What if a dependency returns unexpected values?
- Any additional blind spot we may be missing?

**Systematic approach:**

```
For each invariant INV-N:
  1. Read the assertion in code
  2. Identify what conditions could make it false
  3. Write code to trigger those conditions
  4. Verify whether the assertion catches it
```

### 3. Boundary Attacks

Go beyond normal boundary testing:

- **Integer overflow/underflow**: What happens at MAX_INT, MIN_INT?
- **String attacks**: Empty, very long, unicode, null bytes, special chars
- **Timing attacks**: Can operation order reveal information?
- **Resource limits**: What happens at limits (max connections, max file size)?
- **Race conditions**: Can concurrent access cause data corruption?
- **Off-by-one**: Fencepost errors in loops, indexes, ranges
- Any other edge case or boundary

### 4. Security Analysis

For every input that crosses a trust boundary:

- **Injection**: SQL, command, path traversal, template injection
- **Authentication/Authorization**: Can you access without proper creds?
- **Data exposure**: Are secrets logged? Error messages leak internals?
- **Denial of service**: Can you cause excessive resource consumption?
- **Input validation**: Are all external inputs validated and sanitized?
- More complex and advanced state-of-the-art attacks?

### 5. Error Recovery

Test that error handling actually works:

- What happens if a dependency throws an unexpected exception type?
- What if a network call hangs forever?
- What if a transaction partially completes?
- Are resources properly cleaned up on error paths? (file handles, connections, locks)
- Does the system recover to a valid state after an error?

### 6. Edge Case Discovery

Look for edge cases the spec didn't consider:

- What if the same operation is called twice? (idempotency)
- What if operations are called out of order?
- What if the system is in an unexpected state?
- What about leap seconds, timezone edges, daylight saving transitions?
- What about empty collections being iterated?
- What about circular references in data?

### 7. Mutation Testing Mindset

Think about whether the tests actually catch bugs:

- If you changed a `<` to `<=` in the implementation, would a test fail?
- If you removed an error check, would a test fail?
- If you changed a constant, would a test fail?
- If you swapped two operations, would a test fail?

If the answer is "no" for any of these, the test coverage has a gap.

## Process

### 1. Read Everything

Read the blueprint, implementation, and tests thoroughly. Understand the intended
behavior before trying to break it.

### 2. Systematic Attack

Work through each attack vector above. For each finding:

- **Severity**: Critical / High / Medium / Low
- **Category**: Spec Violation / Invariant Break / Security / Edge Case / Test Gap
- **Description**: What you found and how to reproduce it
- **Evidence**: Input that triggers the issue, or proof of concept
- **Fix suggestion**: How to address it

### 3. Write Adversarial Tests

For each finding that reveals a real bug:

- Write a test that demonstrates the bug (it should fail)
- Mark it as `@adversarial` or include "ADVERSARIAL" in the test name
- Document the finding in the report

### 4. Generate Report

Write the adversarial report to `.nibble/factory/reports/adversarial/YYYY-MM-DD_<feature>.md`:

```markdown
# Adversarial Report: <Feature Name>

## Summary
- Total findings: N
- Critical: N | High: N | Medium: N | Low: N

## Findings

### FIND-1: <Title>
- **Severity**: Critical/High/Medium/Low
- **Category**: Spec Violation / Invariant Break / Security / Edge Case / Test Gap
- **Description**: ...
- **Evidence**: ...
- **Fix**: ...
- **Status**: Resolved / Unresolved / Accepted Risk

(repeat for each finding)
```

### 5. Check Lessons Learned

Load `factory-lessons` for known adversarial blind spots. Ensure you've checked those
categories. Add any new blind spots discovered during this run.

## Output

An adversarial report at `.nibble/factory/reports/adversarial/YYYY-MM-DD_<feature>.md` containing:

- All findings with severity, category, and fix suggestions
- Adversarial test cases for each confirmed finding
- Summary of what was checked and what wasn't

## Gate Criteria

Before proceeding to risk scoring (load `factory-risk-score`):

- All 7 attack vectors have been checked
- Every invariant has been actively tested for violation
- All spec compliance checks passed or deviations documented
- No Critical findings remain Unresolved
- High findings are either resolved or explicitly Accepted Risk with justification
- Report is complete and saved to the expected location
