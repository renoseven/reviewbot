//! Stage 1. A URL or a raw diff becomes one `ChangeSet`. Normalization only;
//! fetching is `platform`'s job.
//!
//! Both line sets are built here because this is the last moment the full
//! diff is in hand: nothing downstream can rebuild them.

pub mod diff;

use serde::{Deserialize, Serialize};

use crate::domain::{ChangeSet, Locator, Narrative};
use crate::platform::ChangeRef;
use crate::record::{InputIdentity, InputKind, InputRecord};

use diff::UnifiedDiff;

use super::{Adapters, StageContext, StageError};

pub const NUMBER: u8 = 1;
pub const NAME: &str = "input";

/// What the positional argument turned out to be. `http(s)://` is a platform
/// URL, `-` is standard input, anything else is a diff file.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum Source {
    Url(String),
    /// `origin` is the file path or `-`; the content is what identifies it.
    Diff {
        origin: String,
        content: String,
    },
}

impl Source {
    /// The host that picks the `[[platform]]` entry, if there is one.
    pub fn host(&self) -> Option<String> {
        match self {
            Source::Url(url) => ChangeRef::parse(url).ok().map(|change| change.host),
            Source::Diff { .. } => None,
        }
    }

    /// Rebuild the input a run started from, which `resume` needs when the
    /// input stage never finished. A diff is re-read from the recorded path
    /// and checked against the content hash it was identified by.
    pub fn from_record(record: &InputRecord) -> Result<Self, StageError> {
        match record.kind {
            InputKind::Url => Ok(Source::Url(record.source.clone())),
            InputKind::Diff => {
                let content = std::fs::read_to_string(&record.source).map_err(|error| {
                    StageError::UnreadableInput {
                        reason: format!("{}: {error}", record.source),
                    }
                })?;
                if InputIdentity::diff(&content) != record.identity {
                    return Err(StageError::UnreadableInput {
                        reason: format!(
                            "{} no longer holds the diff this run started from",
                            record.source
                        ),
                    });
                }
                Ok(Source::Diff {
                    origin: record.source.clone(),
                    content,
                })
            }
        }
    }
}

pub struct Input;

impl Input {
    /// What this run is, resolved before the run directory exists because
    /// `run_id` is built from it.
    pub fn identify(adapters: &Adapters, source: &Source) -> Result<InputRecord, StageError> {
        match source {
            Source::Url(url) => {
                let change = ChangeRef::parse(url)?;
                let platform =
                    adapters
                        .platform
                        .as_ref()
                        .ok_or_else(|| StageError::UnreadableInput {
                            reason: format!("no platform configured for {}", change.host),
                        })?;
                tracing::info!(
                    host = %change.host,
                    project = %change.project,
                    number = change.number,
                    "resolved change from URL"
                );
                let head_sha = platform.head_sha(&change)?;
                Ok(InputRecord {
                    kind: InputKind::Url,
                    source: url.clone(),
                    identity: InputIdentity::Platform {
                        host: change.host,
                        project: change.project,
                        number: change.number,
                    },
                    head_sha,
                })
            }
            Source::Diff { origin, content } => {
                // A worktree the run opened for itself stands on no commit of
                // its own, and that is recorded as an empty sha rather than
                // invented: the run id is built out of this.
                let head_sha = adapters.worktree.head_sha()?.unwrap_or_default();
                Ok(InputRecord {
                    kind: InputKind::Diff,
                    source: origin.clone(),
                    identity: InputIdentity::diff(content),
                    head_sha,
                })
            }
        }
    }

    pub fn run(context: &mut StageContext<'_>, source: &Source) -> Result<ChangeSet, StageError> {
        let changeset = match source {
            Source::Url(url) => Self::from_platform(context, url)?,
            Source::Diff { content, .. } => Self::from_diff(context, content)?,
        };
        tracing::info!(
            files = changeset.files.len(),
            "input normalized into a change set"
        );
        context.complete(NUMBER, NAME, &changeset)?;
        Ok(changeset)
    }

    fn from_platform(context: &mut StageContext<'_>, url: &str) -> Result<ChangeSet, StageError> {
        let change = ChangeRef::parse(url)?;
        let platform =
            context
                .adapters
                .platform
                .as_ref()
                .ok_or_else(|| StageError::UnreadableInput {
                    reason: format!("no platform configured for {}", change.host),
                })?;
        let fetched = platform.fetch_change(&change)?;

        // A checkout standing on a different commit would make every line
        // number wrong, so it is refused before anything is read. A worktree
        // the run filled itself has no commit of its own to disagree with:
        // every file in it was fetched at this very sha.
        if let Some(actual) = context.adapters.worktree.head_sha()?
            && actual != fetched.head_sha
        {
            return Err(StageError::Worktree(
                crate::worktree::WorktreeError::HeadMismatch {
                    actual,
                    expected: fetched.head_sha.clone(),
                },
            ));
        }

        // The platform's diff endpoint returns the same unified diff a local
        // file holds, so it goes through the same parser.
        Ok(ChangeSet {
            locator: Locator {
                host: Some(change.host),
                project: Some(change.project),
                number: Some(change.number),
                head_sha: Some(fetched.head_sha),
                base_sha: fetched.base_sha,
                start_sha: fetched.start_sha,
            },
            files: UnifiedDiff::parse(&fetched.diff)?.into_files(),
            narrative: fetched.narrative,
        })
    }

    fn from_diff(context: &mut StageContext<'_>, content: &str) -> Result<ChangeSet, StageError> {
        let head_sha = context.adapters.worktree.head_sha()?;
        Ok(ChangeSet {
            locator: Locator {
                head_sha,
                ..Locator::default()
            },
            files: UnifiedDiff::parse(content)?.into_files(),
            // A diff on disk has no author's account of itself. Nothing is
            // invented from the file name.
            narrative: Narrative::default(),
        })
    }
}
