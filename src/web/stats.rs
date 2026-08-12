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
pub struct SandboxInfo {
    pub name: String,
    /// Best-effort repo/project label from the task DB (may be absent).
    pub project: Option<String>,
    pub repo_path: Option<String>,
    pub started_at: String,
    pub ports: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionRef {
    pub id: String,
    pub title: String,
    pub project: String,
    pub output_tokens: i64,
    pub duration_secs: i64,
}

#[derive(Debug, Serialize)]
pub struct ToolStat {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Serialize)]
pub struct TimeStats {
    pub total_active_hours: f64,
    pub avg_session_minutes: f64,
    pub median_session_minutes: f64,
    pub longest_session: Option<SessionRef>,
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
    /// Live sandbox containers, sourced from the container runtime (not the
    /// task DB, whose rows can be stale).
    pub live_sandboxes: Vec<SandboxInfo>,
    /// Sessions started per UTC hour (index 0..23). The UI rotates this to
    /// the browser's local timezone.
    pub hours: [u32; 24],
    /// Sessions started per weekday, Monday-first (index 0 = Monday).
    pub weekdays: [u32; 7],
    pub streak_current: u32,
    pub streak_longest: u32,
    pub time: TimeStats,
    pub top_tools: Vec<ToolStat>,
    /// Top 10 sessions by output tokens.
    pub top_sessions: Vec<SessionRef>,
}

fn day_of(ts: &str) -> &str {
    // ISO-8601 timestamps start with YYYY-MM-DD.
    ts.get(..10).unwrap_or("")
}

fn parse_ts(ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// Current and longest streaks over a set of active YYYY-MM-DD days.
/// The current streak is alive if today *or* yesterday had activity
/// (GitHub semantics: today might just not have happened yet).
pub fn streaks(active_days: &std::collections::HashSet<String>, today: chrono::NaiveDate) -> (u32, u32) {
    let parse = |s: &str| chrono::NaiveDate::from_ymd_opt(
        s.get(..4).and_then(|y| y.parse().ok()).unwrap_or(0),
        s.get(5..7).and_then(|m| m.parse().ok()).unwrap_or(0),
        s.get(8..10).and_then(|d| d.parse().ok()).unwrap_or(0),
    );
    let mut days: Vec<chrono::NaiveDate> = active_days.iter().filter_map(|s| parse(s)).collect();
    days.sort();
    days.dedup();

    let mut longest = 0u32;
    let mut run = 0u32;
    let mut prev: Option<chrono::NaiveDate> = None;
    for d in &days {
        run = match prev {
            Some(p) if *d == p.succ_opt().unwrap_or(p) => run + 1,
            _ => 1,
        };
        longest = longest.max(run);
        prev = Some(*d);
    }

    let mut current = 0u32;
    let anchor = if active_days.contains(&today.to_string()) {
        Some(today)
    } else if active_days.contains(&(today.pred_opt().unwrap_or(today)).to_string()) {
        today.pred_opt()
    } else {
        None
    };
    if let Some(mut d) = anchor {
        while active_days.contains(&d.to_string()) {
            current += 1;
            match d.pred_opt() {
                Some(nd) => d = nd,
                None => break,
            }
        }
    }
    (current, longest)
}

/// Build the dashboard overview from summaries. Pure function — DB access
/// happens in the caller, which passes pre-extracted task rows.
pub fn build_overview(
    summaries: &[SessionSummary],
    live_sandboxes: Vec<SandboxInfo>,
    days: usize,
) -> Overview {
    let mut per_model: HashMap<String, ModelStat> = HashMap::new();
    let mut per_day: HashMap<String, DayStat> = HashMap::new();
    let mut per_project: HashMap<String, ProjectStat> = HashMap::new();
    let mut tool_totals: HashMap<String, u64> = HashMap::new();
    let mut hours = [0u32; 24];
    let mut weekdays = [0u32; 7];
    let mut durations: Vec<i64> = Vec::with_capacity(summaries.len());
    let mut longest: Option<SessionRef> = None;

    let mut totals = (0i64, 0i64, 0i64, 0i64, 0f64, 0usize);

    for s in summaries {
        totals.0 += s.input_tokens;
        totals.1 += s.output_tokens;
        totals.2 += s.cache_read_tokens;
        totals.3 += s.cache_write_tokens;
        totals.4 += s.cost_usd;
        totals.5 += s.message_count;

        for (name, n) in &s.tool_counts {
            *tool_totals.entry(name.clone()).or_insert(0) += n;
        }

        if let Some(ts) = parse_ts(&s.started_at) {
            use chrono::Datelike;
            use chrono::Timelike;
            hours[ts.hour() as usize] += 1;
            weekdays[ts.weekday().num_days_from_monday() as usize] += 1;
        }

        durations.push(s.duration_secs);
        if longest
            .as_ref()
            .is_none_or(|l| s.duration_secs > l.duration_secs)
        {
            longest = Some(SessionRef {
                id: s.id.clone(),
                title: s.title.clone(),
                project: s.project.clone(),
                output_tokens: s.output_tokens,
                duration_secs: s.duration_secs,
            });
        }

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

    let active_days: std::collections::HashSet<String> =
        per_day.iter().map(|d| d.day.clone()).collect();
    let today = chrono::Utc::now().date_naive();
    let (streak_current, streak_longest) = streaks(&active_days, today);

    durations.sort_unstable();
    let total_secs: i64 = durations.iter().sum();
    let n = durations.len() as f64;
    let time = TimeStats {
        total_active_hours: total_secs as f64 / 3600.0,
        avg_session_minutes: if durations.is_empty() {
            0.0
        } else {
            total_secs as f64 / n / 60.0
        },
        median_session_minutes: if durations.is_empty() {
            0.0
        } else {
            durations[durations.len() / 2] as f64 / 60.0
        },
        longest_session: longest,
    };

    let mut top_tools: Vec<ToolStat> = tool_totals
        .into_iter()
        .map(|(name, count)| ToolStat { name, count })
        .collect();
    top_tools.sort_by_key(|t| std::cmp::Reverse(t.count));
    top_tools.truncate(15);

    let mut top_sessions: Vec<SessionRef> = summaries
        .iter()
        .map(|s| SessionRef {
            id: s.id.clone(),
            title: s.title.clone(),
            project: s.project.clone(),
            output_tokens: s.output_tokens,
            duration_secs: s.duration_secs,
        })
        .collect();
    top_sessions.sort_by_key(|s| std::cmp::Reverse(s.output_tokens));
    top_sessions.truncate(10);

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
        live_sandboxes,
        hours,
        weekdays,
        streak_current,
        streak_longest,
        time,
        top_tools,
        top_sessions,
    }
}
