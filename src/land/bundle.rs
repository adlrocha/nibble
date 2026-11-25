//! The decision and evidence `nibble-review`'s land reports for one pull request.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The bundle shape this build reads.
pub(super) const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Mode {
    Off,
    Shadow,
    Enforce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Profile {
    SideProject,
    Standard,
    Critical,
}

impl Profile {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Profile::SideProject => "side-project",
            Profile::Standard => "standard",
            Profile::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Outcome {
    Ready,
    NotReady,
    Ask,
}

/// Read first, because an `off` bundle carries nothing else.
#[derive(Debug, Deserialize)]
pub(super) struct Header {
    pub version: u32,
    pub mode: Mode,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Bundle {
    pub mode: Mode,
    pub policy: Policy,
    pub pull_request: PullRequest,
    pub decision: Decision,
    pub files: Vec<File>,
    /// Missing when GitHub refuses a very large diff.
    pub diff: Option<String>,
    pub risk_map: Option<String>,
    pub issues: Vec<Issue>,
    pub reviews: Vec<Review>,
    pub threads: Vec<Thread>,
    pub comments: Vec<Comment>,
}

/// The `[land]` table. Unknown keys are kept, so the judge sees the whole policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Policy {
    pub profile: Profile,
    #[serde(flatten)]
    pub rest: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PullRequest {
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub base: String,
    pub head_sha: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Decision {
    pub outcome: Outcome,
    pub blockers: Vec<String>,
    pub asks: Vec<String>,
    pub signals: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct File {
    pub filename: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Review {
    pub author: String,
    pub state: String,
    pub at: String,
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Thread {
    pub path: Option<String>,
    pub resolved: bool,
    pub code_changed: bool,
    pub comments: Vec<ThreadComment>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct ThreadComment {
    pub author: String,
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Comment {
    pub author: String,
    pub association: String,
    pub at: String,
    pub body: String,
}
