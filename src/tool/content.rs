//! The eight content tools: four that read the local side, four that ask the
//! repository. Each action appears once, and the name carries the source, so
//! the model does not have to infer from a description which of two identically
//! named tools it was offered.
//!
//! The names are fixed. What varies with the run — whether the local side is
//! the whole project, a cache of what has been fetched, or empty — lives in
//! the description, written from the three `Worktree` variants.
//!
//! Every one of them does the same three things in the same order: read the
//! model's arguments, put them through `PathPolicy`, and only then ask the
//! worktree. The check comes first because this is the only place the model
//! can reach the worktree at all, which is the whole of why the read boundary
//! holds (§6 security). The worktree takes plain repository relative paths.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use globset::Glob;
use serde_json::Value;

use crate::common::truncate;
use crate::config::Config;
use crate::platform::{File, LineRange, SearchKind};
use crate::record::ContextFile;
use crate::security::PathPolicy;
use crate::worktree::{Worktree, WorktreeError};

use super::availability::{
    NO_FILES_PRECONDITION, NO_FILES_SHORT, NO_KEYWORD_PRECONDITION, NO_KEYWORD_SHORT,
    NO_REGEX_PRECONDITION, NO_REGEX_SHORT, NO_REPO_PRECONDITION, NO_REPO_SHORT, local_side,
    no_files, no_keyword_search, no_regex_search, no_repo, unavailable_description,
};
use super::signature::{Arguments, Parameter, Shape, Signature};
use super::{Purpose, Round, Tool, ToolError, ToolOutput};

/// How much a content tool may fetch and how much it may hand back, all of it
/// from `[review]`. Carried as one value so the eight tools cannot drift apart
/// in their caps, and so the descriptions quote the same numbers the code
/// enforces.
///
/// `max_file_bytes` lives here rather than on `PathPolicy` because it is not a
/// permission: the policy answers whether a path may be read at all, this
/// answers how much of it fits.
#[derive(Clone, Copy, Debug)]
pub struct ToolLimits {
    pub max_file_bytes: u64,
    pub max_output_bytes: u64,
    pub max_files_per_listing: usize,
    pub max_hits_per_search: usize,
    pub max_files_per_fetch: usize,
}

impl ToolLimits {
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_file_bytes: config.review.max_file_bytes,
            max_output_bytes: config.review.max_tool_output_bytes,
            max_files_per_listing: config.review.max_files_per_listing as usize,
            max_hits_per_search: config.review.max_hits_per_search as usize,
            max_files_per_fetch: config.review.max_files_per_fetch as usize,
        }
    }
}

/// Content is investigated, never concluded with: once the tools come off, the
/// only thing left to do is hand over what was found.
const INVESTIGATION: &[Round] = &[Round::Investigation];

/// Said when the platform could not hand over the whole tree. It has to be
/// said: a half list read as a whole one is how a reviewer concludes that
/// something is not there.
const INCOMPLETE: &str = "Note: this listing is incomplete. The platform would not hand \
                          over the whole tree at once, so a path not listed here may still \
                          exist. Narrow the glob to a specific directory and try again.";

/// What the model is shown for a listing, a search or a file. Everything
/// answers through this, so the caps and the wording cannot drift apart
/// between the eight.
struct Answer<'a> {
    paths: &'a PathPolicy,
    limits: ToolLimits,
}

impl Answer<'_> {
    fn from(paths: &PathPolicy, limits: ToolLimits) -> Answer<'_> {
        Answer { paths, limits }
    }

    /// Denied paths are dropped rather than replaced with a placeholder: these
    /// are structured results, and a path the model can see is a path it will
    /// try to read. The extension whitelist is not applied here — that a
    /// `Makefile` exists is worth knowing, and reading it is refused later.
    fn listing(&self, found: Vec<File>, complete: bool, extra: Option<&str>) -> ToolOutput {
        let ceiling = self.limits.max_files_per_listing;
        let visible: Vec<File> = found
            .into_iter()
            .filter(|file| !self.paths.is_denied(&file.path))
            .collect();
        let total = visible.len();
        let mut lines: Vec<String> = visible
            .into_iter()
            .take(ceiling)
            .map(|file| match file.bytes {
                Some(bytes) => format!("{} ({bytes} bytes)", file.path),
                None => file.path,
            })
            .collect();
        if lines.is_empty() {
            lines.push("No paths matched.".to_string());
        }
        if total > ceiling {
            lines.push(format!(
                "{} more paths matched; narrow the glob.",
                total - ceiling
            ));
        }
        if !complete {
            lines.push(INCOMPLETE.to_string());
        }
        if let Some(extra) = extra {
            lines.push(extra.to_string());
        }
        self.text(lines.join("\n"))
    }

    /// Counts are taken after `deny_paths`, so a number never leaks that a
    /// denied path exists. Files whose hits were cut are still named, with
    /// their own counts — that is what a flat "N more" used to hide.
    fn hits(
        &self,
        found: Vec<crate::worktree::SearchHit>,
        empty_note: Option<&str>,
        extras: &[String],
    ) -> ToolOutput {
        let kept: Vec<&crate::worktree::SearchHit> = found
            .iter()
            .filter(|hit| !self.paths.is_denied(&hit.path))
            .collect();
        let n = kept.len();
        let mut order = Vec::new();
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for hit in &kept {
            *counts.entry(hit.path.as_str()).or_insert(0) += 1;
            if !order.contains(&hit.path.as_str()) {
                order.push(hit.path.as_str());
            }
        }
        let m = order.len();
        let ceiling = self.limits.max_hits_per_search;
        let listed: Vec<&crate::worktree::SearchHit> = kept.iter().copied().take(ceiling).collect();
        let mut lines = vec![format!("{n} hits across {m} files")];
        if kept.is_empty() {
            lines.push("No matches.".to_string());
            if let Some(note) = empty_note {
                lines.push(note.to_string());
            }
        } else {
            for path in order {
                let count = counts.get(path).copied().unwrap_or(0);
                lines.push(format!("{path} ({count})"));
                for hit in listed.iter().filter(|hit| hit.path == path) {
                    lines.push(format!("{}: {}", hit.line, hit.text));
                }
            }
        }
        lines.extend(extras.iter().cloned());
        self.text(lines.join("\n"))
    }

    /// Bank bodies the search already paid for, after PathPolicy, and only
    /// those `cached_body` already holds. Never issues a request.
    fn warm(&self, worktree: &Worktree, tool: &str, hits: &[crate::worktree::SearchHit]) {
        let mut seen = BTreeSet::new();
        let mut warmed = 0usize;
        for hit in hits {
            if !seen.insert(hit.path.as_str()) {
                continue;
            }
            if warmed >= self.limits.max_files_per_fetch {
                break;
            }
            let Ok(path) = checked_path(tool, self.paths, worktree, &hit.path) else {
                continue;
            };
            let Some(body) = worktree.cached_body(&path) else {
                continue;
            };
            if worktree.warm(&path, &body).is_ok() {
                warmed += 1;
            }
        }
    }

    /// A file body, and the record that this run really read it.
    ///
    /// Nothing here is ever truncated. Part of a file reads exactly like all
    /// of it, and a caller who does not know a line is missing will conclude
    /// the thing on that line does not exist. So both ceilings answer with a
    /// refusal carrying the numbers, and choosing what to leave out is left
    /// to whoever asked: `suggest_local_read` says how big the file is, and a
    /// line range says which part of it to hand back.
    fn file(
        &self,
        tool: &str,
        path: &str,
        range: Option<LineRange>,
        body: String,
    ) -> Result<ToolOutput, ToolError> {
        let stat = Stat::of(&body);
        let fetch_ceiling = self.limits.max_file_bytes;
        if range.is_none() && stat.bytes > fetch_ceiling {
            return Err(ToolError::Rejected {
                tool: tool.to_string(),
                reason: format!(
                    "{path} is {} bytes over {} lines, past max_file_bytes ({fetch_ceiling}); \
                     no part of it can be read, because reading any part means reading all of it. \
                     Use a search to find what you need instead",
                    stat.bytes, stat.lines
                ),
            });
        }
        let shown = match range {
            Some(range) => range.slice(&body),
            None => body,
        };
        let room = self.limits.max_output_bytes;
        if shown.len() as u64 > room {
            let asked = match range {
                Some(range) => format!("lines {}-{} of {path} are", range.first, range.last),
                None => format!("{path} is {} lines and", stat.lines),
            };
            return Err(ToolError::Rejected {
                tool: tool.to_string(),
                reason: format!(
                    "{asked} {} bytes, over the {room} one read may return. \
                     Nothing is truncated: ask for a line range instead, {} lines at a time \
                     around what you need. {}",
                    shown.len(),
                    stat.suggested_window(room),
                    stat.describe(path)
                ),
            });
        }
        let first = range.map(|range| range.first).unwrap_or(1);
        let last = match shown.lines().count() as u32 {
            0 => first,
            counted => first + counted - 1,
        };
        Ok(
            ToolOutput::new(shown.clone()).with_context_file(ContextFile {
                path: path.to_string(),
                first_line: first,
                last_line: last,
                body: shown,
            }),
        )
    }

    /// What `suggest_local_read` answers: the numbers needed to plan reads,
    /// and nothing else. Deliberately tiny, so asking is always cheaper than
    /// finding out by being refused.
    fn stat(&self, path: &str, local: crate::worktree::Stat) -> ToolOutput {
        let room = self.limits.max_output_bytes;
        let fetch_ceiling = self.limits.max_file_bytes;
        let (numbers, verdict) = match local.lines {
            None => (
                format!("{path}: {} bytes.", local.bytes),
                format!(
                    "cannot be read — over max_file_bytes ({fetch_ceiling} bytes), no range of \
                     it can be read either; use a search instead"
                ),
            ),
            Some(lines) if local.bytes > room => {
                let helper = Stat {
                    bytes: local.bytes,
                    lines,
                };
                let window = helper.suggested_window(room);
                let ranges = lines.div_ceil(window);
                (
                    format!("{path}: {} bytes, {lines} lines.", local.bytes),
                    format!(
                        "read in ranges — {window} lines per range, {ranges} ranges, first \
                         range 1-{window}"
                    ),
                )
            }
            Some(lines) => (
                format!("{path}: {} bytes, {lines} lines.", local.bytes),
                format!("reads whole — one read_local_file with no range will do ({lines} lines)"),
            ),
        };
        ToolOutput::new(format!("{numbers}\n{verdict}"))
    }

    fn text(&self, text: String) -> ToolOutput {
        let cut = truncate(&text, self.limits.max_output_bytes as usize);
        ToolOutput::clipped(cut.text, cut.omitted_bytes)
    }
}

