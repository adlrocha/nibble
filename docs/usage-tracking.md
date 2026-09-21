# Token Usage Tracking

`nibble usage` tracks per-message token consumption across both Claude Code and
pi CLI sessions, and applies a pricing table to estimate what that usage would
cost on a pay-as-you-go plan. The goal is **order-of-magnitude cost visibility**
when you're on a subscription — not exact billing.

## Why it works the way it does

Both Claude Code and pi already record every assistant message to disk as JSONL,
with the model name and token counts (input, output, cache read, cache write)
included. We don't need to instrument the CLIs, run a proxy, or hook into
anything dynamic — we just walk the logs.

Because `~/.claude` and `~/.pi` are bind-mounted into every nibble sandbox
([AGENTS.md](../AGENTS.md)), a single scanner on the host sees usage from
every sandbox, no matter where the agent ran.

## Data flow

```
┌──────────────┐   writes   ┌──────────────────────────┐
│ claude code  │ ─────────► │ ~/.claude/projects/      │
│   (any       │            │   <slug>/<sid>.jsonl     │
│   sandbox)   │            └──────────────────────────┘
└──────────────┘                          │
                                          ▼
┌──────────────┐   writes   ┌──────────────────────────┐    ┌──────────────────┐
│   pi cli     │ ─────────► │ ~/.pi/agent/sessions/    │ ─► │ nibble usage     │
│   (any       │            │   <slug>/<ts>_<sid>.jsonl│    │   scan           │
│   sandbox)   │            └──────────────────────────┘    │   (systemd timer │
└──────────────┘                                            │    every 15 min) │
                                                            └────────┬─────────┘
                                                                     │ upsert
                                                                     ▼
                                                       ┌──────────────────────┐
                                                       │ ~/.nibble/tasks.db   │
                                                       │   token_usage table  │
                                                       └──────────┬───────────┘
                                                                  │ query
                                                                  ▼
                                                       ┌──────────────────────┐
                                                       │ nibble usage report  │
                                                       └──────────────────────┘
```

## Storage

Schema v11 adds a single table:

```sql
CREATE TABLE token_usage (
    provider           TEXT NOT NULL,          -- 'claude' | 'pi'
    api_provider       TEXT,                   -- 'anthropic', 'zai', 'kimi-coding', ...
    model              TEXT NOT NULL,          -- 'claude-sonnet-4-6', 'glm-5.1', ...
    session_id         TEXT NOT NULL,
    message_id         TEXT NOT NULL,
    ts                 INTEGER NOT NULL,       -- unix seconds
    cwd                TEXT,                   -- working dir / sandbox identity
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    estimated_cost_usd REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (provider, message_id)
);
```

The primary key is `(provider, message_id)`, so re-scanning is **idempotent** —
each message is upserted with `INSERT OR IGNORE`, and existing rows are
refreshed with the latest token counts and estimated cost. The scheduled scan runs
every 15 minutes without growing duplicates, and a pricing-table change is
reflected for historical rows on the next scan.

## Pricing model

Pricing lives at `~/.nibble/pricing.toml` and is merged on top of bundled
defaults. The TOML schema is:

```toml
[<api_provider>."<model>"]
input       = <usd per 1M input tokens>
output      = <usd per 1M output tokens>
cache_read  = <usd per 1M cache-read tokens>
cache_write = <usd per 1M cache-write tokens (assume 5m tier)>
```

Bundled defaults cover the Anthropic models I commonly use (Opus 4.6/4.7,
Sonnet 4.5/4.6, Haiku 4.5). Everything else is **0.00 until you fill it in**.

### Why zero-cost rows appear

Several rows can show `n/a` or `$0.00` in the report:

| Reason                              | Examples                                  | Fix                                      |
|-------------------------------------|-------------------------------------------|------------------------------------------|
| No pricing entry for model          | `glm-5.1`, `kimi-for-coding`, `gpt-5.4`   | Add a `[<api_provider>."<model>"]` block |
| Free or local provider              | `Qwen3.6-*.gguf` via `local-llama`        | Real cost is 0; leave as-is              |
| pi already recorded `cost.total: 0` | Same — pi trusts its provider's bill      | Same                                     |
| Tokens are genuinely zero           | Tool-only messages with no LLM call       | Nothing to do                            |

