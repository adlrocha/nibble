//! Shared formatting utilities for memory and session listings.
//!
//! Keeps the display logic consistent between `nibble memory list`
//! and `nibble session list`.

use chrono::{DateTime, Utc};

/// Group items by date (Today / Yesterday / Month DD, YYYY).
/// Items must be sorted newest-first before calling.
pub fn group_by_date<T>(items: &[T], get_date: impl Fn(&T) -> DateTime<Utc>) -> Vec<DateGroup<T>>
where
    T: Clone,
{
    let mut groups: Vec<DateGroup<T>> = Vec::new();
    let now = chrono::Local::now().date_naive();
    let yesterday = now.pred_opt().unwrap_or(now);

    for item in items {
        let item_date = get_date(item).date_naive();
        let label = if item_date == now {
            "Today".to_string()
        } else if item_date == yesterday {
            "Yesterday".to_string()
        } else {
            item_date.format("%B %d, %Y").to_string()
        };

        if let Some(last) = groups.last_mut() {
            if last.label == label {
                last.items.push(item.clone());
                continue;
            }
        }
        groups.push(DateGroup {
            label,
            items: vec![item.clone()],
        });
    }

    groups
}

/// A group of items from the same calendar date.
#[derive(Debug, Clone)]
pub struct DateGroup<T> {
    pub label: String,
    pub items: Vec<T>,
}

/// Short agent symbol for columnar display.
pub fn agent_short_name(agent: &str) -> &'static str {
    match agent {
        "claude" => "c",
        "pi" => "π",
        "opencode" => "o",
        "hermes" => "h",
        "manual" => "✋",
        _ => "?",
    }
}

/// Compute max title width for aligned columnar display.
pub fn compute_max_title_width(titles: &[String], min: usize, max: usize) -> usize {
    let mut w = min;
    for t in titles {
        w = w.max(t.len().min(max));
    }
    w
}

/// Truncate and clean a title for single-line display.
pub fn truncate_title(text: &str, max_len: usize) -> String {
    let cleaned = text.lines().next().unwrap_or(text).trim();
    if cleaned.len() <= max_len {
        cleaned.to_string()
    } else {
        format!("{}…", &cleaned[..max_len.saturating_sub(1)])
    }
}