/// The size of a file, in both units that matter: bytes bound what one read
/// may return, lines are what a range is written in and what a finding is
/// filed against.
#[derive(Clone, Copy)]
struct Stat {
    bytes: u64,
    lines: u32,
}

impl Stat {
    fn of(body: &str) -> Self {
        Self {
            bytes: body.len() as u64,
            lines: body.lines().count() as u32,
        }
    }

    /// How many lines are likely to fit in `room` bytes, from this file's own
    /// average line length rather than a fixed guess: 80 lines of a header
    /// and 80 lines of generated JSON are not the same amount of text. Kept
    /// well under the ceiling, since the average is not the maximum.
    fn suggested_window(self, room: u64) -> u32 {
        if self.lines == 0 || self.bytes == 0 {
            return 1;
        }
        let per_line = (self.bytes / u64::from(self.lines)).max(1);
        let fits = room * 3 / 4 / per_line;
        u32::try_from(fits).unwrap_or(u32::MAX).clamp(1, self.lines)
    }

    fn describe(self, path: &str) -> String {
        format!("{path}: {} bytes, {} lines.", self.bytes, self.lines)
    }
}

/// Normalize, deny list, extension whitelist, the symlink rule, and the
/// resolved path still has to sit under the worktree root. The condition that
/// would refuse this run is asked first, because `root` is `None` on Empty
/// and a tool that skipped the condition would have nothing to check against.
fn checked_path(
    tool: &str,
    paths: &PathPolicy,
    worktree: &Worktree,
    given: &str,
) -> Result<String, ToolError> {
    let root = match worktree.root() {
        Some(root) => root,
        None => {
            return Err(ToolError::Unavailable {
                tool: tool.to_string(),
                reason: match no_files(worktree) {
                    Some(reason) => reason.to_string(),
                    None => "this run has no worktree directory".to_string(),
                },
            });
        }
    };
    paths
        .check_worktree_path(given, root)
        .map_err(|rejection| ToolError::Rejected {
            tool: tool.to_string(),
            reason: rejection.to_string(),
        })
}

/// A pattern the model may point a listing or a search at. `PathPolicy`
/// answers about paths and a glob is not one, so the shape rules are spelled
/// out here and the deny list is asked about the pattern itself: a listing
/// aimed into a denied directory would hand back the very names being kept.
fn check_glob(paths: &PathPolicy, tool: &str, given: &str) -> Result<String, ToolError> {
    let rejected = |reason: String| ToolError::Rejected {
        tool: tool.to_string(),
        reason,
    };
    if given.starts_with('/') {
        return Err(rejected(format!(
            "{given}: absolute globs are not accepted; give a repository relative one"
        )));
    }
    if given.split('/').any(|segment| segment == "..") {
        return Err(rejected(format!("{given}: `..` is not accepted")));
    }
    if paths.denies_glob(given) {
        return Err(rejected(format!("{given}: denied by deny_paths")));
    }
    Glob::new(given).map_err(|error| ToolError::InvalidArguments {
        tool: tool.to_string(),
        reason: format!("invalid glob {given:?}: {error}"),
    })?;
    Ok(given.to_string())
}

fn glob_signature() -> Signature {
    Signature::new(vec![Parameter::required(
        "glob",
        Shape::text(),
        "Repository-relative glob, same syntax as deny_paths, e.g. src/**/*.c",
    )])
}

fn fetch_signature() -> Signature {
    Signature::new(vec![Parameter::required(
        "paths",
        Shape::Array(Box::new(Shape::Path)),
        "Repository-relative paths to fetch, e.g. [\"src/parse.c\", \"src/lex.c\"]",
    )])
}

/// `ranged` is false for a size, which is a fact about the whole file and has
/// no range to ask about.
fn path_signature(ranged: bool) -> Signature {
    let mut parameters = vec![Parameter::required(
        "path",
        Shape::Path,
        "Repository-relative path, e.g. src/parse.c",
    )];
    if ranged {
        parameters.push(Parameter::optional(
            "first_line",
            Shape::counting(),
            "Start of the line range; supply with last_line",
        ));
        parameters.push(Parameter::optional(
            "last_line",
            Shape::counting(),
            "End of the line range; supply with first_line",
        ));
    }
    Signature::new(parameters)
}

fn search_signature() -> Signature {
    Signature::new(vec![
        Parameter::required("query", Shape::text(), "What to search for"),
        Parameter::optional(
            "glob",
            Shape::text(),
            "Optional. Limit the search to files matching this glob",
        ),
    ])
}

/// Both ends of a range come together. Half a range would have to be guessed
/// at, and a guess about how much of a file the model wanted is the kind of
/// thing it cannot notice going wrong.
fn range_of(tool: &str, arguments: &Arguments) -> Result<Option<LineRange>, ToolError> {
    let first = arguments.integer("first_line");
    let last = arguments.integer("last_line");
    match (first, last) {
        (None, None) => Ok(None),
        (Some(first), Some(last)) if last >= first => Ok(Some(LineRange {
            first: first as u32,
            last: last as u32,
        })),
        (Some(first), Some(last)) => Err(ToolError::InvalidArguments {
            tool: tool.to_string(),
            reason: format!(
                "first_line {first} and last_line {last} are not a range: last_line cannot be \
                 smaller than first_line"
            ),
        }),
        _ => Err(ToolError::InvalidArguments {
            tool: tool.to_string(),
            reason: "first_line and last_line go together; give both or neither".to_string(),
        }),
    }
}

pub struct ListLocalFiles {
    worktree: Arc<Worktree>,
    paths: PathPolicy,
    limits: ToolLimits,
    description: String,
    signature: Signature,
}

impl ListLocalFiles {
    pub const NAME: &'static str = "list_local_files";

    pub fn new(worktree: Arc<Worktree>, paths: PathPolicy, limits: ToolLimits) -> Self {
        let description = Self::describe(&worktree, limits);
        Self {
            worktree,
            paths,
            limits,
            description,
            signature: glob_signature(),
        }
    }

    fn describe(worktree: &Worktree, limits: ToolLimits) -> String {
        const WHAT: &str =
            "List the paths on the local side of this run's worktree matching a glob.";
        if no_files(worktree).is_some() {
            return unavailable_description(WHAT, NO_FILES_SHORT);
        }
        let source = if worktree.is_checkout() {
            "It scans the checkout, so the listing is complete and a path missing from it is not \
             there."
        } else {
            "It lists only what has been fetched so far, so a path missing from it may still be \
             in the repository — fetch_repo_file first, or list_repo_files to see the whole tree."
        };
        format!(
            "{WHAT} {} {source} A size that is there was handed over in one go; a size that is \
             missing does not mean the file cannot be measured. At most {} paths; overflow says how \
             many remain.",
            local_side(worktree),
            limits.max_files_per_listing,
        )
    }
}

impl Tool for ListLocalFiles {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Content
    }

    fn rounds(&self) -> &'static [Round] {
        INVESTIGATION
    }

    fn unavailable(&self) -> Option<&str> {
        no_files(&self.worktree)
    }

    fn precondition(&self) -> Option<&'static str> {
        Some(NO_FILES_PRECONDITION)
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let glob = check_glob(
            &self.paths,
            self.name(),
            checked.text("glob").unwrap_or_default(),
        )?;
        let listing = self
            .worktree
            .list_local(&glob)
            .map_err(|error| ToolError::failed(self.name(), error))?;
        let extra = self.worktree.is_cache().then_some(
            "this listed the files that are local right now; a path missing from it may \
             still be in the repository — try list_repo_files, or fetch the relevant files \
             first",
        );
        Ok(Answer::from(&self.paths, self.limits).listing(listing.files, listing.complete, extra))
    }
}

/// Size without content. The body still has to be read to count lines when
/// the file fits — no platform reports those — but only the numbers come
/// back, so this stays affordable for a file far too big to read.
pub struct SuggestLocalRead {
    worktree: Arc<Worktree>,
    paths: PathPolicy,
    limits: ToolLimits,
    description: String,
    signature: Signature,
}

