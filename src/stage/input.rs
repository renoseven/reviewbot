//! Stage 1. A URL or a raw diff becomes one `ChangeSet`. Normalization only;
//! fetching is `platform`'s job.
//!
//! Both line sets are built here because this is the last moment the full
//! diff is in hand: nothing downstream can rebuild them. One parser serves
//! both a local diff file and the text a platform's diff endpoint returns,
//! and it tells the accepted format from the refused one by content, never
//! by file name.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::domain::{ChangeSet, DEV_NULL, FileChange, Hunk, Locator, Narrative, Stage};
use crate::platform::ChangeRef;
use crate::record::{InputIdentity, InputKind, InputRecord};
use crate::worktree::{Worktree, WorktreeError};

use super::{Adapters, StageContext, StageError};

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
}

pub struct Input;

impl Input {
    /// What this run is, resolved before the run directory exists because
    /// `run_id` is built from it — and so before there is a worktree. The
    /// checkout is therefore read by path: `checkout` is `--worktree`, and
    /// `Worktree::head_at` needs no instance.
    pub fn identify(
        adapters: &Adapters,
        source: &Source,
        checkout: Option<&Path>,
    ) -> Result<InputRecord, StageError> {
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
                // A checkout parked on another commit would make every line
                // number wrong, so it is refused here — before the run
                // directory exists, let alone anything being read out of it.
                if let Some(root) = checkout {
                    let actual = Worktree::head_at(root)?;
                    if actual != head_sha {
                        return Err(StageError::Worktree(WorktreeError::HeadMismatch {
                            actual,
                            expected: head_sha,
                        }));
                    }
                }
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
                // A cache the run fills for itself stands on no commit of its
                // own, and that is recorded as an empty sha rather than
                // invented: the run id is built out of this.
                let head_sha = match checkout {
                    Some(root) => Worktree::head_at(root)?,
                    None => String::new(),
                };
                Ok(InputRecord {
                    kind: InputKind::Diff,
                    source: origin.clone(),
                    identity: InputIdentity::diff(content),
                    head_sha,
                })
            }
        }
    }

    /// The change set itself. Nothing here reads the worktree: a change is
    /// whatever the platform's diff or the diff file says it is, and the
    /// commit a checkout stands on was settled by `identify`.
    pub fn run(context: &mut StageContext<'_>, source: &Source) -> Result<ChangeSet, StageError> {
        let changeset = match source {
            Source::Url(url) => Self::from_platform(context, url)?,
            Source::Diff { content, .. } => Self::from_diff(context, content)?,
        };
        tracing::info!(
            files = changeset.files.len(),
            "input normalized into a change set"
        );
        context.complete(Stage::Input, &changeset)?;
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
        // The commit the checkout stood on, as `identify` read it before this
        // run had a directory. Read back rather than read again: the run is
        // named after that sha, so a second reading could only disagree with
        // the one already on disk. Empty means there was no checkout.
        let head_sha = Some(context.recorder.meta().input.head_sha.clone())
            .filter(|head_sha| !head_sha.is_empty());
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

#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    #[error(
        "only unified diff is accepted; this is git format-patch output. \
         Run `git diff base..head > change.diff` and feed that instead"
    )]
    NotUnifiedDiff,
    #[error("no `diff --git` or `---`/`+++` header anywhere in this text, so it is not a diff")]
    NoFileHeader,
    #[error("line {line}: cannot read the hunk header {header:?}")]
    BadHunkHeader { line: usize, header: String },
}

/// A parsed unified diff: one `FileChange` per file, both line sets built.
pub struct UnifiedDiff {
    files: Vec<FileChange>,
}

