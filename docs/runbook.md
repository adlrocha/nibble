# nibble sdlc runbook

`nibble sdlc runbook` prints this page, followed by the settings of the repository you run it in. The page is also at `docs/runbook.md` in the nibble repository.

## What it is

You say what you want. An agent builds it, has it reviewed, and merges it if nobody needs to look. You only hear about the pull requests that need you, each with one question.

1. **Spec.** You write what you want, in an issue or in the prompt. The agent asks only when the answer changes what it builds.
2. **Build.** The agent works on a branch, with tests, and has a fresh sub-agent review the diff before pushing.
3. **Review loop.** nibble-review reviews every push. The agent fixes or answers each finding until the current commit gets an LGTM. There is no round limit. It comes to you only when the findings point to a design problem.
4. **Land.** The agent runs `nibble land pr <number>`. Land checks the fixed rules, then asks a fresh judge, which is the same agent and model with no tools. Then it merges or tells the agent to ask you.

| Piece | Runs | Does |
| --- | --- | --- |
| nibble-review | GitHub Actions, on every push | Reviews, and posts a `## Risk` comment with the fixed rules' result |
| `nibble-sdlc` skill | Your agent, Claude Code or pi | Steps 1 to 4 |
| `nibble land` | Your machine, with your `gh` login | Fixed rules, judge, merge |

Nothing runs on a schedule, and there is no server to run.

## Set up a repository

1. `nibble sdlc install` opens a PR with the reviewer workflow and `.nibble/sdlc.toml` in shadow mode. Merge it.
2. Add the `OPENCODE_API_KEY` repository secret if the repository doesn't have it.
3. In `.nibble/sdlc.toml`, set `required_checks` to your CI check names as GitHub shows them (`gh pr checks <number>`), `authors` to your login, and a `profile`.
4. Write `.nibble/risk.md` (see The risk map below).
5. If the repository's `AGENTS.md` says agents never merge, change that step to "run `nibble land pr <number>`".
6. Run `nibble land backtest <a few recent PR numbers>` and read what it would have done.
7. Set `mode = "enforce"` when you agree with it.
8. Turn on a ruleset that requires your checks on the default branch, so GitHub enforces them even if land is wrong.
9. Allow `nibble land` in your agent's permissions, or every merge waits for a prompt.

Steps 3, 4, 5 and 7 go in a pull request like any other change. Changing `.nibble/sdlc.toml` always needs you, so land never merges it.

## The levers

All of them live in the `[land]` table of `.nibble/sdlc.toml`, read from the default branch, so a pull request can't relax its own rules.

| Setting | Default | What it does | Change it when |
| --- | --- | --- | --- |
| `mode` | `shadow` | `off` does nothing. `shadow` decides and reports but never merges. `enforce` also merges | You trust its decisions (enforce), or want everything stopped (off) |
| `profile` | `critical` | Which paths always need you, and how much risk the judge may accept. See Profiles below | Almost always. Pick the one that fits the repository |
| `required_checks` | none | CI checks that must pass. With none, nothing merges unattended | Always. Name your test job |
| `authors` | any person | Whose pull requests may merge unattended. Anyone else's always needs you | Others push to the repository |
| `always_human` | none | Extra paths that always need you, added to the profile's | A folder is too risky to merge unseen, like payments or legal copy |
| `[land.ask_when]` | none | Situations, in words, the judge must send to you, like `newsletter = "Changes who gets an email"` | The risk is in what a change does, not where it is |
| `main_is_staging` | `false` | Tells the judge that merging only reaches staging, so a merge is easier to undo | `main` deploys to dev and a release deploys to production |
| `max_lines` | none | A hard size limit. Markdown and text files, tests, snapshots and lockfiles don't count | You want big changes seen whatever they are |
| `reviewer` | `github-actions[bot]` | The account nibble-review posts as | nibble-review posts as a GitHub App of yours |

Review rounds and size never call you unless you set `max_lines`. These always need you, whatever the judge says:

