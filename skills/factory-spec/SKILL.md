---
name: factory-spec
description: AI Factory — Blueprint design with tiered templates. Load for the Spec stage.
---

# Stage: Spec (Blueprint)

Write a clear, unambiguous spec. A poor spec leads to poor everything downstream.

## Process

1. Read the task description. Flag ambiguities as `NEEDS CLARIFICATION` — do not guess silently.
2. Survey the codebase: find related code, patterns, conventions, what can be reused.
3. Select template by tier and fill it in.
4. Check lessons-learned for known spec gaps before finalizing.

## Quick Template (≤ 3 functions, no security/API change)

```markdown
# Blueprint: <Name>

## Summary
One paragraph: what and why.

## What's Changing
- Files/functions modified and what changes in each.

## Invariants
1. (INV-1) <verifiable condition>
2. (INV-2) <verifiable condition>

## Acceptance Criteria
1. (AC-1) <testable given/when/then>
2. (AC-2) <testable given/when/then>
```

## Standard Template (4–15 functions)

All sections from Quick, plus:

```markdown
## Interfaces
| Name | Input | Output | Errors |
|------|-------|--------|--------|
| `fn foo(bar: Baz)` | `Baz` | `Result<Qux>` | `InvalidInput` |

## Error Handling
- Expected errors → defined types with recovery
- Unexpected errors → propagate with context, never swallow

## Edge Cases
Known edge cases and how they're handled.
```

## Full Template (16+ functions or security-sensitive)

All sections from Standard, plus:

```markdown
## Scope
- In / Out of scope / Dependencies

## Data Types
All new types with field constraints, defaults, validation rules.

## State Machine / Flow
[state diagram or numbered flow]

## Events / Side Effects
State mutations, I/O, network calls, logs emitted.

## Constraints
Performance, security, compatibility, platform requirements.
```

## Gate Criteria (all tiers)

- All template sections filled (or explicitly N/A with justification)
- At least 2 invariants for Quick, 3 for Standard/Full
- At least 2 acceptance criteria for Quick, 3 for Standard/Full
- No unresolved `NEEDS CLARIFICATION` items

### Spec Quality Check
Before finalizing, verify:
- Invariants are verifiable conditions, not vague goals (bad: "system is reliable", good: "balance >= 0")
- Every public interface has defined error cases
- No undefined terms or ambiguous language
