//! The whole-change view, assembled once and carried in `instructions`.
//!
//! Review looks at one file at a time, and that is deliberate: it keeps "look
//! somewhere else" an explicit act rather than an implicit assumption
//! (§7). But one file at a time was costing the model something it had no
//! way to work out, and that was free to hand over.
//!
//! It did not know **what else this change touches**. Reviewing `parse.c`, it
//! could read `parse.h` at `head_sha` and so would see the updated header,
//! but nothing told it the header was part of the same change — so "the
//! caller was never updated" and "the caller is being updated in a file I
//! cannot see" looked identical from where it stood.
//!
//! That is not a summary of what other files *say*: that is the thing
//! §7 refused, because a model cannot check a summary and will reason from it
//! anyway. The changed paths come out of `ChangeSet` and go into
//! `instructions`, which is assembled once and byte identical for the whole
//! run, so the vendor's prompt cache means the run pays for them once rather
//! than once per chunk.
//!
//! A directory digest of the repository is not handed over. Naming the
//! fattest crates, or even the directories next to this change, is a map the
//! model walks; the change list already names the nearby paths, and a listing
//! or a search is what answers for any other.

use crate::domain::{ChangeSet, DEV_NULL, FileChange};

use super::prompt::{CappedList, Keep, Overflow};

/// How many changed files the manifest names before it starts counting. A
/// change wide enough to overflow this is one where the tail adds nothing:
/// the model cannot read them all anyway.
const MANIFEST_FILES: usize = 60;

/// The run-constant block the prompt substitutes. Empty is a legal value: a
/// single-file change has no manifest worth printing.
pub struct Orientation {
    change: String,
}

impl Orientation {
    pub fn build(changeset: &ChangeSet) -> Self {
        Self {
            change: manifest(changeset),
        }
    }

    #[cfg(test)]
    pub fn none() -> Self {
        Self {
            change: String::new(),
        }
    }

    #[cfg(test)]
    pub fn with_change(change: impl Into<String>) -> Self {
        Self {
            change: change.into(),
        }
    }

    pub fn change(&self) -> &str {
        &self.change
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
         may simply be one this change updates elsewhere. The list is for judging the file \
         you were given, not for reviewing the others: a finding still has to be anchored \
         in the diff in front of you, and a defect in another file belongs to that file's \
         turn."
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

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(
            !text.contains("worth fetching"),
            "the list named the siblings; fetching them was a duty: {text}"
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
}
