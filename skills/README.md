# Nibble Skills

Skills shipped to every nibble sandbox (copied by `install.sh` to
`~/.claude/skills/`, `~/.nibble/skills/`, and `~/.pi/agent/skills/`). Provenance
and integrity are tracked in [`../skills-lock.json`](../skills-lock.json); run
`scripts/skills-lock.sh` to regenerate and `--check` to detect drift.

Only each skill's name + description are in the agent's system prompt
(progressive disclosure). Full content loads on demand — so prefer few,
well-named skills with on-demand `references/` over many top-level skills.

## Engineering practices (general-purpose, stack-neutral)

A single `engineering` skill holds universal discipline and **routes** to
on-demand topic and stack references:

- `engineering/SKILL.md` — universal principles + the routing table.
- `engineering/references/` — topics: `code-review`, `testing`, `debugging`,
  `refactoring`, `interface-design`, `error-handling`, `security-basics`.
- `engineering/references/stacks/` — currently supported stacks:
  - `backend.md` (services, APIs, databases, jobs)
  - `frontend.md` (web UI, components, state, a11y)

The general skill routes to the right stack; add new stacks as
`references/stacks/<name>.md` and list them in `SKILL.md`.

`git-commit-pr` (vendored from `we-are-singular/skills`, MIT) is a standalone
skill for the commit/PR workflow.

## Other skills

| Skill | Purpose |
|-------|---------|
| `nibble-memory` | Cross-session memory capture/search (pairs with the nibble-memory extension). |
| `nibble-pr-review` | Track and review GitHub PRs where you're a reviewer. |
| `omarchy-migration` | Arch/Omarchy migration guidance. |

## Installation

`install.sh` copies each `skills/*/` directory (including its `references/`) to
the host skill dirs above, which Claude Code and Pi scan automatically.

## Adding a skill

1. Create `skills/<name>/SKILL.md` (frontmatter: `name`, `description`).
2. Add on-demand depth under `references/` if needed.
3. Add a provenance entry to `PROVENANCE` in `scripts/skills-lock.sh`.
4. Run `scripts/skills-lock.sh` to update `skills-lock.json`.
