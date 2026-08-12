# QA Gate: Web Session Inspector (`nibble web`)

**Date**: 2026-08-12
**Tier**: Full | Spec ✓ | Implement ✓ | Verify ✓ (7/7 tests, full suite 312 green) | Audit ✓ (4 findings, all fixed)

## Decision

**APPROVED** — no open Critical/High/Medium findings. Audit findings were
fixed inline and re-verified before gating (empty-token bypass, UI XSS sink,
DB migration side effect, systemd unit robustness).

## Verified Against Blueprint

- INV-1 corrupt JSONL skipped — `summarize_counts_and_tokens` (garbage file + line)
- INV-2 read-only — 405 on POST; DB existence-guarded; no runtime fs writes
- INV-3 no client-controlled fs paths — charset whitelist + traversal test
- INV-4 token gates every route incl. `/` — `http_requires_token_when_configured`
- INV-5 token totals == Σ per-message usage — asserted in summary test
- INV-6 id uniqueness — `seen.insert` retain in both scan paths
- AC-1..AC-5 — covered by the 7 web tests; AC-6 (install.sh) verified by
  `bash -n` + manual review (systemd user session unavailable in sandbox)
- Real-data smoke test: 140 sessions, /api/overview, search, detail, raw, UI — all 200

## Items for Human Awareness (no action needed)

1. Binds `0.0.0.0:7878` by default (Tailscale requirement); install.sh always
   sets a token. `--host 127.0.0.1` for localhost-only.
2. `/api/session/{id}/raw` reads whole file into memory (fine at current scale).
3. Thread-per-request, unbounded — acceptable behind token + Tailscale.
