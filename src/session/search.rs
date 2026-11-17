//! Ranked search across every session: summary, title and transcript body.
//!
//! Each query term is scored per field and the field scores are weighted
//! summary > title > body, so a session whose summary mentions the terms
//! ranks above one that only mentions them deep in its transcript. Every
//! term must match somewhere in a session for it to be listed; ties go to
//! the more recent session.
//!
//! Summary and title are short, so they are matched fuzzily (nucleo, the
//! matcher vert uses). Bodies can be megabytes, where a fuzzy subsequence
//! matches almost anything, so they are matched as case-insensitive
//! substrings. Bodies hold only the conversational text (user and assistant
//! messages, no tool output) and are extracted once, then cached by
//! transcript mtime and size.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config as MatchConfig, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};

use super::SessionInfo;

// Field weights are spaced so precedence is strict per term: the weakest
// accepted summary match (MIN_FUZZY_QUALITY * FUZZY_PENALTY = 0.48, so 48)
// beats the best title + body (10 + 1), and the weakest title match (4.8)
// beats the best body match (1). Body counts and recency only order
// sessions within a tier.
/// Weight of a term matching the session summary.
pub const WEIGHT_SUMMARY: f64 = 100.0;
/// Weight of a term matching the session title.
pub const WEIGHT_TITLE: f64 = 10.0;
/// Weight of a term matching the transcript body.
pub const WEIGHT_BODY: f64 = 1.0;
/// Fuzzy word matches scoring below this fraction of a perfect match are
/// ignored.
const MIN_FUZZY_QUALITY: f64 = 0.6;
/// A fuzzy (typo-tolerant) word match never scores as high as an exact one.
const FUZZY_PENALTY: f64 = 0.8;
/// Body occurrences counted per term; more add nothing to the score.
pub const MAX_BODY_COUNT: usize = 100;
/// Bump when the cached body format changes, to discard old caches.
const CACHE_VERSION: u32 = 1;

/// A transcript's conversational text, with an ASCII-lowercased copy for
/// matching. ASCII lowercasing keeps byte offsets identical between the two,
/// so a match found in `lower` can be shown from `text`.
#[derive(Debug, Clone, Default)]
pub struct Body {
    text: String,
    lower: String,
}

impl Body {
    /// Wrap extracted transcript text.
    pub fn new(text: String) -> Self {
        let lower = text.to_ascii_lowercase();
        Self { text, lower }
    }
}

/// One searchable session.
#[derive(Debug, Clone)]
pub struct SearchDoc {
    pub session: SessionInfo,
    pub title: String,
    pub summary: String,
    /// Host repo the session belongs to (resolved through the task DB for
    /// sandbox sessions), else the transcript's own workspace.
    pub repo: Option<String>,
    /// The repo as a host path, when that is known: what Ctrl-F narrows
    /// on. `None` when `repo` is only a sandbox container cwd (or unknown).
    pub host_repo: Option<String>,
    /// Transcript text; `None` until the background indexer has loaded it.
    pub body: Option<Body>,
    title_lower: String,
    summary_lower: String,
}

impl SearchDoc {
    /// A session with its title and summary; the body arrives later.
    pub fn new(session: SessionInfo, title: String, summary: String, repo: Option<String>) -> Self {
        Self {
            title_lower: title.to_ascii_lowercase(),
            summary_lower: summary.to_ascii_lowercase(),
            session,
            title,
            summary,
            repo,
            host_repo: None,
            body: None,
        }
    }
}

/// A session matching a query, with where each term was found.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Index into the searched docs.
    pub index: usize,
    pub score: f64,
    /// Some term matched the summary.
    pub in_summary: bool,
    /// Some term matched the title.
    pub in_title: bool,
    /// Total body occurrences across terms (each capped at [`MAX_BODY_COUNT`]).
    pub body_count: usize,
    /// Some term hit [`MAX_BODY_COUNT`], so `body_count` is a lower bound.
    pub body_capped: bool,
}

