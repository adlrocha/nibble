# Market token prices (`nibble-market`)

One analysis card in the Omarchy **agents** panel (the accent row next to the
quota cards): blended €/Mtok per model family across public price sources,
the 30-day trend, and what the last 30 days of nibble-tracked usage would
cost at today's rates. Fully auto-fetched — no config, no manual input.

## How it works

```
models.dev api.json        per-Mtok costs (225 providers: Fireworks, Together,
                           DeepSeek, Z.AI, Moonshot, OpenCode, …)
OpenRouter /api/v1/models  per-token costs (460+ variants)
frankfurter.app            USD→EUR rate (fallback 0.92 offline)
~/.nibble/tasks.db         30-day volume + in/out/cache split (token_usage)
        │
        ▼
~/.local/bin/nibble-market --write   (systemd user timer, every 10 min)
        │
        ├─ ~/.local/state/omarchy/agents/usage/market.json   (the panel record)
        └─ ~/.local/state/nibble/market/market-history.jsonl (365-day trend)
```

## What the card shows

- **tier line** — flash-family floor, e.g. `flash 0.031 €/Mtok ▼8%/30d`.
- **banner** — per-family floors with the cheapest provider for *the models
  this machine actually runs*, plus the 30-day usage priced at today's rates:
  `flash 0.031 €/Mtok (glm-5.3-flash @ openrouter) · flagship 0.185 €/Mtok
  (kimi-k2.7-code @ moonshotai) · your 30d ≈ 21.40 € at market`.

Rates blend the real input/output/**cache** split from `token_usage` — cache
reads dominate coding workloads and cost ~10x less than fresh input, so they
are priced at the cache rate. Floors prefer the models this machine ran in
the last 30 days over global promo minimums, and skip `:free`/`:batch`/
`:nitro` routing variants. Prices are snapshotted daily (365 days) so the
trend line survives reboots.

## CLI

```bash
nibble-market            # print the record as JSON
nibble-market --write    # write the record (what the timer runs)
nibble-market --force    # re-fetch prices now, ignoring the 10-min cache
```

## Service

```bash
systemctl --user status nibble-market.timer
journalctl --user -u nibble-market.service -n 20
```