impl SuggestLocalRead {
    pub const NAME: &'static str = "suggest_local_read";

    pub fn new(worktree: Arc<Worktree>, paths: PathPolicy, limits: ToolLimits) -> Self {
        let description = Self::describe(&worktree, limits);
        Self {
            worktree,
            paths,
            limits,
            description,
            signature: path_signature(false),
        }
    }

    fn describe(worktree: &Worktree, limits: ToolLimits) -> String {
        const WHAT: &str = "How to read one local file: reads whole, read in ranges with the cuts \
                            already made, or cannot be read. Advice, not a mandate — you may still \
                            ask for any range you want.";
        if no_files(worktree).is_some() {
            return unavailable_description(WHAT, NO_FILES_SHORT);
        }
        format!(
            "{WHAT} {} Returns the conclusion and the numbers it was computed from, never file \
             content, so it is cheap. Ask this before reading a file whose size you do not know: one \
             call here turns one refused read into a plan, against the {} bytes a single answer may \
             carry. On a cache, a file that is not on disk yet has to be fetched with fetch_repo_file \
             first.",
            local_side(worktree),
            limits.max_output_bytes,
        )
    }
}

impl Tool for SuggestLocalRead {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Content
    }

    fn rounds(&self) -> &'static [Round] {
        INVESTIGATION
    }

    fn unavailable(&self) -> Option<&str> {
        no_files(&self.worktree)
    }

    fn precondition(&self) -> Option<&'static str> {
        Some(NO_FILES_PRECONDITION)
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let path = checked_path(
            self.name(),
            &self.paths,
            &self.worktree,
            checked.text("path").unwrap_or_default(),
        )?;
        let stat = self
            .worktree
            .stat_local(&path, self.limits.max_file_bytes)
            .map_err(|error| ToolError::failed_read(self.name(), &path, error))?;
        Ok(Answer::from(&self.paths, self.limits).stat(&path, stat))
    }
}

pub struct ReadLocalFile {
    worktree: Arc<Worktree>,
    paths: PathPolicy,
    limits: ToolLimits,
    description: String,
    signature: Signature,
}

impl ReadLocalFile {
    pub const NAME: &'static str = "read_local_file";

    pub fn new(worktree: Arc<Worktree>, paths: PathPolicy, limits: ToolLimits) -> Self {
        let description = Self::describe(&worktree, limits);
        Self {
            worktree,
            paths,
            limits,
            description,
            signature: path_signature(true),
        }
    }

    fn describe(worktree: &Worktree, limits: ToolLimits) -> String {
        const WHAT: &str = "Read one file off the local side of this run's worktree.";
        if no_files(worktree).is_some() {
            return unavailable_description(WHAT, NO_FILES_SHORT);
        }
        let fetching = if worktree.is_cache() {
            " A file not on disk yet is a miss, not a fetch: call fetch_repo_file first, then \
             read it. Reading it again after that is free."
        } else {
            ""
        };
        format!(
            "{WHAT} {}{fetching} Optional line range (first_line \
             and last_line together); without one you get the whole file. Nothing is ever truncated: \
             a file over {} bytes cannot be read at all, and a single answer may carry at most {} \
             bytes, so a read that would not fit is refused with the file's size. Ask \
             suggest_local_read first when you do not know how big a file is, and page through a \
             large one in line ranges. The path must be one the diff or a listing gave you: a \
             guessed path that misses costs a whole round.",
            local_side(worktree),
            limits.max_file_bytes,
            limits.max_output_bytes,
        )
    }
}

impl Tool for ReadLocalFile {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Content
    }

    fn rounds(&self) -> &'static [Round] {
        INVESTIGATION
    }

    fn unavailable(&self) -> Option<&str> {
        no_files(&self.worktree)
    }

    fn precondition(&self) -> Option<&'static str> {
        Some(NO_FILES_PRECONDITION)
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let path = checked_path(
            self.name(),
            &self.paths,
            &self.worktree,
            checked.text("path").unwrap_or_default(),
        )?;
        let range = range_of(self.name(), &checked)?;
        // The whole file first: `max_file_bytes` is a statement about the
        // file, and a range must not become a way to read a slice of one that
        // is too big to read. `read_local` enforces that from the directory
        // entry, so the body is never taken for a file past the ceiling.
        let body = self
            .worktree
            .read_local(&path, None, self.limits.max_file_bytes)
            .map_err(|error| ToolError::failed_read(self.name(), &path, error))?;
        Answer::from(&self.paths, self.limits).file(self.name(), &path, range, body)
    }
}

pub struct SearchLocalRegex {
    worktree: Arc<Worktree>,
    paths: PathPolicy,
    limits: ToolLimits,
    description: String,
    signature: Signature,
}

impl SearchLocalRegex {
    pub const NAME: &'static str = "search_local_regex";

    pub fn new(worktree: Arc<Worktree>, paths: PathPolicy, limits: ToolLimits) -> Self {
        let description = Self::describe(&worktree, limits);
        Self {
            worktree,
            paths,
            limits,
            description,
            signature: search_signature(),
        }
    }

    fn describe(worktree: &Worktree, limits: ToolLimits) -> String {
        const WHAT: &str =
            "Search the local side of this run's worktree with a regular expression.";
        if no_files(worktree).is_some() {
            return unavailable_description(WHAT, NO_FILES_SHORT);
        }
        let strength = if worktree.is_checkout() {
            "query is a regular expression matched over every file in the checkout, so a miss \
             usually means it is not there."
        } else {
            "query is a regular expression matched over the files that have been fetched so far, \
             not over the repository. A miss does not mean it is absent from the repository — \
             search_repo_regex or search_repo_keyword, or fetch the relevant files and search \
             again."
        };
        format!(
            "{WHAT} {} {strength} Optional glob to limit the files. At most {} hits, grouped by \
             file with counts; files whose hits were cut are named with their counts.",
            local_side(worktree),
            limits.max_hits_per_search,
        )
    }
}

impl Tool for SearchLocalRegex {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Content
    }

    fn rounds(&self) -> &'static [Round] {
        INVESTIGATION
    }

    fn unavailable(&self) -> Option<&str> {
        no_files(&self.worktree)
    }

    fn precondition(&self) -> Option<&'static str> {
        Some(NO_FILES_PRECONDITION)
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let query = checked.text("query").unwrap_or_default();
        let glob = match checked.text("glob") {
            Some(given) => Some(check_glob(&self.paths, self.name(), given)?),
            None => None,
        };
        let scan = self
            .worktree
            .search_local(query, glob.as_deref())
            .map_err(|error| ToolError::failed(self.name(), error))?;
        let mut extras = Vec::new();
        if self.worktree.is_cache() {
            extras.push(format!(
                "this searched the {} files that are local right now; a miss does not mean the \
                 repository lacks it — try search_repo_*, or fetch the relevant files first and \
                 search again",
                scan.searched
            ));
        }
        if scan.skipped > 0 {
            extras.push(format!("skipped {} non-text files", scan.skipped));
        }
        Ok(Answer::from(&self.paths, self.limits).hits(scan.hits, None, &extras))
    }
}

pub struct ListRepoFiles {
    worktree: Arc<Worktree>,
    paths: PathPolicy,
    limits: ToolLimits,
    description: String,
    signature: Signature,
}

impl ListRepoFiles {
    pub const NAME: &'static str = "list_repo_files";

    pub fn new(worktree: Arc<Worktree>, paths: PathPolicy, limits: ToolLimits) -> Self {
        let description = Self::describe(&worktree, limits);
        Self {
            worktree,
            paths,
            limits,
            description,
            signature: glob_signature(),
        }
    }

    fn describe(worktree: &Worktree, limits: ToolLimits) -> String {
        const WHAT: &str =
            "List the paths in the repository at the reviewed commit matching a glob.";
        if no_repo(worktree).is_some() {
            return unavailable_description(WHAT, NO_REPO_SHORT);
        }
        let source = if worktree.is_checkout() {
            "It asks the platform API about the reviewed commit. The local side already holds \
             the whole project, so this is worth it when you want the platform's view rather \
             than another scan of the checkout."
        } else {
            "It asks the platform API about the reviewed commit rather than about what has been \
             fetched so far, so it answers for the whole repository. An answer the platform \
             could not complete says so."
        };
        format!(
            "{WHAT} {} {source} A size that is there was handed over in one go; a size that is \
             missing does not mean the file cannot be measured. At most {} paths; overflow says how \
             many remain.",
            local_side(worktree),
            limits.max_files_per_listing,
        )
    }
}

impl Tool for ListRepoFiles {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Content
    }

    fn rounds(&self) -> &'static [Round] {
        INVESTIGATION
    }

    fn unavailable(&self) -> Option<&str> {
        no_repo(&self.worktree)
    }

    fn precondition(&self) -> Option<&'static str> {
        Some(NO_REPO_PRECONDITION)
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let glob = check_glob(
            &self.paths,
            self.name(),
            checked.text("glob").unwrap_or_default(),
        )?;
        let listing = self
            .worktree
            .list_repo(&glob)
            .map_err(|error| ToolError::failed(self.name(), error))?;
        Ok(Answer::from(&self.paths, self.limits).listing(listing.files, listing.complete, None))
    }
}

