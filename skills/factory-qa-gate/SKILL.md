---
name: factory-qa-gate
description: AI Factory Stage 5 — QA Gate (Tape-out). Present critical and high risk sections to the human one at a time for approve/reject/changes. The final human stamp before a feature ships.
---

# Stage 5: QA Gate (Tape-out)

You are in the **QA Gate** phase — the final stage of the AI Factory pipeline. This is
the equivalent of tape-out review in chip development: the human gives the final stamp.

Your job: present ONLY what needs human attention, one item at a time, clearly and
concisely. The human should never have to search for what to review.

## Input

- Blueprint at `.nibble/factory/blueprints/YYYY-MM-DD_<feature>.md`
- Risk report at `.nibble/factory/reports/risk/YYYY-MM-DD_<feature>.md`
- Adversarial report at `.nibble/factory/reports/adversarial/YYYY-MM-DD_<feature>.md`

The date is the date the pipeline run started (not today's date if resumed across sessions).

## Process

### 1. Prepare the Review Queue

From the risk report, gather all Critical and High risk sections. Sort by risk score
(highest first). These are the items the human MUST review.

For each item, prepare:

- **What it is**: Function name, exact file path, exact line numbers
- **Why it matters**: Which risk dimensions scored high and why
- **What to verify**: Specific, concrete, actionable questions — not vague descriptions
- **The code**: See "Code Presentation Rule" below
- **Related findings**: All unresolved adversarial findings for this section, with
  exact evidence and suggested fix

### 2. Code Presentation Rule

Humans reviewing code almost always have an IDE open. Choose the presentation format
based on the size of the section being reviewed:

**Short sections (≤ 40 lines):** Inline the code with line numbers. This is faster
than switching to the IDE for small functions.

```
  Code (src/pay.ts:42-58):
  ─────────────────────────────────────────────
  42 | function validate(x: string) {
  43 |   if (!x) throw new Error('empty');
  ...
  58 | }
  ─────────────────────────────────────────────
```

**Long sections (> 40 lines):** Give the file:line reference and describe exactly
what to look at. Do not paste a wall of code — it wastes review time and buries the
actual concern.

```
  Code: src/main.rs:2557-2616  (59 lines — open in IDE)
  ─────────────────────────────────────────────
  Focus on:
  • Lines 2578-2583: the grep + prepend branch
  • Lines 2585-2594: the awk block-replacement branch
  The function signature and happy path are low risk.
  ─────────────────────────────────────────────
```

**The goal:** the human should understand exactly what to look at, whether they read
the snippet here or open the file. Never leave them guessing.

### 3. Present Summary First

Before showing individual items, give the human a brief summary:

```
═══════════════════════════════════════════════
  QA GATE: <Feature Name>
═══════════════════════════════════════════════

  Pipeline status:
  ✓ Spec        — Blueprint complete
  ✓ Implement   — Code compiles, lint passes
  ✓ TDD         — All tests green, coverage OK
  ✓ Adversarial — 3 findings (1 resolved, 2 accepted risk)
  ✓ Risk Score  — 12 functions scored

  Review required: 3 sections (1 Critical, 2 High)
  Estimated review time: ~5 minutes

  Say "proceed" to review items one by one, or
  "approve all" to accept everything.
═══════════════════════════════════════════════
```

### 4. Present Items One by One

```
───────────────────────────────────────────────
  Review Item 1 of 3
───────────────────────────────────────────────

  Function: processPayment
  Location: src/pay.ts:42-89
  Risk: 20/25 (High)

  Why flagged:
  • Handles payment processing (Security: 5/5)
  • Complex error recovery (Error Handling: 4/5)
  • External service dependency (Dependency: 5/5)

  Adversarial finding FIND-3 (Unresolved — High):
  • Password not hashed before API call
  • Evidence: credentials.password passed raw on line 67
  • Fix: hash with bcrypt before send, or confirm API handles it

  What to verify:
  1. Is credentials.password ever logged? (lines 50-55)
  2. Does the catch block on line 78 leak payment details in the error message?
  3. Is the retry loop (lines 60-75) bounded to prevent infinite retries?

  Code (src/pay.ts:42-58):           ← short section: inlined
  ─────────────────────────────────────────────
  42 | export function processPayment(
  43 |   amount: number,
  44 |   credentials: PaymentCredentials,
  45 | ): Promise<PaymentResult> {
  46 |   const response = await fetch('/api/pay', {
  47 |     body: JSON.stringify({ amount, ...credentials }),
  ...
  58 | }
  ─────────────────────────────────────────────

  Actions: approve | reject | request changes
───────────────────────────────────────────────
```

Wait for the human's response before proceeding to the next item.

### 5. Handle Human Responses

For each item, the human will respond with one of:

- **approve** — The code is acceptable. Mark as approved, move to next item.
- **reject** — The code has a problem. Record the issue, move to next item.
- **request changes** — The human wants specific changes. Record the change request,
  implement it, then run the Change Re-Verification loop below before re-presenting.

If the human says **approve all** at any point, mark all remaining items as approved.

### 5a. Change Re-Verification Loop

Whenever you implement changes requested during QA — whether for a single item or
across multiple items — you MUST re-verify those changes before re-presenting them
to the human. Do not assume a fix is correct just because it compiles or the tests
pass. Shortcuts here defeat the purpose of the gate.

**For every set of implemented changes:**

1. **Re-run adversarial analysis on the changed code only** (not the whole feature).
   Apply attack vectors 1–7 from `factory-adversarial` scoped to the modified
   functions. Ask: did the fix introduce any new issue that wasn't there before?
   Common examples:
   - A fix for an injection bug introduces a TOCTOU race
   - A fix for a missing null-check breaks idempotency
   - A new helper function has no tests

2. **Re-score the modified functions** using the risk dimensions from
   `factory-risk-score`. Confirm the risk level went down (or stayed the same).
   If a fix raises the risk score, flag it before re-presenting.

3. **Run the test suite.** All tests must pass. If the fix required new tests, verify
   the new tests actually fail without the fix (mutation check).

4. **Re-present the item** to the human with a clear diff summary:
   - What was changed and where (file:line)
   - What the adversarial re-check found (or "no new issues found")
   - Updated risk score
   - Test result

```
───────────────────────────────────────────────
  Re-review Item N of M  (after changes)
───────────────────────────────────────────────

  Change implemented: <one-line description>
  Modified: <file>:<lines>

  Re-verification:
  • Adversarial re-check: no new issues found
    (or: NEW ISSUE found — <description>)
  • Risk score: was 20 → now 15 (High → Medium)
  • Tests: 82 passed, 0 failed

  Code (if short) or file:line reference (if long):
  ─────────────────────────────────────────────
  ...
  ─────────────────────────────────────────────

  Actions: approve | reject | request changes
───────────────────────────────────────────────
```

If the re-verification finds a new issue, resolve it before re-presenting — do not
surface a known bug to the human without a fix ready.

### 6. Record Decisions

Write the QA decisions to `.nibble/factory/reports/qa/YYYY-MM-DD_<feature>.md`:

```markdown
# QA Gate: <Feature Name>

**Date**: YYYY-MM-DD
**Reviewer**: Human
**Result**: APPROVED / APPROVED WITH CHANGES / REJECTED

## Items Reviewed

| # | Function | Risk | Decision | Notes |
|---|----------|------|----------|-------|
| 1 | processPayment | High | Approved | Accepted risk on FIND-3 |
| 2 | validateInput | High | Changes Requested | Add null check |
| 3 | formatReceipt | Critical | Approved | — |

## Change Requests

### CR-1: validateInput — Add null check
- **Requested by**: Human
- **Description**: Add null check for `credentials` parameter
- **Status**: Implemented / Pending

## Unresolved Items
None / List any rejected items
```

### 7. Final Pipeline Report

After all items are reviewed, output a **short terminal summary** — enough for the human
to see the result at a glance and decide whether to dig into the full reports.

Keep it under 20 lines. Do not repeat content from the individual review items.

```
═══════════════════════════════════════════════════════════
  PIPELINE COMPLETE: <Feature Name>
  <one-sentence description of what this feature does>
═══════════════════════════════════════════════════════════

  Result:  APPROVED ✓   (or APPROVED WITH CHANGES / REJECTED)
  Date:    YYYY-MM-DD
  Tests:   <N> passed, 0 failed

  Findings: <N total>  (<N> Critical / <N> High / <N> Medium / <N> Low)
  Resolved: <N>  |  Accepted: <N>  |  Unresolved: <N>

  QA items reviewed: <N>  (Approved: <N> | Changes: <N> | Rejected: <N>)

  Key decisions:
  • <FIND-N>: <one-line outcome — Fixed / Accepted / Deferred>
  • ...  (only Critical and High findings; skip if none)

  Artifacts:
  • Blueprint:  .nibble/factory/blueprints/YYYY-MM-DD_<feature>.md
  • QA report:  .nibble/factory/reports/qa/YYYY-MM-DD_<feature>.md

═══════════════════════════════════════════════════════════
```

The full QA report (written in step 6) is the authoritative record. The terminal summary
is just the at-a-glance view. Do not duplicate the full item-by-item breakdown here.

### 8. Update Lessons Learned

If anything notable happened during QA, load `factory-lessons` and add entries:
- Did the human catch something all automated stages missed? → QA Catches
- Was the review process too slow or too granular? → Process Improvements
- Were any risk scores off (too high or too low)? → Risk Scoring Misses

## Gate Criteria

- All Critical and High risk sections have been reviewed by the human
- Every item has a recorded decision (approve/reject/changes)
- Change requests have been implemented, re-verified (adversarial + risk + tests),
  and re-approved by the human — no change ships without re-verification
- QA report is saved to the expected location
- Final pipeline report has been output to the human
- Lessons learned are updated if applicable
