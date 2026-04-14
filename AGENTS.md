# nibble Sandbox Agent Instructions

You are running inside an isolated Podman sandbox managed by **nibble** for the **nibble** project. This file contains all instructions for how to operate inside this environment. Read it fully before starting any task.

## Environment

- **Working directory**: `/workspace` (the project repo, mounted read-write)
- **Full sudo access**: install any system package with `apt-get install`
- **Ports forwarded** to the host: services on `localhost:3000`, `:8080`, etc. are reachable from outside
- **Internet access** is available
- **Git** is configured with the host user's identity and SSH keys
- Both `claude` and `opencode` are available if you need a nested agent session

## Toolchain Setup

Project dependencies are installed automatically at sandbox spawn via `.nibble/setup.sh` if that script exists. By the time you receive a task, dependencies should already be built and ready.

- If `.nibble/setup.sh` **exists**: it was already run at spawn — do not re-run it unless something is broken. If you need a new system dependency or build step, update the script and run it manually once, then commit the change.
- If `.nibble/setup.sh` **does not exist**: dependencies won't be pre-installed. Check for manifest files below and install them yourself. Create (or ask to create) `.nibble/setup.sh` so future spawns are automatic.

The following dependency manifests were detected:

| Manifest | Install command | Run/test |
|----------|----------------|----------|
| Rust | `cargo build  # rustup + cargo pre-installed by .nibble/setup.sh; binary at ~/.cargo/bin/cargo` | `cargo run / cargo test` |

If a command fails due to missing system tools, install them with `sudo apt-get install <package>`.

## General Working Principles

- Make small, focused changes and run tests after each one
- The container persists between sessions — installed packages and build artifacts are retained
- When you finish a task, summarise what you did clearly so the notification sent to the user is informative
- Ask before making changes outside the project's stated scope

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

<!-- nibble-sandbox:begin -->
# nibble Sandbox Agent Instructions

You are running inside an isolated Podman sandbox managed by **nibble** for the **nibble** project. This file contains all instructions for how to operate inside this environment. Read it fully before starting any task.

## Environment

- **Working directory**: `/workspace` (the project repo, mounted read-write)
- **Full sudo access**: install any system package with `apt-get install`
- **Ports forwarded** to the host: services on `localhost:3000`, `:8080`, etc. are reachable from outside
- **Internet access** is available
- **Git** is configured with the host user's identity and SSH keys
- Both `claude` and `opencode` are available if you need a nested agent session

## Toolchain Setup

Project dependencies are installed automatically at sandbox spawn via `.nibble/setup.sh` if that script exists. By the time you receive a task, dependencies should already be built and ready.

- If `.nibble/setup.sh` **exists**: it was already run at spawn — do not re-run it unless something is broken. If you need a new system dependency or build step, update the script and run it manually once, then commit the change.
- If `.nibble/setup.sh` **does not exist**: dependencies won't be pre-installed. Check for manifest files below and install them yourself. Create (or ask to create) `.nibble/setup.sh` so future spawns are automatic.

The following dependency manifests were detected:

| Manifest | Install command | Run/test |
|----------|----------------|----------|
| Rust | `cargo build  # rustup + cargo pre-installed by .nibble/setup.sh; binary at ~/.cargo/bin/cargo` | `cargo run / cargo test` |

If a command fails due to missing system tools, install them with `sudo apt-get install <package>`.

## General Working Principles

- Make small, focused changes and run tests after each one
- The container persists between sessions — installed packages and build artifacts are retained
- When you finish a task, summarise what you did clearly so the notification sent to the user is informative
- Ask before making changes outside the project's stated scope

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

<!-- nibble-sandbox:end -->
