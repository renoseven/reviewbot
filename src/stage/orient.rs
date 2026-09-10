//! The whole-change view, assembled once and carried in `instructions`.
//!
//! Review looks at one file at a time, and that is deliberate: it keeps "look
//! somewhere else" an explicit act rather than an implicit assumption
//! (§7). But one file at a time was costing the model two things it had no
//! way to work out, and both of them were free to hand over.
//!
//! It did not know **what else this change touches**. Reviewing `parse.c`, it
//! could read `parse.h` at `head_sha` and so would see the updated header,
//! but nothing told it the header was part of the same change — so "the
//! caller was never updated" and "the caller is being updated in a file I
//! cannot see" looked identical from where it stood.
//!
//! And it did not know **the shape of the project**, so learning that headers
//! live under `include/` rather than beside the sources cost a round of
//! `list_repo_files` on every chunk, every run.
//!
//! Neither of these is a summary of what other files *say*: that is the thing
//! §7 refused, because a model cannot check a summary and will reason from it
//! anyway. These are facts already in hand — the changed paths come out of
//! `ChangeSet`, the layout out of the tree the repository source caches for
//! the run — and they go into `instructions`, which is assembled once and
//! byte identical for the whole run, so the vendor's prompt cache means the
//! run pays for them once rather than once per chunk.

use std::collections::BTreeMap;

use crate::domain::{ChangeSet, DEV_NULL, FileChange};
use crate::security::PathPolicy;

use super::prompt::{CappedList, Keep, Overflow};
use super::{StageContext, StageError};

/// How many changed files the manifest names before it starts counting. A
/// change wide enough to overflow this is one where the tail adds nothing:
/// the model cannot read them all anyway.
const MANIFEST_FILES: usize = 60;

/// How many directories the layout digest names, and how deep it goes. Two
/// levels answer "where do the headers live"; a third mostly answers "this
/// project has directories", at several times the tokens.
const LAYOUT_DIRS: usize = 24;
const LAYOUT_DEPTH: usize = 2;

/// How many extensions are named per directory before the rest are counted.
const LAYOUT_EXTENSIONS: usize = 3;

fn files(count: usize) -> String {
    match count {
        1 => "1 file".to_string(),
        counted => format!("{counted} files"),
    }
}

/// The two run-constant blocks the prompt substitutes. Empty is a legal value
/// for either: a single-file change has no manifest worth printing, and a
/// diff with no repository source behind it has no layout at all.
pub struct Orientation {
    pub change: String,
    pub layout: String,
}

impl Orientation {
    pub fn build(context: &StageContext<'_>, changeset: &ChangeSet) -> Self {
        Self {
            change: manifest(changeset),
            layout: layout(context),
        }
    }

    #[cfg(test)]
    pub fn none() -> Self {
        Self {
            change: String::new(),
            layout: String::new(),
        }
    }
}

/// Every path this change touches, with how much of each moved. Renames are
/// spelled out both ways: a model told only the new path would read the file
/// as new.
fn manifest(changeset: &ChangeSet) -> String {
    if changeset.files.len() < 2 {
        return String::new();
    }
    let mut lines = vec![format!(
        "This change touches {} files. You review one of them at a time, so here is the \
         whole list — what else is in flight is not something you could work out from the \
         diff in front of you:",
        changeset.files.len()
    )];
    lines.push(
        CappedList::new(
            changeset.files.iter().map(describe).collect(),
            MANIFEST_FILES,
            Keep::First,
            Overflow::Counted("files"),
        )
        .render(),
    );
    lines.push(String::new());
    lines.push(
        "Two things follow. A file on this list already carries its change at the commit you \
         read, because the read-a-file tools read `head_sha` — so a caller that looks wrong \
         may simply be one this change updates elsewhere, and it is worth fetching before \
         you file anything about it. And the list is for judging the file you were given, \
         not for reviewing the others: a finding still has to be anchored in the diff in \
         front of you, and a defect in another file belongs to that file's turn."
            .to_string(),
    );
    lines.join("\n")
}

fn describe(file: &FileChange) -> String {
    let deleted = file.new_path == DEV_NULL;
    let (path, kind) = match file.old_path.as_str() {
        DEV_NULL => (file.new_path.clone(), Some("added")),
        old if deleted => (old.to_string(), Some("deleted")),
        old if old != file.new_path => (format!("{old} -> {}", file.new_path), Some("renamed")),
        old => (old.to_string(), None),
    };
    let mut notes: Vec<String> = kind.into_iter().map(str::to_string).collect();
    if file.binary {
        notes.push("binary".to_string());
    }
    // A deletion has no line count worth printing: the whole file went, and
    // there is nothing at the reviewed commit to point a finding at.
    if !file.binary && !deleted {
        notes.push(match file.changed_lines.len() {
            1 => "1 line changed".to_string(),
            counted => format!("{counted} lines changed"),
        });
    }
    format!("{path} ({})", notes.join(", "))
}

