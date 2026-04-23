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

- [2026-04-21] [nibble/memory] Frontmatter parser used `content.find(line)` to locate closing `---`, which matched the first occurrence (the opening delimiter) instead of the actual closing one. Fix: track byte position by iterating lines and accumulating line lengths. This bug would have caused multiline YAML frontmatter to leak into the body and empty bodies to fail.

## Testing Gaps
<!-- Patterns where tests didn't catch real bugs -->

- [2026-04-21] [nibble/memory] Memory module was shipped with zero unit tests. Tests were written retroactively and immediately caught a frontmatter parsing bug. Prevention: write tests as part of the implementation phase, not after. For any file I/O format (YAML frontmatter, JSONL, Markdown), test edge cases: empty content, multiline values, special characters in body, missing delimiters.
- [2026-04-21] [nibble/memory] Test for invariant INV-8 (lesson lifecycle transitions) had incorrect logic — `valid_transitions.contains(&to)` always returns true for "active" since "active" is a valid status. Fix: test the forbidden transitions explicitly, not via a membership check on the valid set.

## Audit Blind Spots
<!-- Attack vectors or edge cases the audit missed -->

## QA Catches
<!-- Issues the human caught that all automated stages missed -->

- [2026-04-08] [nibble/factory-pipeline] QA items presented without file:line references. Fix: QA Gate skill now mandates code snippets + line numbers in every review item.

## Process Improvements
<!-- Pipeline meta-improvements -->

- [2026-04-08] [nibble/factory-pipeline] Artifact naming: `YYYY-MM-DD_<slug>.md` for sortability. Blueprints and QA reports committed. Audit reports gitignored (stale after fixes).