/// Split a query into lowercase terms. Lowercasing is ASCII-only (as is
/// body matching) so byte offsets line up between a text and its lowercased
/// copy; non-ASCII letters therefore match case-sensitively outside the
/// fuzzy summary/title path.
pub fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Scores sessions against queries. Holds the fuzzy matcher's scratch
/// buffers, so reuse one across keystrokes.
pub struct Ranker {
    matcher: Matcher,
    buf: Vec<char>,
}

impl Default for Ranker {
    fn default() -> Self {
        Self {
            matcher: Matcher::new(MatchConfig::DEFAULT),
            buf: Vec::new(),
        }
    }
}

impl Ranker {
    /// Rank `docs` for `query`, best first. A blank query lists every doc in
    /// its given order (the caller passes them newest first).
    pub fn rank(&mut self, docs: &[SearchDoc], query: &str) -> Vec<Hit> {
        let terms = query_terms(query);
        if terms.is_empty() {
            return (0..docs.len())
                .map(|index| Hit {
                    index,
                    score: 0.0,
                    in_summary: false,
                    in_title: false,
                    body_count: 0,
                    body_capped: false,
                })
                .collect();
        }
        let atoms: Vec<(Atom, f64)> = terms
            .iter()
            .map(|t| {
                let atom = Atom::new(
                    t,
                    CaseMatching::Ignore,
                    Normalization::Smart,
                    AtomKind::Fuzzy,
                    false,
                );
                let ideal = self.score(&atom, t).unwrap_or(1).max(1);
                (atom, f64::from(ideal))
            })
            .collect();
        let mut hits: Vec<Hit> = docs
            .iter()
            .enumerate()
            .filter_map(|(index, doc)| self.score_doc(index, doc, &terms, &atoms))
            .collect();
        hits.sort_by(|a, b| {
            b.score.total_cmp(&a.score).then_with(|| {
                let time = |h: &Hit| docs[h.index].session.effective_time();
                time(b).cmp(&time(a))
            })
        });
        hits
    }

    fn score(&mut self, atom: &Atom, haystack: &str) -> Option<u16> {
        atom.score(Utf32Str::new(haystack, &mut self.buf), &mut self.matcher)
    }

    /// Match quality of `term` in a lowercased field, in 0..=1: 1 for an
    /// exact substring, else the best fuzzy match against a single word.
    /// Fuzzy matching runs per word because nucleo rewards word-initial
    /// letters, so a term's letters scattered across a long summary
    /// ("s e a r c h", "jwt" in "just wrote tests") would otherwise score
    /// close to a real match.
    fn quality(&mut self, term: &str, atom: &Atom, ideal: f64, lower: &str) -> f64 {
        if lower.contains(term) {
            return 1.0;
        }
        let best = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() >= term.len().saturating_sub(2))
            .filter_map(|w| self.score(atom, w))
            .max()
            .map_or(0.0, |s| (f64::from(s) / ideal).min(1.0));
        if best >= MIN_FUZZY_QUALITY {
            best * FUZZY_PENALTY
        } else {
            0.0
        }
    }

    fn score_doc(
        &mut self,
        index: usize,
        doc: &SearchDoc,
        terms: &[String],
        atoms: &[(Atom, f64)],
    ) -> Option<Hit> {
        let mut hit = Hit {
            index,
            score: 0.0,
            in_summary: false,
            in_title: false,
            body_count: 0,
            body_capped: false,
        };
        for (term, (atom, ideal)) in terms.iter().zip(atoms) {
            let summary = self.quality(term, atom, *ideal, &doc.summary_lower);
            let title = self.quality(term, atom, *ideal, &doc.title_lower);
            let count = doc
                .body
                .as_ref()
                .map_or(0, |b| count_occurrences(&b.lower, term, MAX_BODY_COUNT));
            let term_score =
                WEIGHT_SUMMARY * summary + WEIGHT_TITLE * title + WEIGHT_BODY * body_quality(count);
            if term_score == 0.0 {
                return None;
            }
            hit.score += term_score;
            hit.in_summary |= summary > 0.0;
            hit.in_title |= title > 0.0;
            hit.body_count += count;
            hit.body_capped |= count >= MAX_BODY_COUNT;
        }
        Some(hit)
    }
}

