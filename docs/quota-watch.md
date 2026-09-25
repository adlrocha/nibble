# Quota auto-continue (`nibble quota-watch`)

When a Claude Code / pi / omp turn dies on a **subscription quota error**
("usage limit reached", "You've hit your limit", out-of-credits, …), the task
usually isn't done — the quota window resets hours later and the session can
continue where it left off. The quota-watch daemon does that automatically:

1. **Detect** — tails agent session transcripts for quota errors.
2. **Wait** — parses the reset time from the error when possible, marks the
   task blocked (`quota limit reached — auto-continue scheduled for Fri 17:00`)
   so `nibble status`, the sidebar and the web UI show why it is parked.
3. **Continue** — once the window resets (plus a 2-minute buffer), the task is
   continued automatically.

Works for **host sessions and sandbox sessions alike**, because the agent
config homes (`~/.claude`, `~/.pi`, `~/.omp`) are bind-mounted into every
sandbox — one host-side scanner sees every transcript.

## How each session is continued

| Session | Mechanism |
|---------|-----------|
| Host, in a zellij pane | `zellij action write-chars --pane-id <id>` types `continue` into the **live TUI** (pane + session recorded by the wrappers) — you watch it pick up where it stopped |
| Host, no pane / pane closed | Headless resume turn in the task's working directory (`claude --resume <sid>`, `omp --resume <path> -p`, `pi --session <path> -p`) |
| Sandbox (any state) | Same headless resume turn via `podman exec -i`; the container is started first if it is down |

Headless turns run with the same autonomous flags nibble uses everywhere
(`--dangerously-skip-permissions` for Claude, `--auto-approve` for omp) and
carry `AGENT_TASK_ID`, so hooks/extensions keep updating the task status
normally.

## What is detected

Only errors that a quota reset can actually fix. Observed formats and their
classification:

| Example | Class | Action |
|---|---|---|
| `Usage limit reached for 5 hour. Your limit will reset at 2026-09-22 00:18:31` (zai) | quota | wait for exact reset time |
| `You've hit your limit · resets 5pm (UTC)` (Claude Code) | quota | wait for reset time-of-day |
| `… retry-after-ms=6565000` (zai) | quota | wait for the interval |
| `You've reached your 5-hour usage limit` / `… for this billing cycle` (Kimi) | quota | no timestamp → retry every 30 min |
| `You have run out of credits … spending-limit` (Grok) | quota | no timestamp → retry every 30 min |
| `temporarily overloaded, please try again later`, `Connection error.`, timeouts | transient | ignored — the agent retries these itself |
| `401 Authentication Failed`, `subscription plan does not yet include access` | other | ignored — waiting cannot fix these |

If a continue attempt lands on a still-active quota (or the provider moved
the window), the watcher re-arms with the new reset time. After
`max_attempts` (default 8) it parks the task with
`quota: gave up after N auto-continue attempts`.

## Service

`install.sh` installs and enables a systemd user service:

```bash
systemctl --user status nibble-quota-watch.service
journalctl … tail -f ~/.nibble/logs/quota-watch.log
systemctl --user disable --now nibble-quota-watch.service   # turn it off
```

One manual pass (no daemon): `nibble quota-watch --once`.

## Configuration (`~/.nibble/config.toml`)

```toml
[quota_watch]
enabled = true                # master switch (service exits when false)
poll_secs = 60                # transcript scan interval
continue_message = "continue" # what is sent/typed to continue the task
max_attempts = 8              # per quota episode
unknown_retry_secs = 1800     # retry interval when no reset time is parseable
reset_buffer_secs = 120       # extra wait after the stated reset time
```

## Notes

- Zellij keystroke continuation needs the pane id **and** session name; the
  wrappers record both since this feature (older task rows lack the session
  name and fall back to headless turns).
- Reset times without a timezone (zai absolute timestamps) are treated as UTC.
- Claude Code time-of-day resets (`resets 5pm (UTC)`) are parsed for UTC only;
  other timezones fall back to the fixed retry interval.
