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
- **banner** — what the last 30 days of tracked usage would cost at today's
  rates: `your 30d ≈ 84.77 € at market`.
- **price sections** — one per family (`FLASH`, `FLAGSHIP`, `FRONTIER`),
  each a stock-ticker-style list: model rows with the blended €/Mtok on the
  right, the two-sided spot quote under it (`in 0.023 · out 0.135 · cache
  0.003 €/M`), a relative-price bar (fill ∝ rate vs the priciest source
  shown), the cheapest row painted in the accent color, and 30-day ▼/▲
  deltas.
  - **flash** — your open flash-tier models first (GLM-5.3-Flash, …),
    cheapest context models after.
  - **flagship** — GLM-5.3, Kimi K3, DeepSeek V4 Pro tier.
  - **frontier** — the closed pulse: Claude Opus/Sonnet, GPT-5/6, Grok 4,
    Gemini 3 — one row per notable prefix when the machine doesn't run them.

Rates blend the real input/output/**cache** split from `token_usage` — cache
reads dominate coding workloads and cost ~10x less than fresh input, so they
are priced at the cache rate. Rows prefer the models this machine ran in the
last 30 days over global promo minimums, one row per model at its cheapest
source, skipping `:free`/`:batch`/`:nitro` routing variants. Prices are
snapshotted daily (365 days) so the trend line survives reboots.

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
