# Slack remote control (`nibble rc`)

Carry on with an agent session from Slack while it keeps running in zellij,
or start a new one from your phone.

- **`/nibble-rc`** inside a Claude Code, pi or omp session (on the host or in
  a sandbox) posts the session to your DM with the nibble Slack app as a new
  thread, with its last reply. From then on:
  - a message in that thread is typed into the session's zellij pane;
  - each finished turn's reply comes back to the thread;
  - a permission prompt is posted as a note (reply with a number to pick a
    menu option, or answer at the terminal);
  - Slack's stop button interrupts the turn (Escape in the pane).
- **`/nibble`** in Slack opens a form: host or sandbox, agent (Claude Code
  or pi), repo or directory, and the first message. omp is not offered:
  these sessions open in a `nibble mux` session, and mux does not restore
  omp tabs. A running omp session can still be sent with `/nibble-rc`.
  The session opens in a new tab of the zellij session named by
  `rc.zellij_session` (`main` by default; started in the background and
  recorded by `nibble mux` if it isn't running) and gets its own thread.
  A plain message to the app offers the same form, prefilled with what you
  wrote.
- **Disconnect** a thread with the button on its session card, or
  `/nibble-rc stop` in the session. This only stops relaying: the zellij tab
  and the agent keep running.
- **End session** (on the card, after a confirmation) is offered only for
  sessions started from Slack. It closes the tab the daemon opened, which
  stops the agent; if you added panes of your own to that tab, only the
  agent's pane is closed. The conversation stays in the agent's history
  (`claude --resume`, or pi's session list). Sessions you sent with
  `/nibble-rc` are never closed from Slack.
- **`/nibble list`** in Slack, or `nibble rc list`, shows the sessions in
  Slack.

The session in Slack is the one in the pane, so you can type at the
terminal and in Slack interchangeably. A session started from Slack on the
host is a fresh Claude conversation (`claude --fresh`); in a sandbox it is a
`--btw` side session, so the repo's main session is left alone.

## How it works

```
zellij pane (claude | pi | omp, host or sandbox)
   ▲ typed in: zellij write-chars        │ hooks / pi extension:
   │ (bracketed paste for multi-line)     ▼ nibble report status …
 nibble rc daemon ◄──── nibble's database (tasks, rc:* records) ────► transcript
   ▲  Socket Mode (outbound WebSocket, no public URL)                  (last reply)
   ▼
 Slack DM with the nibble app: one thread per session
```

- One daemon (`nibble-rc.service`) holds the Slack connection. It watches the
  task status agents already report: working → the thread shows Slack's
  "processing" state; a permission prompt → a note; a finished turn → the
  reply, read from the agent's transcript.
- `/nibble-rc` never talks to Slack itself. It leaves a request in nibble's
  database and waits for the daemon's answer. The Slack tokens live in
  `$XDG_CONFIG_HOME/nibble/slack.env` (default `~/.config/nibble/slack.env`,
  mode 600), and which thread drives which pane is kept in
  `$XDG_STATE_HOME/nibble/rc/rc.db` (default `~/.local/state/nibble/rc`), both
  outside `~/.nibble`, because every sandbox mounts `~/.nibble`: sandboxed
  agents can neither read the tokens nor steer what is typed where.
- In Claude Code, `/nibble-rc` is handled by a `UserPromptSubmit` hook that
  blocks the prompt, so it spends no model turn. In pi/omp it is an extension
  command.
- Only your Slack member ID is listened to. Messages from anyone else, and
  from other channels, are ignored.

## Setup

1. **Create the Slack app.** At <https://api.slack.com/apps>: *Create New
   App* → *From a manifest* → pick your workspace → paste the output of
   `nibble rc manifest` (the file is `docs/rc-slack-manifest.yml`) → *Create*.
   Leave *OAuth & Permissions* alone otherwise: no redirect URLs, PKCE or
   token rotation (the daemon needs a token that does not expire).
2. **Install it.** *Install App* → *Install to Workspace*. Copy the *Bot User
   OAuth Token* (`xoxb-…`). If you change the manifest later and Slack asks
   to reinstall, copy the token again afterwards.
3. **App-level token.** *Basic Information* → *App-Level Tokens* → *Generate
   Token and Scopes*, add scope `connections:write`, generate, copy the
   `xapp-…` token.
4. **Your member ID.** In Slack: your profile → ⋮ → *Copy member ID*
   (`U…`).
5. **Store them:** `nibble rc setup`. Tokens are read without echo, like a
   password: paste and press Enter. Each is shown back masked (first and
   last characters and its length) so you can tell a wrong or cut-short
   paste. Nothing is saved until Slack accepts all three.
6. **Start the daemon:** `systemctl --user enable --now nibble-rc` (or re-run
   `./install.sh`, which enables it once the credentials exist).
7. **Check:** `nibble rc doctor` verifies the credentials, your DM with the
   app, Socket Mode, zellij and the daemon.

Then open the app in Slack (it is under *Apps*, or the agent entry in the
top bar), and send `/nibble-rc` from any agent running in zellij.

## Privacy

- The app is private to the workspace you create it in. Do not turn on
  *Manage Distribution* or submit it to the Slack Marketplace.
- No public URL or open port: Socket Mode is an outbound connection from
  your machine.
- The daemon acts only for the member ID you gave `nibble rc setup`.
  Messages, buttons and forms from anyone else are ignored, and `/nibble`
  from anyone else answers only "This nibble belongs to someone else."
  Other members of the workspace can still see the app.
- Agent replies (which can include your code) are stored in Slack like any
  DM. Set the workspace's message retention if that matters.
- If a token leaks, regenerate it on the app's page and run `nibble rc
  setup` again.

## More than one machine

Create one Slack app per machine (for example *nibble-laptop* and
*nibble-desktop*, each from `nibble rc manifest` with a different
`display_information.name`), and run `nibble rc setup` with that app's
tokens on each. Two daemons sharing one app's tokens would split Slack's
events between them at random.

## Configuration

`~/.nibble/config.toml`:

```toml
[rc]
zellij_session = "main"         # where sessions started from Slack open
repo_roots = ["~/workspace"]    # searched for git repos for the form
repo_depth = 3                  # levels below each root
poll_ms = 1500                  # how often finished turns are checked
start_timeout_secs = 240        # how long a new session may take to start
```

The daemon reads `[rc]` when it starts: after editing it, run
`systemctl --user restart nibble-rc`.

The form lists the directories of your recent nibble sessions first, then
the repos found under `repo_roots` (up to Slack's 100 options). Any other
directory can be typed in.

## Troubleshooting

- **`nibble rc doctor`** first.
- Daemon log: `~/.nibble/logs/rc.log`.
- *"the nibble rc daemon is not running"*: `systemctl --user status nibble-rc`.
- *"The agent is no longer running in its pane"*: the agent exited or its
  pane was closed, so your message was not typed (it would have run in the
  shell left behind). If the zellij session is still running, start the
  agent again in it and send `/nibble-rc` from it. If the whole session is down,
  `nibble mux <name>` (the session the note names) brings its recorded tabs
  back, each resuming its conversation; then send `/nibble-rc` again.
- *"zellij session '<name>' is not running and nibble mux has N recorded
  agent tab(s)"*: starting a session from Slack never rebuilds a session with
  recorded agents (that would lose them). Run `nibble mux <name>` once (the
  configured `rc.zellij_session`, `main` by default), then start the session
  again.
- If Slack refuses the agent-session calls (status and titles), the daemon
  logs it once and carries on without them; replies still flow.
- `/nibble-rc` spends a model turn in Claude Code: the hook is missing; re-run
  `./install.sh`.
- *"bot token check failed … invalid_auth"* in setup: copy the *Bot User OAuth
  Token* again from *OAuth & Permissions* (reinstalling the app can replace
  it) and check the masked echo.
