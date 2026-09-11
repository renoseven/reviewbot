//! Where this run reads code from, and the one place a fetched file may land.
//!
//! Three shapes, and which one a run has is the type's own answer rather than
//! a field it carries. A checkout named on the command line is the whole
//! project standing on the reviewed commit. Without one, a run with a
//! repository behind it opens a cache of its own under the run directory and
//! fills it a file at a time. A plain diff with no platform behind it has no
//! directory at all, because nobody would read it.
//!
//! **Writing needs a root and a repository together, and `Cache` is the only
//! variant holding both.** That is how "the checkout is left byte for byte as
//! it was" is kept: not by a branch somebody has to remember, but by there
//! being nowhere for the write to happen.
//!
//! The methods come in two halves that never call each other. The local half
//! answers only about what is in `root` at this moment; the repository half
//! asks the platform about the reviewed commit. Fetching and reading are two
//! separate actions — a cache miss is a miss, and `read_local` never fetches
//! — so a file the model wants is a file it asked for.
//!
//! Ceilings are passed in rather than owned: `fetch`, `read_local` and
//! `stat_local` enforce the number they are handed and never decide it, and
//! the words the model reads about it are the tool layer's to write.
//!
//! Repository content vocabulary — `LineRange`, `Listing`, `SearchHit` — is
//! defined once, over in `platform::source`, and spoken here rather than
//! declared a second time. `platform` is the one dependency allowed inside
//! the adapter layer.
//!
//! Not an extension point: there is only one local filesystem.

use std::path::{Path, PathBuf};

use globset::Glob;
use regex::Regex;

use crate::platform::{PlatformError, Repo, SearchKind};

pub use crate::platform::{File, LineRange, Listing, SearchHit};

/// What this run's cache is called inside the run directory. The name lives
/// in the run-directory layout so `run prune` and the worktree agree.
pub const DIRECTORY: &str = crate::record::layout::CACHE;

/// How much of a file is looked at before calling it text. A NUL in the first
/// few kilobytes is what every other tool takes for "not text", and one
/// definition serves both the fetch that writes and the search that reads.
const TEXT_PROBE_BYTES: usize = 8 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("{path}: {reason}")]
    Unreadable { path: String, reason: String },
    #[error("{path} is not a directory")]
    NotADirectory { path: PathBuf },
    #[error("cannot open the worktree at {path}: {reason}")]
    Unopenable { path: PathBuf, reason: String },
    #[error("invalid glob {pattern:?}: {reason}")]
    InvalidGlob { pattern: String, reason: String },
    #[error("invalid search pattern {query:?}: {reason}")]
    InvalidQuery { query: String, reason: String },
    #[error("cannot determine the worktree HEAD under {path}")]
    NoHead { path: PathBuf },
    #[error("worktree HEAD {actual} does not match the change head {expected}")]
    HeadMismatch { actual: String, expected: String },
    /// The reason is the platform's own words rather than its error type: a
    /// fetch that failed is answered to the model as text, and nothing
    /// branches on which way it failed.
    #[error("cannot fetch {path} at the reviewed commit: {reason}")]
    Unfetchable { path: String, reason: String },
    #[error("{path} is not text, so it is not written to the worktree")]
    Binary { path: String },
    /// Past the ceiling the caller handed in. The number comes back so the
    /// tool layer can say it; what to do about it is not decided here.
    #[error("{bytes} bytes, past the ceiling this call was given")]
    TooBig { bytes: u64 },
}

/// A checkout on disk, before a run directory exists. The head sha goes
/// into the run id, and the run id names the directory the worktree opens
/// in, so this is asked first and on its own.
pub struct Checkout {
    root: PathBuf,
}

