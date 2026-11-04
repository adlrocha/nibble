# Recoverable zellij sessions (`nibble mux`)

`nibble mux [NAME]` opens a named zellij session (default `main`) whose
agents come back after the session is killed by mistake, zellij crashes, or
the machine reboots. Each agent resumes its own conversation in its old tab
and directory.

```bash
nibble mux                # open "main": attach if running, rebuild if gone
nibble mux work           # any other name (letters, digits, - _ .)
nibble mux work --show    # the recorded agents per tab, flagging any that can't come back
nibble mux work --forget  # drop nibble's record for "work"
```

`nibble mux NAME` refuses to run inside a *different* zellij session (zellij
cannot nest); detach first (`Ctrl o`, `d`). Run it inside a session *of that
name* to start recording a session you already have open.

## What gets recorded

While the session runs, a background watcher (`nibble mux NAME --watch`,
logging to `~/.nibble/logs/mux-NAME.log`) lists the session's panes every
3 seconds and maps each pane to the agent registered in it. Agents register
their zellij pane *and* session at start (wrappers, Claude hooks, the pi
extension, `sandbox attach`), since pane ids are only unique within one
session. The record is kept in `~/.nibble/tasks.db` (`kv_store`, key
`mux:NAME`).

- An agent you close drops out of the record within a few seconds: its task
  is marked exited, or its pane returns to a plain shell.
- Agents that die *with* the session (kill, crash, reboot) stay in it: a
  change is only written once two consecutive samples agree, so a session
  caught while being torn down never overwrites a good record.
- Only agents nibble can resume are recorded: Claude Code and pi, on the host
  or in a sandbox. Hermes and omp panes are left out.

One watcher runs per session (a lock in `~/.nibble/mux/NAME.lock`). It exits
when the session has been gone for two samples, or after about a minute of
failed samples (e.g. zellij missing from its `PATH`). A watcher started for a
freshly rebuilt session waits up to 90 seconds for the previous one to exit,
so reopening right after a crash is still recorded.

## What happens on open

| zellij state of NAME | nibble has agents to restore | `nibble mux NAME` does |
|---|---|---|
| running | — | attach (and make sure it is recorded) |
| dead (`EXITED`) | yes | delete zellij's dead copy, rebuild the agent tabs |
| missing | yes | rebuild the agent tabs |
| dead (`EXITED`) | no | let zellij resurrect its saved copy |
| missing | no | start an empty session |

"Agents to restore" means recorded agents whose resume can still be planned:
the task is still in the database and the agent type is supported. Others
are reported and skipped *before* zellij's dead copy is deleted, so a session
with nothing restorable is left to zellij. A missing nibble wrapper stops the
restore instead, with nothing touched: re-run the installer (the
one-command install, or `./install.sh` in a clone) and open again.
The saved record keeps every recorded agent until the rebuilt session's
watcher records what actually came back.

Rebuilding writes a layout to `~/.nibble/mux/NAME.kdl` and starts the session
with it. Each agent pane runs `nibble mux --resume <task-id>`, which becomes:

| Agent | Resume command |
|---|---|
| Claude Code (host) | `~/.nibble/wrappers/claude-wrapper --resume <session-id>` (the wrapper picks the newest session for the directory if none was recorded) |
| pi (host) | `~/.nibble/wrappers/pi-wrapper --session <file>` (`--continue` if the file is gone) |
| Sandbox window | `nibble sandbox attach <repo> [--pi] --session <id>`, starting the container first if it is stopped |

When the agent exits, the pane drops to your `$SHELL` in the same directory.
Leaving that shell closes the pane, and the tab once its last pane is gone.
A recorded directory that no longer exists (a pruned worktree, a moved repo)
is left out, so the pane opens in zellij's default directory instead.
The resumed agents register as new tasks, and the record moves over to them
within a few seconds, so a second crash restores the latest conversations.

## Limits

- Panes without an agent (editors, servers, plain shells) are not rebuilt.
  When nibble rebuilds a session, zellij's saved copy (which would have
  restored them) is deleted so the name can be reused.
- Rebuilt tabs use zellij's tab bar and status bar, or the tab bar and nibble
  sidebar when the sidebar is your default layout (`layouts/default.kdl` from
  `nibble sidebar --install`, or the layout zellij's `default_layout` names).
  Other custom default layouts are not applied to rebuilt sessions.
- `--forget` only drops nibble's record, and refuses while the session is
  running (its watcher would write the agents straight back). If zellij
  still holds a saved copy of the dead session, the next open resurrects
  that copy.