/// Where things live in this repository, as counts rather than paths. A full
/// tree would be the wrong trade twice over: it would spend on the window the
/// very tokens the chunk limit is protecting, and a listing is what
/// `list_repo_files` is for. What the model cannot get from a listing is the
/// shape — which is exactly what it would otherwise spend a round guessing.
fn layout(context: &StageContext<'_>) -> String {
    let (paths, complete) = match tree(context) {
        Ok(Some(tree)) => tree,
        Ok(None) => return String::new(),
        // A digest is an optimisation, not a stage: this is the same fetch
        // the model's first listing would have paid for, and when that fails
        // the model is told and carries on. Ending the run over it would be
        // worse than starting without it.
        Err(error) => {
            tracing::warn!("no layout digest this run: {error}");
            return String::new();
        }
    };
    digest(paths, complete, context.paths)
}

/// Split from the fetch so the counting can be tested without a run.
fn digest(paths: Vec<String>, complete: bool, policy: &PathPolicy) -> String {
    // Only what a read could actually return: `deny_paths` because naming a
    // denied directory hands over the very names it withholds, and the
    // extension whitelist because a `target/` of ten thousand object files
    // is present, unreadable, and would crowd out the source tree. A
    // listing does not filter on extension — there, existence is the answer
    // to a question the model asked. Here reviewbot is volunteering a
    // summary, and a summary's job is to be representative.
    let readable: Vec<String> = paths
        .into_iter()
        .filter(|path| policy.check_repo_path(path).is_ok())
        .collect();
    if readable.is_empty() {
        return String::new();
    }
    let mut dirs: BTreeMap<String, Directory> = BTreeMap::new();
    for path in &readable {
        for prefix in prefixes(path) {
            dirs.entry(prefix).or_default().add(extension(path));
        }
    }
    let mut ranked: Vec<(String, Directory)> = dirs.into_iter().collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .files
            .cmp(&left.1.files)
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut lines = vec![format!(
        "The shape of this repository at the commit under review: the {} you could read, \
         counted by directory to {LAYOUT_DEPTH} levels. Counts include subdirectories.",
        files(readable.len())
    )];
    lines.push(
        CappedList::new(
            ranked
                .iter()
                .map(|(path, directory)| format!("{path} — {}", directory.describe()))
                .collect(),
            LAYOUT_DIRS,
            Keep::First,
            Overflow::Counted("directories"),
        )
        .render(),
    );
    lines.push(String::new());
    lines.push(
        "This is a digest, not a listing, and it is the answer to \"where would that live\" \
         rather than \"does this path exist\". Read a path off it and you are still \
         guessing; a directory absent from it holds nothing you could read, which is not \
         the same as holding nothing. For actual paths, list or search."
            .to_string(),
    );
    if !complete {
        lines.push(
            "The platform would not hand over the whole tree, so these counts are a lower \
             bound."
                .to_string(),
        );
    }
    lines.join("\n")
}

/// Straight through this run's worktree, and which half of it depends on the
/// shape rather than on a preference for the repository: a checkout is the
/// whole tree already and free to walk, while a cache holds the handful of
/// files fetched so far and listing that would describe the project as "the
/// four files I happen to have". An empty worktree has no shape to describe.
fn tree(context: &StageContext<'_>) -> Result<Option<(Vec<String>, bool)>, StageError> {
    let Some(listing) = context.worktree.list_project()? else {
        return Ok(None);
    };
    Ok(Some((
        listing.paths().map(str::to_string).collect(),
        listing.complete,
    )))
}

/// `src/net/http.c` at two levels is `src/` and `src/net/`. The file's own
/// directory is what the model needs; the path itself is not a directory.
fn prefixes(path: &str) -> Vec<String> {
    let segments: Vec<&str> = path.split('/').collect();
    let depth = segments.len().saturating_sub(1).min(LAYOUT_DEPTH);
    match depth {
        0 => vec!["(top level)".to_string()],
        _ => (1..=depth)
            .map(|level| format!("{}/", segments[..level].join("/")))
            .collect(),
    }
}

