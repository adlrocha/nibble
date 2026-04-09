---
name: factory-spec
description: AI Factory Stage 0 — Blueprint Design. Write a clear, unambiguous spec before coding. Produces blueprints/YYYY-MM-DD_<feature>.md with interfaces, invariants, and acceptance criteria.
---

# Stage 0: Blueprint Design (Spec)

You are in the **Spec** phase of the AI Factory pipeline. Your job is to produce a clear,
unambiguous specification — the equivalent of an RTL design spec in chip development.

This is the most critical stage. A poor spec leads to poor everything downstream.

**Before starting: load `factory-lessons` and read the lessons-learned log.**

## Input

- A task description (from the human, a cron job, or the pipeline)
- The existing codebase (for context)
- Lessons-learned log (load `factory-lessons`)

## Process

### 1. Understand the Task

Read the task description carefully. If anything is ambiguous:
- State your assumptions explicitly in the spec
- Flag ambiguous areas as `NEEDS CLARIFICATION` — do not guess silently

### 2. Survey the Codebase

Before writing the spec, understand what already exists:
- Find related code, interfaces, patterns
- Identify what can be reused vs. what must be built new
- Note existing conventions that should be followed

### 3. Write the Blueprint

Create a file at `.nibble/factory/blueprints/YYYY-MM-DD_<feature>.md` using this template:

The date is today's date (when the pipeline run starts). Use the same date across all
artifacts for the same feature run so they sort together.

```markdown
# Blueprint: <Feature Name>

## Summary
One paragraph. What this feature does and why.

## Scope
- In scope: ...
- Out of scope: ...
- Dependencies: ... (other features, libraries, services)

## Interfaces

### Public API / Exports
For each public function, method, endpoint, or export:

| Name | Input | Output | Errors |
|------|-------|--------|--------|
| `fn foo(bar: Baz)` | `Baz` | `Result<Qux>` | `InvalidInput`, `NotFound` |

### Data Types
Define all new types, structs, enums, or schemas. Include:
- Field types and constraints (e.g., "non-empty string", "positive integer")
- Default values
- Validation rules

### Events / Side Effects
List everything that happens beyond the return value:
- State mutations
- Network calls
- File I/O
- Database writes
- Logs/metrics emitted

## State Machine / Flow
For features with state transitions, describe them explicitly:

```
[State A] ── event X ──▶ [State B]
[State B] ── event Y ──▶ [State C]
[State B] ── event Z ──▶ [State A] (error recovery)
```

Or for simpler flows, a numbered step-by-step.

## Invariants
Conditions that MUST ALWAYS be true. These become assertions in code and checks in tests.
Number them for easy reference (INV-1, INV-2, etc.).

1. (INV-1) <description of invariant>
2. (INV-2) <description of invariant>

Examples:
- "Balance never goes below zero"
- "Every created resource has exactly one owner"
- "Cache entry TTL is always > 0"
- "File handle is always closed after use"

## Error Handling Strategy
- What errors are expected vs. unexpected
- How each error type is handled (retry, fallback, propagate, log)
- Error recovery paths

## Acceptance Criteria
Testable conditions that MUST pass for this feature to be complete.
Number them for easy reference (AC-1, AC-2, etc.).

1. (AC-1) <given/when/then or simple assertion>
2. (AC-2) <given/when/then or simple assertion>

Each AC should map to at least one test case.

## Constraints
- Performance requirements (latency, throughput, memory)
- Security requirements (auth, encryption, input validation)
- Compatibility requirements (API versioning, browser support)
- Platform requirements (OS, runtime version)

## Open Questions
List anything you couldn't resolve from the task description or codebase.
These will be flagged to the human during the QA Gate if not resolved earlier.
```

### 4. Self-Review

Before finalizing, check your spec against this checklist:

- [ ] Every public interface has defined inputs, outputs, and error cases
- [ ] Invariants are stated as verifiable conditions, not vague goals
- [ ] Acceptance criteria are testable (can be proven true/false with a test)
- [ ] State transitions are explicit (no implicit state changes)
- [ ] Edge cases are addressed (empty inputs, null values, concurrent access)
- [ ] Error handling covers both expected and unexpected failures
- [ ] Security-sensitive operations are identified
- [ ] No undefined terms or ambiguous language

### 5. Check Lessons Learned

Load `factory-lessons` and verify you haven't repeated any known spec gaps.

## Output

A completed blueprint at `.nibble/factory/blueprints/YYYY-MM-DD_<feature>.md`.

## Gate Criteria

Before proceeding to implementation (load `factory-implement`):
- All template sections are filled (or explicitly marked N/A with justification)
- At least 3 invariants are stated (or justified why fewer)
- At least 3 acceptance criteria are stated
- No unresolved `NEEDS CLARIFICATION` items (resolve them or escalate to human)
- Codebase survey is complete (no surprises about existing code)
