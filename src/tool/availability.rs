//! What a tool says when this run's worktree cannot answer it.
//!
//! The worktree reports facts — which abilities it provides — and every word
//! written about those facts is here. Two reasons for the split. The model
//! only ever reads the tool layer, so prose in `worktree` would be prose in a
//! place nobody thinks to check. And the same missing ability has to be said
//! twice, in the description before a call and in the refusal after one; one
//! file is how those two stay the same sentence.
//!
//! The rule every line here is written to: **a refusal is a fact about this
//! run, never an answer about the code**. A model reads a failed call as an
//! answer about the repository unless told otherwise, and that reading is how
//! "reviewbot could not look" becomes a published finding saying there is
//! nothing there.

use crate::worktree::Abilities;

/// Marks a description whose ability this run cannot answer. Deliberately
/// short: the model needs to spot it while scanning a list, and the reason it
/// prefixes is spelled out once in the prompt's worktree paragraph rather than
/// repeated under every tool. Six copies of one paragraph is what the model
/// reads instead of the descriptions.
const MARK: &str = "NOT AVAILABLE THIS RUN";

/// A description for an ability this run's worktree cannot answer. It still
/// says what the ability is — the model has to be able to tell it from the
/// ones that do work — then the mark, then one clause naming what is missing.
pub(crate) fn unavailable_description(what: &str, missing: Abilities) -> String {
    format!("{what} {MARK}: {}.", short_reason(missing))
}

/// What a call gets when the worktree cannot answer it. Longer than the
/// description's clause, because this one has to stop the model reading a
/// refusal as a finding, and it is paid for only when a call really happens.
pub(crate) fn refusal(missing: Abilities) -> &'static str {
    match first(missing) {
        Some(Abilities::CONTENT) => {
            "this run's worktree is empty and has nothing behind it — the input was a plain diff \
             with no platform to fetch from — so no file can be read or scanned this run. That is \
             a fact about this run, not about the repository: it does not mean the file, the \
             symbol or the caller you were after is absent. Work from the diff you were given, \
             and say in the finding what you could not confirm."
        }
        Some(Abilities::SEARCH) => {
            "no search can be answered this run: nothing behind this worktree can run one, and \
             matching the handful of files fetched into it so far would report on those files \
             rather than on the repository. That is a fact about this run, not evidence that what \
             you searched for is absent. Use list_files and read_file instead."
        }
        Some(Abilities::CHECKOUT) => {
            "this needs a whole checkout and this run's worktree is not one: it holds only the \
             files fetched into it so far, so a checker over it would report the project files it \
             cannot find rather than defects in this change. That is a fact about this run, not \
             about the code."
        }
        // Nothing is missing, so nobody asks; kept truthful rather than
        // unreachable, and kept from ever reading as a verdict on the code.
        _ => "this run's worktree can answer this",
    }
}

/// The one clause a description carries, naming what is missing.
fn short_reason(missing: Abilities) -> &'static str {
    match first(missing) {
        Some(Abilities::CONTENT) => "there is no code to read this run, only the diff",
        Some(Abilities::SEARCH) => "nothing can answer a search this run, and a miss is not proof",
        Some(Abilities::CHECKOUT) => {
            "this needs a whole checkout and this run has only the files fetched so far"
        }
        _ => "this run's worktree can answer this",
    }
}

/// What `tool list` prints under `needs`, one phrase per ability. A catalog
/// has no run to report on, so it names the conditions instead.
pub fn precondition(ability: Abilities) -> &'static str {
    match ability {
        Abilities::CONTENT => "the worktree has code to read",
        Abilities::SEARCH => "the worktree can answer a search",
        Abilities::CHECKOUT => "the worktree is a whole checkout",
        _ => "the worktree",
    }
}

/// What `report.md` and `summary.json` say a run went without.
///
/// The report has to carry this. A run whose worktree could answer nothing
/// produces the same clean-looking report as one that read everything and
/// found nothing — same empty finding list, same score, same "no findings"
/// paragraph — and a person reading it has no other way to tell the two apart.
/// The model knows; this is how the knowledge survives as far as the reader.
pub fn went_without(ability: Abilities) -> &'static str {
    match ability {
        Abilities::CONTENT => "could not read any file: it saw the diff and nothing else",
        Abilities::SEARCH => "could not search the code",
        Abilities::CHECKOUT => "had no whole checkout, so a checker that needs one could not run",
        _ => "",
    }
}