impl UnifiedDiff {
    pub fn parse(text: &str) -> Result<Self, DiffError> {
        if Mbox::detected(text) {
            return Err(DiffError::NotUnifiedDiff);
        }
        let mut parser = Parser::default();
        for (offset, line) in text.lines().enumerate() {
            parser.read(offset + 1, line)?;
        }
        let files = parser.finish();
        // An empty diff is an empty change set; anything else with no file
        // header at all is some other kind of text and is not guessed at.
        if files.is_empty() && !text.trim().is_empty() {
            return Err(DiffError::NoFileHeader);
        }
        Ok(Self { files })
    }

    pub fn files(&self) -> &[FileChange] {
        &self.files
    }

    pub fn into_files(self) -> Vec<FileChange> {
        self.files
    }
}

/// git format-patch output. Refused outright rather than stripped: its
/// headers carry a commit message that is not part of the change, and
/// guessing where the mail ends and the diff starts is how patches get
/// truncated without anyone noticing.
struct Mbox;

impl Mbox {
    fn detected(text: &str) -> bool {
        let mut author = false;
        let mut patch_subject = false;
        for line in text.lines() {
            if line.starts_with("diff --git ") || line.starts_with("--- ") || line.starts_with("@@")
            {
                break;
            }
            if Self::is_separator(line) {
                return true;
            }
            author |= line.starts_with("From: ");
            patch_subject |= line.starts_with("Subject: ") && line.contains("[PATCH");
        }
        author && patch_subject
    }

    /// `From <40 hex> Mon Sep 17 00:00:00 2001`, the mbox record separator.
    fn is_separator(line: &str) -> bool {
        let Some(rest) = line.strip_prefix("From ") else {
            return false;
        };
        let hash = rest.split(' ').next().unwrap_or_default();
        hash.len() >= 40 && hash.chars().all(|c| c.is_ascii_hexdigit())
    }
}

#[derive(Default)]
struct Parser {
    files: Vec<FileChange>,
    file: Option<FileBuilder>,
    hunk: Option<HunkCursor>,
}

impl Parser {
    fn read(&mut self, number: usize, line: &str) -> Result<(), DiffError> {
        match self.hunk.as_mut() {
            Some(cursor) if cursor.accepts(line) => {
                cursor.push(line);
                return Ok(());
            }
            Some(_) => self.close_hunk(),
            None => {}
        }
        self.read_header(number, line)
    }

    fn read_header(&mut self, number: usize, line: &str) -> Result<(), DiffError> {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (old_path, new_path) = git_header_paths(rest);
            let file = self.start_file();
            file.old_path = old_path;
            file.new_path = new_path;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            // Without `diff --git` this pair is the only file boundary there
            // is, so a second `---` starts the next file.
            let starts_another = self.file.as_ref().is_none_or(|file| file.paired);
            let file = match starts_another {
                true => self.start_file(),
                false => self.file.as_mut().expect("present"),
            };
            file.old_path = header_path(rest);
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let file = self.file_mut();
            file.new_path = header_path(rest);
            file.paired = true;
        } else if line.starts_with("@@") {
            let header = HunkHeader::parse(line).ok_or_else(|| DiffError::BadHunkHeader {
                line: number,
                header: line.to_string(),
            })?;
            self.file_mut();
            self.hunk = Some(HunkCursor::new(header, line));
        } else if let Some(notice) = BinaryNotice::parse(line) {
            let file = self.file_mut();
            file.binary = true;
            notice.name(file);
        }
        Ok(())
    }

    fn start_file(&mut self) -> &mut FileBuilder {
        self.close_hunk();
        self.flush_file();
        self.file.insert(FileBuilder::default())
    }

    fn file_mut(&mut self) -> &mut FileBuilder {
        if self.file.is_none() {
            self.file = Some(FileBuilder::default());
        }
        self.file.as_mut().expect("just created")
    }

    fn close_hunk(&mut self) {
        if let Some(cursor) = self.hunk.take() {
            cursor.commit(self.file_mut());
        }
    }

    fn flush_file(&mut self) {
        if let Some(file) = self.file.take()
            && let Some(change) = file.finish()
        {
            self.files.push(change);
        }
    }

    fn finish(mut self) -> Vec<FileChange> {
        self.close_hunk();
        self.flush_file();
        self.files
    }
}

