---
name: factory-risk-score
description: AI Factory Stage 4 — Risk Scoring. Score every function on 5 dimensions (Complexity, Dependency Risk, Security Sensitivity, Error Handling Load, Test Coverage Gap) and flag critical sections for human review.
---

# Stage 4: Risk Scoring (Analysis)

You are in the **Risk Scoring** phase of the AI Factory pipeline. Your job is to score
every function and module for risk, then flag the critical sections for human review —
the equivalent of timing/power analysis and design rule checking (DRC) in chip
development.

The goal: ensure the human only reviews what actually matters.

**Before starting: load `factory-lessons` and read the lessons-learned log.**

## Input

- Blueprint at `.nibble/factory/blueprints/YYYY-MM-DD_<feature>.md`
- Implementation code
- Test suite + coverage report
- Adversarial report at `.nibble/factory/reports/adversarial/YYYY-MM-DD_<feature>.md`
- Lessons-learned log (load `factory-lessons`)

## Risk Scoring Criteria

Each function/method/module is scored on five dimensions (1-5 each):

### Complexity (C)
| Score | Meaning |
|-------|---------|
| 1 | Straight-line code, no branching |
| 2 | Simple conditionals, no nesting |
| 3 | Multiple conditionals, some nesting |
| 4 | Complex logic, deep nesting, loops |
| 5 | Recursive, concurrent, or algorithmically complex |

### Dependency Risk (D)
| Score | Meaning |
|-------|---------|
| 1 | Pure function, no external dependencies |
| 2 | Only uses well-tested standard library |
| 3 | Uses one external library |
| 4 | Uses multiple external libraries or external services |
| 5 | Critical dependency on external service, filesystem, or hardware |

### Security Sensitivity (S)
| Score | Meaning |
|-------|---------|
| 1 | No security relevance |
| 2 | Processes user input indirectly |
| 3 | Directly handles user input |
| 4 | Handles authentication, authorization, or encryption |
| 5 | Handles secrets, financial data, or security-critical operations |

### Error Handling Load (E)
| Score | Meaning |
|-------|---------|
| 1 | No error cases |
| 2 | Simple error return |
| 3 | Multiple error types with recovery |
| 4 | Complex error recovery with state rollback |
| 5 | Distributed/cascading failure handling |

### Test Coverage Gap (T)
| Score | Meaning |
|-------|---------|
| 1 | 100% coverage, all paths tested |
| 2 | >90% coverage, minor gaps |
| 3 | 70-90% coverage, notable gaps |
| 4 | 50-70% coverage, significant gaps |
| 5 | <50% coverage or no tests |

### Composite Risk Score

```
Risk = C + D + S + E + T
```

| Total | Level | Human Review Required |
|-------|-------|-----------------------|
| 5-10 | Low | No (automated review sufficient) |
| 11-15 | Medium | Skim (quick human check) |
| 16-20 | High | Review (detailed human review) |
| 21-25 | Critical | Deep Review (line-by-line human review) |

## Process

### 1. Enumerate All New/Modified Functions

List every function, method, and module added or changed for this feature. Include:
- File path and line numbers
- Function name
- Brief description (one line)

### 2. Score Each Function

For each function, score the five dimensions. Record:

```markdown
| Function | File:Lines | C | D | S | E | T | Risk | Level |
|----------|-----------|---|---|---|---|---|------|-------|
| <fn> | <file>:<lines> | <1-5> | <1-5> | <1-5> | <1-5> | <1-5> | <sum> | <level> |
```

### 3. Incorporate Adversarial Findings

For any unresolved findings from the adversarial report:
- Increase the Security Sensitivity (S) score by +1 for each security finding
- Increase the Error Handling Load (E) score by +1 for each unhandled error case
- Increase the Test Coverage Gap (T) score by +1 for each finding that reveals a test gap
- Cap each dimension at 5

### 4. Identify Critical Sections

Flag all functions with Risk Level ≥ Medium for the QA Gate. For each flagged section:

- **Why it's flagged**: Which dimensions drove the high score
- **What to look for**: Specific concerns based on the dimensions
- **Adversarial findings**: Any unresolved findings affecting this section
- **Code context**: The function and its immediate dependencies

### 5. Generate Risk Report

Write the risk report to `.nibble/factory/reports/risk/YYYY-MM-DD_<feature>.md`:

```markdown
# Risk Report: <Feature Name>

## Summary
- Total functions scored: N
- Critical: N | High: N | Medium: N | Low: N
- Functions requiring human review: N

## Score Distribution

| Function | File:Lines | C | D | S | E | T | Risk | Level |
|----------|-----------|---|---|---|---|---|------|-------|
| ... | ... | ... | ... | ... | ... | ... | ... | ... |

## Critical & High Risk Sections (Human Review Required)

### <Function Name> — Risk: N (Level)
- **Location**: file:lines
- **Why flagged**: High complexity (C=5), handles secrets (S=5)
- **What to look for**: Correct encryption, proper key management, no data leakage
- **Adversarial findings**: FIND-3 (unresolved, password not hashed)
- **Code**:
  ```
  <relevant code snippet>
  ```

(repeat for each Critical/High function)

## Medium Risk Sections (Quick Human Check)

### <Function Name> — Risk: N (Level)
- **Location**: file:lines
- **Why flagged**: ...
- **Quick check**: What to verify in 30 seconds

## Low Risk Sections (No Review Needed)

Functions with Low risk scores. Listed for completeness but no human review required.

| Function | Risk | Reason |
|----------|------|--------|
| <fn> | <score> | <one-line reason> |
```

### 6. Check Lessons Learned

Load `factory-lessons` for known risk patterns. Ensure any historically risky patterns
are accounted for in the scoring.

## Output

A risk report at `.nibble/factory/reports/risk/YYYY-MM-DD_<feature>.md` containing:
- Every new/modified function scored on 5 dimensions
- Composite risk level for each function
- Detailed flagging for Critical and High risk sections
- Quick-check summaries for Medium risk sections
- Integration of adversarial findings into risk scores

## Gate Criteria

Before proceeding to QA Gate (load `factory-qa-gate`):
- Every new/modified function is scored
- Adversarial findings are reflected in risk scores
- Critical and High risk sections have detailed review guidance
- Report is saved to the expected location