impl Checkout {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, WorktreeError> {
        let root = root.into();
        if !root.is_dir() {
            return Err(WorktreeError::NotADirectory { path: root });
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The commit this checkout stands on, read straight off `.git/HEAD`
    /// and one level of ref rather than by shelling out to git.
    pub fn head(&self) -> Result<String, WorktreeError> {
        let no_head = || WorktreeError::NoHead {
            path: self.root.clone(),
        };
        let text =
            std::fs::read_to_string(self.root.join(".git").join("HEAD")).map_err(|_| no_head())?;
        let text = text.trim();
        let Some(reference) = text.strip_prefix("ref: ") else {
            return Ok(text.to_string());
        };
        std::fs::read_to_string(self.root.join(".git").join(reference))
            .map(|sha| sha.trim().to_string())
            .map_err(|_| no_head())
    }
}

/// This run's worktree.
pub enum Worktree {
    /// Nothing to read: a plain diff with no `--worktree` and no platform.
    /// No directory, because nobody would read one.
    Empty,
    /// The checkout the command line named: the whole file tree at the
    /// reviewed commit, read only, and known before the run directory exists.
    ///
    /// Whether there is a repository too is a separate question — a URL input
    /// has one, a plain diff does not. A whole tree on disk is no reason to
    /// keep the model from asking the platform to search, so it carries one
    /// when there is one.
    Local { root: PathBuf, repo: Option<Repo> },
    /// The cache this run opened for itself, at `<run_dir>/cache`. There is
    /// always a repository behind it: without one there would be nothing to
    /// cache.
    Cache { root: PathBuf, repo: Repo },
}

/// What a fetch left behind: where the file is now and how big it is. Never
/// the body — reading it is the other half's job, and a separate call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fetched {
    pub path: String,
    pub bytes: u64,
}

/// The size of a local file in both units that matter. `lines` is `None` when
/// the file was past the ceiling and so was never read: counting lines takes
/// the body, and the whole point of the ceiling is not to take it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stat {
    pub bytes: u64,
    pub lines: Option<u32>,
}

/// What a local search found, and over how much. The corpus is part of the
/// answer: a cache holds only what has been fetched into it, so "no matches"
/// says nothing without these two counts beside it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Scan {
    pub hits: Vec<SearchHit>,
    /// Files whose text was really matched against.
    pub searched: usize,
    /// Files passed over because they are not text. Searching a binary for a
    /// pattern is not a search anybody wants, and the hits it invents crowd
    /// out the real ones.
    pub skipped: usize,
}

impl Worktree {
    /// The one place a run's worktree is decided. Called once the run
    /// directory exists, so all three shapes know their root: the cache's
    /// lives under it and is created here, which is why this waits for the
    /// lock.
    pub fn open(
        checkout: Option<PathBuf>,
        repo: Option<Repo>,
        run_dir: &Path,
    ) -> Result<Self, WorktreeError> {
        match (checkout, repo) {
            (Some(root), repo) => match root.is_dir() {
                true => Ok(Worktree::Local { root, repo }),
                false => Err(WorktreeError::NotADirectory { path: root }),
            },
            // Never reused across runs: reuse needs rules for archiving and
            // for invalidating by commit, and reuse without them is stale
            // data read as current.
            (None, Some(repo)) => {
                let root = run_dir.join(DIRECTORY);
                std::fs::create_dir_all(&root).map_err(|error| WorktreeError::Unopenable {
                    path: root.clone(),
                    reason: error.to_string(),
                })?;
                Ok(Worktree::Cache { root, repo })
            }
            (None, None) => Ok(Worktree::Empty),
        }
    }

    /// Where the files are. `Empty` has no directory at all, and the tools
    /// that would need one refuse before they ask.
    pub fn root(&self) -> Option<&Path> {
        match self {
            Worktree::Empty => None,
            Worktree::Local { root, .. } | Worktree::Cache { root, .. } => Some(root),
        }
    }

    /// The repository behind this worktree, when there is one. A plain diff
    /// has none; a cache always has one; a checkout has one only when the
    /// input came with a platform.
    pub fn repo(&self) -> Option<&Repo> {
        match self {
            Worktree::Empty => None,
            Worktree::Local { repo, .. } => repo.as_ref(),
            Worktree::Cache { repo, .. } => Some(repo),
        }
    }

    /// Nothing on disk and nothing behind it: a plain diff, no platform.
    pub fn is_empty(&self) -> bool {
        matches!(self, Worktree::Empty)
    }

    /// The checkout the command line named: the whole project, already on disk.
    pub fn is_checkout(&self) -> bool {
        matches!(self, Worktree::Local { .. })
    }

    /// This run's own cache of fetched files, not a checkout.
    pub fn is_cache(&self) -> bool {
        matches!(self, Worktree::Cache { .. })
    }

    /// The listing that describes the project. A checkout is already the
    /// whole tree and free to walk; a cache holds only what has been fetched,
    /// so the repository listing is the one that describes the project.
    /// Empty has none.
    pub fn list_project(&self) -> Result<Option<Listing>, WorktreeError> {
        match self {
            Worktree::Empty => Ok(None),
            Worktree::Local { .. } => self.list_local("**").map(Some),
            Worktree::Cache { .. } => self.list_repo("**").map(Some),
        }
    }