#[derive(Default)]
struct FileBuilder {
    old_path: String,
    new_path: String,
    hunks: Vec<Hunk>,
    commentable_lines: BTreeSet<u32>,
    changed_lines: BTreeSet<u32>,
    binary: bool,
    /// Both `---` and `+++` have been read, so the next `---` is a new file.
    paired: bool,
}

impl FileBuilder {
    /// `None` when nothing named a file, which is how stray text between two
    /// diffs is dropped instead of becoming an unnamed entry.
    fn finish(mut self) -> Option<FileChange> {
        if self.old_path.is_empty() && self.new_path.is_empty() {
            return None;
        }
        if self.new_path.is_empty() {
            self.new_path = self.old_path.clone();
        }
        if self.old_path.is_empty() {
            self.old_path = self.new_path.clone();
        }
        Some(FileChange {
            old_path: self.old_path,
            new_path: self.new_path,
            hunks: self.hunks,
            commentable_lines: self.commentable_lines,
            changed_lines: self.changed_lines,
            binary: self.binary,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct HunkHeader {
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
}

impl HunkHeader {
    /// `@@ -old,count +new,count @@ optional section heading`. A missing
    /// count means one line, which is what git omits it for.
    fn parse(line: &str) -> Option<Self> {
        let body = line.strip_prefix("@@")?;
        let end = body.find("@@")?;
        let mut ranges = body[..end].split_whitespace();
        let (old_start, old_count) = Self::range(ranges.next()?, '-')?;
        let (new_start, new_count) = Self::range(ranges.next()?, '+')?;
        Some(Self {
            old_start,
            old_count,
            new_start,
            new_count,
        })
    }

    fn range(token: &str, sign: char) -> Option<(u32, u32)> {
        let token = token.strip_prefix(sign)?;
        match token.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((token.parse().ok()?, 1)),
        }
    }
}

/// One hunk being read. Line numbers are new file numbers throughout: the
/// `+` side of the `@@` header is the only side a comment can hang on.
struct HunkCursor {
    header: HunkHeader,
    text: String,
    new_line: u32,
    remaining_old: u32,
    remaining_new: u32,
    added: bool,
    binary: bool,
    commentable_lines: BTreeSet<u32>,
    changed_lines: BTreeSet<u32>,
    /// One entry per run of `-` lines: the new file line the deletion sits
    /// in front of.
    deletions: Vec<u32>,
    in_deletion: bool,
}

impl HunkCursor {
    fn new(header: HunkHeader, line: &str) -> Self {
        let mut text = String::with_capacity(line.len() + 1);
        text.push_str(line);
        text.push('\n');
        Self {
            // A zero length new side is numbered from the line before it,
            // which is the convention git writes `+N,0` with.
            new_line: match header.new_count {
                0 => header.new_start + 1,
                _ => header.new_start,
            },
            remaining_old: header.old_count,
            remaining_new: header.new_count,
            header,
            text,
            added: false,
            binary: false,
            commentable_lines: BTreeSet::new(),
            changed_lines: BTreeSet::new(),
            deletions: Vec::new(),
            in_deletion: false,
        }
    }

    /// The body runs for exactly as many lines as the header counted. After
    /// that only the "no newline" marker may still belong to it.
    fn accepts(&self, line: &str) -> bool {
        if self.remaining_old == 0 && self.remaining_new == 0 {
            return line.starts_with('\\');
        }
        matches!(line.chars().next(), None | Some(' ' | '+' | '-' | '\\'))
    }

    fn push(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
        self.binary |= line.contains('\0');
        match line.chars().next() {
            Some('\\') => {}
            Some('+') => {
                self.added = true;
                self.in_deletion = false;
                self.commentable_lines.insert(self.new_line);
                self.changed_lines.insert(self.new_line);
                self.new_line += 1;
                self.remaining_new = self.remaining_new.saturating_sub(1);
            }
            Some('-') => {
                if !self.in_deletion {
                    self.in_deletion = true;
                    self.deletions.push(self.new_line);
                }
                self.remaining_old = self.remaining_old.saturating_sub(1);
            }
            // A context line, including the bare empty line some tools emit
            // where the code itself is blank.
            _ => {
                self.in_deletion = false;
                self.commentable_lines.insert(self.new_line);
                self.new_line += 1;
                self.remaining_new = self.remaining_new.saturating_sub(1);
                self.remaining_old = self.remaining_old.saturating_sub(1);
            }
        }
    }

    /// A hunk with no `+` line took code out and put nothing back, so the
    /// line next to the deletion counts as changed: it is the only place a
    /// comment about the removal can hang. Context lines stay commentable
    /// but unchanged everywhere else.
    fn commit(mut self, file: &mut FileBuilder) {
        if !self.added {
            for deletion in std::mem::take(&mut self.deletions) {
                if let Some(line) = self.surviving_line(deletion) {
                    self.commentable_lines.insert(line);
                    self.changed_lines.insert(line);
                }
            }
        }
        file.commentable_lines.append(&mut self.commentable_lines);
        file.changed_lines.append(&mut self.changed_lines);
        file.binary |= self.binary;
        file.hunks.push(Hunk {
            old_start: self.header.old_start,
            old_count: self.header.old_count,
            new_start: self.header.new_start,
            new_count: self.header.new_count,
            text: self.text,
        });
    }

    /// The line after the deleted block while the new file still has one,
    /// otherwise the last line before it. Nothing at all when the hunk left
    /// no new lines, as with a file deleted outright.
    fn surviving_line(&self, after_deletion: u32) -> Option<u32> {
        let end = self.header.new_start + self.header.new_count;
        if self.header.new_count > 0 && after_deletion < end {
            return Some(after_deletion);
        }
        after_deletion.checked_sub(1).filter(|line| *line >= 1)
    }
}

/// The two ways a diff says "this file is not text".
struct BinaryNotice {
    old_path: String,
    new_path: String,
}

impl BinaryNotice {
    fn parse(line: &str) -> Option<Self> {
        if line.starts_with("GIT binary patch") {
            return Some(Self {
                old_path: String::new(),
                new_path: String::new(),
            });
        }
        let body = line
            .strip_prefix("Binary files ")?
            .strip_suffix(" differ")?;
        let (old_path, new_path) = body.split_once(" and ")?;
        Some(Self {
            old_path: strip_side(old_path),
            new_path: strip_side(new_path),
        })
    }

    /// A `Binary files ... differ` line names both sides, which is all there
    /// is to go on when the diff carried no `diff --git` header.
    fn name(self, file: &mut FileBuilder) {
        if file.old_path.is_empty() {
            file.old_path = self.old_path;
        }
        if file.new_path.is_empty() {
            file.new_path = self.new_path;
        }
    }
}

/// `diff --git a/old b/new`. Paths with spaces are split on the ` b/` that
/// separates the two sides.
fn git_header_paths(rest: &str) -> (String, String) {
    match rest.find(" b/") {
        Some(at) => (strip_side(&rest[..at]), strip_side(&rest[at + 1..])),
        None => {
            let mut parts = rest.split_whitespace();
            (
                strip_side(parts.next().unwrap_or_default()),
                strip_side(parts.next().unwrap_or_default()),
            )
        }
    }
}

/// `--- a/src/parse.c` or `+++ b/src/parse.c`, with the timestamp some
/// generators append after a tab.
fn header_path(rest: &str) -> String {
    strip_side(rest.split('\t').next().unwrap_or_default())
}

fn strip_side(path: &str) -> String {
    let path = path.trim();
    if path == DEV_NULL {
        return path.to_string();
    }
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Vec<FileChange> {
        UnifiedDiff::parse(text)
            .expect("a unified diff")
            .into_files()
    }

    fn lines(set: &BTreeSet<u32>) -> Vec<u32> {
        set.iter().copied().collect()
    }

    const TYPICAL: &str = "\
diff --git a/src/parse.c b/src/parse.c
index 1111111..2222222 100644
--- a/src/parse.c
+++ b/src/parse.c
@@ -10,3 +10,4 @@ static int parse(void)
 int before(void);
-int gone(void);
+int added(void);
+int also_added(void);
 int after(void);
";

    #[test]
    fn added_and_context_lines_are_commentable_but_only_added_lines_changed() {
        let files = parse(TYPICAL);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.old_path, "src/parse.c");
        assert_eq!(file.new_path, "src/parse.c");
        assert_eq!(lines(&file.commentable_lines), vec![10, 11, 12, 13]);
        assert_eq!(
            lines(&file.changed_lines),
            vec![11, 12],
            "the two added lines, and not the context around them"
        );
        assert!(
            file.changed_lines.is_subset(&file.commentable_lines),
            "changed lines are always commentable"
        );
    }

    #[test]
    fn a_pure_deletion_hunk_puts_the_adjacent_line_in_both_sets() {
        let files = parse(
            "\
--- a/src/parse.c
+++ b/src/parse.c
@@ -10,4 +10,2 @@
 int before(void);
-int gone(void);
-int also_gone(void);
 int after(void);
",
        );
        let file = &files[0];
        assert_eq!(lines(&file.commentable_lines), vec![10, 11]);
        assert_eq!(
            lines(&file.changed_lines),
            vec![11],
            "the line the deletion now sits in front of"
        );
        assert!(file.changed_lines.is_subset(&file.commentable_lines));
    }

    #[test]
    fn a_deletion_that_runs_to_the_end_falls_back_to_the_line_before_it() {
        let files = parse(
            "\
--- a/src/parse.c
+++ b/src/parse.c
@@ -10,3 +10,1 @@
 int before(void);
-int gone(void);
-int also_gone(void);
",
        );
        let file = &files[0];
        assert_eq!(lines(&file.commentable_lines), vec![10]);
        assert_eq!(lines(&file.changed_lines), vec![10]);
    }

    #[test]
    fn context_only_lines_are_commentable_and_nothing_is_changed() {
        let files = parse(
            "\
--- a/src/parse.c
+++ b/src/parse.c
@@ -10,2 +10,2 @@
 int before(void);
 int after(void);
",
        );
        let file = &files[0];
        assert_eq!(lines(&file.commentable_lines), vec![10, 11]);
        assert!(file.changed_lines.is_empty());
    }

    #[test]
    fn a_new_file_has_every_line_in_both_sets() {
        let files = parse(
            "\
diff --git a/src/new.c b/src/new.c
new file mode 100644
--- /dev/null
+++ b/src/new.c
@@ -0,0 +1,3 @@
+int one(void);
+int two(void);
+int three(void);
",
        );
        let file = &files[0];
        assert_eq!(file.old_path, DEV_NULL);
        assert_eq!(file.new_path, "src/new.c");
        assert_eq!(lines(&file.commentable_lines), vec![1, 2, 3]);
        assert_eq!(lines(&file.changed_lines), vec![1, 2, 3]);
    }

    #[test]
    fn a_deleted_file_keeps_its_paths_and_leaves_both_sets_empty() {
        let files = parse(
            "\
diff --git a/src/old.c b/src/old.c
deleted file mode 100644
--- a/src/old.c
+++ /dev/null
@@ -1,2 +0,0 @@
-int one(void);
-int two(void);
",
        );
        let file = &files[0];
        assert_eq!(file.old_path, "src/old.c");
        assert_eq!(file.new_path, DEV_NULL);
        assert!(file.changed_lines.is_empty(), "no line survives to hang on");
        assert!(file.commentable_lines.is_empty());
    }

    #[test]
    fn format_patch_output_is_refused_by_content() {
        let mbox = "\
From 0123456789abcdef0123456789abcdef01234567 Mon Sep 17 00:00:00 2001
From: Someone <someone@example.com>
Date: Mon, 8 Sep 2026 10:00:00 +0800
Subject: [PATCH] fix the parser

---
 src/parse.c | 1 +
 1 file changed, 1 insertion(+)

diff --git a/src/parse.c b/src/parse.c
--- a/src/parse.c
+++ b/src/parse.c
@@ -1,1 +1,2 @@
 int before(void);
+int added(void);
";
        assert!(matches!(
            UnifiedDiff::parse(mbox),
            Err(DiffError::NotUnifiedDiff)
        ));
    }

    #[test]
    fn a_mail_without_the_separator_is_still_recognised_by_its_headers() {
        let mail = "\
From: Someone <someone@example.com>
Subject: [PATCH 1/2] fix the parser

diff --git a/src/parse.c b/src/parse.c
--- a/src/parse.c
+++ b/src/parse.c
@@ -1,1 +1,1 @@
 int before(void);
";
        assert!(matches!(
            UnifiedDiff::parse(mail),
            Err(DiffError::NotUnifiedDiff)
        ));
    }

    #[test]
    fn the_format_is_judged_by_content_so_a_patch_extension_changes_nothing() {
        // The same bytes a `.patch` file would hold: plain unified diff.
        let files = parse(TYPICAL);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].new_path, "src/parse.c");
    }

