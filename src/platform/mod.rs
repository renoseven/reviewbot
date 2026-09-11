//! Code hosting platforms: URL parsing, host matching, fetching the change,
//! posting comments, and the platform side of repository reads.
//!
//! One of the three extension points.

pub mod error;
pub mod github;
pub mod gitlab;
mod http;
pub mod source;
pub mod types;
pub mod url;

use crate::config::PlatformKind;

pub use error::PlatformError;
pub use source::{File, LineRange, Listing, RepoSource, SearchHit, SearchKind};
pub(crate) use types::read_token;
pub use types::{
    Capabilities, DiffPaths, DiffRefs, ExistingComment, OutgoingComment, PlatformChange, Repo,
};
pub use url::ChangeRef;

pub trait Platform: Send + Sync {
    fn kind(&self) -> PlatformKind;

    fn host(&self) -> &str;

    /// The repository this platform answers about, together with the searches
    /// it can run. `repo_source()` and `capabilities()` used to be separate;
    /// they were always used together, and splitting them left a seam where
    /// one platform's source could be paired with a `Capabilities` that is
    /// not its own.
    fn repo(&self) -> Repo;

    /// Do you already hold this file's bytes? Hand them over, and never issue
    /// a request for this. Later the search tools warm the local cache from
    /// hits that already paid for a body; a `None` here must not turn into a
    /// download the model never asked for.
    fn cached_body(&self, path: &str) -> Option<String>;

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
    fn bind_repo(&self, change: &ChangeRef, head_sha: &str) -> Result<(), PlatformError>;
}