/// Body score for `count` occurrences: 0.5 for one, rising to 1.0 at
/// [`MAX_BODY_COUNT`], so a session that keeps discussing a term edges out
/// one that mentions it once.
fn body_quality(count: usize) -> f64 {
    if count == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)] // count <= MAX_BODY_COUNT
    let ratio = (count as f64).log10() / (MAX_BODY_COUNT as f64).log10();
    0.5 + 0.5 * ratio.min(1.0)
}

fn count_occurrences(haystack: &str, needle: &str, cap: usize) -> usize {
    haystack.match_indices(needle).take(cap).count()
}

/// Byte ranges in `text` where any term occurs (ASCII case-insensitive),
/// sorted and merged.
pub fn match_ranges(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let lower = text.to_ascii_lowercase();
    let mut ranges: Vec<(usize, usize)> = terms
        .iter()
        .filter(|t| !t.is_empty())
        .flat_map(|t| {
            lower
                .match_indices(t.as_str())
                .map(|(i, m)| (i, i + m.len()))
        })
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Up to `max` one-line excerpts of `body` around term occurrences, each
/// with `radius` bytes of context either side. Whitespace runs are
/// collapsed so excerpts fit on a line.
pub fn snippets(body: &Body, terms: &[String], max: usize, radius: usize) -> Vec<String> {
    let mut starts: Vec<(usize, usize)> = terms
        .iter()
        .filter(|t| !t.is_empty())
        .flat_map(|t| {
            body.lower
                .match_indices(t.as_str())
                .take(max)
                .map(|(i, m)| (i, i + m.len()))
        })
        .collect();
    starts.sort_unstable();
    let mut out = Vec::new();
    let mut covered_to = 0;
    for (start, end) in starts {
        if out.len() >= max {
            break;
        }
        if start < covered_to {
            continue;
        }
        // Start no earlier than the previous excerpt ended, so excerpts
        // never repeat text.
        let from = body
            .text
            .floor_char_boundary(start.saturating_sub(radius).max(covered_to));
        let to = body
            .text
            .ceil_char_boundary((end + radius).min(body.text.len()));
        covered_to = to;
        let excerpt = body.text[from..to]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let prefix = if from > 0 { "…" } else { "" };
        let suffix = if to < body.text.len() { "…" } else { "" };
        out.push(format!("{prefix}{excerpt}{suffix}"));
    }
    out
}

// ── Body extraction ──────────────────────────────────────────────────────────

/// Conversational text of a transcript: user and assistant message text,
/// one message per paragraph. Tool calls, tool results and meta messages
/// (system reminders, command wrappers) are left out. Works for claude and
/// pi-family transcripts, and reads `.jsonl.gz` archives.
pub fn extract_body(path: &Path) -> Result<String> {
    let reader = super::open_session_reader(path)
        .with_context(|| format!("could not open transcript {}", path.display()))?;
    let mut out = String::new();
    for line in reader.lines() {
        // An unreadable line (e.g. a write cut mid-character) costs only
        // that line, not the whole transcript.
        let Ok(line) = line else { continue };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(text) = message_text(&val) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&text);
        }
    }
    Ok(out)
}

