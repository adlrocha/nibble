# Web Session Inspector (`nibble web`)

A read-only web UI for browsing, searching, and reviewing your pi agent
sessions — inspired by [pi-session-manager](https://github.com/Dwsy/pi-session-manager),
built into the nibble binary.

## What you get

- **Dashboard** — total sessions/messages/tokens/cost, output-token activity
  chart (last 90 days), per-model and per-project breakdowns, and live
  running agents from the nibble task DB.
- **Sessions** — every session under `~/.pi/agent/sessions/`, sorted by
  recency, filterable by project, with full-text search across message bodies.
- **Session detail** — the full conversation: user messages, assistant text,
  thinking blocks, tool calls with arguments, tool results (errors
  highlighted), and model changes. Raw JSONL download included.

Dark mode, single page, no external assets (works offline and on a phone).

## Running

Installed and started automatically by `install.sh` as the systemd user
service `nibble-web.service`:

```bash
systemctl --user status nibble-web
journalctl --user -u nibble-web -f   # logs
```

Or run it by hand:

```bash
nibble web                      # 0.0.0.0:7878, token from $NIBBLE_WEB_TOKEN
nibble web --port 9000
nibble web --host 127.0.0.1     # localhost only
```

## Access & auth

The service binds `0.0.0.0:7878` by default so it's reachable from other
Tailscale devices. Because that also exposes it on the physical LAN,
`install.sh` generates a bearer token into `~/.nibble/web.env` and prints the
full URL at install time:

```
http://<tailscale-ip>:7878/?token=<token>
```

The token is accepted as `?token=` (bookmarkable) or an
`Authorization: Bearer` header. With no token configured the UI is open —
only do that with `--host 127.0.0.1`.

The service is strictly read-only: it never writes to session files or the
task DB.

## API

| Endpoint | Description |
|----------|-------------|
| `GET /` | The UI |
| `GET /api/overview` | Totals, per-model/per-day/per-project stats, running tasks |
| `GET /api/sessions?q=&project=` | Session summaries, optional full-text search |
| `GET /api/session/{id}` | Full event stream for one session |
| `GET /api/session/{id}/raw` | Raw JSONL download |
