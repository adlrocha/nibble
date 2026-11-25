# Nibble Skills

Skills shipped to every nibble sandbox (copied by `install.sh` to
`~/.claude/skills/`, `~/.nibble/skills/`, and `~/.pi/agent/skills/`). Provenance
and each `SKILL.md` hash are tracked in [`../skills-lock.json`](../skills-lock.json);
run `scripts/skills-lock.sh` to regenerate and `--check` to detect drift in those
files or their metadata.

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
| `inference-optimisation` | Measure and improve local LLM serving performance. |
| `skill-distill` | Turn a successful agent session into a reusable skill for another model. |
| `nibble-memory` | Cross-session memory capture/search (pairs with the nibble-memory extension). |
| `nibble-pr-review` | Track and review GitHub PRs where you're a reviewer. |
| `nibble-sdlc` | Take a spec to a reviewed PR with nibble-review. |
| `omarchy-migration` | Arch/Omarchy migration guidance. |

## Installation

`install.sh` replaces each installed `skills/*/` directory with the complete
copy from its checkout, including scripts, references and licenses. It does not
fetch upstream skills. From a development clone, update the clone first; the
one-command installer fetches nibble before installing it.

## Updating external skills

`skills-sources.json` records each external repository, branch, skill path,
license path and last imported commit. Nibble-owned skills have no external
source and stay in this checkout. To check upstreams and open an update PR:

```bash
./scripts/skills-update-pr.sh
```

This needs git, gh (authenticated with push and PR access), Python 3.10+, pytest,
jq and the Rust toolchain. It fetches `origin/main`, checks upstreams in a
temporary worktree, runs the checks and opens a PR only when content changed.
It does not touch the calling worktree or merge the PR. It includes upstream
skill files even when their names match nibble's ignore rules, and pushes them
to the nibble repository. Register only sources whose contents can be shared
there. An existing branch for the same update must be resolved or removed
before trying again.

Edits to a vendored skill already in `origin/main` are merged against its pinned
upstream base; conflicts stop the update. Files containing NULs or non-UTF-8
bytes are not line-merged when both sides change their content. Uncommitted or
unpushed edits in the calling worktree stay untouched and are not included in
the update PR. If an upstream is unavailable, the bundled copy remains usable.
After the PR is reviewed and merged, update the nibble checkout and run its
installer to apply the copies.

## Adding a skill

For a nibble-owned skill, create `skills/<name>/SKILL.md` (frontmatter: `name`,
`description`) and add any references or scripts inside the directory. Add its
license to the local-skill `case` in `scripts/skills-lock.sh`, then run that
script to update the lock.

For an external skill, add an entry to `skills-sources.json` with `repository`,
`branch`, the repository-relative `path` to the skill directory, an empty
`revision`, and the `license` identifier. Set `license_file` to a
repository-relative license file when needed. Run `python3 scripts/skills-sync.py`
to import the complete directory and pin its commit, then review and submit
those changes through a PR. The local sync command writes to the current
checkout; it refuses to import over an existing directory without a base. Run
only one local sync at a time in a given checkout.
Once merged, the PR command above checks it alongside the other sources.

For both nibble-owned and external skills, add the name to `NIBBLE_SKILLS` in
`uninstall.sh`. Uninstall discovers bundled skills from a checkout, but a piped
uninstall has no checkout to inspect.
