//! What land writes on a pull request, and how it reads its own records back.

use super::judge::Level;
use serde::{Deserialize, Serialize};

const MARKER: &str = "<!-- nibble:land ";
const FLAG_MARKER: &str = "<!-- nibble:land-flag ";
const MARKER_END: &str = " -->";

/// Accounts whose records count: anyone else could write the same text.
const TRUSTED: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

/// What land decided about one commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Decided {
    Merge,
    Fix,
    Ask,
}

/// One decision, stored in the comment that explains it.
///
/// Only fixed vocabulary is stored, never text a model wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Record {
    pub sha: String,
    pub decided: Decided,
    /// True when a judge decided, false when a fixed rule did.
    pub judged: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub impact: Option<Level>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reversible: Option<bool>,
}

/// What the owner says land should have done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Flag {
    ShouldAsk,
    ShouldMerge,
}

impl Flag {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Flag::ShouldAsk => "should-ask",
            Flag::ShouldMerge => "should-merge",
        }
    }
}

/// Whether an account's records count.
pub(super) fn trusted(association: &str) -> bool {
    TRUSTED.contains(&association)
}

/// Opens a comment with the record of its decision.
pub(super) fn marker(record: &Record) -> serde_json::Result<String> {
    Ok(format!(
        "{MARKER}{}{MARKER_END}",
        serde_json::to_string(record)?
    ))
}

/// The record a comment opens with, if it is one of land's.
pub(super) fn parse(body: &str) -> Option<Record> {
    let rest = body.strip_prefix(MARKER)?;
    serde_json::from_str(&rest[..rest.find(MARKER_END)?]).ok()
}

/// A comment recording a flag.
pub(super) fn flag_comment(flag: Flag, why: &str) -> String {
    format!(
        "{FLAG_MARKER}{}{MARKER_END}\nLand {}: {}",
        flag.as_str(),
        flag.as_str(),
        plain(why)
    )
}

/// The flag a comment records, with its reason.
pub(super) fn parse_flag(body: &str) -> Option<&str> {
    let rest = body.strip_prefix(FLAG_MARKER)?;
    let text = rest[rest.find(MARKER_END)? + MARKER_END.len()..].trim();
    text.strip_prefix("Land ")
}

/// Makes model or user text safe to post from the owner's account.
///
/// It must not mention anyone, trigger the reviewer by name, or hide a record.
pub(super) fn plain(text: &str) -> String {
    text.trim()
        .replace('@', "@\u{200b}")
        .replace("<!--", "&lt;!--")
}
