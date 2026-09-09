//! The local checkout, when `--worktree` names one. Symmetrical with
//! `platform` as the other source of repository content, and read only: the
//! trait has no write method, so "reviewbot never writes the worktree" has
//! nowhere to happen.
//!
//! Not an extension point: there is only one local filesystem.

use std::path::{Path, PathBuf};

use globset::Glob;
use regex::Regex;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("{path}: {reason}")]
    Unreadable { path: String, reason: String },
    #[error("{path} is not a directory")]
    NotADirectory { path: PathBuf },
    #[error("invalid glob {pattern:?}: {reason}")]
    InvalidGlob { pattern: String, reason: String },
    #[error("invalid search pattern {query:?}: {reason}")]
    InvalidQuery { query: String, reason: String },
    #[error("cannot determine the worktree HEAD under {path}")]
    NoHead { path: PathBuf },
    #[error("worktree HEAD {actual} does not match the change head {expected}")]
    HeadMismatch { actual: String, expected: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineRange {
    pub first: u32,
    pub last: u32,
}

impl LineRange {
    /// The lines this range names, 1 based and inclusive. A range that runs
    /// past the end of the file simply stops there.
    pub fn slice(&self, text: &str) -> String {
        text.lines()
            .enumerate()
            .filter(|(index, _)| {
                let number = *index as u32 + 1;
                number >= self.first && number <= self.last
            })
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchHit {
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// Read methods only.
pub trait WorktreeSource: Send + Sync {
    fn root(&self) -> &Path;

    /// The commit the checkout is sitting on, so it can be compared against
    /// the change's `head_sha` before anything is read.
    fn head_sha(&self) -> Result<String, WorktreeError>;

    fn list_files(&self, glob: &str) -> Result<Vec<String>, WorktreeError>;

    fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, WorktreeError>;

    /// Local regular expression matching over the tree.
    fn search(&self, query: &str, glob: Option<&str>) -> Result<Vec<SearchHit>, WorktreeError>;
}

pub struct LocalWorktree {
    root: PathBuf,
}

impl LocalWorktree {
    pub fn open(root: PathBuf) -> Result<Self, WorktreeError> {
        if !root.is_dir() {
            return Err(WorktreeError::NotADirectory { path: root });
        }
        Ok(Self { root })
    }

    /// Fail early when the checkout is not the commit under review.
    pub fn require_head(&self, expected: &str) -> Result<(), WorktreeError> {
        let actual = self.head_sha()?;
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

    fn slice_lines(text: &str, lines: Option<LineRange>) -> String {
        match lines {
            Some(range) => range.slice(text),
            None => text.to_string(),
        }
    }
}

impl WorktreeSource for LocalWorktree {
    fn root(&self) -> &Path {
        &self.root
    }

    /// Read `.git/HEAD` and follow one level of ref. Enough to compare
    /// against `head_sha` without shelling out to git.
    fn head_sha(&self) -> Result<String, WorktreeError> {
        let head = self.root.join(".git").join("HEAD");
        let text = std::fs::read_to_string(&head).map_err(|_| WorktreeError::NoHead {
            path: self.root.clone(),
        })?;
        let text = text.trim();
        let Some(reference) = text.strip_prefix("ref: ") else {
            return Ok(text.to_string());
        };
        let target = self.root.join(".git").join(reference);
        std::fs::read_to_string(&target)
            .map(|sha| sha.trim().to_string())
            .map_err(|_| WorktreeError::NoHead {
                path: self.root.clone(),
            })
    }

    fn list_files(&self, glob: &str) -> Result<Vec<String>, WorktreeError> {
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
        Ok(found)
    }

    fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, WorktreeError> {
        let full = self.root.join(path);
        let text = std::fs::read_to_string(&full).map_err(|error| WorktreeError::Unreadable {
            path: path.to_string(),
            reason: error.to_string(),
        })?;
        Ok(Self::slice_lines(&text, lines))
    }

    fn search(&self, query: &str, glob: Option<&str>) -> Result<Vec<SearchHit>, WorktreeError> {
        let pattern = Regex::new(query).map_err(|error| WorktreeError::InvalidQuery {
            query: query.to_string(),
            reason: error.to_string(),
        })?;
        let mut hits = Vec::new();
        for path in self.list_files(glob.unwrap_or("**/*"))? {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree() -> (tempfile::TempDir, LocalWorktree) {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        std::fs::write(
            directory.path().join("src/parse.c"),
            "int main(void)\n{\n}\n",
        )
        .unwrap();
        std::fs::write(directory.path().join("README.md"), "# demo\n").unwrap();
        let worktree = LocalWorktree::open(directory.path().to_path_buf()).unwrap();
        (directory, worktree)
    }

    #[test]
    fn globs_select_paths_and_reads_can_be_narrowed_to_a_range() {
        let (_directory, worktree) = worktree();
        assert_eq!(
            worktree.list_files("**/*.c").unwrap(),
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
        let (_directory, worktree) = worktree();
        let hits = worktree.search(r"int\s+main", None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/parse.c");
        assert_eq!(hits[0].line, 1);
    }
}
