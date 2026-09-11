use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The normalized review target. Every stage after `input` reads this and
/// never the URL or the raw diff it came from.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ChangeSet {
    pub locator: Locator,
    pub files: Vec<FileChange>,
    /// `default` so a checkpoint written before this field existed can still be
    /// read back, as an unnarrated change.
    #[serde(default)]
    pub narrative: Narrative,
}

/// What the author said this change is for: the title, the description and
/// the commit subjects, as written. Diff input has none of it.
///
/// This is the one thing reaching the model that its own author wrote to be
/// read, which makes it the highest-value context available and the only
/// prompt-injection surface reviewbot fetches on purpose. It says what the
/// change was *meant* to do; it is never evidence about what the code does,
/// and it goes to the model fenced as material next to the diff rather than
/// into `instructions`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Narrative {
    pub title: Option<String>,
    pub description: Option<String>,
    /// Subject lines, in the order the platform listed them.
    pub commits: Vec<String>,
    /// Whether more commits exist than are listed here. Not a count: neither
    /// platform gives one without another round trip, and "there are more"
    /// is the whole of what the model can act on.
    pub more_commits: bool,
}

impl Narrative {
    /// As many commit subjects as are worth sending. A long branch's later
    /// commits are mostly fixups of its earlier ones, so the tail buys less
    /// per token than anything else in the prompt.
    pub const COMMITS: usize = 20;

    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.description.is_none() && self.commits.is_empty()
    }
}

/// Where the change lives, so comments can be pinned back onto it.
///
/// Diff input has no platform number; it only carries the worktree HEAD when
/// `--worktree` was given.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Locator {
    pub host: Option<String>,
    pub project: Option<String>,
    pub number: Option<u64>,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub start_sha: Option<String>,
}

/// The side a file does not have: `/dev/null` stands in for it in both the
/// diff and here.
pub const DEV_NULL: &str = "/dev/null";

/// One file of the change, with both line sets built while the full diff is
/// still in hand.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FileChange {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<Hunk>,
    /// Added lines plus context lines: where the platform accepts a comment.
    pub commentable_lines: BTreeSet<u32>,
    /// Added lines plus the line next to a pure deletion: what this change touched.
    pub changed_lines: BTreeSet<u32>,
    /// A file the diff could only describe as changed, never line by line.
    #[serde(default)]
    pub binary: bool,
}

/// One hunk. `text` is the whole thing including its `@@` header line, so a
/// chunk can be reassembled into a valid unified diff fragment.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diff_has_no_narrative_at_all() {
        assert!(Narrative::default().is_empty());
    }
}
