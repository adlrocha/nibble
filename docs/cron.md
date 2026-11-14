# Scheduled jobs (`nibble cron`)

> The scheduler that runs jobs on their schedules lands with
> `nibble-cron.service` in the next change; until then `nibble cron run`
> runs a job by hand.

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
and everything after `## Prompt` is the prompt, sent as written but for
leading and trailing blank space, which is trimmed. An unknown
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
| `notify` | `"slack"` | `"slack"` posts a report when the run finds something; `"none"` keeps it local |
| `read_only` | `true` | Withholds Claude Code's file-editing tools (Edit, Write, NotebookEdit). The run keeps Bash and is only asked not to change anything, so this guards against slips, not against a run set on changing files |

For now `nibble cron run` types the prompt into the repo's running sandbox
session, where `timeout`, `model`, `env`, `notify` and `read_only` do not
apply yet. A manual run is one you asked for, so it also ignores
`enabled`, `expires_at`, `skip_if_running` and a `nibble cron stop` pause;
it only needs the job approved here.

## Commands

```bash
nibble cron add                       # register the repo around the current directory,
                                      # and review its job files
nibble cron add --repo ~/src/shop     # register another repo
nibble cron add --schedule "0 8 * * *" --prompt "Audit dependencies" --label "Deps audit"
                                      # write .nibble/cron/deps-audit.md, then register
nibble cron add --file job.md         # write a job file's settings and prompt into the repo
                                      # (in nibble's layout, comments dropped), then register
nibble cron list [repo]               # jobs, status, next and last run
nibble cron approve deps-audit        # review a new or changed job file
nibble cron edit deps-audit           # open the file in $EDITOR
nibble cron edit deps-audit --schedule "0 9 * * *" --expires +30d
nibble cron stop deps-audit           # pause on this machine only
nibble cron start deps-audit          # resume
nibble cron run deps-audit            # run now, in the repo's running sandbox
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
ran them on a schedule. `nibble cron` commands remind you of them until
you run `nibble cron import`; run it again later for any it skipped (say,
once their repo is cloned). `nibble cron import` lists each one, with the file it would write
(`.nibble/cron/<name>.md`, the name made from its label, `-2` added when
taken) and the repo it would register, and asks before doing anything,
because sandboxes can write that database. Imported jobs get
`read_only = false`, as they could edit before. Existing files are never
replaced; jobs whose repo is missing or is not a git repository are
skipped and listed. The old rows stay in `tasks.db`.
