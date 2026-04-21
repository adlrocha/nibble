---
name: factory-lessons
description: AI Factory — Continuous improvement log. Loaded once at pipeline start. Appended at pipeline end when something slips through.
---

# Lessons Learned

Continuous learning log. Past mistakes prevent future ones.

**Load once at pipeline start. Append at pipeline end if applicable.**

## How to Contribute

When something slips through a prior stage, add an entry:
```
- [YYYY-MM-DD] [project] <description + prevention>
```

If a lesson is recurring, consider encoding it into the relevant skill file directly.

## Spec Gaps
<!-- Patterns where the spec was incomplete, leading to implementation bugs -->

## Implementation Bugs
<!-- Patterns where bugs survived testing -->

## Testing Gaps
<!-- Patterns where tests didn't catch real bugs -->

## Audit Blind Spots
<!-- Attack vectors or edge cases the audit missed -->

## QA Catches
<!-- Issues the human caught that all automated stages missed -->

- [2026-04-08] [nibble/factory-pipeline] QA items presented without file:line references. Fix: QA Gate skill now mandates code snippets + line numbers in every review item.

## Process Improvements
<!-- Pipeline meta-improvements -->

- [2026-04-08] [nibble/factory-pipeline] Artifact naming: `YYYY-MM-DD_<slug>.md` for sortability. Blueprints and QA reports committed. Audit reports gitignored (stale after fixes).
