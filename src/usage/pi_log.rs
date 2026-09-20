//! Parser for pi session transcripts at
//! `~/.pi/agent/sessions/<slug>/<timestamp>_<sessionId>.jsonl`.
//!
//! Each file begins with a `{"type":"session", id, cwd, ...}` header line.
//! Token usage lives on `type:"message"` lines with `message.role:"assistant"`.

use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct SessionHeader {
    id: Option<String>,
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageLine {
    id: Option<String>,
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    role: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input: i64,
    #[serde(default)]
    output: i64,
    #[serde(default, rename = "cacheRead")]
    cache_read: i64,
    #[serde(default, rename = "cacheWrite")]
    cache_write: i64,
    cost: Option<Cost>,
}

#[derive(Debug, Deserialize)]
struct Cost {
    #[serde(default)]
    total: f64,
}

#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub model: String,
    pub api_provider: Option<String>,
    pub message_id: String,
    pub session_id: String,
    pub ts: i64,
    pub cwd: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Cost reported by pi itself (USD). 0 for free providers.
    pub reported_cost_usd: f64,
}

/// All pi-family session roots (pi + omp, a fork with an identical layout).
pub fn sessions_roots() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    [".pi", ".omp"]
        .iter()
        .map(|d| PathBuf::from(&home).join(d).join("agent").join("sessions"))
        .collect()
}

pub fn iter_records<F>(mut sink: F) -> Result<()>
where
    F: FnMut(UsageRecord),
{
    for root in sessions_roots() {
        iter_records_in(&root, &mut sink)?;
    }
    Ok(())
}

fn iter_records_in<F>(root: &std::path::Path, sink: &mut F) -> Result<()>
where
    F: FnMut(UsageRecord),
{
    if !root.exists() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(file);

        let mut session_id: Option<String> = None;
        let mut cwd: Option<String> = None;

        for line in reader.lines().map_while(Result::ok) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Session preamble.
            if line.contains("\"type\":\"session\"") && session_id.is_none() {
                if let Ok(hdr) = serde_json::from_str::<SessionHeader>(line) {
                    session_id = hdr.id;
                    cwd = hdr.cwd;
                    continue;
                }
            }
            let Ok(parsed) = serde_json::from_str::<MessageLine>(line) else {
                continue;
            };
            let Some(msg) = parsed.message else { continue };
            if msg.role.as_deref() != Some("assistant") {
                continue;
            }
            let Some(usage) = msg.usage else { continue };
            let Some(model) = msg.model else { continue };
            let Some(message_id) = parsed.id else {
                continue;
            };
            let sid = match &session_id {
                Some(s) => s.clone(),
                None => continue,
            };
            let ts = parsed.timestamp.as_deref().and_then(parse_ts).unwrap_or(0);
            sink(UsageRecord {
                model,
                api_provider: msg.provider,
                message_id,
                session_id: sid,
                ts,
                cwd: cwd.clone(),
                input_tokens: usage.input,
                output_tokens: usage.output,
                cache_read_tokens: usage.cache_read,
                cache_write_tokens: usage.cache_write,
                reported_cost_usd: usage.cost.map(|c| c.total).unwrap_or(0.0),
            });
        }
    }
    Ok(())
}

fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}
