//! Aggregation of session summaries into dashboard statistics.

use super::sessions::SessionSummary;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Serialize)]
pub struct ModelStat {
    pub model: String,
    pub sessions: usize,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Serialize)]
pub struct DayStat {
    /// YYYY-MM-DD (UTC, taken from the session timestamps).
    pub day: String,
    pub sessions: usize,
    pub messages: usize,
    pub output_tokens: i64,
}

#[derive(Debug, Serialize)]
pub struct ProjectStat {
    pub project: String,
    pub cwd: String,
    pub sessions: usize,
    pub output_tokens: i64,
    pub last_active_at: String,
}

#[derive(Debug, Serialize)]
pub struct TaskInfo {
    pub task_id: String,
    pub agent_type: String,
    pub title: String,
    pub status: String,
    pub updated_at: String,
    pub repo_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Overview {
    pub total_sessions: usize,
    pub total_messages: usize,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
    pub total_cost_usd: f64,
    pub per_model: Vec<ModelStat>,
    /// Last `days` entries, oldest first.
    pub per_day: Vec<DayStat>,
    pub per_project: Vec<ProjectStat>,
    pub running_tasks: Vec<TaskInfo>,
}

fn day_of(ts: &str) -> &str {
    // ISO-8601 timestamps start with YYYY-MM-DD.
    ts.get(..10).unwrap_or("")
}

/// Build the dashboard overview from summaries. Pure function — DB access
/// happens in the caller, which passes pre-extracted task rows.
pub fn build_overview(
    summaries: &[SessionSummary],
    running_tasks: Vec<TaskInfo>,
    days: usize,
) -> Overview {
    let mut per_model: HashMap<String, ModelStat> = HashMap::new();
    let mut per_day: HashMap<String, DayStat> = HashMap::new();
    let mut per_project: HashMap<String, ProjectStat> = HashMap::new();

    let mut totals = (0i64, 0i64, 0i64, 0i64, 0f64, 0usize);

    for s in summaries {
        totals.0 += s.input_tokens;
        totals.1 += s.output_tokens;
        totals.2 += s.cache_read_tokens;
        totals.3 += s.cache_write_tokens;
        totals.4 += s.cost_usd;
        totals.5 += s.message_count;

        // Attribute the session's usage to each model it used (a session that
        // switched models is counted once per model; sessions >> models, so
        // per-model session counts may sum above total_sessions).
        for m in &s.models {
            let e = per_model.entry(m.clone()).or_insert_with(|| ModelStat {
                model: m.clone(),
                sessions: 0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cost_usd: 0.0,
            });
            e.sessions += 1;
            e.input_tokens += s.input_tokens;
            e.output_tokens += s.output_tokens;
            e.cache_read_tokens += s.cache_read_tokens;
            e.cost_usd += s.cost_usd;
        }

        let day = day_of(&s.started_at).to_string();
        if !day.is_empty() {
            let e = per_day.entry(day.clone()).or_insert_with(|| DayStat {
                day,
                sessions: 0,
                messages: 0,
                output_tokens: 0,
            });
            e.sessions += 1;
            e.messages += s.message_count;
            e.output_tokens += s.output_tokens;
        }

        let e = per_project
            .entry(s.project.clone())
            .or_insert_with(|| ProjectStat {
                project: s.project.clone(),
                cwd: s.cwd.clone(),
                sessions: 0,
                output_tokens: 0,
                last_active_at: String::new(),
            });
        e.sessions += 1;
        e.output_tokens += s.output_tokens;
        if s.last_active_at > e.last_active_at {
            e.last_active_at = s.last_active_at.clone();
        }
    }

    let mut per_model: Vec<_> = per_model.into_values().collect();
    per_model.sort_by_key(|m| std::cmp::Reverse(m.output_tokens));

    let mut per_day: Vec<_> = per_day.into_values().collect();
    per_day.sort_by(|a, b| a.day.cmp(&b.day));
    if per_day.len() > days {
        per_day = per_day.split_off(per_day.len() - days);
    }

    let mut per_project: Vec<_> = per_project.into_values().collect();
    per_project.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));

    Overview {
        total_sessions: summaries.len(),
        total_messages: totals.5,
        total_input_tokens: totals.0,
        total_output_tokens: totals.1,
        total_cache_read_tokens: totals.2,
        total_cache_write_tokens: totals.3,
        total_cost_usd: totals.4,
        per_model,
        per_day,
        per_project,
        running_tasks,
    }
}
