# Scheduled jobs (`nibble cron`)

A cron job is a prompt that runs on a schedule against a repository. Jobs
are files in the repository they belong to, so they are versioned with the
code and any machine can run them: clone the repo, register it, done.

## Job files

Each job is `<repo>/.nibble/cron/<name>.md`. The file name is the job's
name: up to 64 lowercase letters, digits, `-` and `_`, starting with a
letter or digit.

```markdown
# Production logs

schedule = "*/30 * * * *"
timeout = "20m"
env = ["NORTHFLANK_API_TOKEN"]

## Prompt

Read the last 30 minutes of production logs and report anything wrong.
```

The `# ` line is the title. Everything between it and `## Prompt` is TOML,
and everything after `## Prompt` is the prompt, sent as written. An unknown
key or a bad value makes the file invalid; `nibble cron list` shows why, and
the repo's other jobs still run.

| Key | Default | Meaning |
|-----|---------|---------|
| `schedule` | required | Cron expression (5 fields), in this machine's local time |
| `enabled` | `true` | `false` turns the job off on every machine |
| `skip_if_running` | `true` | Skip a due run while the previous one is still going |
| `expires_at` | none | RFC3339 time after which the job no longer runs |
| `timeout` | `"30m"` | Longest a run may take (`s`, `m`, `h`, `d`; at most `6h`) |
| `model` | Claude Code's default | Model for the run |
| `env` | `[]` | Names of variables the run gets from this machine's [secrets](#secrets) |
| `notify` | `"slack"` | `"slack"` posts a report when the run finds something; `"none"` keeps it local (see [Reports in Slack](#reports-in-slack)) |
| `read_only` | `true` | Withholds Claude Code's file-editing tools (Edit, Write, NotebookEdit). The run keeps Bash and is only asked not to change anything, so this guards against slips, not against a run set on changing files |

## Commands

```bash
nibble cron add                       # register the repo around the current directory,
                                      # and review its job files
nibble cron add --repo ~/src/shop     # register another repo
nibble cron add --schedule "0 8 * * *" --prompt "Audit dependencies" --label "Deps audit"
                                      # write .nibble/cron/deps-audit.md, then register
nibble cron add --file job.md         # copy a job file into the repo, then register
nibble cron list [repo]               # jobs, status, next and last run
nibble cron approve deps-audit        # review a new or changed job file
nibble cron edit deps-audit           # open the file in $EDITOR
nibble cron edit deps-audit --schedule "0 9 * * *" --expires +30d
nibble cron stop deps-audit           # pause on this machine only
nibble cron start deps-audit          # resume
nibble cron run deps-audit            # run now and print the report (fails if the run did)
nibble cron runs [deps-audit]         # recent runs
nibble cron show 12                   # one run's full report
nibble cron kill deps-audit           # delete the job file
nibble cron forget ~/src/shop         # stop running the repo's jobs here
nibble cron import                    # move jobs from older nibble versions (see below)
```

Your home directory, anything inside `~/.nibble`, and symlinked job files
are refused: sandboxes can write `~/.nibble`, and a link could point a job
anywhere.

A job is named by its name, by `repo/name` (the repo's directory name) when
two registered repos have a job of that name, or by the path of its file.
When a bare name is in several repos and you are inside one of them, that
one is used.

`edit` flags rewrite the file in nibble's own layout, so comments in the
settings are lost; edit in `$EDITOR` to keep them. Changes to job files
are read every time, with nothing to reload. Commit them to share them.

`stop` and `start` only affect this machine. To turn a job off everywhere,
set `enabled = false` in its file (`nibble cron edit <job> --disable`).
These switches, and `expires_at`, are for the schedule: `nibble cron run`
is a run you asked for, so it runs a disabled, expired or stopped job too.
It only needs the job approved here, and still skips while a run is going
(unless `skip_if_running = false`).

## How a job runs

`nibble-cron.service` (installed by `install.sh`, running `nibble cron
daemon`) checks every 30 seconds for due jobs. It re-reads the files each
time, so a `git pull` that brings new or changed jobs applies by itself,
once you approve them (see below).

Each run is a fresh headless Claude Code session (`claude -p`) in the repo's
sandbox, started if none is running. It never touches the conversation you
work in, and it shows in the sidebar as its own row (`… · cron <name>`)
while it runs. The prompt gets notes saying the run is unattended, must not
commit, push or change files, and must end with a report:

- `status`: `ok`, `warn` or `critical`;
- `summary`: what was checked and found;
- `findings`: one per problem, each with a `fingerprint` that stays the same
  across runs while the problem lasts, a severity, a title, evidence and a
  suggested action.

Claude Code is held to that shape with `--json-schema`. A run that times
out, crashes or gives no valid report is recorded as `timeout` or `failed`
with the reason. The timeout is enforced inside the container, and the host
gives up 90 seconds later and kills what is left. Runs are kept (the newest
200 per job) with their report, cost and session id; `nibble cron show`
prints the id, to resume the session inside the sandbox.

A slot missed while the machine was off runs once when it is back. A job
whose previous run is still going is skipped (unless `skip_if_running =
false`), whether that run came from the daemon or `nibble cron run`. At most
four runs go at once; others wait for a free slot. Stopping or upgrading the
service cuts the runs in flight off from nibble: their sessions in the
sandbox go on until their timeout or the daemon's next start, which records
them as failed and stops what is left.

Before passing secrets, the runner checks with podman that the container
really mounts the repo, since the record naming it is in `~/.nibble`, and
then runs in that same container by id.

The daemon logs to `$XDG_STATE_HOME/nibble/cron/daemon.log`
(`~/.local/state/nibble/cron/daemon.log` by default).

The service gets none of your shell's exports. When it has to start a
repo's sandbox (after a reboot, or `nibble sandbox kill`), that sandbox
only has the keys the service has, so put any your sandboxes need, such as
`ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL` or `GITHUB_TOKEN`, in
`~/.config/nibble/cron.env` (`NAME=value` lines, mode 0600) and run
`systemctl --user restart nibble-cron`.

## Reports in Slack

With [`nibble rc`](rc.md) set up, a run that finds something posts a card
to your DM with the Slack app: the job, the summary, up to eight findings
(new ones first, marked) and three buttons.

- **Investigate** starts a sandbox session in the card's thread, given the
  report, to confirm the cause and propose a fix without changing anything.
  Reply in the thread to steer it, or ask it to fix.
- **Fix → PR** starts one told to fix the findings and open a pull request
  through the repo's delivery flow (the `nibble-sdlc` skill where
  `.nibble/sdlc.toml` exists). It stops at the PR; merging stays yours.
- **Snooze 7d** silences the card's findings on this machine for a week.

Both sessions run like any other started from Slack: in a new tab of the
`nibble mux` session, relayed to the thread. The report is passed as data
to check, since it was written from logs and tool output.

A problem is reported when it first appears, again when it gets worse (warn
→ critical), and once a day while it lasts. A problem that goes away and
comes back is reported again. A run that fails or times out is a problem of
its own, reported the same way (so a job that keeps failing is reported
daily); it leaves the job's other findings as they were. `notify = "none"` keeps a job's reports out
of Slack; they stay in `nibble cron runs`.

Cards wait in `nibble rc`'s private queue, so a report made while the Slack
daemon is down is posted when it is back (cards older than 30 days are
dropped). A card Slack refuses is retried after 1, 5, 15, 60 and 360
minutes, then given up.
Without `nibble rc` set up nothing is queued, and reports stay in `nibble
cron runs`.

## Approving jobs

A job file can be written by anyone who can write the repo, including an
agent in its sandbox. So a job only runs on a machine once you have
approved it there: nibble keeps the approved definition in its private
state and runs the file only while it still matches.

- `nibble cron add` shows each job file of the repo and asks. `--yes`
  approves them all.
- A job you create with `nibble cron add` is approved, being yours. So is
  one you edit with `nibble cron edit` while it is approved; editing a job
  that is `changed` or `unapproved` leaves it for `nibble cron approve`.
- `nibble cron import` shows each older job in full; importing approves it.
- A file that changes (a `git pull`, an edit in the sandbox) shows as
  `changed` and stops running until `nibble cron approve <job>`. A new file
  shows as `unapproved`. Changes that leave the definition the same, such as
  a comment, keep the approval.
- A file that goes away for a while (a branch without it) keeps its
  approval, and is compared against it when it is back.

## Secrets

Some jobs need credentials, such as a read-only token for a log API. They
are kept per repo in `~/.config/nibble/cron-secrets.toml` (mode 600, outside
`~/.nibble`, which sandboxes mount):

```bash
nibble cron secret set NORTHFLANK_API_TOKEN --repo ~/src/shop   # prompts without echo
nibble cron secret list
nibble cron secret unset NORTHFLANK_API_TOKEN --repo ~/src/shop
```

A job file lists the names it needs in `env`. A value is only given to jobs
of the repo it was set for, so one repo's job file cannot ask for another
repo's token, and a job only gets values once you approved its file. Give
jobs the narrowest token that works (read-only where possible): the run's
agent can read every value it is given, and while a run goes the value is
in the environment of a process in the repo's sandbox, where the sandbox's
other agents could read it.

## State

What this machine knows about cron (registered repos, next and last runs,
paused jobs) is in `~/.local/state/nibble/cron/cron.db` (`$XDG_STATE_HOME`
is honoured). It is kept out of `~/.nibble` so a sandboxed agent cannot
register repos, approve jobs or unpause them. What a job does is in its
file, which a sandbox can change; that is what approval is for.

Older nibble versions stored jobs in `~/.nibble/tasks.db`, where nothing
ran them on a schedule. `nibble cron` commands remind you while any are
left. `nibble cron import` lists each one, with the file it would write
(`.nibble/cron/<name>.md`, the name made from its label, `-2` added when
taken) and the repo it would register, and asks before doing anything,
because sandboxes can write that database. Imported jobs get
`read_only = false`, as they could edit before. Existing files are never
replaced; jobs whose repo is missing or is not a git repository are
skipped and listed. The old rows stay in `tasks.db`.
