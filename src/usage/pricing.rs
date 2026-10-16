//! Per-model pricing in USD per 1M tokens. Loaded from
//! `~/.nibble/pricing.toml` with bundled defaults as fallback.
//!
//! TOML schema:
//! ```toml
//! [anthropic."claude-opus-4-7"]
//! input = 15.0
//! output = 75.0
//! cache_read = 1.50
//! cache_write = 18.75
//! ```
//!
//! Prices are baked in for the common Claude models so the tool works out of
//! the box, but they may drift — override the TOML file to correct them.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelPrice {
    /// USD per 1M input tokens.
    #[serde(default)]
    pub input: f64,
    /// USD per 1M output tokens.
    #[serde(default)]
    pub output: f64,
    /// USD per 1M cache-read tokens.
    #[serde(default)]
    pub cache_read: f64,
    /// USD per 1M cache-write tokens (assume 5-minute tier).
    #[serde(default)]
    pub cache_write: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PricingTable {
    /// Outer key: api provider ("anthropic", "zai", "openai", …).
    /// Inner key: model name as it appears in the transcript.
    #[serde(flatten)]
    pub providers: HashMap<String, HashMap<String, ModelPrice>>,
}

impl PricingTable {
    /// Look up pricing for `(api_provider, model)`. Matching is fuzzy because
    /// Anthropic sometimes appends a date suffix to model IDs in transcripts
    /// (e.g. `claude-haiku-4-5-20251001`) while pricing pages list the base
    /// name. We try, in order:
    ///   1. exact match
    ///   2. strip a trailing `-YYYYMMDD` and retry
    ///   3. longest known key that the model name starts with
    pub fn lookup(&self, api_provider: &str, model: &str) -> Option<&ModelPrice> {
        let models = self.providers.get(api_provider)?;
        if let Some(p) = models.get(model) {
            return Some(p);
        }
        if let Some(stripped) = strip_date_suffix(model) {
            if let Some(p) = models.get(stripped) {
                return Some(p);
            }
        }
        // Longest-prefix fallback for variants like `claude-opus-4-7-thinking`.
        let mut best: Option<(&String, &ModelPrice)> = None;
        for (key, price) in models {
            if model.starts_with(key.as_str()) && best.is_none_or(|(k, _)| key.len() > k.len()) {
                best = Some((key, price));
            }
        }
        best.map(|(_, p)| p)
    }

    /// Compute estimated cost in USD. Token counts are in absolute units.
    pub fn estimate_cost(
        &self,
        api_provider: &str,
        model: &str,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> f64 {
        let Some(p) = self.lookup(api_provider, model) else {
            return 0.0;
        };
        let per = 1_000_000.0;
        (input as f64) * p.input / per
            + (output as f64) * p.output / per
            + (cache_read as f64) * p.cache_read / per
            + (cache_write as f64) * p.cache_write / per
    }

    /// Load from `~/.nibble/pricing.toml`, falling back to bundled
    /// defaults if absent or unreadable. User entries override defaults.
    pub fn load() -> Result<Self> {
        let mut table = bundled_defaults();
        let path = default_pricing_path();
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let user: PricingTable = toml::from_str(&text)?;
            for (provider, models) in user.providers {
                let entry = table.providers.entry(provider).or_default();
                for (model, price) in models {
                    entry.insert(model, price);
                }
            }
        }
        Ok(table)
    }
}

/// If `model` ends in `-YYYYMMDD` (e.g. `claude-haiku-4-5-20251001`), return
/// the prefix without that suffix; otherwise None.
fn strip_date_suffix(model: &str) -> Option<&str> {
    let (head, tail) = model.rsplit_once('-')?;
    if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) {
        Some(head)
    } else {
        None
    }
}

pub fn default_pricing_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".nibble").join("pricing.toml")
}

/// Bundled defaults — USD per 1M tokens. Verify against Anthropic's current
/// pricing page; override via `~/.nibble/pricing.toml` when stale.
fn bundled_defaults() -> PricingTable {
    let mut anthropic = HashMap::new();
    anthropic.insert(
        "claude-opus-4-8".to_string(),
        ModelPrice {
            input: 5.0,
            output: 25.0,
            cache_read: 0.50,
            cache_write: 6.25,
        },
    );
    anthropic.insert(
        "claude-opus-4-7".to_string(),
        ModelPrice {
            input: 5.0,
            output: 25.0,
            cache_read: 0.50,
            cache_write: 6.25,
        },
    );
    anthropic.insert(
        "claude-opus-4-6".to_string(),
        ModelPrice {
            input: 5.0,
            output: 25.0,
            cache_read: 0.50,
            cache_write: 6.25,
        },
    );
    anthropic.insert(
        "claude-fable-5".to_string(),
        ModelPrice {
            input: 10.0,
            output: 50.0,
            cache_read: 1.00,
            cache_write: 12.50,
        },
    );
    anthropic.insert(
        "claude-sonnet-4-6".to_string(),
        ModelPrice {
            input: 3.0,
            output: 15.0,
            cache_read: 0.30,
            cache_write: 3.75,
        },
    );
    anthropic.insert(
        "claude-sonnet-4-5".to_string(),
        ModelPrice {
            input: 3.0,
            output: 15.0,
            cache_read: 0.30,
            cache_write: 3.75,
        },
    );
    anthropic.insert(
        "claude-haiku-4-5".to_string(),
        ModelPrice {
            input: 1.0,
            output: 5.0,
            cache_read: 0.10,
            cache_write: 1.25,
        },
    );

    let mut providers = HashMap::new();
    providers.insert("anthropic".to_string(), anthropic);
    PricingTable { providers }
}
