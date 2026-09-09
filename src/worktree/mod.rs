//! This run's worktree: the one place code comes from, and always a local
//! directory.
//!
//! There are two shapes of it, not two modes. A checkout the command line
//! named is the whole project on disk, and reviewbot never writes to it: the
//! type has no write method, so "the checkout stays untouched" has nowhere to
//! happen. Without one, the run opens an empty directory of its own under the
//! run directory and fills it from the platform API as files are asked for.
//! That directory is not a checkout and does not pretend to be one: it says
//! so through `Reach`, which is what the tool descriptions are written from.
//!
//! The platform API is therefore an attribute of the worktree rather than a
//! second source with its own tools. It is the one dependency allowed inside
//! the adapter layer, and repository content vocabulary — `LineRange`,
//! `Listing`, `SearchHit` — is defined once, over in `platform::source`, and
//! spoken here rather than declared a second time.
//!
//! Not an extension point: there is only one local filesystem.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use globset::Glob;
use regex::Regex;

use crate::platform::{Capabilities, RepoSource};

pub use crate::platform::{LineRange, Listing, SearchHit};

/// What the run's own worktree is called inside the run directory. Not a
/// scratch area: it is this run's worktree, and it is deleted with the run.
pub const DIRECTORY: &str = "worktree";

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
}

/// What this run's worktree can answer, and with how much authority.
///
/// It exists because the tool names no longer say: one group of tools reads
/// the worktree whatever shape it is in, and a description is the only place
/// left that can tell a whole checkout from a directory holding the four
/// files somebody happened to ask for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reach {
    pub content: Content,
    pub search: Search,
}

impl Reach {
    /// Nothing to read, and so no content tool and no checker: a diff with no
    /// platform behind it leaves the worktree empty for the whole run.
    pub fn has_content(self) -> bool {
        self.content != Content::Empty
    }

    /// Whether a checker that needs the whole project has one.
    pub fn is_checkout(self) -> bool {
        self.content == Content::Checkout
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Content {
    /// The checkout the command line named, standing on the reviewed commit.
    Checkout,
    /// This run's own directory, filled from the platform API on demand.
    Fetched,
    /// A directory with nothing in it and nothing behind it.
    Empty,
}

/// How, or whether, a search can be answered. A keyword index and a regular
/// expression search are not the same instrument, and the difference decides
/// what an empty result means.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Search {
    Regex,
    Keyword,
    /// Not answerable, so no search tool is registered rather than one that
    /// answers nothing.
    Unavailable,
}

impl Search {
    fn from(capabilities: Capabilities) -> Self {
        match (capabilities.code_search, capabilities.regex_search) {
            (true, true) => Search::Regex,
            (true, false) => Search::Keyword,
            (false, _) => Search::Unavailable,
        }
    }
}

/// Read methods only, plus `supply`, which puts a file on disk without
/// handing its content back — an external command opens the file itself.
pub trait WorktreeSource: Send + Sync {
    fn root(&self) -> &Path;

    fn reach(&self) -> Reach;

    /// Take the directory this worktree will use, now that the run directory
    /// exists. A checkout was named on the command line and is already open,
    /// so it does nothing; a run's own worktree creates itself here. Called
    /// once, before any stage runs.
    fn open_in(&self, run_dir: &Path) -> Result<(), WorktreeError>;

    /// The commit this worktree stands on, when it stands on one. `None` for
    /// a directory this run made: it holds files, not a checkout, and an
    /// invented sha would end up in the run id.
    fn head_sha(&self) -> Result<Option<String>, WorktreeError>;

    /// Make sure `path` is on disk. A checkout already has everything; a
    /// fetched worktree pulls it from the platform at the reviewed commit.
    fn supply(&self, path: &str) -> Result<(), WorktreeError>;

    fn list_files(&self, glob: &str) -> Result<Listing, WorktreeError>;

    fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, WorktreeError>;

    fn search(&self, query: &str, glob: Option<&str>) -> Result<Vec<SearchHit>, WorktreeError>;
}

/// The checkout `--worktree` named. Read only, and complete: it is the whole
/// project standing on the commit under review, which `input` checks before
/// anything is read.
pub struct Checkout {
    root: PathBuf,
}

impl Checkout {
    pub fn open(root: PathBuf) -> Result<Self, WorktreeError> {
        if !root.is_dir() {
            return Err(WorktreeError::NotADirectory { path: root });
        }
        Ok(Self { root })
    }

    /// Fail early when the checkout is not the commit under review.
    pub fn require_head(&self, expected: &str) -> Result<(), WorktreeError> {
        let actual = self.head_sha()?.ok_or_else(|| WorktreeError::NoHead {
            path: self.root.clone(),
        })?;
        if actual == expected {
            Ok(())
        } else {
            Err(WorktreeError::HeadMismatch {
                actual,
                expected: expected.to_string(),
            })
        }
    }

