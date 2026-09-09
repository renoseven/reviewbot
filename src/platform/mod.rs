//! Code hosting platforms: URL parsing, host matching, fetching the change,
//! posting comments, and the platform side of repository reads.
//!
//! One of the three extension points. Adding a platform means implementing
//! `Platform` and naming its `kind` in the config.

pub mod github;
pub mod gitlab;
mod http;
pub mod source;
pub mod url;

use std::sync::Arc;

use crate::common::{Backoff, Secret, SecretSource};
use crate::config::{Config, PlatformEntry, PlatformKind};
use crate::domain::Narrative;

pub use source::{LineRange, Listing, RepoSource, SearchHit};
pub use url::ChangeRef;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("cannot read {url} as a merge request or pull request URL")]
    UnparsableUrl { url: String },
    #[error("no [[platform]] entry for host {host:?}; add one to the config")]
    UnknownHost { host: String },
    #[error("{operation} against {host} failed: {reason}")]
    Request {
        operation: &'static str,
        host: String,
        reason: String,
    },
    #[error(
        "{host} refused {operation} (HTTP {status}) at {url}: {said}. \
         If it is the token, GitLab needs the `api` scope and GitHub needs \
         `pull_requests: write`. The run stops rather than falling back to a \
         report only"
    )]
    Permission {
        operation: &'static str,
        host: String,
        status: u16,
        url: String,
        /// What the platform put in the body. A 403 is not always the token:
        /// GitHub sends one for a missing `User-Agent` too, and without this
        /// the only readable explanation is thrown away.
        said: String,
    },
    #[error("{operation} against {host} was rejected (HTTP 422): {reason}")]
    Unprocessable {
        operation: &'static str,
        host: String,
        reason: String,
    },
    #[error("{host} does not support {capability}")]
    Unsupported {
        host: String,
        capability: &'static str,
    },
    #[error("{operation} is not implemented yet for {kind}")]
    NotImplemented {
        operation: &'static str,
        kind: &'static str,
    },
}

/// What this particular instance can actually do. Code search is the only
/// optional one, and `tool` registers `search_repo` only when it is there.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    pub code_search: bool,
    /// Whether that search takes a regular expression. A keyword-only index
    /// treats metacharacters as literal text, and the model has no way to
    /// notice the difference, so `search_repo`'s description is written from
    /// this at registration time.
    pub regex_search: bool,
}

/// The change as the platform describes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlatformChange {
    pub diff: String,
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub start_sha: Option<String>,
    /// Title, description and commit subjects. Empty when the platform
    /// would not give them up: a description that cannot be read is worth
    /// a warning, never a failed run.
    pub narrative: Narrative,
}

/// Old and new paths as the diff named them. GitLab discussions need both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffPaths {
    pub old_path: String,
    pub new_path: String,
}

impl DiffPaths {
    pub fn for_path(path: &str) -> Self {
        Self {
            old_path: path.to_string(),
            new_path: path.to_string(),
        }
    }

    /// The path a GitHub review comment hangs on: the new side, or the old
    /// side when the file was deleted.
    pub fn display(&self) -> &str {
        if self.new_path != crate::domain::DEV_NULL && !self.new_path.is_empty() {
            &self.new_path
        } else {
            &self.old_path
        }
    }
}

/// The SHAs a comment is pinned against. GitLab needs all three; GitHub
/// uses `head_sha` as the review `commit_id`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffRefs {
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub start_sha: Option<String>,
}

/// A comment on its way out, already carrying its idempotency marker.
#[derive(Clone, Debug, PartialEq)]
pub struct OutgoingComment {
    pub paths: DiffPaths,
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub body: String,
    pub marker: String,
}

impl OutgoingComment {
    pub fn is_summary(&self) -> bool {
        self.marker.contains(":summary -->")
    }
}

/// A comment that is already on the merge request, identified by the marker
/// hidden in its body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExistingComment {
    pub marker: String,
    pub url: Option<String>,
    /// True when a 422 forced this comment onto the file instead of a line.
    pub degraded_to_file: bool,
}

impl ExistingComment {
    pub fn from_body(body: &str, url: Option<String>) -> Vec<Self> {
        http::markers_in(body)
            .into_iter()
            .map(|marker| Self {
                marker,
                url: url.clone(),
                degraded_to_file: false,
            })
            .collect()
    }
}

pub trait Platform: Send + Sync {
    fn kind(&self) -> PlatformKind;

    fn host(&self) -> &str;

    fn capabilities(&self) -> Capabilities;

    /// The head commit alone. `run_id` needs it before the run directory
    /// exists, which is earlier than the full diff is wanted.
    fn head_sha(&self, change: &ChangeRef) -> Result<String, PlatformError>;

    /// Metadata, diff and the SHAs comments are pinned against.
    fn fetch_change(&self, change: &ChangeRef) -> Result<PlatformChange, PlatformError>;

    /// Read before posting, so a rerun does not say everything twice.
    fn existing_comments(&self, change: &ChangeRef) -> Result<Vec<ExistingComment>, PlatformError>;

    fn post_comments(
        &self,
        change: &ChangeRef,
        refs: &DiffRefs,
        comments: &[OutgoingComment],
    ) -> Result<Vec<ExistingComment>, PlatformError>;

    /// Point the repository reads at the change and the commit they belong
    /// to. Content is always read by `head_sha`, and that sha is known before
    /// any stage runs, so the source is told once instead of a `ChangeRef`
    /// travelling through every read.
    fn bind_repo(&self, change: &ChangeRef, head_sha: &str);

    /// Shared rather than borrowed: the builtin tools outlive the call that
    /// registers them, and they hold this for the whole run.
    fn repo_source(&self) -> Arc<dyn RepoSource>;
}

/// Pick the `[[platform]]` entry whose host matches, resolve its `kind`, and
/// build the implementation. Never falls back to `gitlab.com`.
pub fn resolve(
    config: &Config,
    host: &str,
    backoff: Backoff,
) -> Result<Box<dyn Platform>, PlatformError> {
    let entry = config
        .platform(host)
        .ok_or_else(|| PlatformError::UnknownHost {
            host: host.to_string(),
        })?;
    let kind = entry
        .resolved_kind()
        .ok_or_else(|| PlatformError::UnknownHost {
            host: host.to_string(),
        })?;
    let token = read_token(entry)?;
    Ok(match kind {
        PlatformKind::Gitlab => Box::new(gitlab::GitLab::new(entry.clone(), token, backoff)),
        PlatformKind::Github => Box::new(github::GitHub::new(entry.clone(), token, backoff)),
    })
}

fn read_token(entry: &PlatformEntry) -> Result<Secret, PlatformError> {
    let field = format!("platform.{}.api_token", entry.host);
    SecretSource::parse(&field, &entry.api_token)
        .and_then(|source| source.read(&field, None))
        .map_err(|error| PlatformError::Request {
            operation: "reading the API token",
            host: entry.host.clone(),
            reason: error.to_string(),
        })
}