/// The abilities worth naming in a report, out of the ones a run lacked.
///
/// Having no content explains having no search and no checkout, so it is said
/// on its own: three lines that all mean "there was no code" bury the one that
/// says why.
pub fn worth_reporting(missing: Abilities) -> Vec<Abilities> {
    if missing.contains(Abilities::CONTENT) {
        return vec![Abilities::CONTENT];
    }
    [Abilities::SEARCH, Abilities::CHECKOUT]
        .into_iter()
        .filter(|ability| missing.contains(*ability))
        .collect()
}

/// Highest-priority missing ability. Content first: a worktree with nothing in
/// it is also not a checkout and cannot search, and saying all three would
/// bury the one that explains the other two. That ordering is also why no
/// wording here has to ask *why* an ability is missing — by the time the
/// checkout clause is reached, there is content, so there is only one way to
/// be short of a checkout.
fn first(missing: Abilities) -> Option<Abilities> {
    [Abilities::CONTENT, Abilities::SEARCH, Abilities::CHECKOUT]
        .into_iter()
        .find(|ability| missing.contains(*ability))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::{Content, Reach, Search};

    fn reach(content: Content, search: Search) -> Reach {
        Reach { content, search }
    }

    /// The rule the whole file exists for, asserted over every shape of
    /// worktree and every ability it could be short of.
    #[test]
    fn every_refusal_says_that_nothing_about_the_code_follows_from_it() {
        for content in [Content::Checkout, Content::Fetched, Content::Empty] {
            for search in [Search::Regex, Search::Keyword, Search::Unavailable] {
                let missing = reach(content, search).unmet(Abilities::all());
                if missing.is_empty() {
                    continue;
                }
                let said = refusal(missing);
                assert!(
                    said.contains("not about the repository")
                        || said.contains("not evidence")
                        || said.contains("not about the code"),
                    "{content:?}/{search:?}: {said}"
                );
                assert!(said.contains("this run"), "{said}");
            }
        }
    }

    /// The description carries a mark and one clause, not the paragraph. A
    /// real run where nothing could be answered showed the same 60 words
    /// under all six tools, which is what the model read instead of the
    /// descriptions, and it is paid for on every cached chunk.
    #[test]
    fn a_description_carries_the_mark_and_one_clause_not_the_paragraph() {
        let written = unavailable_description("Read one file.", Abilities::CONTENT);
        assert_eq!(
            written,
            "Read one file. NOT AVAILABLE THIS RUN: there is no code to read this run, only the \
             diff."
        );
        assert!(
            written.len() < refusal(Abilities::CONTENT).len() / 2,
            "the description is the cheap half: {written}"
        );
    }

    /// Content explains the other two on an empty worktree, so it is what
    /// gets said — in the refusal and in the report alike.
    #[test]
    fn content_is_named_ahead_of_what_it_causes() {
        let empty = reach(Content::Empty, Search::Unavailable).unmet(Abilities::all());
        assert_eq!(first(empty), Some(Abilities::CONTENT));
        assert_eq!(worth_reporting(empty), vec![Abilities::CONTENT]);

        let fetched = reach(Content::Fetched, Search::Unavailable).unmet(Abilities::all());
        assert_eq!(
            worth_reporting(fetched),
            vec![Abilities::SEARCH, Abilities::CHECKOUT],
            "two real limits, neither explaining the other"
        );

        let whole = reach(Content::Checkout, Search::Regex).unmet(Abilities::all());
        assert!(worth_reporting(whole).is_empty());
    }

    /// A report line has to read as a bound on coverage, never as a finding
    /// about the code.
    #[test]
    fn what_a_run_went_without_reads_as_coverage() {
        for ability in [Abilities::CONTENT, Abilities::SEARCH, Abilities::CHECKOUT] {
            let said = went_without(ability);
            assert!(
                said.starts_with("could not") || said.starts_with("had no"),
                "{said}"
            );
        }
    }
}
