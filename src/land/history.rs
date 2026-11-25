//! What earlier pull requests teach the judge about this one.
//!
//! Nothing is stored: GitHub already remembers what land decided, what the
//! owner flagged, and which changes were reverted or needed a fix.

use super::record::{self, Decided};
use serde::Deserialize;
use std::collections::HashSet;

/// How many recent merged pull requests are considered.
pub(super) const LOOKBACK: u32 = 50;

/// Most lessons shown to the judge, newest first.
const MAX_LESSONS: usize = 40;

pub(super) const QUERY: &str = "query($owner:String!,$name:String!,$count:Int!){repository(owner:$owner,name:$name){pullRequests(states:[MERGED,CLOSED],first:$count,orderBy:{field:CREATED_AT,direction:DESC}){nodes{number title state createdAt mergedAt files(first:100){nodes{path}} comments(last:40){nodes{body authorAssociation}} reviewThreads(first:60){nodes{path isOutdated comments(first:1){nodes{body}}}}}}}}";

#[derive(Debug, Deserialize)]
pub(super) struct Response {
    data: Data,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Data {
    repository: Repository,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    pull_requests: Nodes<Past>,
}

#[derive(Debug, Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Past {
    number: u64,
    title: String,
    state: String,
    created_at: String,
    merged_at: Option<String>,
    files: Option<Nodes<PastFile>>,
    comments: Nodes<PastComment>,
    review_threads: Nodes<PastThread>,
}

#[derive(Debug, Deserialize)]
struct PastFile {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PastComment {
    body: String,
    author_association: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PastThread {
    path: Option<String>,
    is_outdated: bool,
    comments: Nodes<ThreadBody>,
}

#[derive(Debug, Deserialize)]
struct ThreadBody {
    body: String,
}

impl Past {
    fn paths(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .flat_map(|files| files.nodes.iter().map(|file| file.path.as_str()))
    }

    fn overlap<'a>(&'a self, changed: &HashSet<&str>) -> Vec<&'a str> {
        self.paths().filter(|path| changed.contains(path)).collect()
    }

    /// A revert or fix by its title, the convention in these repositories.
    fn repairs(&self) -> bool {
        let title = self.title.to_lowercase();
        self.state == "MERGED" && (title.starts_with("revert") || title.starts_with("fix"))
    }
}

/// Lessons from past pull requests, newest first: every owner flag, and what
/// happened to changes that touched the same files.
///
/// A merge that nobody complained about teaches nothing, because owners merge
/// most things out of habit and agents merge without one.
///
/// `replayed` keeps only what was known when pull request `number` was opened,
/// so a backtest can't see how it turned out.
pub(super) fn lessons(
    response: &Response,
    number: u64,
    files: &[&str],
    replayed: bool,
) -> Vec<String> {
    let changed: HashSet<&str> = files.iter().copied().collect();
    let all = &response.data.repository.pull_requests.nodes;
    let opened = all
        .iter()
        .find(|pr| pr.number == number)
        .map(|pr| pr.created_at.as_str());
    let past: Vec<&Past> = all
        .iter()
        .filter(|pr| !replayed || opened.is_some_and(|opened| pr.created_at.as_str() < opened))
        .collect();
    let mut lessons = Vec::new();
    for pr in past.iter().copied().filter(|pr| pr.number != number) {
        let name = format!("#{} \"{}\"", pr.number, pr.title);
        let trusted = || {
            pr.comments
                .nodes
                .iter()
                .filter(|comment| record::trusted(&comment.author_association))
        };
        // Flags say what the owner wants in general, so they always count.
        lessons.extend(
            trusted()
                .filter_map(|comment| record::parse_flag(&comment.body))
                .map(|flag| format!("owner flag on {name}: {flag}")),
        );
        let shared = pr.overlap(&changed);
        if shared.is_empty() {
            continue;
        }
        let shown = shared
            .iter()
            .take(3)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        if pr.state == "CLOSED" {
            lessons.push(format!("{name} touched {shown} and was closed unmerged"));
        }
        if pr.repairs() {
            lessons.push(format!("{name} later repaired {shown}"));
        }
        if trusted()
            .filter_map(|comment| record::parse(&comment.body))
            .any(|record| record.judged && record.decided == Decided::Ask)
        {
            lessons.push(format!(
                "{name} touched {shown} and the judge asked the owner"
            ));
        }
        if let Some(merged) = &pr.merged_at {
            let repaired: Vec<String> = past
                .iter()
                .filter(|other| other.number != pr.number && other.repairs())
                // Both are GitHub's UTC timestamps, so text order is time order.
                .filter(|other| other.created_at > *merged)
                .filter(|other| other.paths().any(|path| shared.contains(&path)))
                .map(|other| format!("#{}", other.number))
                .collect();
            if !repaired.is_empty() {
                lessons.push(format!(
                    "{name} touched {shown} and the same files were repaired after it merged, in {}",
                    repaired.join(", ")
                ));
            }
        }
        // Only findings whose code then changed: a dismissed one may have been wrong.
        let severe = pr
            .review_threads
            .nodes
            .iter()
            .filter(|thread| thread.is_outdated)
            .filter(|thread| {
                thread
                    .path
                    .as_deref()
                    .is_some_and(|path| changed.contains(path))
            })
            .filter(|thread| {
                thread.comments.nodes.first().is_some_and(|comment| {
                    ["**high:**", "**critical:**"]
                        .iter()
                        .any(|prefix| comment.body.contains(prefix))
                })
            })
            .count();
        if severe > 0 {
            lessons.push(format!(
                "{name}: the reviewer found {severe} high or critical problem(s) in these files, since fixed"
            ));
        }
    }
    lessons.truncate(MAX_LESSONS);
    lessons
}
