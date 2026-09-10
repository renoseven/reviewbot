//! What a tool says when this run's worktree cannot answer it.
//!
//! The worktree reports facts — which of the three shapes it is, what the
//! repository behind it can do, and what a report should say it went without
//! — and every word the model reads about those facts is here. Two reasons
//! for the split. The model only ever reads the tool layer, so refusal prose
//! in `worktree` would be prose in a place nobody thinks to check.
//! And the same missing condition has to be said three times: the refusal
//! after a call, the short clause a description carries, and the line
//! `tool list` prints as a precondition. One file is how those three stay
//! the same sentence.
//!
//! The rule every line here is written to: **a refusal is a fact about this
//! run, never an answer about the code**. A model reads a failed call as an
//! answer about the repository unless told otherwise, and that reading is how
//! "reviewbot could not look" becomes a published finding saying there is
//! nothing there.

use crate::platform::Capabilities;
use crate::worktree::Worktree;

/// Marks a description whose condition this run cannot answer. Deliberately
/// short: the model needs to spot it while scanning a list, and the reason it
/// prefixes is spelled out once in the prompt's worktree paragraph rather than
/// repeated under every tool.
const MARK: &str = "NOT AVAILABLE THIS RUN";

/// A description for a condition this run's worktree cannot answer. It still
/// says what the tool is — the model has to be able to tell it from the ones
/// that do work — then the mark, then one clause naming what is missing.
pub(crate) fn unavailable_description(what: &str, short: &str) -> String {
    format!("{what} {MARK}: {short}.")
}

/// Empty: nothing on disk and nothing behind it.
const NO_FILES_REFUSAL: &str = "this run's worktree is empty and has nothing behind it — the input \
     was a plain diff with no platform to fetch from — so no file can be read or scanned this \
     run. That is a fact about this run, not about the repository: it does not mean the file, \
     the symbol or the caller you were after is absent. Work from the diff you were given, and \
     say in the finding what you could not confirm.";
pub(crate) const NO_FILES_SHORT: &str = "there is no code to read this run, only the diff";
pub(crate) const NO_FILES_PRECONDITION: &str = "the worktree has code to read";

pub(crate) fn no_files(worktree: &Worktree) -> Option<&'static str> {
    worktree.is_empty().then_some(NO_FILES_REFUSAL)
}

/// No repository at all: a plain diff, whether or not a checkout was named.
const NO_REPO_REFUSAL: &str = "this run has no repository behind its worktree, so nothing can be \
     listed or fetched from the platform. That is a fact about this run, not about the \
     repository: it does not mean the file you were after is absent. Work from the diff you \
     were given, and say in the finding what you could not confirm.";
pub(crate) const NO_REPO_SHORT: &str = "this run has no repository to list or fetch from";
pub(crate) const NO_REPO_PRECONDITION: &str = "the worktree has a repository behind it";

pub(crate) fn no_repo(worktree: &Worktree) -> Option<&'static str> {
    worktree.repo().is_none().then_some(NO_REPO_REFUSAL)
}

/// No repository, or one that cannot match regular expressions.
const NO_REGEX_REFUSAL: &str = "no regular-expression search of the repository can be answered \
     this run: nothing behind this worktree can run one. That is a fact about this run, not \
     evidence that what you searched for is absent. Use search_local_regex on files you have, \
     or list_repo_files and fetch_repo_file.";
pub(crate) const NO_REGEX_SHORT: &str =
    "nothing can answer a repository regex search this run, and a miss is not proof";
pub(crate) const NO_REGEX_PRECONDITION: &str =
    "the repository can answer a regular-expression search";

pub(crate) fn no_regex_search(worktree: &Worktree) -> Option<&'static str> {
    worktree
        .repo()
        .filter(|repo| repo.capabilities().contains(Capabilities::REGEX_SEARCH))
        .is_none()
        .then_some(NO_REGEX_REFUSAL)
}