pub struct FetchRepoFile {
    worktree: Arc<Worktree>,
    paths: PathPolicy,
    limits: ToolLimits,
    description: String,
    signature: Signature,
}

impl FetchRepoFile {
    pub const NAME: &'static str = "fetch_repo_file";

    pub fn new(worktree: Arc<Worktree>, paths: PathPolicy, limits: ToolLimits) -> Self {
        let description = Self::describe(&worktree, limits);
        Self {
            worktree,
            paths,
            limits,
            description,
            signature: fetch_signature(),
        }
    }

    fn describe(worktree: &Worktree, limits: ToolLimits) -> String {
        const WHAT: &str = "Fetch files from the repository at the reviewed commit onto the local \
             side. Takes a list of paths. Returns one line per path: the byte size on success, the \
             reason on failure. A batch is not failed as a whole. Never the body — read_local_file \
             is the other half.";
        if no_repo(worktree).is_some() {
            return unavailable_description(WHAT, NO_REPO_SHORT);
        }
        let landing = if worktree.is_checkout() {
            "The checkout already is the whole tree at the reviewed commit, so a path missing \
             from it is one the repository does not have, and nothing is written."
        } else {
            "Each file is written into this run's cache. A file already on disk is the answer, \
             and costs no request."
        };
        format!(
            "{WHAT} {} {landing} A file over {} bytes is refused without being downloaded. At most \
             {} paths; a longer list is refused without fetching any.",
            local_side(worktree),
            limits.max_file_bytes,
            limits.max_files_per_fetch,
        )
    }
}

impl Tool for FetchRepoFile {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Content
    }

    fn rounds(&self) -> &'static [Round] {
        INVESTIGATION
    }

    fn unavailable(&self) -> Option<&str> {
        no_repo(&self.worktree)
    }

    fn precondition(&self) -> Option<&'static str> {
        Some(NO_REPO_PRECONDITION)
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let asked = checked.texts("paths");
        if asked.is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: self.name().to_string(),
                reason: "paths is empty".to_string(),
            });
        }
        let cap = self.limits.max_files_per_fetch;
        if asked.len() > cap {
            return Err(ToolError::Rejected {
                tool: self.name().to_string(),
                reason: format!(
                    "this call asked for {} files, past max_files_per_fetch ({cap}); nothing \
                     was fetched. Ask for at most that many",
                    asked.len()
                ),
            });
        }
        let lines: Vec<String> = asked.iter().map(|path| self.fetch_one(path)).collect();
        Ok(Answer::from(&self.paths, self.limits).text(lines.join("\n")))
    }
}

impl FetchRepoFile {
    fn fetch_one(&self, given: &str) -> String {
        let path = match checked_path(self.name(), &self.paths, &self.worktree, given) {
            Ok(path) => path,
            Err(ToolError::Rejected { reason, .. })
            | Err(ToolError::InvalidArguments { reason, .. })
            | Err(ToolError::Unavailable { reason, .. }) => return reason,
            Err(error) => return format!("{given}: {error}"),
        };
        match self.worktree.fetch(&path, self.limits.max_file_bytes) {
            Ok(fetched) => format!("{}: {} bytes.", fetched.path, fetched.bytes),
            Err(WorktreeError::TooBig { bytes }) => format!(
                "{path} is {bytes} bytes, past max_file_bytes; \
                 no part of it can be read, because reading any part means reading all of it. \
                 Use a search to find what you need instead"
            ),
            Err(error) => error.to_string(),
        }
    }
}

pub struct SearchRepoRegex {
    worktree: Arc<Worktree>,
    paths: PathPolicy,
    limits: ToolLimits,
    description: String,
    signature: Signature,
}

impl SearchRepoRegex {
    pub const NAME: &'static str = "search_repo_regex";

    pub fn new(worktree: Arc<Worktree>, paths: PathPolicy, limits: ToolLimits) -> Self {
        let description = Self::describe(&worktree, limits);
        Self {
            worktree,
            paths,
            limits,
            description,
            signature: search_signature(),
        }
    }

    fn describe(worktree: &Worktree, limits: ToolLimits) -> String {
        const WHAT: &str =
            "Search the repository at the reviewed commit with a regular expression.";
        if no_regex_search(worktree).is_some() {
            return unavailable_description(WHAT, NO_REGEX_SHORT);
        }
        let strength = if worktree.is_checkout() {
            "query is a regular expression. The platform index covers the default branch, while \
             the local side stands on the reviewed commit's head_sha, so a miss here is not the \
             same as a miss from search_local_regex — ask the local side when you need to know \
             what this commit actually has."
        } else {
            "query is a regular expression, matched by the platform over the reviewed commit, so \
             a miss usually means it is not there."
        };
        format!(
            "{WHAT} {} {strength} Optional glob to limit the files. At most {} hits, grouped by \
             file with counts; files whose hits were cut are named with their counts.",
            local_side(worktree),
            limits.max_hits_per_search,
        )
    }
}

impl Tool for SearchRepoRegex {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Content
    }

    fn rounds(&self) -> &'static [Round] {
        INVESTIGATION
    }

    fn unavailable(&self) -> Option<&str> {
        no_regex_search(&self.worktree)
    }

    fn precondition(&self) -> Option<&'static str> {
        Some(NO_REGEX_PRECONDITION)
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let query = checked.text("query").unwrap_or_default();
        let glob = match checked.text("glob") {
            Some(given) => Some(check_glob(&self.paths, self.name(), given)?),
            None => None,
        };
        let hits = self
            .worktree
            .search_repo(SearchKind::Regex, query, glob.as_deref())
            .map_err(|error| ToolError::failed(self.name(), error))?;
        let answer = Answer::from(&self.paths, self.limits);
        answer.warm(&self.worktree, self.name(), &hits);
        Ok(answer.hits(hits, None, &[]))
    }
}

/// Keyword index over the default branch. A miss says nothing about the
/// branch under review, so an empty answer says that rather than just being
/// empty.
pub struct SearchRepoKeyword {
    worktree: Arc<Worktree>,
    paths: PathPolicy,
    limits: ToolLimits,
    description: String,
    signature: Signature,
}

impl SearchRepoKeyword {
    pub const NAME: &'static str = "search_repo_keyword";

    const EMPTY_KEYWORD_NOTE: &'static str = "A miss does not mean it is absent: this search used a keyword index that only covers \
         the default branch, so something new on this branch may not be indexed. To confirm \
         presence, use list_repo_files, fetch_repo_file and read_local_file, or \
         search_local_regex.";

    pub fn new(worktree: Arc<Worktree>, paths: PathPolicy, limits: ToolLimits) -> Self {
        let description = Self::describe(&worktree, limits);
        Self {
            worktree,
            paths,
            limits,
            description,
            signature: search_signature(),
        }
    }

    fn describe(worktree: &Worktree, limits: ToolLimits) -> String {
        const WHAT: &str = "Search the repository with the platform's keyword index.";
        if no_keyword_search(worktree).is_some() {
            return unavailable_description(WHAT, NO_KEYWORD_SHORT);
        }
        let contrast = if worktree.is_checkout() {
            " The local side stands on the reviewed commit's head_sha, so a miss here is not \
             the same as a miss from search_local_regex."
        } else {
            ""
        };
        format!(
            "{WHAT} {} query is case-insensitive keyword matching against the platform's index; \
             regex metacharacters are literal, not a regex. That index covers the default branch \
             only, so a miss does not mean it is absent — confirm with list_repo_files, \
             fetch_repo_file and read_local_file, or search_local_regex on files you have, before \
             concluding anything from an empty result.{contrast} Optional glob to limit the files. \
             At most {} hits, grouped by file with counts; files whose hits were cut are named with \
             their counts.",
            local_side(worktree),
            limits.max_hits_per_search,
        )
    }
}