fn message_text(val: &serde_json::Value) -> Option<String> {
    if val.get("isMeta").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    let msg = val.get("message")?;
    let role = msg.get("role")?.as_str()?;
    if role != "user" && role != "assistant" {
        return None;
    }
    let text = match msg.get("content")? {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let trimmed = text.trim();
    if trimmed.is_empty() || (role == "user" && trimmed.starts_with('<')) {
        return None;
    }
    Some(trimmed.to_string())
}

// ── Body cache and background indexing ───────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct CachedBody {
    mtime: u64,
    size: u64,
    text: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BodyCache {
    version: u32,
    entries: HashMap<String, CachedBody>,
}

/// Where extracted bodies are cached: `$XDG_CACHE_HOME/nibble/session-search.json`
/// (`~/.cache/nibble/…`). Deliberately outside `~/.nibble`, which every
/// sandbox mounts read-write: the cache holds the text of every session on
/// the host, including repos and agents a sandbox cannot otherwise see.
pub fn cache_path() -> PathBuf {
    dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("nibble")
        .join("session-search.json")
}

/// A missing, unreadable or outdated cache is an empty one: the bodies
/// are re-extracted and the cache rewritten.
fn load_cache(path: &Path) -> HashMap<String, CachedBody> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<BodyCache>(&s).ok())
        .filter(|c| c.version == CACHE_VERSION)
        .map(|c| c.entries)
        .unwrap_or_default()
}

fn save_cache(path: &Path, entries: HashMap<String, CachedBody>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let cache = BodyCache {
        version: CACHE_VERSION,
        entries,
    };
    // Per-process temp name: concurrent searches must not share one.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    write_private(&tmp, &serde_json::to_vec(&cache)?)
        .with_context(|| format!("could not write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))
}

/// Write `data` to `path` readable by the owner only (0600): the cache
/// holds full conversation text.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies on creation; tighten a leftover file too.
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(data)
}

fn mtime_secs(t: Option<SystemTime>) -> u64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Progress from the background body indexer.
#[derive(Debug)]
pub enum IndexEvent {
    /// The body of the session at `index` is ready.
    Body { index: usize, body: Body },
    /// The transcript at `index` could not be read; it stays unindexed.
    Failed { index: usize, error: String },
    /// All bodies were sent. `cache_error` says why the cache could not be
    /// saved, when it could not.
    Done { cache_error: Option<String> },
}

/// Load the bodies of `sessions` on a background thread, from the cache at
/// `cache` when the transcript's mtime and size are unchanged, else by
/// extracting them. The cache is rewritten at the end with the given
/// sessions plus the old entries for transcripts outside them (a `--repo`
/// search) that still exist on disk; entries for deleted transcripts go.
///
/// Dropping the receiver stops the indexer after its current transcript; it
/// still saves what it extracted (keeping the old entries for the
/// transcripts it did not reach), so join the handle before exiting to keep
/// that work.
pub fn spawn_indexer(
    sessions: Vec<SessionInfo>,
    cache: PathBuf,
) -> (mpsc::Receiver<IndexEvent>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || index_bodies(&sessions, &cache, &tx));
    (rx, handle)
}