/// No repository, or one that cannot match keywords.
const NO_KEYWORD_REFUSAL: &str = "no keyword search of the repository can be answered this run: \
     nothing behind this worktree can run one. That is a fact about this run, not evidence \
     that what you searched for is absent. Use search_local_regex on files you have, or \
     list_repo_files and fetch_repo_file.";
pub(crate) const NO_KEYWORD_SHORT: &str =
    "nothing can answer a repository keyword search this run, and a miss is not proof";
pub(crate) const NO_KEYWORD_PRECONDITION: &str = "the repository can answer a keyword search";

pub(crate) fn no_keyword_search(worktree: &Worktree) -> Option<&'static str> {
    worktree
        .repo()
        .filter(|repo| repo.capabilities().contains(Capabilities::KEYWORD_SEARCH))
        .is_none()
        .then_some(NO_KEYWORD_REFUSAL)
}

/// Not a checkout: Empty has no tree, Cache has only what has been fetched.
const NO_WHOLE_TREE_REFUSAL: &str = "this needs a whole checkout and this run's worktree is not \
     one: it is either empty or it holds only the files fetched into it so far, so a checker \
     over it would report the project files it cannot find rather than defects in this change. \
     That is a fact about this run, not about the code.";
pub(crate) const NO_WHOLE_TREE_SHORT: &str =
    "this needs a whole checkout and this run does not have one";
pub(crate) const NO_WHOLE_TREE_PRECONDITION: &str = "the worktree is a whole checkout";

pub(crate) fn no_whole_tree(worktree: &Worktree) -> Option<&'static str> {
    (!worktree.is_checkout()).then_some(NO_WHOLE_TREE_REFUSAL)
}

