//! Scan claude + pi transcripts and upsert per-message usage rows.

use anyhow::Result;

use crate::db::Database;
use crate::usage::pricing::PricingTable;
use crate::usage::{claude_log, pi_log};

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub claude_seen: u64,
    pub claude_inserted: u64,
    pub pi_seen: u64,
    pub pi_inserted: u64,
}

/// Upsert params for one transcript record.
struct UsageRow {
    provider: &'static str,
    api_provider: Option<String>,
    model: String,
    session_id: String,
    message_id: String,
    ts: i64,
    cwd: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    estimated_cost_usd: f64,
}

impl UsageRow {
    fn claude(r: claude_log::UsageRecord, pricing: &PricingTable) -> Self {
        // Claude Code transcripts come from Anthropic.
        let api_provider = "anthropic";
        UsageRow {
            provider: "claude",
            api_provider: Some(api_provider.to_string()),
            estimated_cost_usd: pricing.estimate_cost(
                api_provider,
                &r.model,
                r.input_tokens,
                r.output_tokens,
                r.cache_read_tokens,
                r.cache_write_tokens,
            ),
            model: r.model,
            session_id: r.session_id,
            message_id: r.message_id,
            ts: r.ts,
            cwd: r.cwd,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
        }
    }

    fn pi(r: pi_log::UsageRecord, pricing: &PricingTable) -> Self {
        // Prefer pi's self-reported cost when present; otherwise fall back to
        // our pricing table (useful if you fill in pricing for a free model
        // to estimate "what would this have cost on a paid provider").
        let estimated_cost_usd = if r.reported_cost_usd > 0.0 {
            r.reported_cost_usd
        } else {
            pricing.estimate_cost(
                r.api_provider.as_deref().unwrap_or(""),
                &r.model,
                r.input_tokens,
                r.output_tokens,
                r.cache_read_tokens,
                r.cache_write_tokens,
            )
        };
        UsageRow {
            provider: "pi",
            api_provider: r.api_provider,
            estimated_cost_usd,
            model: r.model,
            session_id: r.session_id,
            message_id: r.message_id,
            ts: r.ts,
            cwd: r.cwd,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
        }
    }
}

/// Records are flushed in chunks inside one write transaction each. A
/// per-statement implicit transaction per row kept the writer lock hot for
/// the whole (multi-minute) scan and starved concurrent writers — agents'
/// `report status` / `memory capture` calls stalled for seconds whenever a
/// scan was running. Chunking also collapses 50k+ fsyncs into ~100.
const FLUSH_CHUNK: usize = 500;

fn flush_chunk(db: &Database, chunk: &mut Vec<UsageRow>, stats: &mut ScanStats) {
    if chunk.is_empty() {
        return;
    }
    let outcome = db.with_write_tx(|db| {
        for r in chunk.drain(..) {
            match db.upsert_token_usage(
                r.provider,
                r.api_provider.as_deref(),
                &r.model,
                &r.session_id,
                &r.message_id,
                r.ts,
                r.cwd.as_deref(),
                r.input_tokens,
                r.output_tokens,
                r.cache_read_tokens,
                r.cache_write_tokens,
                r.estimated_cost_usd,
            ) {
                Ok(true) => {
                    if r.provider == "claude" {
                        stats.claude_inserted += 1;
                    } else {
                        stats.pi_inserted += 1;
                    }
                }
                Ok(false) => {}
                Err(e) => eprintln!("nibble usage: {} upsert failed: {e}", r.provider),
            }
        }
        Ok(())
    });
    if let Err(e) = outcome {
        eprintln!("nibble usage: chunk commit failed: {e}");
    }
}

pub fn scan_all(db: &Database, pricing: &PricingTable) -> Result<ScanStats> {
    let mut stats = ScanStats::default();
    let mut chunk: Vec<UsageRow> = Vec::with_capacity(FLUSH_CHUNK);

    claude_log::iter_records(|r| {
        stats.claude_seen += 1;
        chunk.push(UsageRow::claude(r, pricing));
        if chunk.len() >= FLUSH_CHUNK {
            flush_chunk(db, &mut chunk, &mut stats);
        }
    })?;
    flush_chunk(db, &mut chunk, &mut stats);

    pi_log::iter_records(|r| {
        stats.pi_seen += 1;
        chunk.push(UsageRow::pi(r, pricing));
        if chunk.len() >= FLUSH_CHUNK {
            flush_chunk(db, &mut chunk, &mut stats);
        }
    })?;
    flush_chunk(db, &mut chunk, &mut stats);

    Ok(stats)
}