    fn walk(&self, directory: &Path, found: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(&self.root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative == ".git" {
                continue;
            }
            if path.is_dir() {
                self.walk(&path, found);
            } else {
                found.push(relative);
            }
        }
    }
}

impl WorktreeSource for Checkout {
    fn root(&self) -> &Path {
        &self.root
    }

    fn reach(&self) -> Reach {
        Reach {
            content: Content::Checkout,
            search: Search::Regex,
        }
    }

    /// Nothing to open: this worktree existed before the run did, and the run
    /// directory is not where it lives.
    fn open_in(&self, _run_dir: &Path) -> Result<(), WorktreeError> {
        Ok(())
    }

    /// Read `.git/HEAD` and follow one level of ref. Enough to compare
    /// against `head_sha` without shelling out to git.
    fn head_sha(&self) -> Result<Option<String>, WorktreeError> {
        let head = self.root.join(".git").join("HEAD");
        let text = std::fs::read_to_string(&head).map_err(|_| WorktreeError::NoHead {
            path: self.root.clone(),
        })?;
        let text = text.trim();
        let Some(reference) = text.strip_prefix("ref: ") else {
            return Ok(Some(text.to_string()));
        };
        let target = self.root.join(".git").join(reference);
        std::fs::read_to_string(&target)
            .map(|sha| Some(sha.trim().to_string()))
            .map_err(|_| WorktreeError::NoHead {
                path: self.root.clone(),
            })
    }

    /// Nothing to do, and nothing that could write here even if there were.
    fn supply(&self, _path: &str) -> Result<(), WorktreeError> {
        Ok(())
    }

    fn list_files(&self, glob: &str) -> Result<Listing, WorktreeError> {
        let matcher = Glob::new(glob)
            .map_err(|error| WorktreeError::InvalidGlob {
                pattern: glob.to_string(),
                reason: error.to_string(),
            })?
            .compile_matcher();
        let mut found = Vec::new();
        self.walk(&self.root.clone(), &mut found);
        found.retain(|path| matcher.is_match(path));
        found.sort();
        Ok(Listing {
            paths: found,
            complete: true,
        })
    }

    fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, WorktreeError> {
        let text = read_from_disk(&self.root, path)?;
        Ok(slice(&text, lines))
    }

    fn search(&self, query: &str, glob: Option<&str>) -> Result<Vec<SearchHit>, WorktreeError> {
        let pattern = Regex::new(query).map_err(|error| WorktreeError::InvalidQuery {
            query: query.to_string(),
            reason: error.to_string(),
        })?;
        let mut hits = Vec::new();
        for path in self.list_files(glob.unwrap_or("**/*"))?.paths {
            let Ok(text) = std::fs::read_to_string(self.root.join(&path)) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if pattern.is_match(line) {
                    hits.push(SearchHit {
                        path: path.clone(),
                        line: index as u32 + 1,
                        text: line.to_string(),
                    });
                }
            }
        }
        Ok(hits)
    }
}

/// The worktree a run opens for itself when no checkout was named: an empty
/// directory under the run directory, filled from the platform API one file
/// at a time.
///
/// Listing and searching are forwarded to the platform and are never
/// answered from what happens to be on disk. Answering locally would turn
/// "what has been fetched so far" into "what this repository has", and one
/// miss would read as "it does not exist".
pub struct FetchedWorktree {
    /// Bound when the run directory exists, which is later than the tools
    /// are registered: what may be registered follows from the platform, not
    /// from where the files will land.
    root: OnceLock<PathBuf>,
    repository: Option<Arc<dyn RepoSource>>,
    reach: Reach,
}

impl FetchedWorktree {
    pub fn new(repository: Option<Arc<dyn RepoSource>>, capabilities: Capabilities) -> Self {
        let reach = match repository.is_some() {
            true => Reach {
                content: Content::Fetched,
                search: Search::from(capabilities),
            },
            // Nothing behind the directory, so nothing can come out of it —
            // not even a search the platform would have answered.
            false => Reach {
                content: Content::Empty,
                search: Search::Unavailable,
            },
        };
        Self {
            root: OnceLock::new(),
            repository,
            reach,
        }
    }

    fn repository(&self, path: &str) -> Result<&Arc<dyn RepoSource>, WorktreeError> {
        self.repository
            .as_ref()
            .ok_or_else(|| WorktreeError::Unreadable {
                path: path.to_string(),
                reason: "this run has no repository behind its worktree, so nothing can be \
                         fetched into it"
                    .to_string(),
            })
    }
}

impl WorktreeSource for FetchedWorktree {
    fn root(&self) -> &Path {
        self.root
            .get()
            .map(PathBuf::as_path)
            .expect("the worktree is opened in the run directory before any stage runs")
    }