/// What every local tool's description says first: what the local side is,
/// which is the one thing the names no longer vary by run to carry.
pub(crate) fn local_side(worktree: &Worktree) -> &'static str {
    if worktree.is_checkout() {
        "The local side is the checkout under review, standing on the reviewed commit, so it \
         holds the whole project."
    } else if worktree.is_cache() {
        "The local side is a directory of this run's own, which started empty and holds the \
         files that have been fetched into it from the platform API at the reviewed commit. \
         It is not a checkout: only what has been asked for is on disk."
    } else {
        // Reached only by a caller that did not check `no_files` first; every
        // description here does, and says more than this.
        "Nothing can be read this whole run, and that says nothing about whether the \
         repository has the file."
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::platform::{
        Capabilities, LineRange, Listing, PlatformError, Repo, RepoSource, SearchHit, SearchKind,
    };

    struct Silent;

    impl RepoSource for Silent {
        fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
            Ok(Listing::default())
        }

        fn read_file(
            &self,
            _path: &str,
            _lines: Option<LineRange>,
        ) -> Result<String, PlatformError> {
            Ok(String::new())
        }

        fn size(&self, _path: &str) -> Result<u64, PlatformError> {
            Ok(0)
        }

        fn search(
            &self,
            _kind: SearchKind,
            _query: &str,
            _glob: Option<&str>,
        ) -> Result<Vec<SearchHit>, PlatformError> {
            Ok(Vec::new())
        }
    }

    fn repo(capabilities: Capabilities) -> Repo {
        Repo::new(Arc::new(Silent) as Arc<dyn RepoSource>, capabilities)
    }

    fn local(repository: Option<Repo>) -> Worktree {
        Worktree::Local {
            root: PathBuf::from("/local"),
            repo: repository,
        }
    }

    fn cache(capabilities: Capabilities) -> Worktree {
        Worktree::Cache {
            root: PathBuf::from("/cache"),
            repo: repo(capabilities),
        }
    }

    fn refusals() -> impl Iterator<Item = &'static str> {
        [
            NO_FILES_REFUSAL,
            NO_REPO_REFUSAL,
            NO_REGEX_REFUSAL,
            NO_KEYWORD_REFUSAL,
            NO_WHOLE_TREE_REFUSAL,
        ]
        .into_iter()
    }

    /// The rule the whole file exists for, asserted over every condition.
    #[test]
    fn every_refusal_says_that_nothing_about_the_code_follows_from_it() {
        for said in refusals() {
            assert!(
                said.contains("not about the repository")
                    || said.contains("not evidence")
                    || said.contains("not about the code"),
                "{said}"
            );
            assert!(said.contains("this run"), "{said}");
        }
    }

    /// The description carries a mark and one clause, not the paragraph. A
    /// real run where nothing could be answered showed the same 60 words
    /// under all six tools, which is what the model read instead of the
    /// descriptions, and it is paid for on every cached chunk.
    #[test]
    fn a_description_carries_the_mark_and_one_clause_not_the_paragraph() {
        let written = unavailable_description("Read one file.", NO_FILES_SHORT);
        assert_eq!(
            written,
            "Read one file. NOT AVAILABLE THIS RUN: there is no code to read this run, only the \
             diff."
        );
        assert!(
            written.len() < NO_FILES_REFUSAL.len() / 2,
            "the description is the cheap half: {written}"
        );
    }

    /// Having nothing to read explains the other two, so an empty worktree
    /// names only that — in the condition and in the report alike.
    #[test]
    fn having_nothing_to_read_is_named_ahead_of_what_it_causes() {
        assert_eq!(no_files(&Worktree::Empty), Some(NO_FILES_REFUSAL));
        assert_eq!(no_repo(&Worktree::Empty), Some(NO_REPO_REFUSAL));
        assert_eq!(no_regex_search(&Worktree::Empty), Some(NO_REGEX_REFUSAL));
        assert_eq!(no_whole_tree(&Worktree::Empty), Some(NO_WHOLE_TREE_REFUSAL));
        assert_eq!(
            Worktree::Empty.went_without(),
            vec!["could not read any file: it saw the diff and nothing else"]
        );

        let fetched = cache(Capabilities::empty());
        assert!(no_files(&fetched).is_none());
        assert!(no_repo(&fetched).is_none());
        assert_eq!(no_regex_search(&fetched), Some(NO_REGEX_REFUSAL));
        assert_eq!(no_keyword_search(&fetched), Some(NO_KEYWORD_REFUSAL));
        assert_eq!(no_whole_tree(&fetched), Some(NO_WHOLE_TREE_REFUSAL));
        assert_eq!(
            fetched.went_without(),
            vec![
                "had no whole checkout, so a checker that needs one could not run",
                "could not search the repository",
            ],
            "two real limits, neither explaining the other"
        );

        let whole = local(Some(repo(Capabilities::all())));
        assert!(no_files(&whole).is_none());
        assert!(no_repo(&whole).is_none());
        assert!(no_regex_search(&whole).is_none());
        assert!(no_keyword_search(&whole).is_none());
        assert!(no_whole_tree(&whole).is_none());
        assert!(whole.went_without().is_empty());
    }

    /// A report line has to read as a bound on coverage, never as a finding
    /// about the code.
    #[test]
    fn what_a_run_went_without_reads_as_coverage() {
        for said in [
            Worktree::Empty.went_without(),
            cache(Capabilities::empty()).went_without(),
            local(None).went_without(),
        ]
        .into_iter()
        .flatten()
        {
            assert!(
                said.starts_with("could not") || said.starts_with("had no"),
                "{said}"
            );
        }
    }

    #[test]
    fn regex_and_keyword_are_independent() {
        let keyword_only = cache(Capabilities::KEYWORD_SEARCH);
        assert_eq!(no_regex_search(&keyword_only), Some(NO_REGEX_REFUSAL));
        assert!(no_keyword_search(&keyword_only).is_none());

        let regex_only = cache(Capabilities::REGEX_SEARCH);
        assert!(no_regex_search(&regex_only).is_none());
        assert_eq!(no_keyword_search(&regex_only), Some(NO_KEYWORD_REFUSAL));
    }
}
