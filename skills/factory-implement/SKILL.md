---
name: factory-implement
description: AI Factory Stage 1 — Synthesis. Implement code from the blueprint using DFT principles, assertion-based invariants, and clean architecture boundaries.
---

# Stage 1: Implementation (Synthesis)

You are in the **Implement** phase of the AI Factory pipeline. Your job is to turn the
blueprint into working code — the equivalent of logic synthesis in chip development.

**Before starting: load `factory-lessons` and read the lessons-learned log.**

## Input

- Blueprint at `.nibble/factory/blueprints/YYYY-MM-DD_<feature>.md`
- The existing codebase
- Lessons-learned log (load `factory-lessons`)

## Principles

### Design for Testability (DFT)

Every function and module must be designed so it can be tested in isolation:

- **Dependency injection** — external dependencies (databases, APIs, filesystem) are
  passed as parameters or interfaces, not hardcoded. Tests provide mocks/stubs.
- **Pure functions preferred** — separate business logic from side effects. Pure logic
  is trivially testable.
- **No hidden state** — all state is either passed in or explicitly managed.
- **Observable behavior** — effects are visible through return values, events, or logs,
  not buried in internal implementation details.

### Assertion-Based Verification

Encode every invariant from the blueprint as a runtime assertion:

```
// INV-1: Balance never goes below zero
assert!(balance >= 0, "INV-1 violated: balance is {}", balance);
```

Use the invariant IDs from the blueprint (INV-1, INV-2, etc.) so assertions map back to
the spec. These assertions serve dual purpose:
1. Catch bugs during development
2. Document the contract in executable form

### Clean Architecture Boundaries

Follow the dependency rule: inner layers never depend on outer layers.

```
┌─────────────────────────────────┐
│  Frameworks & Drivers           │  (HTTP handlers, DB access, CLI)
├─────────────────────────────────┤
│  Interface Adapters             │  (controllers, gateways, presenters)
├─────────────────────────────────┤
│  Application Business Rules     │  (use cases, orchestration)
├─────────────────────────────────┤
│  Enterprise Business Rules      │  (domain entities, pure logic)
└─────────────────────────────────┘
```

Not every feature needs all four layers. But the direction of dependency must always
point inward. Business logic never imports infrastructure code.

## Process

### 1. Read the Blueprint

Read the complete blueprint. Understand every interface, invariant, and acceptance
criterion. If anything is unclear, resolve it now (not during coding).

### 2. Plan the Implementation

Before writing code, outline the approach:

1. List all new files to create and existing files to modify
2. Identify which blueprint interfaces go in which files
3. Note the order of implementation (dependencies first)
4. Identify any infrastructure needed (configs, migrations, etc.)

### 3. Implement Bottom-Up

Build from the inside out:

1. **Data types first** — Define all structs, enums, types from the blueprint
2. **Pure logic next** — Implement business rules with assertions for all invariants
3. **Interfaces/adapters** — Wire pure logic to infrastructure (DB, HTTP, etc.)
4. **Entry points last** — Controllers, CLI commands, API handlers

### 4. Follow Existing Conventions

Before writing code, study the existing codebase:

- Naming conventions (camelCase, snake_case, etc.)
- Error handling patterns (Result types, exceptions, error codes)
- Module organization (one class per file? grouped by feature?)
- Import style (absolute vs. relative, barrel exports)
- Testing patterns (describe/it, test functions, pytest, etc.)
- Logging patterns (structured? what level for what?)
- Comment style (doc comments, inline comments, or none)

When in Rome, do as the Romans do. Consistency beats perfection.

### 5. Encode Invariants

For every invariant in the blueprint:

```python
# Python example
assert balance >= 0, f"INV-1: balance must be non-negative, got {balance}"
```

```rust
// Rust example
assert!(balance >= 0, "INV-1: balance must be non-negative");
```

```typescript
// TypeScript example
if (balance < 0) throw new InvariantViolationError("INV-1", balance);
```

Place assertions:
- At function entry (preconditions)
- At function exit (postconditions)
- At state transitions
- At loop boundaries

### 6. Handle Errors per Blueprint

Implement the error handling strategy from the blueprint exactly:
- Expected errors → defined error types with proper recovery
- Unexpected errors → propagate with context, don't swallow
- Never silently ignore an error

### 7. Self-Review Checklist

- [ ] Every interface from the blueprint is implemented
- [ ] Every invariant has a corresponding assertion
- [ ] No hardcoded dependencies (everything injectable/mockable)
- [ ] Business logic is separated from infrastructure
- [ ] Error handling matches the blueprint strategy
- [ ] Code follows existing codebase conventions
- [ ] No TODO/FIXME/HACK comments (resolve them now)
- [ ] Code compiles and lint/typecheck passes

### 8. Check Lessons Learned

Load `factory-lessons` and verify you haven't repeated known implementation bugs.

## Output

Working source code that:
- Implements every interface from the blueprint
- Has assertions for all invariants
- Follows clean architecture boundaries
- Follows existing codebase conventions
- Compiles and passes lint/typecheck

## Gate Criteria

Before proceeding to TDD (load `factory-tdd`):
- All blueprint interfaces implemented
- All invariants have assertions (searchable by INV-N pattern)
- Code compiles without errors
- Lint passes with zero warnings
- Typecheck passes (if language has types)
- No TODO/FIXME/HACK in new code