impl Tool for SearchRepoKeyword {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Content
    }

    fn rounds(&self) -> &'static [Round] {
        INVESTIGATION
    }

    fn unavailable(&self) -> Option<&str> {
        no_keyword_search(&self.worktree)
    }

    fn precondition(&self) -> Option<&'static str> {
        Some(NO_KEYWORD_PRECONDITION)
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let query = checked.text("query").unwrap_or_default();
        let glob = match checked.text("glob") {
            Some(given) => Some(check_glob(&self.paths, self.name(), given)?),
            None => None,
        };
        let hits = self
            .worktree
            .search_repo(SearchKind::Keyword, query, glob.as_deref())
            .map_err(|error| ToolError::failed(self.name(), error))?;
        let answer = Answer::from(&self.paths, self.limits);
        answer.warm(&self.worktree, self.name(), &hits);
        Ok(answer.hits(hits, Some(Self::EMPTY_KEYWORD_NOTE), &[]))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::config::SecuritySettings;
    use crate::platform::{Capabilities, File, Listing, Repo, RepoSource, SearchHit};
    use crate::platform::{PlatformError, SearchKind};

    /// Counts every question it is asked. That is the whole point of this
    /// fake: an out of bounds argument has to be refused *before* the
    /// worktree is reached, and only a counter can tell that from a worktree
    /// that was asked and happened to answer with nothing.
    struct CountingRepository {
        calls: AtomicUsize,
        reads: AtomicUsize,
        listing: Listing,
        body: String,
        bodies: BTreeMap<String, String>,
        held: Mutex<BTreeMap<String, String>>,
        hits: Vec<SearchHit>,
        capabilities: Capabilities,
    }

    impl CountingRepository {
        fn empty() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                reads: AtomicUsize::new(0),
                listing: Listing {
                    files: Vec::new(),
                    complete: true,
                },
                body: String::new(),
                bodies: BTreeMap::new(),
                held: Mutex::new(BTreeMap::new()),
                hits: Vec::new(),
                capabilities: Capabilities::all(),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }

        fn body_of(&self, path: &str) -> String {
            self.bodies
                .get(path)
                .cloned()
                .unwrap_or_else(|| self.body.clone())
        }
    }

    impl RepoSource for CountingRepository {
        fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.listing.clone())
        }

        fn read_file(
            &self,
            path: &str,
            _lines: Option<LineRange>,
        ) -> Result<String, PlatformError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.body_of(path))
        }

        fn size(&self, path: &str) -> Result<u64, PlatformError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.body_of(path).len() as u64)
        }

        fn search(
            &self,
            _kind: SearchKind,
            _query: &str,
            _glob: Option<&str>,
        ) -> Result<Vec<SearchHit>, PlatformError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.hits.clone())
        }

        fn cached_body(&self, path: &str) -> Option<String> {
            self.held.lock().expect("held").get(path).cloned()
        }
    }

    fn policy(deny: &[&str], root: Option<&Path>) -> PathPolicy {
        let settings = SecuritySettings {
            deny_paths: deny.iter().map(|pattern| pattern.to_string()).collect(),
            ..SecuritySettings::for_tests()
        };
        PathPolicy::new(&settings, &[], root).expect("valid globs")
    }

    /// The caps have no defaults in config either, so the fixtures name them
    /// once here rather than sprinkling numbers through the tests.
    const LIMITS: ToolLimits = ToolLimits {
        max_file_bytes: 262_144,
        max_output_bytes: 65_536,
        max_files_per_listing: 200,
        max_hits_per_search: 50,
        max_files_per_fetch: 20,
    };

    fn cache(
        repository: Arc<CountingRepository>,
    ) -> (tempfile::TempDir, Arc<Worktree>, Arc<CountingRepository>) {
        let run_dir = tempfile::tempdir().expect("temp dir");
        let capabilities = repository.capabilities;
        let worktree = Worktree::open(
            None,
            Some(Repo::new(
                Arc::clone(&repository) as Arc<dyn RepoSource>,
                capabilities,
            )),
            run_dir.path(),
        )
        .expect("a cache");
        (run_dir, Arc::new(worktree), repository)
    }

    fn checkout() -> (tempfile::TempDir, Arc<Worktree>) {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(directory.path().join("src")).expect("src");
        std::fs::write(
            directory.path().join("src/parse.c"),
            "int main(void)\n{\n}\n",
        )
        .expect("source");
        let worktree = Worktree::open(Some(directory.path().to_path_buf()), None, directory.path())
            .expect("a checkout");
        (directory, Arc::new(worktree))
    }

    fn checkout_with_repo(
        repository: Arc<CountingRepository>,
    ) -> (tempfile::TempDir, Arc<Worktree>, Arc<CountingRepository>) {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(directory.path().join("src")).expect("src");
        std::fs::write(
            directory.path().join("src/parse.c"),
            "int main(void)\n{\n}\n",
        )
        .expect("source");
        let capabilities = repository.capabilities;
        let worktree = Worktree::open(
            Some(directory.path().to_path_buf()),
            Some(Repo::new(
                Arc::clone(&repository) as Arc<dyn RepoSource>,
                capabilities,
            )),
            directory.path(),
        )
        .expect("a checkout");
        (directory, Arc::new(worktree), repository)
    }

    fn tools(worktree: &Arc<Worktree>, deny: &[&str]) -> Vec<Box<dyn Tool>> {
        let paths = policy(deny, worktree.root());
        vec![
            Box::new(ListLocalFiles::new(
                Arc::clone(worktree),
                paths.clone(),
                LIMITS,
            )),
            Box::new(SuggestLocalRead::new(
                Arc::clone(worktree),
                paths.clone(),
                LIMITS,
            )),
            Box::new(ReadLocalFile::new(
                Arc::clone(worktree),
                paths.clone(),
                LIMITS,
            )),
            Box::new(SearchLocalRegex::new(
                Arc::clone(worktree),
                paths.clone(),
                LIMITS,
            )),
            Box::new(ListRepoFiles::new(
                Arc::clone(worktree),
                paths.clone(),
                LIMITS,
            )),
            Box::new(FetchRepoFile::new(
                Arc::clone(worktree),
                paths.clone(),
                LIMITS,
            )),
            Box::new(SearchRepoRegex::new(
                Arc::clone(worktree),
                paths.clone(),
                LIMITS,
            )),
            Box::new(SearchRepoKeyword::new(Arc::clone(worktree), paths, LIMITS)),
        ]
    }

    /// An argument that names a path or a pattern this tool may take, aimed
    /// somewhere it may not go.
    fn out_of_bounds(tool: &str) -> Value {
        match tool {
            ListLocalFiles::NAME | ListRepoFiles::NAME => json!({"glob": "../etc/**"}),
            SuggestLocalRead::NAME | ReadLocalFile::NAME => json!({"path": "../etc/passwd"}),
            FetchRepoFile::NAME => json!({"paths": ["../etc/passwd"]}),
            _ => json!({"query": "password", "glob": "secrets/**"}),
        }
    }

    /// The boundary case for every one of the eight, because the check being
    /// in the tool is the only thing holding the read boundary up: there is
    /// no type that a worktree refuses to be called with (§6 security).
    #[test]
    fn an_out_of_bounds_argument_is_refused_before_the_worktree_is_asked() {
        let repository = Arc::new(CountingRepository::empty());
        let (_run, worktree, repository) = cache(repository);

        for tool in tools(&worktree, &["secrets/**"]) {
            if tool.name() == FetchRepoFile::NAME {
                let output = tool
                    .execute(&out_of_bounds(tool.name()))
                    .unwrap_or_else(|error| panic!("{}: {error}", tool.name()));
                assert!(
                    output.text.contains("..") || output.text.contains("deny_paths"),
                    "{}: {}",
                    tool.name(),
                    output.text
                );
                continue;
            }
            let error = tool
                .execute(&out_of_bounds(tool.name()))
                .expect_err(tool.name());
            assert!(
                matches!(error, ToolError::Rejected { .. }),
                "{}: {error}",
                tool.name()
            );
            assert!(
                error.to_string().contains("..") || error.to_string().contains("deny_paths"),
                "{}: {error}",
                tool.name()
            );
        }
        assert_eq!(repository.calls(), 0, "no read was attempted");
    }

    /// A denied path is dropped from a listing rather than turned into a
    /// placeholder, and the extension whitelist is not applied there at all:
    /// seeing `Makefile` tells the model what kind of project this is, and
    /// trying to read it is refused separately.
    #[test]
    fn listings_drop_denied_paths_and_keep_ones_that_cannot_be_read() {
        let repository = Arc::new(CountingRepository {
            listing: Listing {
                files: vec![
                    File::new("src/parse.c"),
                    File::new("secrets/deploy.toml"),
                    File::new("Makefile"),
                ],
                complete: true,
            },
            ..CountingRepository::empty()
        });
        let (directory, worktree, repository) = cache(repository);
        std::fs::create_dir_all(worktree.root().expect("cache").join("src")).expect("src");
        std::fs::write(
            worktree.root().expect("cache").join("src/parse.c"),
            "int x;\n",
        )
        .expect("source");
        std::fs::create_dir_all(worktree.root().expect("cache").join("secrets")).expect("secrets");
        std::fs::write(
            worktree.root().expect("cache").join("secrets/deploy.toml"),
            "token = 1\n",
        )
        .expect("secret");
        std::fs::write(worktree.root().expect("cache").join("Makefile"), "all:\n").expect("make");
        let paths = policy(&["secrets/**"], worktree.root());

        let listed = ListLocalFiles::new(Arc::clone(&worktree), paths.clone(), LIMITS)
            .execute(&json!({"glob": "**/*"}))
            .expect("listed");
        assert!(listed.text.contains("src/parse.c"), "{}", listed.text);
        assert!(listed.text.contains("Makefile"), "{}", listed.text);
        assert!(!listed.text.contains("secrets/"), "{}", listed.text);

        let listed = ListRepoFiles::new(Arc::clone(&worktree), paths.clone(), LIMITS)
            .execute(&json!({"glob": "**/*"}))
            .expect("listed");
        assert!(listed.text.contains("src/parse.c"), "{}", listed.text);
        assert!(listed.text.contains("Makefile"), "{}", listed.text);
        assert!(!listed.text.contains("secrets/"), "{}", listed.text);

        let refused = ReadLocalFile::new(Arc::clone(&worktree), paths, LIMITS)
            .execute(&json!({"path": "Makefile"}))
            .expect_err("no extension");
        assert!(matches!(refused, ToolError::Rejected { .. }), "{refused}");
        assert_eq!(
            repository.calls(),
            1,
            "the refused read never reached the repository; the listing did"
        );
        drop(directory);
    }

    #[test]
    fn a_search_drops_hits_in_denied_files() {
        let repository = Arc::new(CountingRepository {
            hits: vec![
                SearchHit {
                    path: "src/parse.c".to_string(),
                    line: 12,
                    text: "int token = 5;".to_string(),
                },
                SearchHit {
                    path: "secrets/deploy.toml".to_string(),
                    line: 3,
                    text: "token = \"real\"".to_string(),
                },
            ],
            ..CountingRepository::empty()
        });
        let (_run, worktree, _) = cache(repository);
        let output = SearchRepoKeyword::new(
            Arc::clone(&worktree),
            policy(&["secrets/**"], worktree.root()),
            LIMITS,
        )
        .execute(&json!({"query": "token"}))
        .expect("searched");
        assert!(output.text.contains("src/parse.c (1)"), "{}", output.text);
        assert!(
            output.text.contains("12: int token = 5;"),
            "{}",
            output.text
        );
        assert!(!output.text.contains("secrets/"), "{}", output.text);
        assert!(
            output.text.contains("1 hits across 1 files"),
            "the denied file is not in any count: {}",
            output.text
        );
    }

    #[test]
    fn a_listing_past_the_ceiling_says_how_many_are_left() {
        let repository = Arc::new(CountingRepository {
            listing: Listing {
                files: (0..LIMITS.max_files_per_listing + 5)
                    .map(|index| File::new(format!("src/file{index:04}.c")))
                    .collect(),
                complete: true,
            },
            ..CountingRepository::empty()
        });
        let (_run, worktree, _) = cache(repository);
        let output =
            ListRepoFiles::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
                .execute(&json!({"glob": "src/**/*.c"}))
                .expect("listed");
        assert_eq!(
            output.text.lines().count(),
            LIMITS.max_files_per_listing + 1
        );
        assert!(output.text.contains("5 more paths"), "{}", output.text);
    }

    /// A truncated tree is not an empty directory, and the difference has to
    /// reach the model: a half list read as a whole one is how it decides
    /// something does not exist.
    #[test]
    fn an_incomplete_listing_says_so_in_the_words_the_model_reads() {
        let repository = Arc::new(CountingRepository {
            listing: Listing {
                files: vec![File::new("src/parse.c")],
                complete: false,
            },
            ..CountingRepository::empty()
        });
        let (_run, worktree, _) = cache(repository);
        let output =
            ListRepoFiles::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
                .execute(&json!({"glob": "src/**/*.c"}))
                .expect("listed");
        assert!(
            output.text.contains("listing is incomplete"),
            "{}",
            output.text
        );
        assert!(output.text.contains("may still exist"), "{}", output.text);
    }

    /// The trace's context files are what `merge` checks the model's claimed
    /// evidence against, so a real read has to leave one behind.
    #[test]
    fn a_successful_read_records_what_was_fetched_and_narrows_to_the_range() {
        let (_directory, worktree) = checkout();
        let output =
            ReadLocalFile::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
                .execute(&json!({"path": "src/parse.c", "first_line": 2, "last_line": 3}))
                .expect("read");

        assert_eq!(output.text, "{\n}");
        let file = output.context_file.expect("a read is a context file");
        assert_eq!(file.path, "src/parse.c");
        assert_eq!((file.first_line, file.last_line), (2, 3));
        assert_eq!(file.body, "{\n}");
    }

    /// Past `max_file_bytes` the answer is a refusal with the size in it. A
    /// silent clip would read exactly like a short file.
    #[test]
    fn a_file_over_max_file_bytes_is_refused_rather_than_clipped() {
        let (directory, worktree) = checkout();
        std::fs::write(directory.path().join("src/parse.c"), "x".repeat(1_000)).expect("big");
        let paths = policy(&[], worktree.root());
        let limits = ToolLimits {
            max_file_bytes: 100,
            ..LIMITS
        };
        let error = ReadLocalFile::new(Arc::clone(&worktree), paths.clone(), limits)
            .execute(&json!({"path": "src/parse.c"}))
            .expect_err("over the ceiling");
        assert!(matches!(error, ToolError::Rejected { .. }), "{error}");
        assert!(error.to_string().contains("max_file_bytes"), "{error}");
        assert!(error.to_string().contains("1000"), "{error}");
        assert!(
            error.to_string().contains("src/parse.c"),
            "TooBig carries no path; the tool puts it back: {error}"
        );

        // A range is no way around it: the ceiling is a statement about the
        // file, not about the answer.
        assert!(
            ReadLocalFile::new(Arc::clone(&worktree), paths, limits)
                .execute(&json!({"path": "src/parse.c", "first_line": 1, "last_line": 1}))
                .is_err()
        );
    }

    /// The output ceiling used to clip a file body to whatever fitted, which
    /// is the one thing a read must never do: the model gets the head of a
    /// file and no reason to think the rest exists. It is refused instead,
    /// with the numbers needed to ask again for a part that fits.
    #[test]
    fn a_file_too_big_to_return_is_refused_with_the_numbers_to_page_it() {
        let (directory, worktree) = checkout();
        std::fs::write(
            directory.path().join("src/parse.c"),
            "0123456789\n".repeat(400),
        )
        .expect("pageable");
        let paths = policy(&[], worktree.root());
        let limits = ToolLimits {
            max_output_bytes: 500,
            ..LIMITS
        };

        let error = ReadLocalFile::new(Arc::clone(&worktree), paths.clone(), limits)
            .execute(&json!({"path": "src/parse.c"}))
            .expect_err("4400 bytes do not fit in 500");
        let said = error.to_string();
        assert!(matches!(error, ToolError::Rejected { .. }), "{said}");
        assert!(said.contains("400 lines"), "{said}");
        assert!(said.contains("4400 bytes"), "{said}");
        assert!(said.contains("line range"), "{said}");

        let output = ReadLocalFile::new(Arc::clone(&worktree), paths.clone(), limits)
            .execute(&json!({"path": "src/parse.c", "first_line": 1, "last_line": 30}))
            .expect("30 lines fit");
        assert_eq!(output.text.lines().count(), 30);
        assert_eq!(output.omitted_bytes, 0, "a read is never clipped");

        let refused = ReadLocalFile::new(Arc::clone(&worktree), paths, limits)
            .execute(&json!({"path": "src/parse.c", "first_line": 10, "last_line": 300}))
            .expect_err("291 lines do not fit either");
        assert!(refused.to_string().contains("lines 10-300"), "{refused}");
    }

    /// What `suggest_local_read` is for: the size, and what to do about it,
    /// without paying for any of the content. A file too big to read at all
    /// says so, so the model does not spend a round finding out.
    #[test]
    fn stat_answers_with_the_size_and_a_plan_and_no_content() {
        let (directory, worktree) = checkout();
        std::fs::write(
            directory.path().join("src/parse.c"),
            "0123456789\n".repeat(400),
        )
        .expect("pageable");
        let output = SuggestLocalRead::new(
            Arc::clone(&worktree),
            policy(&[], worktree.root()),
            ToolLimits {
                max_output_bytes: 500,
                ..LIMITS
            },
        )
        .execute(&json!({"path": "src/parse.c"}))
        .expect("stat");
        assert!(
            output.text.contains("4400 bytes, 400 lines"),
            "{}",
            output.text
        );
        assert!(output.text.contains("read in ranges —"), "{}", output.text);
        assert!(output.text.contains("first range 1-"), "{}", output.text);
        assert!(
            !output.text.contains("0123456789"),
            "no content: {}",
            output.text
        );
        assert!(
            output.context_file.is_none(),
            "a size is not a file this run read"
        );

        let output = SuggestLocalRead::new(
            Arc::clone(&worktree),
            policy(&[], worktree.root()),
            ToolLimits {
                max_file_bytes: 100,
                ..LIMITS
            },
        )
        .execute(&json!({"path": "src/parse.c"}))
        .expect("stat still answers for a file too big to read");
        assert!(output.text.contains("cannot be read"), "{}", output.text);
        assert!(output.text.contains("max_file_bytes"), "{}", output.text);
        assert!(output.text.contains("4400 bytes"), "{}", output.text);
    }

    #[test]
    fn half_a_line_range_is_answered_rather_than_guessed_at() {
        let repository = Arc::new(CountingRepository::empty());
        let (_run, worktree, repository) = cache(repository);
        let error = ReadLocalFile::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
            .execute(&json!({"path": "src/parse.c", "first_line": 4}))
            .expect_err("half a range");
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error}"
        );
        assert_eq!(repository.calls(), 0);
    }

    /// The names are fixed now, so the description is the only thing that can
    /// say what this run's worktree is and how far a search reaches. Every
    /// shape has to say it, because the model reads only the description.
    #[test]
    fn the_descriptions_say_which_worktree_this_is_and_how_far_it_reaches() {
        let (_directory, whole, _) = checkout_with_repo(Arc::new(CountingRepository::empty()));
        let read = ReadLocalFile::new(Arc::clone(&whole), policy(&[], whole.root()), LIMITS);
        assert_eq!(read.name(), "read_local_file");
        assert!(read.description().contains("checkout under review"));
        assert!(
            read.description().contains("262144"),
            "the fetch ceiling is a number, not a word: {}",
            read.description()
        );
        assert!(
            SearchLocalRegex::new(Arc::clone(&whole), policy(&[], whole.root()), LIMITS)
                .description()
                .contains("regular expression")
        );
        assert!(
            SearchLocalRegex::new(Arc::clone(&whole), policy(&[], whole.root()), LIMITS)
                .description()
                .contains("usually means it is not there")
        );
        let remote = SearchRepoKeyword::new(Arc::clone(&whole), policy(&[], whole.root()), LIMITS);
        assert!(remote.description().contains("default branch"));
        assert!(remote.description().contains("head_sha"));

        let (_run, fetched, _) = cache(Arc::new(CountingRepository::empty()));
        let read = ReadLocalFile::new(Arc::clone(&fetched), policy(&[], fetched.root()), LIMITS);
        assert!(
            read.description().contains("started empty"),
            "{}",
            read.description()
        );
        assert!(
            read.description().contains("fetch_repo_file"),
            "{}",
            read.description()
        );
        let search =
            SearchLocalRegex::new(Arc::clone(&fetched), policy(&[], fetched.root()), LIMITS);
        assert!(search.description().contains("fetched so far"));
        assert!(search.description().contains("does not mean it is absent"));
        let listing =
            ListLocalFiles::new(Arc::clone(&fetched), policy(&[], fetched.root()), LIMITS);
        assert!(
            listing.description().contains("fetched so far"),
            "a local listing on a cache is not the whole tree: {}",
            listing.description()
        );
        let repo_listing =
            ListRepoFiles::new(Arc::clone(&fetched), policy(&[], fetched.root()), LIMITS);
        assert!(
            repo_listing.description().contains("whole repository"),
            "{}",
            repo_listing.description()
        );
    }

    /// The content tools exist on a run that has nothing to read, because the
    /// set of names is not where "what can this run check" is written down.
    /// Both halves of the answer are, though: the description says so before
    /// the call, and the refusal says so after one, and both say the same
    /// thing about what does not follow from it.
    #[test]
    fn an_empty_worktree_says_so_in_the_description_and_again_in_the_refusal() {
        let worktree = Arc::new(Worktree::Empty);

        for tool in tools(&worktree, &[]) {
            assert!(
                tool.description().contains("NOT AVAILABLE THIS RUN"),
                "{}: {}",
                tool.name(),
                tool.description()
            );
            let reason = tool
                .unavailable()
                .unwrap_or_else(|| panic!("{} answers on an empty worktree", tool.name()));
            assert!(
                reason.contains("not about the repository")
                    || reason.contains("not evidence")
                    || reason.contains("not about the code"),
                "{}: {reason}",
                tool.name()
            );
            assert!(reason.contains("this run"), "{}: {reason}", tool.name());
        }
    }

    /// A platform that cannot search leaves the matching repo-search tool
    /// offered and unable to answer. It has to say which of the two it is: an
    /// empty result would read as "not there", and that reading ends up in a
    /// finding.
    #[test]
    fn a_search_nothing_can_answer_says_so_rather_than_coming_back_empty() {
        let repository = Arc::new(CountingRepository {
            capabilities: Capabilities::empty(),
            ..CountingRepository::empty()
        });
        let (_run, worktree, _) = cache(repository);
        let paths = policy(&[], worktree.root());

        let regex = SearchRepoRegex::new(Arc::clone(&worktree), paths.clone(), LIMITS);
        assert!(
            regex
                .unavailable()
                .is_some_and(|reason| reason.contains("not evidence")),
            "{:?}",
            regex.unavailable()
        );
        assert!(regex.description().contains("NOT AVAILABLE THIS RUN"));

        let keyword = SearchRepoKeyword::new(Arc::clone(&worktree), paths.clone(), LIMITS);
        assert!(
            keyword
                .unavailable()
                .is_some_and(|reason| reason.contains("not evidence")),
            "{:?}",
            keyword.unavailable()
        );

        assert!(
            ReadLocalFile::new(Arc::clone(&worktree), paths.clone(), LIMITS)
                .unavailable()
                .is_none()
        );
        assert!(
            ListRepoFiles::new(Arc::clone(&worktree), paths, LIMITS)
                .unavailable()
                .is_none()
        );
    }

    #[test]
    fn keyword_only_refuses_regex_and_answers_keyword() {
        let repository = Arc::new(CountingRepository {
            capabilities: Capabilities::KEYWORD_SEARCH,
            hits: vec![SearchHit {
                path: "src/parse.c".to_string(),
                line: 1,
                text: "int parse(void);".to_string(),
            }],
            ..CountingRepository::empty()
        });
        let (_run, worktree, repository) = cache(repository);
        let paths = policy(&[], worktree.root());

        let regex = SearchRepoRegex::new(Arc::clone(&worktree), paths.clone(), LIMITS);
        assert!(regex.unavailable().is_some());
        assert!(regex.description().contains("NOT AVAILABLE THIS RUN"));

        let keyword = SearchRepoKeyword::new(Arc::clone(&worktree), paths, LIMITS);
        assert!(keyword.unavailable().is_none());
        let output = keyword
            .execute(&json!({"query": "parse"}))
            .expect("keyword search works");
        assert!(output.text.contains("src/parse.c (1)"), "{}", output.text);
        assert!(
            output.text.contains("1: int parse(void);"),
            "{}",
            output.text
        );
        assert_eq!(repository.calls(), 1);
    }

    #[test]
    fn regex_only_refuses_keyword_and_answers_regex() {
        let repository = Arc::new(CountingRepository {
            capabilities: Capabilities::REGEX_SEARCH,
            hits: vec![SearchHit {
                path: "src/parse.c".to_string(),
                line: 1,
                text: "int parse(void);".to_string(),
            }],
            ..CountingRepository::empty()
        });
        let (_run, worktree, repository) = cache(repository);
        let paths = policy(&[], worktree.root());

        let keyword = SearchRepoKeyword::new(Arc::clone(&worktree), paths.clone(), LIMITS);
        assert!(keyword.unavailable().is_some());

        let regex = SearchRepoRegex::new(Arc::clone(&worktree), paths, LIMITS);
        assert!(regex.unavailable().is_none());
        let output = regex
            .execute(&json!({"query": r"int\s+parse"}))
            .expect("regex search works");
        assert!(output.text.contains("src/parse.c (1)"), "{}", output.text);
        assert!(
            output.text.contains("1: int parse(void);"),
            "{}",
            output.text
        );
        assert_eq!(repository.calls(), 1);
    }

    #[test]
    fn both_search_engines_work_when_the_platform_has_both() {
        let repository = Arc::new(CountingRepository {
            capabilities: Capabilities::all(),
            hits: vec![SearchHit {
                path: "src/parse.c".to_string(),
                line: 1,
                text: "int parse(void);".to_string(),
            }],
            ..CountingRepository::empty()
        });
        let (_run, worktree, repository) = cache(repository);
        let paths = policy(&[], worktree.root());

        let regex = SearchRepoRegex::new(Arc::clone(&worktree), paths.clone(), LIMITS);
        let keyword = SearchRepoKeyword::new(Arc::clone(&worktree), paths, LIMITS);
        assert!(regex.unavailable().is_none());
        assert!(keyword.unavailable().is_none());
        regex.execute(&json!({"query": "parse"})).expect("regex");
        keyword
            .execute(&json!({"query": "parse"}))
            .expect("keyword");
        assert_eq!(repository.calls(), 2);
    }

    /// An empty answer from a keyword index that only covers the default
    /// branch says nothing about the branch under review, so the answer says
    /// that instead of just being empty.
    #[test]
    fn an_empty_keyword_search_says_that_it_is_not_a_denial() {
        let (_run, worktree, _) = cache(Arc::new(CountingRepository::empty()));
        let output =
            SearchRepoKeyword::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
                .execute(&json!({"query": "parse_token"}))
                .expect("searched");
        assert!(
            output.text.contains("does not mean it is absent"),
            "{}",
            output.text
        );
    }

    /// The real `Worktree`, because a symlink out of the tree has to be
    /// refused against a real filesystem rather than a fake.
    #[test]
    fn a_read_goes_through_the_worktree_and_stops_at_its_edge() {
        let (_directory, worktree) = checkout();
        let paths = policy(&[], worktree.root());

        let listed = ListLocalFiles::new(Arc::clone(&worktree), paths.clone(), LIMITS)
            .execute(&json!({"glob": "**/*.c"}))
            .expect("listed");
        assert!(
            listed.text.contains("src/parse.c (19 bytes)"),
            "{}",
            listed.text
        );

        let read = ReadLocalFile::new(Arc::clone(&worktree), paths.clone(), LIMITS)
            .execute(&json!({"path": "src/parse.c"}))
            .expect("read");
        assert!(read.text.contains("int main(void)"), "{}", read.text);
        assert_eq!(
            read.context_file.expect("a read is a context file").path,
            "src/parse.c"
        );

        let hits = SearchLocalRegex::new(Arc::clone(&worktree), paths, LIMITS)
            .execute(&json!({"query": r"int\s+main"}))
            .expect("searched");
        assert!(hits.text.contains("src/parse.c (1)"), "{}", hits.text);
        assert!(hits.text.contains("1: int main(void)"), "{}", hits.text);
    }

    /// One out-of-bounds path, one oversized file, one good file: each line
    /// speaks for itself, the good one lands, the batch does not fail.
    #[test]
    fn a_mixed_fetch_batch_answers_each_path_and_lands_only_the_good_one() {
        let repository = Arc::new(CountingRepository {
            bodies: BTreeMap::from([
                ("src/huge.c".to_string(), "x".repeat(1_000)),
                ("src/parse.c".to_string(), "int x;\n".to_string()),
            ]),
            ..CountingRepository::empty()
        });
        let (_run, worktree, repository) = cache(repository);
        let output = FetchRepoFile::new(
            Arc::clone(&worktree),
            policy(&[], worktree.root()),
            ToolLimits {
                max_file_bytes: 100,
                ..LIMITS
            },
        )
        .execute(&json!({"paths": ["../etc/passwd", "src/huge.c", "src/parse.c"]}))
        .expect("per-entry answers");

        let lines: Vec<&str> = output.text.lines().collect();
        assert_eq!(lines.len(), 3, "{}", output.text);
        assert!(lines[0].contains(".."), "{}", output.text);
        assert!(
            lines[1].contains("src/huge.c") && lines[1].contains("1000"),
            "{}",
            output.text
        );
        assert!(
            lines[2].contains("src/parse.c") && lines[2].contains("bytes"),
            "{}",
            output.text
        );

        let root = worktree.root().expect("cache");
        assert!(root.join("src/parse.c").is_file(), "the good file landed");
        assert!(
            !root.join("src/huge.c").exists(),
            "the oversized file did not"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/parse.c")).expect("body"),
            "int x;\n"
        );
        assert!(repository.reads() >= 1, "the good file was fetched");
    }

    #[test]
    fn a_fetch_batch_over_the_cap_is_refused_with_no_request() {
        let repository = Arc::new(CountingRepository::empty());
        let (_run, worktree, repository) = cache(repository);
        let asked: Vec<String> = (0..=LIMITS.max_files_per_fetch)
            .map(|index| format!("src/file{index}.c"))
            .collect();
        let error = FetchRepoFile::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
            .execute(&json!({"paths": asked}))
            .expect_err("over the cap");
        assert!(matches!(error, ToolError::Rejected { .. }), "{error}");
        assert!(error.to_string().contains("max_files_per_fetch"), "{error}");
        assert_eq!(repository.calls(), 0, "not a single request");
    }

    #[test]
    fn listings_print_a_size_only_when_the_source_handed_one_over() {
        let (_directory, worktree) = checkout();
        let local =
            ListLocalFiles::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
                .execute(&json!({"glob": "**/*.c"}))
                .expect("listed");
        assert!(
            local.text.contains("src/parse.c (19 bytes)"),
            "{}",
            local.text
        );

        let with_sizes = Arc::new(CountingRepository {
            listing: Listing {
                files: vec![File {
                    path: "src/parse.c".to_string(),
                    bytes: Some(42),
                }],
                complete: true,
            },
            ..CountingRepository::empty()
        });
        let (_run, worktree, _) = cache(with_sizes);
        let listed =
            ListRepoFiles::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
                .execute(&json!({"glob": "**/*"}))
                .expect("listed");
        assert!(
            listed.text.contains("src/parse.c (42 bytes)"),
            "{}",
            listed.text
        );

        let without_sizes = Arc::new(CountingRepository {
            listing: Listing {
                files: vec![File::new("src/parse.c")],
                complete: true,
            },
            ..CountingRepository::empty()
        });
        let (_run, worktree, _) = cache(without_sizes);
        let listed =
            ListRepoFiles::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
                .execute(&json!({"glob": "**/*"}))
                .expect("listed");
        assert!(listed.text.contains("src/parse.c"), "{}", listed.text);
        assert!(
            !listed.text.contains("bytes"),
            "a missing size is omitted, not guessed: {}",
            listed.text
        );
    }

    /// The cut ranges are the point: the model used to do this arithmetic
    /// itself. The first range named in the advice has to actually read.
    #[test]
    fn suggest_local_read_cuts_ranges_and_the_first_range_reads() {
        let (directory, worktree) = checkout();
        std::fs::write(
            directory.path().join("src/parse.c"),
            "0123456789\n".repeat(400),
        )
        .expect("pageable");
        let paths = policy(&[], worktree.root());
        let limits = ToolLimits {
            max_output_bytes: 500,
            ..LIMITS
        };
        let output = SuggestLocalRead::new(Arc::clone(&worktree), paths.clone(), limits)
            .execute(&json!({"path": "src/parse.c"}))
            .expect("suggested");
        assert!(
            output
                .text
                .contains("read in ranges — 34 lines per range, 12 ranges, first range 1-34"),
            "{}",
            output.text
        );

        let read = ReadLocalFile::new(Arc::clone(&worktree), paths, limits)
            .execute(&json!({"path": "src/parse.c", "first_line": 1, "last_line": 34}))
            .expect("the first range reads");
        assert_eq!(read.text.lines().count(), 34);
        assert_eq!(read.omitted_bytes, 0, "a range is never clipped");
    }

    #[test]
    fn a_truncated_search_names_cut_files_with_counts_and_drops_denied_ones() {
        let mut hits = Vec::new();
        for line in 1..=40 {
            hits.push(SearchHit {
                path: "src/a.c".to_string(),
                line,
                text: format!("hit {line}"),
            });
        }
        for line in 1..=18 {
            hits.push(SearchHit {
                path: "src/gen/tables.c".to_string(),
                line,
                text: format!("row {line}"),
            });
        }
        hits.push(SearchHit {
            path: "secrets/key.toml".to_string(),
            line: 1,
            text: "token = 1".to_string(),
        });
        let repository = Arc::new(CountingRepository {
            hits,
            ..CountingRepository::empty()
        });
        let (_run, worktree, _) = cache(repository);
        let output = SearchRepoRegex::new(
            Arc::clone(&worktree),
            policy(&["secrets/**"], worktree.root()),
            ToolLimits {
                max_hits_per_search: 40,
                ..LIMITS
            },
        )
        .execute(&json!({"query": "hit"}))
        .expect("searched");

        assert!(
            output.text.contains("58 hits across 2 files"),
            "{}",
            output.text
        );
        assert!(output.text.contains("src/a.c (40)"), "{}", output.text);
        assert!(
            output.text.contains("src/gen/tables.c (18)"),
            "{}",
            output.text
        );
        assert!(!output.text.contains("secrets/"), "{}", output.text);
        assert!(
            !output.text.contains("59 hits") && !output.text.contains("3 files"),
            "the denied file is in no count: {}",
            output.text
        );
    }

    #[test]
    fn search_repo_regex_warms_held_bodies_so_the_next_read_is_local() {
        let body = "int parse(void);\n";
        let repository = Arc::new(CountingRepository {
            hits: vec![SearchHit {
                path: "src/parse.c".to_string(),
                line: 1,
                text: "int parse(void);".to_string(),
            }],
            bodies: BTreeMap::from([("src/parse.c".to_string(), body.to_string())]),
            held: Mutex::new(BTreeMap::from([(
                "src/parse.c".to_string(),
                body.to_string(),
            )])),
            ..CountingRepository::empty()
        });
        let (_run, worktree, repository) = cache(repository);
        SearchRepoRegex::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
            .execute(&json!({"query": "parse"}))
            .expect("searched");
        assert!(
            worktree
                .root()
                .expect("cache")
                .join("src/parse.c")
                .is_file(),
            "the hit was warmed"
        );
        let reads = repository.reads();
        let output =
            ReadLocalFile::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
                .execute(&json!({"path": "src/parse.c"}))
                .expect("local");
        assert_eq!(output.text, body);
        assert_eq!(
            repository.reads(),
            reads,
            "the read came off disk, not a second read_file"
        );
    }

    #[test]
    fn a_search_does_not_warm_when_the_source_has_no_cached_body() {
        let repository = Arc::new(CountingRepository {
            hits: vec![SearchHit {
                path: "src/parse.c".to_string(),
                line: 1,
                text: "int parse(void);".to_string(),
            }],
            bodies: BTreeMap::from([("src/parse.c".to_string(), "int parse(void);\n".to_string())]),
            ..CountingRepository::empty()
        });
        let (_run, worktree, repository) = cache(repository);
        SearchRepoRegex::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
            .execute(&json!({"query": "parse"}))
            .expect("searched");
        assert!(
            !worktree.root().expect("cache").join("src/parse.c").exists(),
            "no cached_body means no write"
        );
        assert_eq!(repository.reads(), 0, "warming must not fetch");
    }

    #[test]
    fn search_local_on_a_cache_states_the_corpus_and_skips_binaries() {
        let (_run, worktree, _) = cache(Arc::new(CountingRepository::empty()));
        let root = worktree.root().expect("cache");
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(root.join("src/parse.c"), "int main(void)\n").expect("source");
        std::fs::write(root.join("src/logo.png"), b"\x89PNG\x00int main(void)\n").expect("binary");

        let output =
            SearchLocalRegex::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
                .execute(&json!({"query": "main"}))
                .expect("searched");
        assert!(
            output
                .text
                .contains("this searched the 1 files that are local right now"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("skipped 1 non-text files"),
            "{}",
            output.text
        );
        assert!(output.text.contains("src/parse.c (1)"), "{}", output.text);
        assert!(!output.text.contains("logo.png"), "{}", output.text);

        let listed =
            ListLocalFiles::new(Arc::clone(&worktree), policy(&[], worktree.root()), LIMITS)
                .execute(&json!({"glob": "**/*"}))
                .expect("listed");
        assert!(
            listed
                .text
                .contains("this listed the files that are local right now"),
            "{}",
            listed.text
        );
    }
}