    /// What `report.md` and `summary.json` say this run went without, in the
    /// order a reader wants them. Empty for a run that could look wherever it
    /// liked, which is the common case and prints nothing.
    ///
    /// The report has to carry this. A run whose worktree could answer nothing
    /// produces the same clean-looking report as one that read everything and
    /// found nothing — same empty finding list, same score, same "no findings"
    /// paragraph — and a person reading it has no other way to tell the two
    /// apart. The model knows; this is how the knowledge survives as far as
    /// the reader.
    ///
    /// Having nothing to read explains having no search and no whole project,
    /// so it is said on its own. Three lines that all mean "there was no
    /// code" bury the one that says why.
    pub fn went_without(&self) -> Vec<&'static str> {
        match self {
            Worktree::Empty => vec![NO_FILES_REPORTED],
            Worktree::Cache { repo, .. } => match repo.capabilities().is_empty() {
                true => vec![NO_WHOLE_TREE_REPORTED, NO_REPO_SEARCH_REPORTED],
                false => vec![NO_WHOLE_TREE_REPORTED],
            },
            // A checkout can search itself. Missing a platform search is
            // not a coverage hole here — local regex walks the tree. The
            // real hole is a cache with no search: the disk holds only
            // what was fetched, and the platform cannot fill the rest.
            Worktree::Local { .. } => Vec::new(),
        }
    }

    /// What is in the worktree directory right now, matching `glob`. On a
    /// cache that is what has been fetched so far and nothing else, which is
    /// a fact about this run rather than about the repository.
    pub fn list_local(&self, glob: &str) -> Result<Listing, WorktreeError> {
        let root = self.directory(glob)?;
        let matcher = Glob::new(glob)
            .map_err(|error| WorktreeError::InvalidGlob {
                pattern: glob.to_string(),
                reason: error.to_string(),
            })?
            .compile_matcher();
        let mut found = Vec::new();
        walk(root, root, &mut found);
        found.retain(|file| matcher.is_match(&file.path));
        found.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(Listing {
            files: found,
            complete: true,
        })
    }

    /// How big a local file is, and how many lines it holds when it is small
    /// enough to have been read. The size comes off the directory entry, so a
    /// file past the ceiling costs nothing to answer for.
    pub fn stat_local(&self, path: &str, max_bytes: u64) -> Result<Stat, WorktreeError> {
        let full = self.directory(path)?.join(path);
        let bytes = size_on_disk(&full, path)?;
        let lines = match bytes > max_bytes {
            true => None,
            false => Some(read_from_disk(&full, path)?.lines().count() as u32),
        };
        Ok(Stat { bytes, lines })
    }

    /// One local file, or the lines of it that were asked for.
    ///
    /// A miss is a miss: nothing is fetched to satisfy it. The ceiling is
    /// checked against the directory entry, so a file too big to answer with
    /// is never read — and a line range is no way around it, because the
    /// ceiling is a statement about the file rather than about the answer.
    pub fn read_local(
        &self,
        path: &str,
        lines: Option<LineRange>,
        max_bytes: u64,
    ) -> Result<String, WorktreeError> {
        let full = self.directory(path)?.join(path);
        let bytes = size_on_disk(&full, path)?;
        if bytes > max_bytes {
            return Err(WorktreeError::TooBig { bytes });
        }
        let text = read_from_disk(&full, path)?;
        Ok(match lines {
            Some(range) => range.slice(&text),
            None => text,
        })
    }

    /// Match `query` against every text file in the worktree directory.
    /// Answers with what it searched as well as what it found, because on a
    /// cache the corpus is the larger half of what an empty result means.
    pub fn search_local(&self, query: &str, glob: Option<&str>) -> Result<Scan, WorktreeError> {
        let root = self.directory(query)?.to_path_buf();
        let pattern = Regex::new(query).map_err(|error| WorktreeError::InvalidQuery {
            query: query.to_string(),
            reason: error.to_string(),
        })?;
        let mut scan = Scan::default();
        for file in self.list_local(glob.unwrap_or("**/*"))?.files {
            let Ok(bytes) = std::fs::read(root.join(&file.path)) else {
                continue;
            };
            if !is_text(&bytes) {
                scan.skipped += 1;
                continue;
            }
            scan.searched += 1;
            for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                if pattern.is_match(line) {
                    scan.hits.push(SearchHit {
                        path: file.path.clone(),
                        line: index as u32 + 1,
                        text: line.to_string(),
                    });
                }
            }
        }
        Ok(scan)
    }

    /// What the repository holds at the reviewed commit, which is not the
    /// same question as what is on disk.
    pub fn list_repo(&self, glob: &str) -> Result<Listing, WorktreeError> {
        self.repository(glob)?
            .source()
            .list_files(glob)
            .map_err(|error| WorktreeError::Unfetchable {
                path: glob.to_string(),
                reason: error.to_string(),
            })
    }

    /// Put a file on disk and say where it is and how big — never what it
    /// says, because reading is the other half.
    ///
    /// Already there is the answer, whatever shape this worktree has. Only a
    /// miss goes further, and only a cache can go anywhere with it: the
    /// checkout the command line named is by definition the whole tree at the
    /// reviewed commit, so a path missing from it is one the repository does
    /// not have, and nothing is ever written into it.
    ///
    /// The size is asked for before the body, so a file past the ceiling is
    /// refused without being downloaded.
    /// Body the repository already holds, if any. Never issues a request:
    /// `None` means this optimisation does not apply, not that the file is
    /// absent. The tool layer asks this after `PathPolicy`, then `warm`.
    pub fn cached_body(&self, path: &str) -> Option<String> {
        self.repo()?.cached_body(path)
    }

    /// Bank a body the caller already holds. Only a cache writes, and only
    /// when the path is not already on disk. Never asks the repository.
    pub fn warm(&self, path: &str, body: &str) -> Result<(), WorktreeError> {
        match self {
            Worktree::Cache { root, .. } => {
                let full = root.join(path);
                if full.is_file() {
                    return Ok(());
                }
                land(root, path, body)?;
                Ok(())
            }
            Worktree::Empty | Worktree::Local { .. } => Ok(()),
        }
    }

    pub fn fetch(&self, path: &str, max_bytes: u64) -> Result<Fetched, WorktreeError> {
        if let Some(root) = self.root() {
            let full = root.join(path);
            if full.is_file() {
                return Ok(Fetched {
                    path: path.to_string(),
                    bytes: size_on_disk(&full, path)?,
                });
            }
        }
        match self {
            // The one arm holding a root and a repository at once, and so the
            // one place in this module that writes. `Local` and `Empty`
            // cannot make the pair, which is why "reviewbot wrote into the
            // checkout" has nowhere to happen rather than being ruled out by
            // a branch.
            Worktree::Cache { root, repo } => {
                let source = repo.source();
                let unfetchable = |error: PlatformError| WorktreeError::Unfetchable {
                    path: path.to_string(),
                    reason: error.to_string(),
                };
                let bytes = source.size(path).map_err(unfetchable)?;
                if bytes > max_bytes {
                    return Err(WorktreeError::TooBig { bytes });
                }
                let body = source.read_file(path, None).map_err(unfetchable)?;
                land(root, path, &body)
            }
            Worktree::Local { .. } => Err(WorktreeError::Unfetchable {
                path: path.to_string(),
                reason: "the worktree is the checkout named on the command line, which is the \
                         whole file tree at the reviewed commit, so a path missing from it is \
                         one the repository does not have at that commit. A checkout is never \
                         written to"
                    .to_string(),
            }),
            Worktree::Empty => Err(WorktreeError::Unfetchable {
                path: path.to_string(),
                reason: "this run has no worktree and no repository behind it".to_string(),
            }),
        }
    }

    /// Ask the repository to search itself, with the engine named. Which
    /// engines it has is the platform's answer and the tool layer's to check
    /// before asking.
    pub fn search_repo(
        &self,
        kind: SearchKind,
        query: &str,
        glob: Option<&str>,
    ) -> Result<Vec<SearchHit>, WorktreeError> {
        self.repository(query)?
            .source()
            .search(kind, query, glob)
            .map_err(|error| WorktreeError::Unfetchable {
                path: query.to_string(),
                reason: error.to_string(),
            })
    }

    /// The worktree directory, for a call that has nowhere to look without
    /// one. `what` is the path or pattern that was asked about, so the
    /// refusal names it.
    fn directory(&self, what: &str) -> Result<&Path, WorktreeError> {
        self.root().ok_or_else(|| WorktreeError::Unreadable {
            path: what.to_string(),
            reason: "this run has no worktree directory: the input was a plain diff with no \
                     checkout and no platform behind it"
                .to_string(),
        })
    }

    fn repository(&self, what: &str) -> Result<&Repo, WorktreeError> {
        self.repo().ok_or_else(|| WorktreeError::Unfetchable {
            path: what.to_string(),
            reason: "this run has no repository behind its worktree".to_string(),
        })
    }
}