    #[test]
    fn several_files_parse_without_a_git_header() {
        let files = parse(
            "\
--- a/src/one.c
+++ b/src/one.c
@@ -1,1 +1,2 @@
 int one(void);
+int added(void);
--- a/src/two.c
+++ b/src/two.c
@@ -5,1 +5,2 @@
 int two(void);
+int also(void);
",
        );
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path, "src/one.c");
        assert_eq!(files[1].new_path, "src/two.c");
        assert_eq!(lines(&files[1].changed_lines), vec![6]);
    }

    #[test]
    fn a_binary_file_is_recorded_rather_than_dropped() {
        let files = parse(
            "\
diff --git a/doc/logo.png b/doc/logo.png
index 1111111..2222222 100644
Binary files a/doc/logo.png and b/doc/logo.png differ
",
        );
        assert_eq!(files.len(), 1);
        assert!(files[0].binary);
        assert_eq!(files[0].new_path, "doc/logo.png");
    }

    #[test]
    fn a_rename_keeps_both_sides() {
        let files = parse(
            "\
diff --git a/src/old.c b/src/new.c
similarity index 95%
rename from src/old.c
rename to src/new.c
--- a/src/old.c
+++ b/src/new.c
@@ -1,1 +1,2 @@
 int one(void);
+int added(void);
",
        );
        assert_eq!(files[0].old_path, "src/old.c");
        assert_eq!(files[0].new_path, "src/new.c");
    }

    #[test]
    fn an_empty_diff_is_an_empty_change_and_prose_is_not() {
        assert!(parse("").is_empty());
        assert!(matches!(
            UnifiedDiff::parse("this is a README, not a diff\n"),
            Err(DiffError::NoFileHeader)
        ));
    }

    #[test]
    fn the_hunk_text_round_trips_with_its_own_header() {
        let files = parse(TYPICAL);
        let hunk = &files[0].hunks[0];
        assert_eq!(hunk.new_start, 10);
        assert_eq!(hunk.new_count, 4);
        assert!(hunk.text.starts_with("@@ -10,3 +10,4 @@"));
        assert!(hunk.text.ends_with(" int after(void);\n"));
    }
}
