# Blueprint: Web Dashboard — Usage Patterns & Session Times

## Summary

Extend the `nibble web` dashboard with GitHub-style usage introspection:
session duration stats, a 365-day activity heatmap, activity streaks,
hour-of-day / day-of-week patterns, top tools, and top sessions.

## What's Changing

- `src/web/sessions.rs`: `SessionSummary` gains `duration_secs: i64` and
  `tool_counts: HashMap<String, u64>` (counted during the existing parse pass).
- `src/web/stats.rs`: `build_overview` gains heatmap (365 days), hour/weekday
  histograms, streaks, time stats, top tools, top sessions.
- `src/web/index.html`: new cards (active time, avg session, streaks) and
  panels (heatmap, hour/weekday charts, top tools, top sessions).
- `src/web/mod.rs`: ACTIVITY_DAYS 90 → 365 (heatmap series; bar chart slices
  the last 90 client-side).

## Invariants

1. (INV-7) `duration_secs >= 0` for every summary (unparseable timestamps → 0).
2. (INV-8) `sum(tool_counts) == tool_call_count` for well-formed files.
3. (INV-9) heatmap covers exactly the last N distinct active days, ascending;
   streaks are computed from the set of active UTC days only.

## Acceptance Criteria

1. (AC-7) Given sessions on consecutive days ending today, `streak_current`
   equals the run length; gaps reset it. Longest streak tracked independently.
2. (AC-8) Hour/weekday histograms sum to total_sessions.
3. (AC-9) Time stats: avg/median/longest computed over `duration_secs`;
   longest session carries id/title for linking.
4. (AC-10) `top_tools` aggregates `tool_counts` across sessions, sorted desc.