/// The three sentences a report has for a bounded run.
const NO_FILES_REPORTED: &str = "could not read any file: it saw the diff and nothing else";
const NO_WHOLE_TREE_REPORTED: &str =
    "had no whole checkout, so a checker that needs one could not run";
const NO_REPO_SEARCH_REPORTED: &str = "could not search the repository";

/// Every file under `directory`, named relative to `root`, with the size the
/// walk already paid for. `.git` is not part of the project.
fn walk(root: &Path, directory: &Path, found: &mut Vec<File>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative == ".git" {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        match metadata.is_dir() {
            true => walk(root, &path, found),
            false => found.push(File {
                path: relative,
                bytes: Some(metadata.len()),
            }),
        }
    }
}

/// The one predicate for "is this text", shared by the fetch that writes and
/// the search that reads. A NUL byte near the front is the whole of it.
fn is_text(bytes: &[u8]) -> bool {
    !bytes
        .iter()
        .take(TEXT_PROBE_BYTES)
        .any(|byte| *byte == b'\0')
}

/// The one place a fetched or warmed body lands on disk. Called only from
/// the `Cache` arm of `fetch` and `warm`.
fn land(root: &Path, path: &str, body: &str) -> Result<Fetched, WorktreeError> {
    if !is_text(body.as_bytes()) {
        return Err(WorktreeError::Binary {
            path: path.to_string(),
        });
    }
    let unwritable = |error: std::io::Error| WorktreeError::Unreadable {
        path: path.to_string(),
        reason: error.to_string(),
    };
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(unwritable)?;
    }
    std::fs::write(&full, body).map_err(unwritable)?;
    // No execute bit, whatever the umask says: a fetched file is material
    // to read, and nothing in a run has any business running it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o644))
            .map_err(unwritable)?;
    }
    Ok(Fetched {
        path: path.to_string(),
        bytes: body.len() as u64,
    })
}