    fn reach(&self) -> Reach {
        self.reach
    }

    /// Create `<run_dir>/worktree` and use it for the rest of the run. The
    /// directory goes when the run goes: it is never reused across runs,
    /// because reuse needs rules for archiving and invalidating by commit,
    /// and reuse without them is stale data.
    fn open_in(&self, run_dir: &Path) -> Result<(), WorktreeError> {
        let root = run_dir.join(DIRECTORY);
        std::fs::create_dir_all(&root).map_err(|error| WorktreeError::Unopenable {
            path: root.clone(),
            reason: error.to_string(),
        })?;
        let _ = self.root.set(root);
        Ok(())
    }

    /// A directory of fetched files stands on no commit of its own. The
    /// reviewed commit is the platform's, and it is already recorded there.
    fn head_sha(&self) -> Result<Option<String>, WorktreeError> {
        Ok(None)
    }

    /// The whole of the fetch: land the file on disk, once, without an
    /// execute bit and never as bytes that are not text. There is no size
    /// ceiling here — this layer answers what may be written, and how much of
    /// a file fits in one answer is the reading tool's question.
    fn supply(&self, path: &str) -> Result<(), WorktreeError> {
        let full = self.root().join(path);
        if full.is_file() {
            return Ok(());
        }
        let body = self
            .repository(path)?
            .read_file(path, None)
            .map_err(|error| WorktreeError::Unfetchable {
                path: path.to_string(),
                reason: error.to_string(),
            })?;
        if body.contains('\0') {
            return Err(WorktreeError::Binary {
                path: path.to_string(),
            });
        }
        write_text(&full, path, &body)
    }

    fn list_files(&self, glob: &str) -> Result<Listing, WorktreeError> {
        self.repository(glob)?
            .list_files(glob)
            .map_err(|error| WorktreeError::Unfetchable {
                path: glob.to_string(),
                reason: error.to_string(),
            })
    }

    fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, WorktreeError> {
        self.supply(path)?;
        let text = read_from_disk(self.root(), path)?;
        Ok(slice(&text, lines))
    }

    fn search(&self, query: &str, glob: Option<&str>) -> Result<Vec<SearchHit>, WorktreeError> {
        self.repository(query)?
            .search(query, glob)
            .map_err(|error| WorktreeError::Unfetchable {
                path: query.to_string(),
                reason: error.to_string(),
            })
    }
}

fn read_from_disk(root: &Path, path: &str) -> Result<String, WorktreeError> {
    std::fs::read_to_string(root.join(path)).map_err(|error| WorktreeError::Unreadable {
        path: path.to_string(),
        reason: error.to_string(),
    })
}

/// Written with no execute bit, whatever the umask says: a fetched file is
/// material to read, and nothing in a run has any business running it.
fn write_text(full: &Path, path: &str, body: &str) -> Result<(), WorktreeError> {
    let unwritable = |error: std::io::Error| WorktreeError::Unreadable {
        path: path.to_string(),
        reason: error.to_string(),
    };
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(unwritable)?;
    }
    std::fs::write(full, body).map_err(unwritable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(full, std::fs::Permissions::from_mode(0o644))
            .map_err(unwritable)?;
    }
    Ok(())
}