- a pull request opened by a bot (Dependabot included) or by someone outside `authors`;
- no `required_checks` set;
- a change to a path the profile or `always_human` protects;
- more counted lines than `max_lines`, if set.

Outside the file:

| Lever | What it does |
| --- | --- |
| `.nibble/risk.md` | What the judge should know about this repository. See The risk map below |
| `nibble land flag <number> should-ask "<why>"` | Tells later judges that a merge was wrong. `should-merge` says an ask was needless |
| `nibble land backtest <numbers> --policy <file> --risk-map <file>` | Tries a changed policy or risk map on past pull requests before you commit it |
| A branch ruleset | Makes GitHub refuse a merge whose checks failed, whatever land decides |

## Profiles

A profile sets two things: which paths always need you, and how much risk the judge may accept.

| Profile | Paths that always need you | The judge merges | Fits |
| --- | --- | --- | --- |
| `side-project` | `.nibble/sdlc.toml`, workflows, local actions | Everything except high impact that can't be reverted | Tools only you use |
| `standard` | Also the rest of `.nibble/` (the risk map included), everything under `.github/`, migrations and schemas, agent instructions and settings (`AGENTS.md`, `.claude/`, `.mcp.json`), git hooks, `.env`, `CODEOWNERS`, `.gitmodules`, `.gitattributes` | Low impact, medium impact that can be reverted, and high impact only if it can be reverted and a problem is unlikely | Products where `main` isn't production, or where a revert is cheap |
| `critical` | Also dependency manifests and lockfiles, build and install scripts, containers, infrastructure | Low impact unless a problem is likely, and medium only if it can be reverted and a problem is unlikely. High impact always asks | What ships straight to users, money, or other people's machines |

The judge can always hold a change back. It can never clear a fixed rule or a threshold.

## The risk map

`.nibble/risk.md` is plain prose for the judge. Write what is true of the repository, and where that comes from, so the judge can tell a copy change from a change that emails ten thousand people. It is context, not rules. To make something always need you, use `always_human` or `ask_when`. In `standard` and `critical`, a change to the risk map itself needs you.

A start:

```markdown
# Risk map

## What a merge sets off
- Merging to main deploys <what> to <where> (see .github/workflows/deploy.yml).

## What becomes public
- <pages, packages, emails, posts>

## Money, people, and stored data
- <payments, wallets, user records, migrations>

## What can't be undone
- <sent emails, published packages, deleted data, on-chain transactions>

## What is safe
- <docs, tests, internal tools: say so, or the judge will be careful with them too>
```

## Day to day

When land needs you, the agent stops and asks you its question with the PR link. The PR gets the `needs-human` label and a `## Land` comment saying why.

- **Yes:** merge it on GitHub.
- **No:** say so in the PR or close it. Asking land again doesn't help, because a held commit is never judged again.

| Command | Shows |
| --- | --- |
| `nibble land queue` | Your pull requests that wait for you, in every repository |
| `nibble land digest` | What merged since the last release, riskiest first |
| `nibble sdlc check` | Whether the repository's setup matches what install expects |

## When it gets it wrong

| It | Do |
| --- | --- |
| Merged something you wanted to see | Revert it, run `nibble land flag <number> should-ask "<why>"`, and add the path to `always_human`, a line to `ask_when`, or a fact to `.nibble/risk.md` |
| Asks about things that are fine | Read the reason in the `## Land` comment. A fixed rule (a protected path, `authors`, `required_checks`, `max_lines`) means changing that setting or the profile. A judge decision means the risk map should say what is safe, and `nibble land flag <number> should-merge "<why>"` |
| Does something you can't explain | Set `mode = "shadow"` or `"off"`, and look at the `## Land` comments |

Try a change on the past before committing it:

```bash
nibble land backtest 41 42 43 --policy new-sdlc.toml --risk-map new-risk.md
```

## Where to read more

- [docs/sdlc.md](sdlc.md): install, uninstall, and check.
- [docs/land.md](land.md): how land decides, merges, learns, and what it can't stop.
- [nibble-review's Land section](https://github.com/adlrocha/nibble-review#land): the fixed rules and the exact profile paths.
