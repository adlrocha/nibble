# SDLC flow

`nibble sdlc` installs a delivery flow into a repository: you write a spec, an agent builds it and loops with nibble-review until LGTM, and `land` reports whether the PR could merge without you.

Land is in **shadow mode**: it reports and never merges or blocks.

## Requirements

- An authenticated `gh`, with write access to the repository.
- A nibble-review release that includes `land`. Until one ships, pass `--ref` with a commit or branch that has it, or the `land` input does nothing.
- If `.github/workflows/nibble-review.yml` already exists, a `permissions:` block in it, at the top level or on the reviewer job. Install adds `statuses: write` to that block and stops when there is none.
- An `OPENCODE_API_KEY` repository secret. Listing secrets needs admin access, so without it the secret check is skipped.

## Install

```bash
nibble sdlc install --dry-run   # show the plan
nibble sdlc install             # open the PR, asking first
```

Scripts and agents need `--yes`, because without a terminal the prompt counts as no.

Install works in a throwaway worktree, so your checkout is untouched. A local `chore/nibble-sdlc` branch with unmerged work stops the run rather than being deleted. It opens one PR on `chore/nibble-sdlc` that:

- creates or updates `.github/workflows/nibble-review.yml`, pinning the action to a commit, enabling `land`, and granting `statuses: write` to the reviewer job so land can post its commit status. Your other inputs are kept;
- adds `.nibble/sdlc.toml`, unless the repository already has one.

`AGENTS.md` and `CLAUDE.md` are not touched. `.nibble/sdlc.toml` marks the repository as using the flow, and the global `nibble-sdlc` skill, which applies wherever that file exists, carries the agent's rules.

Pin to anything other than the latest release with `--ref <tag, branch, or commit>`. Pass `--main-is-staging` when `main` deploys to dev and a release deploys to production. It only applies when install creates the config; after that, edit the file. Rerunning install moves the pin to the latest release, or to `--ref`, and changes nothing else if the repository is already set up. `check` reports an older pin without failing, since it may be deliberate.

The agent's side is the `nibble-sdlc` skill, which `install.sh` installs globally.

## Several repositories

`install`, `uninstall`, `check` and `status` take repositories as `owner/name`. With none, they act on the repository in the current directory.

```bash
nibble sdlc install --dry-run adlrocha/nibble adlrocha/nibble-review
nibble sdlc install --yes adlrocha/nibble adlrocha/nibble-review
nibble sdlc check adlrocha/nibble adlrocha/nibble-review
nibble sdlc status adlrocha/nibble adlrocha/nibble-review
```

Each repository gets a heading. `install`, `uninstall` and `check` work from a temporary clone without a checkout, which is deleted afterwards; `status` only talks to GitHub. A failing repository is reported and the rest still run, and the exit code is non-zero if any failed or, for `check`, drifted. `merge` acts on the current directory only.

## Use it

```bash
nibble sdlc check        # compare the default branch with the expected setup
nibble sdlc status       # list open PRs (up to 500) with their land result
nibble sdlc merge 42     # merge the exact head of PR 42
```

`merge` shows land's result, then asks. It pins the commit, so a later push stops it, and uses `[merge] method` from `.nibble/sdlc.toml` on the default branch. `--auto` lets GitHub merge once required checks pass. It is pinned only when you enable it, and land isn't a required check yet, so a later push can merge without land clearing it. Without a terminal, `--yes` works only when land cleared that commit, so an agent can't use it to override land.

## Remove it

```bash
nibble sdlc uninstall                    # keeps the reviewer
nibble sdlc uninstall --remove-reviewer  # deletes the workflow too
```

It also takes `--dry-run`, and `--yes` for scripts.

Uninstall opens the reverse PR. It removes the `land` input, `statuses: write` and `.nibble/sdlc.toml`, including any of these you added by hand. The plan lists each file before you confirm.

## Settings

The rules land applies are documented in the [nibble-review README](https://github.com/adlrocha/nibble-review#land).
