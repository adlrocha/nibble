use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use croner::Cron;

use crate::models::CronJob;

/// Parse a cron schedule expression and compute the next run time
pub fn compute_next_run(schedule: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let cron = Cron::new(schedule)
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid cron expression '{}': {}", schedule, e))?;

    // Get next occurrence after the given time (using local timezone)
    let local_after = after.with_timezone(&Local);
    let next_local = cron
        .find_next_occurrence(&local_after, false)
        .map_err(|e| anyhow::anyhow!("Failed to compute next run: {}", e))?;

    Ok(next_local.with_timezone(&Utc))
}

/// Validate a cron expression
pub fn validate_schedule(schedule: &str) -> Result<()> {
    Cron::new(schedule)
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid cron expression: {}", e))?;
    Ok(())
}

/// Parse a cron job definition from markdown content
///
/// Expected format:
/// ```markdown
/// # My Cron Job Label
///
/// schedule = "0 9 * * 1-5"
/// enabled = true
/// skip_if_running = true
/// expires_at = "2026-04-01T00:00:00Z"
///
/// ## Prompt
///
/// The prompt text goes here.
/// It can span multiple lines.
/// ```
///
/// Returns `(schedule, prompt, label, enabled, skip_if_running, expires_at)`
pub fn parse_cron_markdown(content: &str) -> Result<(String, String, Option<String>, bool, bool, Option<DateTime<Utc>>)> {
    let mut label = None;
    let mut schedule = None;
    let mut enabled = true;
    let mut skip_if_running = true;
    let mut expires_at: Option<DateTime<Utc>> = None;
    let mut prompt = String::new();

    let lines: Vec<&str> = content.lines().collect();
    let mut in_prompt = false;

    for line in &lines {
        let trimmed = line.trim();

        // Parse heading as label
        if trimmed.starts_with("# ") && label.is_none() {
            label = Some(trimmed[2..].trim().to_string());
            continue;
        }

        // Parse key = value pairs
        if trimmed.starts_with("schedule") && trimmed.contains('=') {
            if let Some(value) = parse_string_value(trimmed) {
                schedule = Some(value);
            }
            continue;
        }

        if trimmed.starts_with("enabled") && trimmed.contains('=') {
            if let Some(value) = parse_bool_value(trimmed) {
                enabled = value;
            }
            continue;
        }

        if trimmed.starts_with("skip_if_running") && trimmed.contains('=') {
            if let Some(value) = parse_bool_value(trimmed) {
                skip_if_running = value;
            }
            continue;
        }

        if trimmed.starts_with("expires_at") && trimmed.contains('=') {
            if let Some(value) = parse_string_value(trimmed) {
                expires_at = Some(
                    chrono::DateTime::parse_from_rfc3339(&value)
                        .with_context(|| format!("Invalid expires_at datetime: {value}"))?
                        .with_timezone(&Utc),
                );
            }
            continue;
        }

        // Detect prompt section
        if trimmed == "## Prompt" || trimmed == "## prompt" {
            in_prompt = true;
            continue;
        }

        // Collect prompt lines
        if in_prompt {
            if !prompt.is_empty() {
                prompt.push('\n');
            }
            prompt.push_str(line);
        }
    }

    let schedule = schedule.context("Missing 'schedule' field in cron definition")?;
    let label = label.or_else(|| Some("Unnamed Cron".to_string()));
    let prompt = prompt.trim().to_string();

    if prompt.is_empty() {
        anyhow::bail!("Missing prompt content in cron definition");
    }

    // Validate the schedule
    validate_schedule(&schedule)?;

    Ok((schedule, prompt, label, enabled, skip_if_running, expires_at))
}

fn parse_string_value(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.splitn(2, '=').collect();
    if parts.len() != 2 {
        return None;
    }
    let value = parts[1].trim();
    // Remove quotes if present
    if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        Some(value[1..value.len() - 1].to_string())
    } else {
        Some(value.to_string())
    }
}

fn parse_bool_value(line: &str) -> Option<bool> {
    let parts: Vec<&str> = line.splitn(2, '=').collect();
    if parts.len() != 2 {
        return None;
    }
    match parts[1].trim().to_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// Build a markdown representation of a cron job
#[allow(dead_code)]
pub fn format_cron_markdown(job: &CronJob) -> String {
    let label = job.label.as_deref().unwrap_or("Unnamed Cron");
    let expires_line = match job.expires_at {
        Some(exp) => format!("\nexpires_at = \"{}\"", exp.to_rfc3339()),
        None => String::new(),
    };
    format!(
        r#"# {}

schedule = "{}"
enabled = {}
skip_if_running = {}{}

## Prompt

{}
"#,
        label,
        job.schedule,
        job.enabled,
        job.skip_if_running,
        expires_line,
        job.prompt
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cron_markdown() {
        let markdown = r#"# Daily Standup

schedule = "0 9 * * 1-5"
enabled = true
skip_if_running = true

## Prompt

Please review yesterday's commits and prepare a summary.
Focus on the main branch changes.
"#;

        let (schedule, prompt, label, enabled, skip_if_running, expires_at) = parse_cron_markdown(markdown).unwrap();

        assert_eq!(schedule, "0 9 * * 1-5");
        assert_eq!(label.unwrap(), "Daily Standup");
        assert!(enabled);
        assert!(skip_if_running);
        assert!(expires_at.is_none());
        assert!(prompt.contains("review yesterday's commits"));
        assert!(prompt.contains("main branch changes"));
    }

    #[test]
    fn test_parse_cron_markdown_minimal() {
        let markdown = r#"# Test

schedule = "*/5 * * * *"

## Prompt

Hello world
"#;

        let (schedule, prompt, label, _enabled, skip_if_running, expires_at) = parse_cron_markdown(markdown).unwrap();

        assert_eq!(schedule, "*/5 * * * *");
        assert_eq!(label.unwrap(), "Test");
        assert!(skip_if_running); // default
        assert!(expires_at.is_none());
        assert_eq!(prompt, "Hello world");
    }

    #[test]
    fn test_parse_cron_markdown_missing_schedule() {
        let markdown = r#"# Test

## Prompt

Hello world
"#;

        let result = parse_cron_markdown(markdown);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_schedule() {
        assert!(validate_schedule("0 9 * * 1-5").is_ok());
        assert!(validate_schedule("*/5 * * * *").is_ok());
        assert!(validate_schedule("invalid").is_err());
    }

    #[test]
    fn test_compute_next_run() {
        let now = Utc::now();
        let next = compute_next_run("0 0 * * *", now).unwrap(); // midnight every day
        assert!(next > now);
        // Should be at midnight
        let next_local = next.with_timezone(&Local);
        assert_eq!(next_local.hour(), 0);
        assert_eq!(next_local.minute(), 0);
    }
}
