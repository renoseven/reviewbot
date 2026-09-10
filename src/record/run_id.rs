//! `run_id = hash(input identity + head_sha)`.
//!
//! The settings are deliberately not in it: the same merge request at the
//! same commit is always the same directory, so editing the config re-enters
//! that run — dropping whichever stages the edit reaches — instead of
//! starting a fresh directory and orphaning the old one. It also makes the
//! id something the caller can work out from the URL alone, which is what
//! `--run-id` is for.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What identifies the thing being reviewed, independent of where the bytes
/// came from. Diff input is keyed by content, so renaming the file still
/// lands on the same run.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputIdentity {
    Platform {
        host: String,
        project: String,
        number: u64,
    },
    Diff {
        content_sha256: String,
    },
}

impl InputIdentity {
    pub fn diff(content: &str) -> Self {
        InputIdentity::Diff {
            content_sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
        }
    }

    fn key(&self) -> String {
        match self {
            InputIdentity::Platform {
                host,
                project,
                number,
            } => format!("platform:{host}:{project}:{number}"),
            InputIdentity::Diff { content_sha256 } => format!("diff:{content_sha256}"),
        }
    }
}

/// `head_sha` is empty for diff input without `--worktree`.
pub fn run_id(identity: &InputIdentity, head_sha: &str) -> String {
    const LENGTH: usize = 16;
    let digest = Sha256::digest(format!("{}\n{head_sha}", identity.key()).as_bytes());
    format!("{digest:x}")[..LENGTH].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diff_is_identified_by_content_not_by_file_name() {
        let one = InputIdentity::diff("--- a\n+++ b\n");
        let same = InputIdentity::diff("--- a\n+++ b\n");
        let other = InputIdentity::diff("--- a\n+++ c\n");
        assert_eq!(run_id(&one, ""), run_id(&same, ""));
        assert_ne!(run_id(&one, ""), run_id(&other, ""));
    }

    /// The commit is half the identity: the same merge request reviewed
    /// again after a push is a different run, because the answers the old
    /// one recorded were about other bytes.
    #[test]
    fn a_new_commit_on_the_same_change_is_a_new_run() {
        let identity = InputIdentity::Platform {
            host: "gitlab.com".to_string(),
            project: "acme/app".to_string(),
            number: 128,
        };
        assert_ne!(
            run_id(&identity, "4b1e0d2"),
            run_id(&identity, "aaaaaaa"),
            "the head sha is in the id"
        );
        assert_eq!(
            run_id(&identity, "4b1e0d2"),
            run_id(&identity, "4b1e0d2"),
            "and nothing else is, so the id is predictable from the URL"
        );
    }
}
