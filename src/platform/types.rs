use std::sync::Arc;

use crate::domain::Narrative;

use super::error::PlatformError;
use super::http;
use super::source::RepoSource;

bitflags::bitflags! {
    /// What this platform's code search can answer. An empty set means it cannot.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct Capabilities: u8 {
        const REGEX_SEARCH   = 1 << 0;
        const KEYWORD_SEARCH = 1 << 1;
    }
}

/// A repository that answers by `head_sha`, together with the searches it can run.
#[derive(Clone)]
pub struct Repo {
    source: Arc<dyn RepoSource>,
    capabilities: Capabilities,
}

impl Repo {
    pub fn new(source: Arc<dyn RepoSource>, capabilities: Capabilities) -> Self {
        Self {
            source,
            capabilities,
        }
    }

    pub fn source(&self) -> Arc<dyn RepoSource> {
        Arc::clone(&self.source)
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Body already in hand at the source. Never issues a request: a `None`
    /// is "this optimisation does not apply", not a miss to go fetch.
    pub fn cached_body(&self, path: &str) -> Option<String> {
        self.source.cached_body(path)
    }
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

use crate::common::{Secret, SecretSource};
use crate::config::PlatformEntry;

pub(crate) fn read_token(entry: &PlatformEntry) -> Result<Secret, PlatformError> {
    let host = entry.host().unwrap_or("unknown");
    let field = format!("platform.{host}.api_token");
    SecretSource::parse(&field, &entry.api_token)
        .and_then(|source| source.read(&field, None))
        .map_err(|error| PlatformError::Request {
            operation: "reading the API token",
            host: host.to_string(),
            reason: error.to_string(),
        })
}