fn extension(path: &str) -> String {
    match path.rsplit_once('.') {
        Some((_, extension)) if !extension.contains('/') => extension.to_string(),
        _ => "no extension".to_string(),
    }
}

#[derive(Default)]
struct Directory {
    files: usize,
    extensions: BTreeMap<String, usize>,
}

impl Directory {
    fn add(&mut self, extension: String) {
        self.files += 1;
        *self.extensions.entry(extension).or_default() += 1;
    }

    fn describe(&self) -> String {
        let mut ranked: Vec<(&String, &usize)> = self.extensions.iter().collect();
        ranked.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
        let named: Vec<String> = ranked
            .iter()
            .take(LAYOUT_EXTENSIONS)
            .map(|(extension, count)| format!(".{extension} {count}"))
            .collect();
        let rest = match ranked.len() > LAYOUT_EXTENSIONS {
            true => format!(", {} other kinds", ranked.len() - LAYOUT_EXTENSIONS),
            false => String::new(),
        };
        format!("{} ({}{rest})", files(self.files), named.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecuritySettings;
    use crate::domain::{FileChange, Hunk};
    use std::collections::BTreeSet;

    fn file(old: &str, new: &str, changed: &[u32], binary: bool) -> FileChange {
        FileChange {
            old_path: old.to_string(),
            new_path: new.to_string(),
            hunks: Vec::<Hunk>::new(),
            commentable_lines: BTreeSet::new(),
            changed_lines: changed.iter().copied().collect(),
            binary,
        }
    }

    fn changeset(files: Vec<FileChange>) -> ChangeSet {
        ChangeSet {
            locator: Default::default(),
            files,
            narrative: Default::default(),
        }
    }

    /// The blind spot this exists to close: reviewing one file, the model is
    /// told which others moved, so an untouched-looking caller can be told
    /// apart from one this change updates elsewhere.
    #[test]
    fn the_manifest_names_every_file_the_change_touches() {
        let text = manifest(&changeset(vec![
            file("src/parse.c", "src/parse.c", &[11, 12], false),
            file("src/parse.h", "src/parse.h", &[4], false),
        ]));
        assert!(text.contains("touches 2 files"), "{text}");
        assert!(text.contains("src/parse.c (2 lines changed)"), "{text}");
        assert!(text.contains("src/parse.h (1 line changed)"), "{text}");
        assert!(
            text.contains("anchored in the diff in front of you"),
            "a wider view is not a wider scope: {text}"
        );
    }

    /// A rename shown only by its new path reads as a new file, and "this
    /// function has no callers" is exactly the wrong conclusion to hand a
    /// reviewer.
    #[test]
    fn a_manifest_spells_out_renames_additions_and_deletions() {
        let text = manifest(&changeset(vec![
            file(DEV_NULL, "src/new.c", &[1], false),
            file("src/gone.c", DEV_NULL, &[], false),
            file("src/old.c", "src/moved.c", &[3, 4, 5], false),
            file("logo.png", "logo.png", &[], true),
        ]));
        assert!(text.contains("src/new.c (added, 1 line changed)"), "{text}");
        assert!(
            text.contains("src/gone.c (deleted)"),
            "a deletion has no line count to print: {text}"
        );
        assert!(
            text.contains("src/old.c -> src/moved.c (renamed, 3 lines changed)"),
            "{text}"
        );
        assert!(text.contains("logo.png (binary)"), "{text}");
    }

    /// One file is the whole change, so there is nothing to orient against.
    /// The ordinary case must not pay for a caveat that says nothing.
    #[test]
    fn a_single_file_change_gets_no_manifest_at_all() {
        let text = manifest(&changeset(vec![file(
            "src/parse.c",
            "src/parse.c",
            &[1],
            false,
        )]));
        assert!(text.is_empty(), "{text}");
    }

    #[test]
    fn a_wide_change_names_what_it_can_and_counts_the_rest() {
        let files: Vec<FileChange> = (0..MANIFEST_FILES + 5)
            .map(|index| {
                let path = format!("src/file{index:03}.c");
                file(&path, &path, &[1], false)
            })
            .collect();
        let text = manifest(&changeset(files));
        assert!(text.contains("src/file000.c"), "{text}");
        assert!(!text.contains("src/file060.c"), "{text}");
        assert!(text.contains("and 5 more files"), "{text}");
    }

    /// Depth two, and the file's own name is not a directory.
    #[test]
    fn a_path_counts_towards_its_directory_and_its_parent() {
        assert_eq!(prefixes("src/net/http.c"), vec!["src/", "src/net/"]);
        assert_eq!(prefixes("src/parse.c"), vec!["src/"]);
        assert_eq!(prefixes("README.md"), vec!["(top level)"]);
        assert_eq!(
            prefixes("a/b/c/d/e.c"),
            vec!["a/", "a/b/"],
            "deeper directories fold into the second level"
        );
    }

    fn policy(deny: &[&str], extensions: &[&str]) -> PathPolicy {
        let settings = SecuritySettings {
            deny_paths: deny.iter().map(|pattern| pattern.to_string()).collect(),
            allow_extensions: extensions.iter().map(|kind| kind.to_string()).collect(),
            ..SecuritySettings::for_tests()
        };
        PathPolicy::new(&settings, &[], None).expect("valid globs")
    }

    /// The point of the digest: the model learns headers live under
    /// `include/` without spending a round of `list_repo_files` finding out.
    #[test]
    fn the_digest_says_where_things_live_biggest_directory_first() {
        let text = digest(
            vec![
                "src/parse.c".to_string(),
                "src/lex.c".to_string(),
                "src/net/http.c".to_string(),
                "include/parse.h".to_string(),
                "README.md".to_string(),
            ],
            true,
            &policy(&[], &["c", "h", "md"]),
        );
        let src = text.find("- src/ \u{2014}").expect("src is named");
        let include = text.find("- include/ \u{2014}").expect("include is named");
        assert!(src < include, "three files outrank one:\n{text}");
        assert!(text.contains("- src/ \u{2014} 3 files (.c 3)"), "{text}");
        assert!(text.contains("- src/net/ \u{2014} 1 file (.c 1)"), "{text}");
        assert!(text.contains("- include/ \u{2014} 1 file (.h 1)"), "{text}");
        assert!(
            text.contains("- (top level) \u{2014} 1 file (.md 1)"),
            "{text}"
        );
        assert!(text.contains("the 5 files you could read"), "{text}");
    }

    /// Absence in a digest is not absence in the repository, and the model
    /// has to be told which of the two it is looking at.
    #[test]
    fn the_digest_says_it_is_not_a_listing() {
        let text = digest(vec!["src/parse.c".to_string()], true, &policy(&[], &["c"]));
        assert!(text.contains("digest, not a listing"), "{text}");
        assert!(!text.contains("lower bound"), "the tree was whole: {text}");

        let partial = digest(vec!["src/parse.c".to_string()], false, &policy(&[], &["c"]));
        assert!(
            partial.contains("lower bound"),
            "a truncated tree undercounts and must say so: {partial}"
        );
    }

    /// A denied directory named in the digest would hand over the very paths
    /// `deny_paths` exists to withhold, and a `target/` full of object files
    /// would crowd out the source tree it is meant to describe.
    #[test]
    fn the_digest_leaves_out_what_a_read_would_refuse() {
        let text = digest(
            vec![
                "src/parse.c".to_string(),
                "secrets/keys.c".to_string(),
                "target/parse.o".to_string(),
                "target/lex.o".to_string(),
                "target/net.o".to_string(),
            ],
            true,
            &policy(&["secrets/**"], &["c"]),
        );
        assert!(text.contains("- src/"), "{text}");
        assert!(
            !text.contains("secrets"),
            "a denied path is not named, not even as a count: {text}"
        );
        assert!(
            !text.contains("target"),
            "three unreadable object files must not outrank the sources: {text}"
        );
        assert!(text.contains("the 1 file you could read"), "{text}");
    }

    /// Nothing readable is nothing to say. An empty digest is better than a
    /// paragraph of caveats about a repository the model cannot read.
    #[test]
    fn nothing_readable_gets_no_digest() {
        let text = digest(
            vec!["target/parse.o".to_string()],
            true,
            &policy(&[], &["c"]),
        );
        assert!(text.is_empty(), "{text}");
    }

    #[test]
    fn a_directory_names_its_common_extensions_and_counts_the_rest() {
        let mut directory = Directory::default();
        for extension in ["c", "c", "c", "h", "h", "md", "toml", "py"] {
            directory.add(extension.to_string());
        }
        let text = directory.describe();
        assert!(text.starts_with("8 files (.c 3, .h 2"), "{text}");
        let mut lone = Directory::default();
        lone.add("c".to_string());
        assert_eq!(lone.describe(), "1 file (.c 1)");
        assert!(text.contains("other kinds"), "{text}");
    }
}
