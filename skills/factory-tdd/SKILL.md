---
name: factory-tdd
description: AI Factory Stage 2 — Design Verification. Write tests from the blueprint (not the code). Covers every acceptance criterion, invariant, boundary, and error path.
---

# Stage 2: Design Verification (TDD)

You are in the **TDD** phase of the AI Factory pipeline. Your job is to prove the
implementation matches the blueprint — the equivalent of design verification (DV) in
chip development.

**Tests are written from the blueprint, not from the implementation.** This is critical.
If you write tests by looking at the code, you'll test what the code does, not what it
should do.

**Before starting: load `factory-lessons` and read the lessons-learned log.**

## Input

- Blueprint at `.nibble/factory/blueprints/YYYY-MM-DD_<feature>.md`
- Implementation code
- Lessons-learned log (load `factory-lessons`)

## Principles

### Coverage-Driven Verification

Like chip DV, we measure and enforce test coverage:

- **Line coverage**: Minimum 80% of new code lines executed by tests
- **Branch coverage**: All conditional branches tested (if/else, match/case)
- **Acceptance criteria coverage**: Every AC from the blueprint has ≥1 test
- **Invariant coverage**: Every INV from the blueprint has ≥1 test that verifies it

### Red First, Then Green

Strict TDD discipline:

1. **RED**: Write a failing test that expresses one acceptance criterion or invariant
2. **GREEN**: Write the minimum code to make it pass
3. **REFACTOR**: Clean up while keeping all tests green

Since the implementation already exists from Stage 1, adapt this to:
1. Write tests that target acceptance criteria (they should pass if implementation is correct)
2. Write tests that target edge cases (they may reveal bugs)
3. Write tests that verify invariants under stress

### Assertion-Based Testing

Tests should verify the invariants from the blueprint:

```
// Test INV-1: Balance never goes below zero
test("withdraw cannot make balance negative", () => {
  const account = new Account(balance: 100);
  account.withdraw(150);
  expect(account.balance).toBeGreaterThanOrEqual(0);
});
```

## Process

### 1. Map Acceptance Criteria to Tests

For each acceptance criterion (AC-1, AC-2, etc.) in the blueprint, write at least one
test. Name the test to reference the AC:

```
test("AC-1: given valid input when processing then returns expected output")
test("AC-2: given empty input when processing then returns validation error")
```

### 2. Map Invariants to Tests

For each invariant (INV-1, INV-2, etc.), write a test that tries to violate it:

```
test("INV-1: balance stays non-negative under concurrent withdrawals")
test("INV-2: every resource has exactly one owner after creation")
```

These tests should exercise edge cases that could break the invariant.

### 3. Write Boundary Tests

Test at the boundaries of every input:

- Empty input (empty string, empty array, zero)
- Minimum valid input
- Maximum valid input
- One past the boundary (off-by-one)
- Null / None / undefined (if applicable)
- Very large input (stress/buffer overflow)

### 4. Write State Transition Tests

For features with state machines in the blueprint, test every transition:

- Valid transitions (happy path)
- Invalid transitions (wrong state for event)
- Missing transitions (undefined event for state)
- Self-transitions (state stays the same)
- Error recovery transitions

### 5. Write Error Path Tests

For each error case in the blueprint's interface table:

- Trigger the error condition
- Verify the correct error type is returned
- Verify any error recovery happens correctly
- Verify any side effects are rolled back

### 6. Write Integration Tests

If the feature integrates with external systems (DB, API, filesystem):

- Test with the real dependency if fast and deterministic
- Test with mocks/stubs if the real dependency is slow or flaky
- Verify the integration contract matches the blueprint's interface definition

### 7. Run and Fix

Run all tests:

1. **If all pass** → Proceed to adversarial phase
2. **If some fail** → Analyze failures:
   - Bug in implementation → Fix the code, keep the test
   - Bug in test (wrong expectation) → Fix the test
   - Bug in spec → Document the discrepancy, fix whichever is wrong

### 8. Check Coverage

Run the coverage tool for your language/framework. Ensure:
- New code meets the coverage targets
- Every acceptance criterion is exercised
- Every invariant is tested
- No dead code (unreachable paths)

### 9. Check Lessons Learned

Load `factory-lessons` for known testing gaps and verify they're covered.

## Test File Organization

```
tests/
  unit/
    <feature>.test.<ext>      # Unit tests for pure logic
  integration/
    <feature>.test.<ext>      # Integration tests with dependencies
  fixtures/
    <feature>.<ext>           # Test data, mocks, stubs
```

Follow the project's existing test structure if it differs from this.

## Output

A test suite that:
- Covers every acceptance criterion from the blueprint
- Tests every invariant
- Tests boundary conditions and error paths
- Achieves ≥80% line coverage on new code
- All tests pass

## Gate Criteria

Before proceeding to adversarial (load `factory-adversarial`):
- Every AC-N has at least one test (searchable by "AC-N" in test names)
- Every INV-N has at least one test (searchable by "INV-N" in test names)
- Coverage meets targets
- All tests pass (green)
- No skipped or commented-out tests