fn slice(text: &str, lines: Option<LineRange>) -> String {
    match lines {
        Some(range) => range.slice(text),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::platform::PlatformError;

    fn checkout() -> (tempfile::TempDir, Checkout) {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        std::fs::write(
            directory.path().join("src/parse.c"),
            "int main(void)\n{\n}\n",
        )
        .unwrap();
        std::fs::write(directory.path().join("README.md"), "# demo\n").unwrap();
        let worktree = Checkout::open(directory.path().to_path_buf()).unwrap();
        (directory, worktree)
    }

    /// Counts what it was asked for, so a test can tell "answered from the
    /// platform" from "answered from whatever was lying on disk".
    struct CountingRepository {
        reads: AtomicUsize,
        listings: AtomicUsize,
        searches: AtomicUsize,
        body: String,
    }

    impl CountingRepository {
        fn holding(body: &str) -> Arc<Self> {
            Arc::new(Self {
                reads: AtomicUsize::new(0),
                listings: AtomicUsize::new(0),
                searches: AtomicUsize::new(0),
                body: body.to_string(),
            })
        }
    }

    impl RepoSource for CountingRepository {
        fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
            self.listings.fetch_add(1, Ordering::SeqCst);
            Ok(Listing {
                paths: vec!["src/parse.c".to_string(), "src/lex.c".to_string()],
                complete: true,
            })
        }

        fn read_file(
            &self,
            _path: &str,
            _lines: Option<LineRange>,
        ) -> Result<String, PlatformError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.body.clone())
        }

        fn search(
            &self,
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

    fn fetched(repository: Arc<CountingRepository>) -> (tempfile::TempDir, FetchedWorktree) {
        let run_dir = tempfile::tempdir().expect("temp dir");
        let worktree = FetchedWorktree::new(
            Some(repository as Arc<dyn RepoSource>),
            Capabilities {
                code_search: true,
                regex_search: true,
            },
        );
        worktree.open_in(run_dir.path()).expect("opened");
        (run_dir, worktree)
    }

    #[test]
    fn globs_select_paths_and_reads_can_be_narrowed_to_a_range() {
        let (_directory, worktree) = checkout();
        assert_eq!(
            worktree.list_files("**/*.c").unwrap().paths,
            vec!["src/parse.c".to_string()]
        );
        assert_eq!(
            worktree
                .read_file("src/parse.c", Some(LineRange { first: 2, last: 3 }))
                .unwrap(),
            "{\n}"
        );
    }

    #[test]
    fn search_reports_the_matching_line_numbers() {
        let (_directory, worktree) = checkout();
        let hits = worktree.search(r"int\s+main", None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/parse.c");
        assert_eq!(hits[0].line, 1);
    }

    /// A checkout is the whole project and answers about itself; a fetched
    /// worktree is neither, and both have to say so, because the tools no
    /// longer differ by name.
    #[test]
    fn the_two_shapes_report_different_reach() {
        let (_directory, checkout) = checkout();
        assert!(checkout.reach().is_checkout());
        assert_eq!(checkout.reach().search, Search::Regex);

        let (_run, worktree) = fetched(CountingRepository::holding("int main(void)\n"));
        assert!(worktree.reach().has_content());
        assert!(
            !worktree.reach().is_checkout(),
            "four fetched files are not a project"
        );

        let empty = FetchedWorktree::new(None, Capabilities::default());
        assert!(!empty.reach().has_content());
        assert_eq!(empty.reach().search, Search::Unavailable);
    }

    /// The point of the merge: a file read through the platform lands on disk,
    /// so an external checker can open it. Without that, a checker could never
    /// run on anything but a checkout.
    #[test]
    fn a_fetched_file_lands_on_disk_once_and_is_read_from_there_after() {
        let repository = CountingRepository::holding("struct token { int id; };\n");
        let (_run, worktree) = fetched(Arc::clone(&repository));

        let body = worktree.read_file("src/parse.h", None).expect("fetched");
        assert_eq!(body, "struct token { int id; };\n");
        let landed = worktree.root().join("src/parse.h");
        assert!(landed.is_file(), "the fetch wrote it down");
        assert_eq!(repository.reads.load(Ordering::SeqCst), 1);

        worktree.read_file("src/parse.h", None).expect("on disk");
        assert_eq!(
            repository.reads.load(Ordering::SeqCst),
            1,
            "the second read came off the disk"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_fetched_file_carries_no_execute_bit() {
        use std::os::unix::fs::PermissionsExt;

        let (_run, worktree) = fetched(CountingRepository::holding("#!/bin/sh\necho hi\n"));
        worktree.supply("src/build.sh").expect("fetched");
        let mode = std::fs::metadata(worktree.root().join("src/build.sh"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0, "mode {mode:o}");
    }

    #[test]
    fn binary_content_is_refused_rather_than_written() {
        let (_run, worktree) = fetched(CountingRepository::holding("PK\u{0}\u{0}payload"));
        let error = worktree.supply("assets/logo.png").expect_err("not text");
        assert!(matches!(error, WorktreeError::Binary { .. }), "{error}");
        assert!(!worktree.root().join("assets/logo.png").exists());
    }

    /// A fetched worktree holds whatever was asked for so far. Answering a
    /// listing or a search from that would say "this repository has two
    /// files", and a miss would read as "it is not there".
    #[test]
    fn listing_and_searching_a_fetched_worktree_go_to_the_platform() {
        let repository = CountingRepository::holding("int main(void)\n");
        let (_run, worktree) = fetched(Arc::clone(&repository));
        worktree.read_file("src/parse.c", None).expect("fetched");

        let listing = worktree.list_files("**/*.c").expect("listed");
        assert_eq!(repository.listings.load(Ordering::SeqCst), 1);
        assert_eq!(listing.paths.len(), 2, "not just the one fetched file");

        let hits = worktree.search("main", None).expect("searched");
        assert_eq!(repository.searches.load(Ordering::SeqCst), 1);
        assert_eq!(hits[0].path, "src/lex.c", "a file that is not on disk");
    }

    /// A directory this run made stands on no commit, and the run id must not
    /// be built out of an invented one.
    #[test]
    fn a_fetched_worktree_reports_no_head_of_its_own() {
        let (_run, worktree) = fetched(CountingRepository::holding(""));
        assert_eq!(worktree.head_sha().expect("no error"), None);
    }
}