fn size_on_disk(full: &Path, path: &str) -> Result<u64, WorktreeError> {
    std::fs::metadata(full)
        .map(|metadata| metadata.len())
        .map_err(|error| WorktreeError::Unreadable {
            path: path.to_string(),
            reason: error.to_string(),
        })
}

fn read_from_disk(full: &Path, path: &str) -> Result<String, WorktreeError> {
    std::fs::read_to_string(full).map_err(|error| WorktreeError::Unreadable {
        path: path.to_string(),
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::platform::{Capabilities, RepoSource};

    /// A repository that counts what it was asked for, so a test can tell an
    /// answer that came off the local disk from one that cost a call. The
    /// fake sits at the extension point — `RepoSource` is what a platform
    /// implements — and the worktree over it is the real one.
    struct CountingRepository {
        bodies: BTreeMap<String, String>,
        reads: AtomicUsize,
        sizes: AtomicUsize,
        searches: AtomicUsize,
    }

    impl CountingRepository {
        fn holding(files: &[(&str, &str)]) -> Arc<Self> {
            Arc::new(Self {
                bodies: files
                    .iter()
                    .map(|(path, body)| (path.to_string(), body.to_string()))
                    .collect(),
                reads: AtomicUsize::new(0),
                sizes: AtomicUsize::new(0),
                searches: AtomicUsize::new(0),
            })
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }

        fn sizes(&self) -> usize {
            self.sizes.load(Ordering::SeqCst)
        }

        fn missing(&self, path: &str) -> PlatformError {
            PlatformError::Request {
                operation: "reading a repository file",
                host: "test".to_string(),
                reason: format!("{path} is not in this repository"),
            }
        }
    }

    impl RepoSource for CountingRepository {
        fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
            Ok(Listing {
                files: self
                    .bodies
                    .iter()
                    .map(|(path, body)| File {
                        path: path.clone(),
                        bytes: Some(body.len() as u64),
                    })
                    .collect(),
                complete: true,
            })
        }

        fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, PlatformError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let body = self
                .bodies
                .get(path)
                .ok_or_else(|| self.missing(path))?
                .clone();
            Ok(match lines {
                Some(range) => range.slice(&body),
                None => body,
            })
        }

        fn size(&self, path: &str) -> Result<u64, PlatformError> {
            self.sizes.fetch_add(1, Ordering::SeqCst);
            self.bodies
                .get(path)
                .map(|body| body.len() as u64)
                .ok_or_else(|| self.missing(path))
        }

        fn search(
            &self,
            _kind: SearchKind,
            _query: &str,
            _glob: Option<&str>,
        ) -> Result<Vec<SearchHit>, PlatformError> {
            self.searches.fetch_add(1, Ordering::SeqCst);
            Ok(vec![SearchHit {
                path: "src/lex.c".to_string(),
                line: 3,
                text: "int main(void)".to_string(),
            }])
        }
    }

    /// The ceiling most of these tests are not about.
    const ROOM: u64 = 262_144;

    fn cache(repository: &Arc<CountingRepository>) -> (tempfile::TempDir, Worktree) {
        let run_dir = tempfile::tempdir().expect("temp dir");
        let repo = Repo::new(
            Arc::clone(repository) as Arc<dyn RepoSource>,
            Capabilities::all(),
        );
        let worktree = Worktree::open(None, Some(repo), run_dir.path()).expect("a cache");
        (run_dir, worktree)
    }

    fn checkout() -> (tempfile::TempDir, Worktree) {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(directory.path().join("src")).expect("src");
        std::fs::write(
            directory.path().join("src/parse.c"),
            "int main(void)\n{\n}\n",
        )
        .expect("source");
        std::fs::write(directory.path().join("README.md"), "# demo\n").expect("readme");
        let worktree = Worktree::open(Some(directory.path().to_path_buf()), None, directory.path())
            .expect("a checkout");
        (directory, worktree)
    }

    #[test]
    fn a_listing_selects_by_glob_and_carries_the_size_the_walk_already_paid_for() {
        let (_directory, worktree) = checkout();
        let listing = worktree.list_local("**/*.c").expect("listed");
        assert_eq!(listing.paths().collect::<Vec<_>>(), vec!["src/parse.c"]);
        assert_eq!(listing.files[0].bytes, Some(19));
        assert!(listing.complete);
    }

    #[test]
    fn a_read_can_be_narrowed_to_a_range() {
        let (_directory, worktree) = checkout();
        assert_eq!(
            worktree
                .read_local("src/parse.c", Some(LineRange { first: 2, last: 3 }), ROOM)
                .expect("read"),
            "{\n}"
        );
    }

    /// The core of the change: fetching and reading are two actions. A cache
    /// answers about what is in it, and what is not in it is not quietly
    /// fetched to make the answer look better.
    #[test]
    fn a_cache_miss_is_a_miss_rather_than_a_fetch() {
        let repository = CountingRepository::holding(&[("src/parse.h", "struct token;\n")]);
        let (_run, worktree) = cache(&repository);

        let error = worktree
            .read_local("src/parse.h", None, ROOM)
            .expect_err("nothing is on disk yet");
        assert!(matches!(error, WorktreeError::Unreadable { .. }), "{error}");
        assert_eq!(repository.reads(), 0, "a miss must not fetch");

        let fetched = worktree.fetch("src/parse.h", ROOM).expect("fetched");
        assert_eq!(fetched.path, "src/parse.h");
        assert_eq!(fetched.bytes, 14);
        assert_eq!(repository.reads(), 1);
        assert_eq!(
            worktree
                .read_local("src/parse.h", None, ROOM)
                .expect("on disk now"),
            "struct token;\n"
        );
        assert_eq!(repository.reads(), 1, "the read came off the disk");

        worktree.fetch("src/parse.h", ROOM).expect("already there");
        assert_eq!(repository.reads(), 1, "a hit is the answer");
    }

    /// The hole this closes: the old fetch downloaded the whole file and only
    /// then measured it, so a 50 MB generated file was pulled over the wire to
    /// be refused. The size is asked for first, and the body never is.
    #[test]
    fn a_file_over_the_ceiling_is_refused_before_it_is_downloaded() {
        let repository = CountingRepository::holding(&[("src/generated.c", &"x".repeat(1_000))]);
        let (_run, worktree) = cache(&repository);

        let error = worktree
            .fetch("src/generated.c", 100)
            .expect_err("over the ceiling");
        assert!(
            matches!(error, WorktreeError::TooBig { bytes: 1_000 }),
            "{error}"
        );
        assert_eq!(repository.reads(), 0, "the body was never asked for");
        assert_eq!(repository.sizes(), 1);
        assert!(
            !worktree
                .root()
                .expect("a cache has a root")
                .join("src/generated.c")
                .exists()
        );
    }

    #[test]
    fn a_local_file_over_the_ceiling_is_refused_without_being_read() {
        let (_directory, worktree) = checkout();
        let error = worktree
            .read_local("src/parse.c", None, 4)
            .expect_err("over the ceiling");
        assert!(
            matches!(error, WorktreeError::TooBig { bytes: 19 }),
            "{error}"
        );
    }

    /// A size still has to be answerable for a file too big to read, because
    /// that is half of what asking for it is for. The line count is what
    /// cannot be answered, and it says so rather than guessing.
    #[test]
    fn a_stat_past_the_ceiling_has_the_bytes_and_no_line_count() {
        let (_directory, worktree) = checkout();

        let small = worktree.stat_local("src/parse.c", ROOM).expect("stat");
        assert_eq!(
            small,
            Stat {
                bytes: 19,
                lines: Some(3)
            }
        );

        let large = worktree.stat_local("src/parse.c", 4).expect("stat");
        assert_eq!(large.bytes, 19);
        assert_eq!(large.lines, None, "the file was never read");
    }

    /// Searching a binary for a pattern is not a search anybody wants: it is
    /// slow, and the hits it invents push the real ones out of the answer. The
    /// count is reported so "why did nothing match in that .png" is not a
    /// mystery.
    #[test]
    fn a_local_search_passes_over_binaries_and_says_how_many() {
        let (directory, worktree) = checkout();
        std::fs::write(
            directory.path().join("src/logo.png"),
            b"\x89PNG\x00int main(void)\n",
        )
        .expect("binary");

        let scan = worktree
            .search_local(r"int\s+main", None)
            .expect("searched");
        assert_eq!(scan.hits.len(), 1);
        assert_eq!(scan.hits[0].path, "src/parse.c");
        assert_eq!(scan.hits[0].line, 1);
        assert_eq!(scan.skipped, 1, "the png was passed over");
        assert_eq!(scan.searched, 2, "the source and the readme");
    }

    /// The guarantee the enum exists for: the pair of a root and a repository
    /// is what a write needs, and a checkout never has it. A path it does not
    /// hold is one the repository does not have at this commit, and that is
    /// what it says.
    #[test]
    fn a_checkout_refuses_a_fetch_it_cannot_answer_rather_than_writing() {
        let (directory, worktree) = checkout();

        let error = worktree
            .fetch("src/missing.c", ROOM)
            .expect_err("not in the checkout");
        assert!(
            matches!(&error, WorktreeError::Unfetchable { reason, .. }
                if reason.contains("whole file tree") && reason.contains("never written to")),
            "{error}"
        );
        assert!(!directory.path().join("src/missing.c").exists());

        let held = worktree.fetch("src/parse.c", ROOM).expect("already there");
        assert_eq!(held.bytes, 19);
    }

    #[cfg(unix)]
    #[test]
    fn a_fetched_file_carries_no_execute_bit() {
        use std::os::unix::fs::PermissionsExt;

        let repository = CountingRepository::holding(&[("src/build.sh", "#!/bin/sh\necho hi\n")]);
        let (_run, worktree) = cache(&repository);
        worktree.fetch("src/build.sh", ROOM).expect("fetched");

        let mode = std::fs::metadata(
            worktree
                .root()
                .expect("a cache has a root")
                .join("src/build.sh"),
        )
        .expect("stat")
        .permissions()
        .mode();
        assert_eq!(mode & 0o111, 0, "mode {mode:o}");
    }

    #[test]
    fn binary_content_is_refused_rather_than_written() {
        let repository = CountingRepository::holding(&[("assets/logo.png", "PK\u{0}\u{0}payload")]);
        let (_run, worktree) = cache(&repository);

        let error = worktree
            .fetch("assets/logo.png", ROOM)
            .expect_err("not text");
        assert!(matches!(error, WorktreeError::Binary { .. }), "{error}");
        assert!(
            !worktree
                .root()
                .expect("a cache has a root")
                .join("assets/logo.png")
                .exists()
        );
    }

    /// A cache holds whatever has been asked for so far. Answering a listing
    /// or a search out of that would say "this repository has two files", and
    /// a miss would read as "it is not there".
    #[test]
    fn the_repository_half_answers_about_the_repository_not_about_the_disk() {
        let repository =
            CountingRepository::holding(&[("src/parse.c", "int main(void)\n"), ("src/lex.c", "")]);
        let (_run, worktree) = cache(&repository);
        worktree.fetch("src/parse.c", ROOM).expect("fetched");

        let listing = worktree.list_repo("**/*.c").expect("listed");
        assert_eq!(listing.files.len(), 2, "not just the one fetched file");

        let hits = worktree
            .search_repo(SearchKind::Regex, "main", None)
            .expect("searched");
        assert_eq!(hits[0].path, "src/lex.c", "a file that is not on disk");

        let local = worktree.search_local("main", None).expect("searched");
        assert_eq!(local.searched, 1, "the local half sees one file");
    }

    /// A checkout with nothing behind it still reads, and still says so when
    /// asked something only a repository could answer.
    #[test]
    fn a_worktree_with_no_repository_refuses_the_repository_half() {
        let (_directory, worktree) = checkout();
        let error = worktree.list_repo("**/*.c").expect_err("no repository");
        assert!(
            matches!(&error, WorktreeError::Unfetchable { reason, .. }
                if reason.contains("no repository")),
            "{error}"
        );
    }

    /// Nothing to read, and nowhere for anything to land.
    #[test]
    fn an_empty_worktree_has_no_directory_and_answers_nothing() {
        let worktree = Worktree::Empty;
        assert!(worktree.root().is_none());
        assert!(worktree.list_local("**/*").is_err());
        assert!(worktree.read_local("src/parse.c", None, ROOM).is_err());
        assert!(worktree.fetch("src/parse.c", ROOM).is_err());
    }

    #[test]
    fn a_cache_opens_under_the_run_directory() {
        let repository = CountingRepository::holding(&[]);
        let (run, worktree) = cache(&repository);
        assert_eq!(
            worktree.root().expect("a cache has a root"),
            run.path().join(DIRECTORY)
        );
        assert!(run.path().join(DIRECTORY).is_dir());
    }

    /// Read before the run directory exists, so the run id can be built out
    /// of it. One level of ref is enough for the checkouts CI produces.
    #[test]
    fn the_checkout_head_is_read_off_the_git_directory() {
        let (directory, _worktree) = checkout();
        let git = directory.path().join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).expect("refs");
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        std::fs::write(git.join("refs/heads/main"), "4b1e0d2c\n").expect("ref");
        assert_eq!(
            Checkout::open(directory.path())
                .expect("a directory")
                .head()
                .expect("a head"),
            "4b1e0d2c"
        );

        std::fs::write(git.join("HEAD"), "4b1e0d2c\n").expect("detached HEAD");
        assert_eq!(
            Checkout::open(directory.path())
                .expect("a directory")
                .head()
                .expect("a head"),
            "4b1e0d2c"
        );

        let elsewhere = tempfile::tempdir().expect("temp dir");
        assert!(matches!(
            Checkout::open(elsewhere.path())
                .expect("a directory")
                .head(),
            Err(WorktreeError::NoHead { .. })
        ));
    }

    #[test]
    fn a_checkout_that_is_not_a_directory_is_refused_when_it_is_opened() {
        let root = tempfile::tempdir().expect("temp dir");
        let file = root.path().join("change.diff");
        std::fs::write(&file, "--- a\n").expect("write");
        assert!(matches!(
            Worktree::open(Some(file), None, root.path()),
            Err(WorktreeError::NotADirectory { .. })
        ));
    }

    /// The shape answers for itself. Callers ask these rather than matching
    /// the variants to reconstruct what the type already knows.
    #[test]
    fn the_shape_answers_for_itself() {
        assert!(Worktree::Empty.is_empty());
        assert!(Worktree::Empty.repo().is_none());
        assert!(Worktree::Empty.list_project().expect("none").is_none());
        assert_eq!(
            Worktree::Empty.went_without(),
            vec!["could not read any file: it saw the diff and nothing else"]
        );

        let (_directory, checkout) = checkout();
        assert!(checkout.is_checkout());
        assert!(checkout.repo().is_none());
        assert!(
            checkout.went_without().is_empty(),
            "local search covers the checkout: {:?}",
            checkout.went_without()
        );
        let listing = checkout
            .list_project()
            .expect("the checkout")
            .expect("files");
        assert!(listing.files.iter().any(|file| file.path == "src/parse.c"));

        let repository = CountingRepository::holding(&[]);
        let (_run, cached) = cache(&repository);
        assert!(cached.is_cache());
        assert!(cached.repo().is_some());
        assert_eq!(
            cached.went_without(),
            vec!["had no whole checkout, so a checker that needs one could not run"]
        );
        let project = cached.list_project().expect("the repository");
        assert!(
            project.is_some(),
            "a cache describes the project via the repo"
        );
    }
}
