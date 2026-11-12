# Scheduled jobs (`nibble cron`)

A cron job is a prompt that runs on a schedule against a repository. Jobs
are files in the repository they belong to, so they are versioned with the
code and any machine can run them: clone the repo, register it, done.

## Job files

Each job is `<repo>/.nibble/cron/<name>.md`. The file name (lowercase
letters, digits, `-`, `_`) is the job's name.

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
| `schedule` | required | Cron expression (5 fields, or 6 with seconds), in this machine's local time |
| `enabled` | `true` | `false` turns the job off on every machine |
| `skip_if_running` | `true` | Skip a due run while the previous one is still going |
| `expires_at` | none | RFC3339 time after which the job no longer runs |
| `timeout` | `"30m"` | Longest a run may take (`s`, `m`, `h`, `d`; at most `6h`) |
| `model` | Claude Code's default | Model for the run |
| `env` | `[]` | Names of variables the run gets from this machine's [secrets](#secrets) |
| `notify` | `"slack"` | `"slack"` posts a report when the run finds something; `"none"` keeps it local |
| `read_only` | `true` | Withholds the file-editing tools, so the run cannot change your working tree |

## Commands

```bash
nibble cron add                       # register the repo around the current directory
nibble cron add --repo ~/src/shop     # register another repo
nibble cron add --schedule "0 8 * * *" --prompt "Audit dependencies" --label "Deps audit"
                                      # write .nibble/cron/deps-audit.md, then register
nibble cron add --file job.md         # copy a job file into the repo, then register
nibble cron list [repo]               # jobs, status, next and last run
nibble cron edit deps-audit           # open the file in $EDITOR
nibble cron edit deps-audit --schedule "0 9 * * *" --expires +30d
nibble cron stop deps-audit           # pause on this machine only
nibble cron start deps-audit          # resume
nibble cron run deps-audit            # run now
nibble cron kill deps-audit           # delete the job file
nibble cron forget ~/src/shop         # stop running the repo's jobs here
```

A job is named by its name, by `repo/name` (the repo's directory name) when
two registered repos have a job of that name, or by the path of its file.

`edit` flags rewrite the file in nibble's own layout, so comments in the
settings are lost; edit in `$EDITOR` to keep them. Changes to job files
apply on the next pass, with nothing to reload. Commit them to share them.

`stop` and `start` only affect this machine. To turn a job off everywhere,
set `enabled = false` in its file (`nibble cron edit <job> --disable`).

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
repo's token. Give jobs the narrowest token that works: the run's agent can
read every value it is given.

## State

What this machine knows about cron (registered repos, next and last runs,
paused jobs) is in `~/.local/state/nibble/cron/cron.db` (`$XDG_STATE_HOME`
is honoured). It is kept out of `~/.nibble` so a sandboxed agent cannot
register repos or change when jobs run.

Older nibble versions stored jobs in `~/.nibble/tasks.db`. The first `nibble
cron` command after upgrading writes each of them to its repo as
`.nibble/cron/<label>.md` (with `read_only = false`, as they could edit
before) and registers the repo. It says which files it wrote; commit them to
keep them.
