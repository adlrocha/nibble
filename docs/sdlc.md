# SDLC flow

`nibble sdlc` installs a delivery flow into a repository: you write a spec, an agent builds it and loops with nibble-review until LGTM, and `land` reports whether the PR could merge without you.

Land is in **shadow mode**: it reports and never merges or blocks.

## Requirements

- An authenticated `gh`, with write access to the repository.
- A nibble-review release that includes `land`. Until one ships, pass `--ref` with a commit or branch that has it, or the `land` input does nothing.
- An `OPENCODE_API_KEY` repository secret. Listing secrets needs admin access, so without it the secret check is skipped.

## Install

```bash
nibble sdlc install --dry-run   # show the plan
nibble sdlc install             # open the PR, asking first
```

Scripts and agents need `--yes`, because without a terminal the prompt counts as no.

Install works in a throwaway worktree, so your checkout is untouched. It opens one PR on `chore/nibble-sdlc` that:

- creates or updates `.github/workflows/nibble-review.yml`, pinning the action to a commit and enabling `land`. Your other inputs are kept;
- adds `.nibble/sdlc.toml`, unless the repository already has one;
- adds a marked block to `AGENTS.md`. Everything outside the markers is left alone.

Pin to anything other than the latest release with `--ref <tag, branch, or commit>`. Pass `--main-is-staging` when `main` deploys to dev and a release deploys to production. It only applies when install creates the config; after that, edit the file. Rerunning install moves the pin to the latest release, or to `--ref`, and changes nothing else if the repository is already set up. `check` reports an older pin without failing, since it may be deliberate.

The agent's side is the `nibble-sdlc` skill, which `install.sh` installs globally.

## Use it

```bash
nibble sdlc check        # compare the default branch with the expected setup
nibble sdlc status       # list open PRs with their land result
nibble sdlc merge 42     # merge the exact head of PR 42
```

`merge` shows land's result, then asks. It pins the commit, so a later push stops it, and uses `[merge] method` from `.nibble/sdlc.toml` on the default branch. `--auto` lets GitHub merge once required checks pass. It is pinned only when you enable it, and land isn't a required check yet, so a later push can merge without land clearing it. Without a terminal, `--yes` works only when land cleared that commit, so an agent can't use it to override land.

## Remove it

```bash
nibble sdlc uninstall                    # keeps the reviewer
nibble sdlc uninstall --remove-reviewer  # deletes the workflow too
```

It also takes `--dry-run`, and `--yes` for scripts.

Uninstall opens the reverse PR. It removes the `land` input, `statuses: write`, `.nibble/sdlc.toml`, and the `AGENTS.md` block, including any of these you added by hand. The plan lists each file before you confirm.

## Settings

The rules land applies are documented in the nibble-review README.
