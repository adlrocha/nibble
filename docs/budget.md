# Budget, market prices, and the self-host trigger (`nibble-budget`)

Two extra cards in the Omarchy **agents** panel, next to the quota cards:

- **Budget** — monthly subscription spend vs your budget, effective €/Mtok per
  subscription (plan price ÷ tokens you actually pushed), and the self-host
  break-even verdict per model family.
- **Market** — best per-token rate per family across providers (models.dev +
  OpenRouter, fetched every 10 min, snapshotted daily), the 30-day trend, and
  your watched-hardware price pulse.

The question this answers: *at my current token volume, when does running my
own model beat paying for tokens?*

## How it works

```
~/.nibble/budget.toml        plans, prices, self-host scenarios, hardware watch
~/.nibble/tasks.db           30-day token volume + in/out split (token_usage)
models.dev + OpenRouter      market per-Mtok rates (public JSON, no auth)
        │
        ▼
~/.local/bin/nibble-budget --write   (systemd user timer, every 10 min)
        │
        ├─ ~/.local/state/omarchy/agents/usage/budget.json
        ├─ ~/.local/state/omarchy/agents/usage/market.json
        └─ ~/.local/state/nibble/budget/market-history.jsonl  (365-day trend)
```

## The math

- **Effective subscription rate** = plan price ÷ your 30-day Mtok through that
  provider. Compare with the market floor: if the plan's effective rate is
  above direct-API pricing, the plan is losing at your volume.
- **Self-host monthly** = hardware ÷ amortization months + power draw at duty
  cycle × €/kWh. Capacity = tokens/s × duty cycle × 730 h.
- **Break-even volume** = self-host monthly € ÷ market floor €/Mtok. Your
  token volume above that line means owning the box beats renting tokens. The
  Budget card states this verdict explicitly.

All rates blend your real input/output split from `token_usage`, since input
and output prices differ by an order of magnitude.

## Configuration — `~/.nibble/budget.toml`

Seeded on first run; never written by the collector.

- `[[subscriptions]]` — name, `provider_id` (must match
  `token_usage.api_provider`), `price_eur`. **Fill in your real plan prices**;
  the seeded placeholders are `0.00` except OpenCode Go ($10/mo).
- `[[selfhost]]` — hardware price, amortization months, power W, duty cycle,
  €/kWh, tokens/s, `serve_families` (`flash`, `flagship`). Set
  `hardware_price_eur = 0` for gear you already own and want to treat as sunk
  cost.
- `[[hardware_watch]]` — manual price pulse for boxes you're considering
  (DGX Spark ≈ $4,699 street 2026-09, RTX 5090, Mac Studio). Update
  `price_date` when you re-check a price; the Market card shows how stale each
  entry is.
- `budget_monthly_eur`, `usd_to_eur` — the meter and currency conversion.

## CLI

```bash
nibble-budget                    # print both records as JSON
nibble-budget --write            # write records (what the timer runs)
nibble-budget --force            # re-fetch market prices now
nibble-budget --seed-config      # write the default config if missing
```

## Service

```bash
systemctl --user status nibble-budget.timer
journalctl --user -u nibble-budget.service -n 20
```
