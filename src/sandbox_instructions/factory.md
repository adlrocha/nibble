## Skills & Lessons

Factory pipeline skills are stored on the **host** at `~/.claude/skills/` and bind-mounted into every sandbox at `/home/node/.claude/skills/`. This means:

- Skills and lessons updates made inside a sandbox are immediately visible on the host and in all other sandboxes — they share the same directory.
- To persist a lessons-learned update, edit the skill file directly (e.g. `~/.claude/skills/factory-lessons/SKILL.md`). No restart or re-injection needed.
- The host `install.sh` re-installs skills from the nibble repo to `~/.claude/skills/` whenever you update them in source. Run it after editing skills in the repo.

<!-- nibble:global:begin -->
## AI Factory Pipeline

When factory is enabled, every non-trivial coding task follows the AI Factory pipeline.

Load skill `factory-pipeline` to classify the task and determine which tier to run:
- **Quick** (≤3 functions, no security/API change): Spec → Implement → Verify
- **Standard** (4–15 functions): Spec → Implement → Verify → Audit
- **Full** (16+ functions, security-sensitive, API changes): Full pipeline with QA Gate

**QA Gate fires for ANY tier** when unfixed Critical or High findings are discovered. For Full tier, QA Gate always fires.

Skills: `factory-pipeline` · `factory-spec` · `factory-verify` · `factory-qa-gate` · `factory-lessons`

```
.nibble/factory/blueprints/    # Feature specs (committed)
.nibble/factory/reports/audit/ # Adversarial + risk findings (gitignored)
.nibble/factory/reports/qa/    # QA gate decisions (committed)
```
<!-- nibble:global:end -->