fn index_bodies(sessions: &[SessionInfo], cache: &Path, tx: &mpsc::Sender<IndexEvent>) {
    let mut old = load_cache(cache);
    let mut new = HashMap::with_capacity(old.len().max(sessions.len()));
    let mut changed = false;
    for (index, s) in sessions.iter().enumerate() {
        let key = s.path.to_string_lossy().to_string();
        let mtime = mtime_secs(s.modified);
        let text = match old.remove(&key) {
            Some(c) if c.mtime == mtime && c.size == s.size_bytes => c.text,
            _ => {
                changed = true;
                match extract_body(&s.path) {
                    Ok(text) => text,
                    Err(e) => {
                        let _ = tx.send(IndexEvent::Failed {
                            index,
                            error: format!("{e:#}"),
                        });
                        continue;
                    }
                }
            }
        };
        let body = Body::new(text.clone());
        new.insert(
            key,
            CachedBody {
                mtime,
                size: s.size_bytes,
                text,
            },
        );
        if tx.send(IndexEvent::Body { index, body }).is_err() {
            // The UI quit early: the entries not reached are kept below.
            break;
        }
    }
    // Keep the entries this run did not index (sessions outside a --repo
    // filter, or not reached before an early quit) while their transcript
    // still exists, so the next search starts warm. Drop deleted ones.
    for (key, c) in old {
        if Path::new(&key).exists() {
            new.insert(key, c);
        } else {
            changed = true;
        }
    }
    let cache_error = if changed {
        save_cache(cache, new).err().map(|e| format!("{e:#}"))
    } else {
        None
    };
    let _ = tx.send(IndexEvent::Done { cache_error });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(title: &str, summary: &str, body: &str, age_secs: u64) -> SearchDoc {
        let mut d = SearchDoc::new(
            SessionInfo {
                agent: "claude".to_string(),
                session_id: format!("id-{title}"),
                workspace: None,
                path: PathBuf::from("/x.jsonl"),
                modified: Some(SystemTime::now() - std::time::Duration::from_secs(age_secs)),
                content_time: None,
                size_bytes: 0,
            },
            title.to_string(),
            summary.to_string(),
            None,
        );
        d.body = Some(Body::new(body.to_string()));
        d
    }

    fn ranked_titles(docs: &[SearchDoc], query: &str) -> Vec<String> {
        Ranker::default()
            .rank(docs, query)
            .iter()
            .map(|h| docs[h.index].title.clone())
            .collect()
    }

    #[test]
    fn blank_query_lists_everything_in_order() {
        let docs = vec![doc("a", "", "", 0), doc("b", "", "", 10)];
        assert_eq!(ranked_titles(&docs, "   "), vec!["a", "b"]);
    }

    #[test]
    fn summary_outranks_title_outranks_body() {
        let docs = vec![
            doc(
                "body only",
                "nothing here",
                "we fixed the zellij keybind",
                0,
            ),
            doc("zellij title", "nothing here", "", 0),
            doc("plain", "worked on zellij panes", "", 0),
        ];
        assert_eq!(
            ranked_titles(&docs, "zellij"),
            vec!["plain", "zellij title", "body only"]
        );
    }

    #[test]
    fn precedence_is_strict_across_mixed_fields() {
        let busy_body = "zellij ".repeat(200);
        let docs = vec![
            doc("zellij title", "", &busy_body, 0),
            doc("plain", "zellij summary", "", 1000),
            doc("zelliij typo", "", "", 0),
            doc("body only", "", &busy_body, 0),
        ];
        // Summary beats title+busy body; even a fuzzy title beats any body.
        assert_eq!(
            ranked_titles(&docs, "zellij"),
            vec!["plain", "zellij title", "zelliij typo", "body only"]
        );
        let hits = Ranker::default().rank(&docs, "zellij");
        assert!(hits[1].body_capped && hits[1].body_count == MAX_BODY_COUNT);
        assert!(!hits[0].body_capped);
    }

    #[test]
    fn every_term_must_match_somewhere() {
        let docs = vec![
            doc("cron runner", "", "slack delivery", 0),
            doc("cron only", "", "", 0),
        ];
        assert_eq!(ranked_titles(&docs, "cron slack"), vec!["cron runner"]);
        assert!(ranked_titles(&docs, "kubernetes").is_empty());
    }

    #[test]
    fn matching_more_fields_scores_higher() {
        let docs = vec![
            doc("sandbox", "", "", 0),
            doc("sandbox", "sandbox", "sandbox", 0),
        ];
        let hits = Ranker::default().rank(&docs, "sandbox");
        assert_eq!(hits[0].index, 1);
        assert!(hits[0].in_summary && hits[0].in_title && hits[0].body_count == 1);
        assert!(!hits[1].in_summary && hits[1].in_title && hits[1].body_count == 0);
    }

    #[test]
    fn ties_go_to_the_most_recent_session() {
        let docs = vec![doc("old vert", "", "", 1000), doc("new vert", "", "", 10)];
        assert_eq!(ranked_titles(&docs, "vert"), vec!["new vert", "old vert"]);
    }

    #[test]
    fn fuzzy_matches_titles_but_ignores_scattered_letters() {
        let docs = vec![
            doc("session search tui", "", "", 0),
            doc(
                "a very long title spelling s e a r c h only by accident",
                "",
                "",
                0,
            ),
        ];
        // Typo-tolerant: "sesion" still finds "session".
        assert_eq!(ranked_titles(&docs, "sesion"), vec!["session search tui"]);
        assert_eq!(ranked_titles(&docs, "srch"), vec!["session search tui"]);
        // Letters spread over several words are not a match.
        assert!(ranked_titles(&docs, "jwt").is_empty());
        let only_accident = vec![docs[1].clone()];
        assert!(ranked_titles(&only_accident, "search").is_empty());
    }

    #[test]
    fn exact_matches_outrank_fuzzy_ones() {
        let docs = vec![doc("session", "", "", 0), doc("sesion typo", "", "", 0)];
        let hits = Ranker::default().rank(&docs, "sesion");
        // "session" holds "sesion" only fuzzily, so the exact one wins.
        assert_eq!(docs[hits[0].index].title, "sesion typo");
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn body_matching_is_case_insensitive_and_counts_occurrences() {
        let docs = vec![
            doc("one", "", "Podman once", 0),
            doc("many", "", &"podman ".repeat(50), 0),
        ];
        let hits = Ranker::default().rank(&docs, "PODMAN");
        assert_eq!(hits[0].index, 1);
        assert_eq!(hits[0].body_count, 50);
        assert_eq!(hits[1].body_count, 1);
    }

    #[test]
    fn unindexed_bodies_match_on_summary_and_title_only() {
        let mut d = doc("t", "", "", 0);
        d.body = None;
        assert!(Ranker::default().rank(&[d], "anything").is_empty());
    }

    #[test]
    fn body_quality_is_bounded() {
        assert_eq!(body_quality(0), 0.0);
        assert_eq!(body_quality(1), 0.5);
        assert_eq!(body_quality(MAX_BODY_COUNT), 1.0);
        assert!(body_quality(10) > 0.5 && body_quality(10) < 1.0);
    }

    #[test]
    fn match_ranges_merges_overlaps() {
        let terms = query_terms("abc BCD");
        assert_eq!(match_ranges("xABCDx abc", &terms), vec![(1, 5), (7, 10)]);
        assert!(match_ranges("nothing", &terms).is_empty());
        assert!(match_ranges("text", &[String::new()]).is_empty());
    }

    #[test]
    fn snippets_show_context_and_respect_char_boundaries() {
        let body = Body::new("ééééé start\n\n  the  Needle is here ééééé".to_string());
        let s = snippets(&body, &query_terms("needle"), 3, 6);
        assert_eq!(s.len(), 1);
        assert!(s[0].contains("the Needle is"));
        assert!(s[0].starts_with('…') && s[0].ends_with('…'));
        // Nearby occurrences share one excerpt; `max` caps the count.
        let body = Body::new("a x a x a ".repeat(20));
        assert_eq!(snippets(&body, &query_terms("a"), 2, 1).len(), 2);
        assert!(snippets(&body, &query_terms("zzz"), 2, 1).is_empty());
        // Close-but-separate matches do not repeat the text between them.
        let body = Body::new("aaaa needle bbbbbbbbbbbbbb ccc needle dddd".to_string());
        let s = snippets(&body, &query_terms("needle"), 5, 12);
        assert_eq!(s.len(), 2);
        assert!(s[0].contains("bbbb"));
        assert!(!s[1].contains("bbbb"), "{s:?}");
    }

    fn write(dir: &Path, name: &str, lines: &[serde_json::Value]) -> PathBuf {
        let path = dir.join(name);
        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        std::fs::write(&path, text.join("\n") + "\nnot json\n").unwrap();
        path
    }

    #[test]
    fn extract_body_keeps_conversation_and_drops_tools_and_meta() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(
            temp.path(),
            "c.jsonl",
            &[
                serde_json::json!({"type":"user","message":{"role":"user","content":"fix the cron bug"}}),
                serde_json::json!({"type":"user","isMeta":true,"message":{"role":"user","content":"meta"}}),
                serde_json::json!({"type":"user","message":{"role":"user","content":"<system-reminder>x</system-reminder>"}}),
                serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[
                    {"type":"text","text":"Looking at it."},
                    {"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}),
                serde_json::json!({"type":"user","message":{"role":"user","content":[
                    {"type":"tool_result","content":[{"type":"text","text":"tool output"}]}]}}),
                // pi-family shape
                serde_json::json!({"type":"message","message":{"role":"toolResult","content":[{"type":"text","text":"pi tool"}]}}),
                serde_json::json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"Done."}]}}),
                serde_json::json!({"type":"message","message":{"role":"assistant","content":42}}),
            ],
        );
        assert_eq!(
            extract_body(&path).unwrap(),
            "fix the cron bug\n\nLooking at it.\n\nDone."
        );
        assert!(extract_body(&temp.path().join("missing.jsonl")).is_err());

        // Invalid UTF-8 on one line skips that line only.
        let mut bytes = br#"{"message":{"role":"user","content":"kept"}}"#.to_vec();
        bytes.extend_from_slice(b"\n\xff\xfe broken\n");
        bytes.extend_from_slice(br#"{"message":{"role":"assistant","content":"also kept"}}"#);
        let mixed = temp.path().join("mixed.jsonl");
        std::fs::write(&mixed, bytes).unwrap();
        assert_eq!(extract_body(&mixed).unwrap(), "kept\n\nalso kept");
    }

    fn session_at(path: &Path) -> SessionInfo {
        let meta = std::fs::metadata(path).unwrap();
        SessionInfo {
            agent: "claude".to_string(),
            session_id: "s".to_string(),
            workspace: None,
            path: path.to_path_buf(),
            modified: meta.modified().ok(),
            content_time: None,
            size_bytes: meta.len(),
        }
    }

    fn collect(
        rx: &mpsc::Receiver<IndexEvent>,
    ) -> (Vec<(usize, String)>, Vec<usize>, Option<String>) {
        let (mut bodies, mut failed) = (Vec::new(), Vec::new());
        for ev in rx {
            match ev {
                IndexEvent::Body { index, body } => bodies.push((index, body.text)),
                IndexEvent::Failed { index, .. } => failed.push(index),
                IndexEvent::Done { cache_error } => return (bodies, failed, cache_error),
            }
        }
        panic!("indexer ended without Done");
    }

    #[test]
    fn indexer_extracts_then_serves_from_cache_until_the_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache").join("search.json");
        let line = serde_json::json!({"type":"user","message":{"role":"user","content":"hello"}});
        let path = write(temp.path(), "a.jsonl", std::slice::from_ref(&line));
        let missing = session_at(&path);
        let mut missing = missing.clone();
        missing.path = temp.path().join("gone.jsonl");

        let (rx, _) = spawn_indexer(vec![session_at(&path), missing], cache.clone());
        let (bodies, failed, err) = collect(&rx);
        assert_eq!(bodies, vec![(0, "hello".to_string())]);
        assert_eq!(failed, vec![1]);
        assert!(err.is_none());
        assert!(cache.exists());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&cache).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "cache must be owner-only");

        // Same mtime and size: served from the cache, even if the content
        // on disk were different. Prove it by poisoning the cached text.
        let mut entries = load_cache(&cache);
        entries
            .values_mut()
            .for_each(|c| c.text = "cached".to_string());
        save_cache(&cache, entries).unwrap();
        let (bodies, _, _) = collect(&spawn_indexer(vec![session_at(&path)], cache.clone()).0);
        assert_eq!(bodies, vec![(0, "cached".to_string())]);

        // A changed file is re-extracted.
        let other =
            serde_json::json!({"type":"user","message":{"role":"user","content":"changed body"}});
        write(temp.path(), "a.jsonl", &[other]);
        let (bodies, _, _) = collect(&spawn_indexer(vec![session_at(&path)], cache.clone()).0);
        assert_eq!(bodies, vec![(0, "changed body".to_string())]);
    }

    #[test]
    fn early_exit_saves_progress_and_keeps_unreached_entries() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("c.json");
        let line =
            |t: &str| serde_json::json!({"type":"user","message":{"role":"user","content":t}});
        let a = write(temp.path(), "a.jsonl", &[line("first")]);
        let b = write(temp.path(), "b.jsonl", &[line("second")]);
        let c = write(temp.path(), "c.jsonl", &[line("third")]);
        // Warm the cache for c only.
        let (rx, h) = spawn_indexer(vec![session_at(&c)], cache.clone());
        collect(&rx);
        h.join().unwrap();

        // Drop the receiver before the indexer runs: its first send fails.
        let (tx, rx) = mpsc::channel();
        drop(rx);
        index_bodies(
            &[session_at(&a), session_at(&b), session_at(&c)],
            &cache,
            &tx,
        );
        let saved = load_cache(&cache);
        let key = |p: &Path| p.to_string_lossy().to_string();
        assert_eq!(saved[&key(&a)].text, "first", "work done is saved");
        assert!(!saved.contains_key(&key(&b)), "unreached and uncached");
        assert_eq!(saved[&key(&c)].text, "third", "unreached cached entry kept");
    }

    #[test]
    fn filtered_run_keeps_other_sessions_and_drops_deleted_ones() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("c.json");
        let line =
            |t: &str| serde_json::json!({"type":"user","message":{"role":"user","content":t}});
        let a = write(temp.path(), "a.jsonl", &[line("first")]);
        let b = write(temp.path(), "b.jsonl", &[line("second")]);
        let c = write(temp.path(), "c.jsonl", &[line("third")]);
        let (rx, h) = spawn_indexer(
            vec![session_at(&a), session_at(&b), session_at(&c)],
            cache.clone(),
        );
        collect(&rx);
        h.join().unwrap();

        // A --repo search indexes only `a`; `c` was deleted meanwhile.
        std::fs::remove_file(&c).unwrap();
        let (rx, h) = spawn_indexer(vec![session_at(&a)], cache.clone());
        let (bodies, _, err) = collect(&rx);
        h.join().unwrap();
        assert_eq!(bodies, vec![(0, "first".to_string())]);
        assert!(err.is_none());
        let saved = load_cache(&cache);
        let key = |p: &Path| p.to_string_lossy().to_string();
        assert_eq!(saved[&key(&b)].text, "second", "out-of-scope entry kept");
        assert!(!saved.contains_key(&key(&c)), "deleted transcript dropped");
    }

    #[test]
    fn outdated_or_corrupt_cache_is_ignored_and_save_errors_are_reported() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("c.json");
        std::fs::write(&cache, "{not json").unwrap();
        assert!(load_cache(&cache).is_empty());
        std::fs::write(
            &cache,
            r#"{"version":0,"entries":{"k":{"mtime":1,"size":1,"text":"t"}}}"#,
        )
        .unwrap();
        assert!(load_cache(&cache).is_empty());

        // A cache path under a regular file cannot be created.
        let blocker = temp.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        let line = serde_json::json!({"type":"user","message":{"role":"user","content":"x"}});
        let path = write(temp.path(), "a.jsonl", &[line]);
        let (_, _, err) =
            collect(&spawn_indexer(vec![session_at(&path)], blocker.join("sub").join("c.json")).0);
        assert!(err.unwrap().contains("could not create"));
    }

    #[test]
    fn cache_path_lives_next_to_memory() {
        let path = cache_path();
        assert!(path.ends_with("nibble/session-search.json"));
        assert!(!path.to_string_lossy().contains(".nibble"));
    }
}