The report distinguishes "0 tokens → \$0.00" from "tokens recorded but no
price → n/a", and lists unpriced models at the bottom so you know exactly which
keys to add.

### Model name matching

Anthropic sometimes ships date-suffixed model IDs in transcripts (e.g.
`claude-haiku-4-5-20251001`). The lookup tries, in order:

1. Exact match: `claude-haiku-4-5-20251001`
2. Strip trailing `-YYYYMMDD`: `claude-haiku-4-5` ✅
3. Longest known prefix: `claude-opus-4-7-thinking` → `claude-opus-4-7`

If none match, the row is marked unpriced.

### pi's self-reported cost

pi writes `usage.cost.total` to each message — for paid providers (Anthropic,
some OpenAI-compatible) that value is authoritative. For free providers (zai's
glm-* tier, local llama, etc.) it's `0`.

The aggregator prefers pi's reported cost when it's > 0; otherwise it falls
back to the pricing table. So you can hypothetically price free models by
adding entries to `pricing.toml` (e.g. "what would these zai calls cost on the
paid OpenAI equivalent?") and the report will use your number.

## CLI

```bash
# One-off scan (the timer does this every 15 min)
nibble usage scan
nibble usage scan --report          # scan then print a summary

# Reports
nibble usage report                       # group by model, all time
nibble usage report --since 7d            # last 7 days
nibble usage report --since 24h
nibble usage report --by provider         # group by 'claude' vs 'pi'
nibble usage report --by sandbox          # group by working directory
nibble usage report --json                # machine-readable: {window, since_filter, rows[]}

# Pricing
nibble usage pricing                      # show effective table
```

`--since` accepts `<n>{s,m,h,d,w}`.

The report header shows the **actual covered window** — the earliest and
latest message timestamp in the matched rows, plus a human-readable span. This
is more useful than echoing back your `--since` filter, because it tells you
whether you actually have data for the period you asked about.

## Scheduling

`install.sh` installs a systemd-user timer:

- `nibble-usage.service` — runs `nibble usage scan`
- `nibble-usage.timer` — `OnBootSec=2min`, `OnUnitActiveSec=15min`

Enable/disable manually:

```bash
systemctl --user enable --now nibble-usage.timer
systemctl --user disable --now nibble-usage.timer
systemctl --user status nibble-usage.timer
journalctl --user -u nibble-usage.service -n 50
```

Scans are cheap — they walk JSONL files and `INSERT OR IGNORE`. A full re-scan
on a few hundred sessions takes well under a second.

## Reset

To start over (drop the table, re-scan from scratch):

```bash
sqlite3 ~/.nibble/tasks.db "DELETE FROM token_usage;"
nibble usage scan
```

Schema migrations preserve the table across nibble upgrades.

## What this is NOT

- **Not exact billing.** Token counts are accurate; pricing entries can drift
  whenever providers change their rates. Treat the totals as estimates.
- **Not a budget guard.** No alerts, no caps, no rate limiting.
- **Not provider-side data.** Everything comes from local log files. If a CLI
  doesn't log usage (or logs it in a format the parser doesn't understand) it
  won't be counted — check the unpriced footer.

## Adding pricing for a new model

1. Find the model in the unpriced footer:
   ```
   ⚠ 3 model(s) have no price entry — cost shown as n/a.
      Unpriced (lookup key shown as `<api_provider>.<model>`):
        • zai.glm-5.1                  (804 msgs, 1,388,574 in / 229,766 out tokens)
   ```
2. Edit `~/.nibble/pricing.toml`:
   ```toml
   [zai."glm-5.1"]
   input  = 0.50
   output = 1.50
   cache_read  = 0.05
   cache_write = 0.50
   ```
3. Re-scan so existing rows pick up the new price:
   ```bash
   nibble usage scan
   ```
   `estimated_cost_usd` is computed and stored at scan time. Token counts are
   preserved on every re-scan because they come straight from the source JSONL
   files.
