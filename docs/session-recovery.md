# Session recovery runbook

How nibble tracks agent sessions, what survives a reboot/crash, and how to
get the right conversation back when more than one session exists for a repo.

## Where sessions live

Session transcripts are plain JSONL files on the **host**, bind-mounted into
every sandbox, so they survive container stops, host reboots, and crashes:

| Agent | Location |
|-------|----------|
| pi | `~/.pi/agent/sessions/<slug>/<timestamp>_<session-id>.jsonl` |
| omp (oh-my-pi) | `~/.omp/agent/sessions/<slug>/<timestamp>_<session-id>.jsonl` |
| Claude Code | `~/.claude/projects/<slug>/<session-id>.jsonl` |

`<slug>` is the container working directory with `/` replaced by `--` and
wrapped in dashes (e.g. `/nibble` → `--nibble--`).

**Nothing is ever deleted.** `--fresh` renames Claude's current transcript to
`.jsonl.bak`; pi/omp sessions simply stay on disk next to newer ones.

## How nibble knows which session belongs to a sandbox

Each sandbox task in the nibble DB (`~/.nibble/tasks.db`) stores the session it
should resume:

- **pi / omp**: `pi_session_path` in the task context. Written:
  - eagerly, by the `nibble-memory` extension at every `session_start`
    (`nibble report session-path`) — this is the authoritative mapping;
  - at attach time, when a session is discovered or picked by hand;
  - shortly after spawn, by a best-effort background discovery.
- **Claude Code**: `claude_session_id`, written by the Stop hook
  (`nibble report session-id`).

`attach --btw` side sessions deliberately carry no `AGENT_TASK_ID`, so they
never overwrite the main session mapping — but they **are** kept on disk like
any other session.

## After a reboot or crash

1. Restart the containers:

   ```bash
   nibble sandbox resume --all
   ```

   (This also runs automatically at boot via `nibble-resume.service` when
   `loginctl enable-linger` is set.)

2. Re-attach:

   ```bash
   nibble sandbox attach <repo>            # or by task id
   nibble sandbox attach <repo> --pi       # pi-family agent (omp by default)
   ```

   Attach resumes the stored session for that task. If there is no stored
   mapping and several sessions exist for the repo, attach shows an
   interactive picker (title, date, size) instead of silently grabbing the
   newest file. Non-interactive callers keep the newest-wins behavior.

## When attach resumes the wrong conversation

Symptoms: you get a `--btw` side session, a Telegram-injected turn, or a
session from before the crash instead of your main conversation.

1. List what exists and what each task is linked to:

   ```bash
   nibble session list                     # the TASK column shows the link
   nibble session list --sandbox /path/to/repo
   ```

2. Read a candidate to confirm it's the one:

   ```bash
   nibble session read <session-id>
   ```

3. Attach to it explicitly — this also re-links the task so future attaches
   resume the same session:

   ```bash
   nibble sandbox attach <repo> --session <session-id>
   ```

## Inspecting sessions without attaching

```bash
nibble session list                       # grouped by day, with titles
nibble session read <id>                  # formatted transcript
nibble session read <id> --raw            # raw JSONL
nibble web                                # browser UI on :7878 (search + viewer)
```

## Troubleshooting

- **`nibble session list` shows no task link for a session you care about.**
  The mapping is only written when the agent runs with `AGENT_TASK_ID` set
  (normal attach) or when attach discovers/picks the session. Attach once
  with `--session <id>` to link it.
- **The stored session file is gone** (e.g. you deleted files by hand).
  Attach detects this, prints "stored session gone, re-discovering…", and
  falls back to the picker / newest session.
- **A crash left the newest file truncated.** Pick the previous session from
  the attach picker or `nibble session list`; truncated files still read
  fine up to the cut point.
- **Sandboxes don't come back after reboot at all.** Check
  `loginctl show-user $USER --property=Linger` (needs `Linger=yes`) and
  `systemctl --user status nibble-resume.service`.
