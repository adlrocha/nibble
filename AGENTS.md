<!-- nibble:global:begin -->
## Coding Behavior Guidelines

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

### 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:

- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

### 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:

- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:

- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:

- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:

```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.

<!-- nibble:global:end -->

## Delivery loop

Every change lands through a pull request that iterates with the nibble-review reviewer until it approves, agent-authored changes included:

1. One branch per change. Implement with tests; run `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt` before pushing (plus `tests/install_test.sh` when `install.sh` or `uninstall.sh` changed).
2. Open the PR ready for review — drafts skip the review entirely.
3. Iterate with the nibble-review action until it LGTMs the **current head** and the `review` check is green. On a ⚠️ or ⛔ verdict, read every finding and resolve each one by fixing it or replying with evidence why it does not apply; never silently ignore one. Re-reviews trigger only on a push or an explicit `@nibble-review` comment — a reply alone never re-runs the reviewer. After a reply-only resolution, post `@nibble-review` to request the re-review, and wait for it before concluding.
4. Stop and hand off for merge. Report the verdict, CI state, and anything the user must decide. Do not merge, enable auto-merge, or push to `main`; merging is a human decision.
