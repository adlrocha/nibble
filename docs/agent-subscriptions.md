# AI subscription dashboard (`nibble-agent-usage`)

All five coding subscriptions — Claude Code, Z.AI GLM, Kimi Code, Grok Code,
OpenCode Go — show live quota state in the Omarchy **agents** bar panel
(`omarchy.agents`, shipped with Omarchy 4.x): limit meters, reset countdowns,
plan name, and per-day/per-model token burn. The panel renders whatever JSON
records appear in `~/.local/state/omarchy/agents/usage/`; this feature writes
the records for the four providers Omarchy has no collector for.

## How it works

```
~/.zsh_secrets / opencode auth.json / ~/.grok/auth.json
        │ credentials (per provider, see below)
        ▼
~/.local/bin/nibble-agent-usage all --write     (systemd user timer, every 10 min)
        │ probes each provider's usage endpoint, merges local token stats
        ▼
~/.local/state/omarchy/agents/usage/{zai,kimi,grok,opencode}.json
        │ file watcher
        ▼
omarchy.agents panel (alongside omarchy's own claude/codex/fireworks records)
```

- **Claude Code** limits come from Omarchy's own `omarchy-agent-usage-claude`
  (Anthropic OAuth usage endpoint). This tool covers the rest.
- **Local token stats** (Tokens by day / by model sections) are read from
  nibble's `token_usage` table in `~/.nibble/tasks.db`, grouped by
  `api_provider` (`zai`, `kimi-coding`, `xai`/`grok`, `opencode-go`).
- Probe results are cached for 10 minutes in
  `~/.cache/omarchy/agent-usage/<id>-limits.json` and reused when a probe
  fails, but only while their reset window is still open — a stale percentage
  never survives its own reset.

## Endpoints (community-discovered, live-verified 2026-09)

| Provider | Endpoint | Auth |
|---|---|---|
| Z.AI GLM Coding | `GET https://api.z.ai/api/monitor/usage/quota/limit` | `Authorization: <key>` — bare key, no `Bearer` |
| Kimi for Coding | `GET https://api.kimi.com/coding/v1/usages` | `Authorization: Bearer <key>` |
| Grok Code | `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits` + `GET https://grok.com/rest/subscriptions` | `Authorization: Bearer <grok CLI OAuth token>` |
| OpenCode Go | `GET https://opencode.ai/zen/go/v1/usage` | `Authorization: Bearer <key>` + `x-opencode-session: <stable id>` (UA required too) |
| xAI prepaid credits | `GET https://management-api.x.ai/v1/teams/{team}/prepaid/balance` | `Authorization: Bearer <Management API key>` (optional) |

None of these are in official docs (xAI's Management API is; OpenCode Go's
usage route is first-party but undocumented and mirrors what omp probes). A
provider API change can break a collector without notice; the panel will show
the error in the status card rather than silently vanishing.

## Credentials (checked in order)

| Provider | 1. Environment | 2. zsh secrets file | 3. opencode auth.json |
|---|---|---|---|
| Z.AI | `ZAI_API_KEY` | `~/.config/zsh/.zsh_secrets` | `zai-coding-plan` entry |
| Kimi | `KIMI_API_KEY` | same file | `kimi-for-coding` entry |
| Grok | — | — | `~/.grok/auth.json` (grok CLI login) |
| OpenCode Go | `OPENCODE_API_KEY` | same file | `opencode-go` entry (falls back to the `opencode`/Zen entry — same console key) |

The secrets-file step exists because systemd timers and the omarchy shell
never source `~/.zshrc`; keys living only there are invisible to the timer.
Note: the opencode-stored Z.AI key on this machine is rejected by the monitor
endpoint (code 1000) — the real key lives in the secrets file.

Grok also reads `XAI_MANAGEMENT_KEY` + `XAI_TEAM_ID` to show prepaid credit
balance instead of (or beside) the subscription allowance.

## Auth button

When a subscription's status card shows an auth problem, the panel shows an
**Auth** button that runs the provider's login flow in a terminal:

| Provider | Command |
|---|---|
| Claude Code | `claude auth login` |
| Z.AI | `opencode auth login --provider zai-coding-plan` |
| Kimi | `opencode auth login --provider kimi-for-coding` |
| Grok | `grok login` |
| OpenCode Go | `opencode auth login --provider opencode-go` |

The same console key also works in omp: run `omp login opencode-go` and paste
it (or export `OPENCODE_API_KEY`).

The button is a small patch on a user-owned clone of the agents plugin at
`~/.config/omarchy/plugins/adlrocha.agents/` (created with
`omarchy plugin clone omarchy.agents`; the bar was switched to the clone).
The clone also shrinks the provider chips once there are more than three, and
teaches the panel's refresh (`r`, Enter, or IPC `refresh`) to run
`nibble-agent-usage all --write` alongside Omarchy's own updater.

## CLI

```bash
nibble-agent-usage zai                    # print one record as JSON
nibble-agent-usage opencode               # OpenCode Go quota as JSON
nibble-agent-usage all --write            # write all records (what the timer runs)
nibble-agent-usage all --write --force    # ignore the 10-min probe cache
```

## Service

```bash
systemctl --user status nibble-agent-usage.timer
journalctl --user -u nibble-agent-usage.service -n 20
systemctl --user disable --now nibble-agent-usage.timer   # turn it off
```

## Manual panel switcher / IPC

```bash
omarchy-shell adlrocha.agents open|close|toggle|refresh|next
```

## Fresh Omarchy machine setup

Everything the panel needs is in this repo:

1. **Collector + timer** — run `./install.sh` (sections 5f/5g) or by hand:
   copy `scripts/agent-usage/nibble-agent-usage` to `~/.local/bin/` and
   enable `scripts/agent-usage/nibble-agent-usage.{service,timer}`.
2. **Panel plugin** — `omarchy plugin clone omarchy.agents` (switches the bar
   to the clone), then overlay the vendored patched files:
   ```bash
   cp ~/path/to/nibble/scripts/omarchy/adlrocha.agents/*.qml \
      ~/path/to/nibble/scripts/omarchy/adlrocha.agents/manifest.json \
      ~/.config/omarchy/plugins/adlrocha.agents/
   cp ~/path/to/nibble/scripts/omarchy/adlrocha.agents/assets/*.svg \
      ~/.config/omarchy/plugins/adlrocha.agents/assets/
   ```
   `install.sh` does this overlay automatically (section 5g). The vendored
   copy is the source of truth — sync live-clone edits back into the repo.
3. **Load it** — `omarchy-restart-shell`. Omarchy ships quickshell with
   hot-reload disabled (`QS_DISABLE_FILE_WATCHER=1` in `omarchy-launch-shell`),
   so QML changes only apply after a shell restart.
4. **Credentials** — per-provider keys in `~/.config/zsh/.zsh_secrets`
   (the systemd timer never sees your interactive shell).

## Files

| File | Purpose |
|---|---|
| `scripts/agent-usage/nibble-agent-usage` | the collector (installed to `~/.local/bin`) |
| `scripts/agent-usage/nibble-agent-usage.{service,timer}` | systemd user units |
| `scripts/omarchy/adlrocha.agents/` | vendored patched panel clone: Auth button, chip sizing, nibble refresh hook, display-name strip |
| `~/.config/omarchy/plugins/adlrocha.agents/` | live install of the vendored clone |
