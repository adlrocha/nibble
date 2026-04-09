---
name: factory-lessons
description: AI Factory continuous improvement log. Read at the start of every pipeline stage. Append new lessons when you discover something a prior stage missed.
---

# Lessons Learned

This file is the continuous learning log for the AI Factory pipeline. It grows over time
as blind spots, bugs, and process improvements are discovered.

**Every skill in the pipeline reads this file before executing.** Past mistakes prevent
future ones.

## How to Contribute

When you discover something during any pipeline stage that wasn't caught by earlier
stages, add an entry in the appropriate category. Use this format:

```
- [YYYY-MM-DD] [project] <description of the issue and how to prevent it>
```

Ask the user to review this lesson and determine if it should be encoded in the AI-factory skills so there's no need to keep it as a separate lesson.

## Spec Gaps

Patterns where the spec was incomplete or ambiguous, leading to implementation bugs.

<!-- Add entries here as issues are discovered -->

## Implementation Bugs

Patterns where the implementation had bugs that the spec should have caught or the
adversarial phase should have found.

<!-- Add entries here as issues are discovered -->

## Testing Gaps

Patterns where tests existed but didn't catch real bugs (mutation testing failures).

<!-- Add entries here as issues are discovered -->

## Adversarial Blind Spots

Attack vectors or edge cases that the adversarial phase didn't check but should have.

<!-- Add entries here as issues are discovered -->

## QA Catches

Issues that the human caught during QA that all automated stages missed.

- [2026-04-08] [nibble/factory-pipeline] QA Gate items were presented without exact
  file:line references or code snippets. The human had to navigate to code manually.
  Fix: skill updated to mandate code snippet + line numbers in every review item.
  The risk report and adversarial report should also include snippets for flagged
  sections so the QA Gate agent can pull them without re-reading the codebase.

## Risk Scoring Misses

Cases where risk scores were too low (something unimportant scored high) or too high
(something critical scored low).

<!-- Add entries here as issues are discovered -->

## Process Improvements

Suggestions for improving the pipeline itself.

- [2026-04-08] [nibble/factory-pipeline] Artifact naming convention: all blueprint and
  report files must use `YYYY-MM-DD_<slug>.md` format so runs are sortable by date and
  distinguishable when the same feature is revisited. Skills (factory-spec, factory-qa-gate,
  etc.) now reference this convention explicitly.

- [2026-04-08] [nibble/factory-pipeline] Commit policy: blueprints and QA gate reports
  are committed (long-term value: design decisions and audit trail). Adversarial and risk
  reports are gitignored (process artifacts: scores are stale once findings are fixed).
  A `.gitignore` in `.nibble/factory/` enforces this.

- [2026-04-08] [nibble/factory-pipeline] Final Pipeline Report: was a wall of text
  duplicating all review items. Replaced with a ≤20-line terminal summary (result,
  test count, finding tally, key decisions, artifact paths). The full detail lives in
  the QA report file — humans can dig in if needed.
