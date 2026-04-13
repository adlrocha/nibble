## Skills & Lessons

Factory pipeline skills (`factory-spec`, `factory-implement`, etc.) and the lessons-learned log are stored on the **host** at `~/.claude/skills/` and bind-mounted into every sandbox at `/home/node/.claude/skills/`. This means:

- Skills and lessons updates made inside a sandbox are immediately visible on the host and in all other sandboxes — they share the same directory.
- To persist a lessons-learned update, edit the skill file directly (e.g. `~/.claude/skills/factory-lessons/SKILL.md`). No restart or re-injection needed.
- The host `install.sh` re-installs skills from the nibble repo to `~/.claude/skills/` whenever you update them in source. Run it after editing skills in the repo.

<!-- nibble:global:begin -->
## AI Factory Pipeline

When factory is enabled, every **non-trivial** coding task follows this pipeline. This is mandatory.

```
 SPEC  ──▶  IMPLEMENT  ──▶   TDD    ──▶  ADVERSARIAL ──▶  RISK SCORE  ──▶  QA GATE
(Blueprint)  (Synthesis)    (DV)        (Red Team)       (Analysis)       (Tape-out)
```

### When to run the pipeline

**Run the full pipeline for:**
- New features or significant new functionality
- Changes to public interfaces or APIs
- Security-sensitive code (auth, secrets, payments)
- Complex business logic
- Database schema changes
- Bug fixes that involve non-trivial logic changes or affect data safety

**Skip the pipeline (confirm with human first) for:**
- Typo fixes or comment updates
- Trivial config tweaks (single-value changes)
- Pure formatting / linting fixes
- Documentation-only changes

### How to run each stage

Load the skill for each stage before executing it. Skills are available via the skill tool:

- **Pipeline overview**: load skill `factory-pipeline`
- **Stage 0 — Spec**: load skill `factory-spec`
- **Stage 1 — Implement**: load skill `factory-implement`
- **Stage 2 — TDD**: load skill `factory-tdd`
- **Stage 3 — Adversarial**: load skill `factory-adversarial`
- **Stage 4 — Risk Score**: load skill `factory-risk-score`
- **Stage 5 — QA Gate**: load skill `factory-qa-gate`
- **Lessons learned** (read at every stage start): load skill `factory-lessons`

### Artifact locations

```
.nibble/
  factory/
    blueprints/          # Feature specs (one file per feature)
    reports/
      adversarial/       # Red team findings per feature
      risk/              # Risk score tables per feature
      qa/                # QA gate decisions per feature
```

### QA Gate behaviour

The QA Gate is an interactive pause. Do not auto-approve. Present each Critical and High risk item one at a time. Wait for the human to say `approve`, `reject`, or `request changes` before proceeding to the next item.

### Lessons learned

The lessons-learned log accumulates pipeline wisdom over time. **Read it at the start of every stage** (load `factory-lessons`). Append new entries whenever something slips through a stage that an earlier stage should have caught.
<!-- nibble:global:end -->
