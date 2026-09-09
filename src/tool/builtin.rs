//! The four content tools: list, size, read, search. One group with one set of
//! names, because there is one place code comes from — this run's worktree.
//!
//! There used to be two groups of the same four, one reading the platform API
//! and one reading a checkout, and the model was expected to tell them apart
//! by name. It cannot: a name says nothing about how deep an answer goes, and
//! a name that changes with the run makes the prompt describe tools that are
//! not registered. So the names are fixed and the *description* carries what
//! varies — where the answer comes from, and what a miss means. It is written
//! from `Reach` when the tool is registered.
//!
//! Every one of them does the same three things in the same order: read the
//! model's arguments, put them through `PathPolicy`, and only then ask the
//! worktree. The check comes first because this is the only place the model
//! can reach the worktree at all, which is the whole of why the read boundary
//! holds (§6 security). The worktree takes plain repository relative paths.

use std::sync::Arc;

use globset::Glob;
use serde_json::Value;

use crate::config::Config;
use crate::platform::LineRange;
use crate::record::ContextFile;
use crate::security::{PathPolicy, truncate};
use crate::worktree::{Content, Reach, Search, WorktreeError, WorktreeSource};

use super::signature::{Arguments, Parameter, Shape, Signature};
use super::{Purpose, Round, Tool, ToolError, ToolOutput};

/// Static facts about a builtin, for `tool list`. The real tools still need a
/// worktree before they can run; this is only what the catalog prints.
pub struct BuiltinSpec {
    pub name: &'static str,
    pub description: String,
    /// Derived from the same declaration the real tool publishes.
    pub parameters: Value,
    /// Whether the worktree has to be a whole checkout for this one.
    pub requires_checkout: bool,
    /// Whether a search has to be answerable at all.
    pub requires_search: bool,
}

/// How much a content tool may fetch and how much it may hand back, all of it
/// from `[review]`. Carried as one value so the four tools cannot drift apart
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
}

impl ToolLimits {
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_file_bytes: config.review.max_file_bytes,
            max_output_bytes: config.review.max_tool_output_bytes,
            max_files_per_listing: config.review.max_files_per_listing as usize,
            max_hits_per_search: config.review.max_hits_per_search as usize,
        }
    }
}

/// What every content tool's description says first: what the worktree is,
/// which is the one thing the names no longer carry.
fn where_from(reach: Reach) -> &'static str {
    match reach.content {
        Content::Checkout => {
            "The worktree is the checkout under review, standing on the reviewed commit, so it \
             holds the whole project."
        }
        Content::Fetched => {
            "The worktree is a directory of this run's own, which started empty and holds the \
             files that have been fetched into it from the platform API at the reviewed commit. \
             It is not a checkout: only what has been asked for is on disk."
        }
        Content::Empty => {
            "The worktree is empty and has nothing behind it, so nothing can be read this run."
        }
    }
}

fn desc_read_file(reach: Reach, limits: ToolLimits) -> String {
    let fetching = match reach.content {
        Content::Fetched => {
            " A file not on disk yet is fetched at the reviewed commit on the first read and \
             kept, so reading it again is free."
        }
        _ => "",
    };
    format!(
        "Read one file out of this run's worktree. {}{fetching} Optional line range (first_line \
         and last_line together); without one you get the whole file. Nothing is ever truncated: \
         a file over {} bytes cannot be read at all, and a single answer may carry at most {} \
         bytes, so a read that would not fit is refused with the file's size. Ask stat_file \
         first when you do not know how big a file is, and page through a large one in line \
         ranges. The path must be one the diff or a listing gave you: a guessed path that misses \
         costs a whole round.",
        where_from(reach),
        limits.max_file_bytes,
        limits.max_output_bytes,
    )
}

fn desc_stat_file(reach: Reach, limits: ToolLimits) -> String {
    format!(
        "Size of one file in this run's worktree, in bytes and lines, plus whether it fits in one \
         read and how many lines to ask for at a time. {} Returns numbers only, never file \
         content, so it is cheap. Ask this before reading a file whose size you do not know: one \
         call here turns one refused read into a plan, against the {} bytes a single answer may \
         carry.",
        where_from(reach),
        limits.max_output_bytes,
    )
}

fn desc_list_files(reach: Reach, limits: ToolLimits) -> String {
    let source = match reach.content {
        Content::Checkout => {
            "It scans the checkout, so the listing is complete and a path missing from it is not \
             there."
        }
        Content::Fetched => {
            "It asks the platform API about the reviewed commit rather than about what has been \
             fetched so far, so it answers for the whole repository. An answer the platform could \
             not complete says so."
        }
        Content::Empty => "There is nothing to list.",
    };
    format!(
        "List the paths in this run's worktree matching a glob. {} {source} At most {} paths; \
         overflow says how many remain.",
        where_from(reach),
        limits.max_files_per_listing,
    )
}

fn desc_search_code(reach: Reach, limits: ToolLimits) -> String {
    let strength = match reach.search {
        Search::Regex => match reach.content {
            Content::Checkout => {
                "query is a regular expression matched over every file in the checkout, so a miss \
                 usually means it is not there."
            }
            _ => {
                "query is a regular expression, matched by the platform over the reviewed commit, \
                 so a miss usually means it is not there."
            }
        },
        Search::Keyword => {
            "query is case-insensitive keyword matching against the platform's index; regex \
             metacharacters are literal, not a regex. That index covers the default branch only, \
             so a miss does not mean it is absent — confirm with list_files or read_file before \
             concluding anything from an empty result."
        }
        // Not registered in this case, so nothing reads this arm; it stays
        // truthful rather than unreachable.
        Search::Unavailable => "No search is available this run.",
    };
    format!(
        "Search the code of this run's worktree. {} {strength} Optional glob to limit the files. \
         At most {} hits; overflow says how many remain.",
        where_from(reach),
        limits.max_hits_per_search,
    )
}

/// The catalog rows. `reach` decides the wording, exactly as it does at
/// registration, so `tool list` prints the contract the model would be given.
pub fn builtin_specs(reach: Reach, limits: ToolLimits) -> [BuiltinSpec; 4] {
    [
        BuiltinSpec {
            name: ListFiles::NAME,
            description: desc_list_files(reach, limits),
            parameters: glob_signature().schema(),
            requires_checkout: false,
            requires_search: false,
        },
        BuiltinSpec {
            name: StatFile::NAME,
            description: desc_stat_file(reach, limits),
            parameters: path_signature(false).schema(),
            requires_checkout: false,
            requires_search: false,
        },
        BuiltinSpec {
            name: ReadFile::NAME,
            description: desc_read_file(reach, limits),
            parameters: path_signature(true).schema(),
            requires_checkout: false,
            requires_search: false,
        },
        BuiltinSpec {
            name: SearchCode::NAME,
            description: desc_search_code(reach, limits),
            parameters: search_signature().schema(),
            requires_checkout: false,
            requires_search: true,
        },
    ]
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

/// Appended to a failed read. The worktree's own words are usually a bare
/// "no such file", which does not tell the model what to do next; most misses
/// are a path it invented, and the next guess costs another round.
const READ_MISS_HINT: &str = "(If the path was wrong, list the files first to see what \
                              the worktree actually has, rather than guessing again.)";

/// What the four content tools share. Cloned into each of them when they are
/// registered, so `Tool::execute` still takes nothing but arguments.
#[derive(Clone)]
pub struct WorktreeContext {
    source: Arc<dyn WorktreeSource>,
    paths: PathPolicy,
    limits: ToolLimits,
}

impl WorktreeContext {
    pub fn new(source: Arc<dyn WorktreeSource>, paths: PathPolicy, limits: ToolLimits) -> Self {
        Self {
            source,
            paths,
            limits,
        }
    }

    pub fn reach(&self) -> Reach {
        self.source.reach()
    }

    pub fn limits(&self) -> ToolLimits {
        self.limits
    }

    fn answer(&self) -> Answer<'_> {
        Answer {
            paths: &self.paths,
            limits: self.limits,
        }
    }

    /// Normalize, deny list, extension whitelist, the symlink rule, and the
    /// resolved path still has to sit under the worktree root. One check for
    /// one worktree: a fetched file lands under that same root, so there is
    /// no second kind of path to reason about.
    fn path(&self, tool: &str, given: &str) -> Result<String, ToolError> {
        self.paths
            .check_worktree_path(given, self.source.root())
            .map_err(|rejection| ToolError::Rejected {
                tool: tool.to_string(),
                reason: rejection.to_string(),
            })
    }

    fn failed(tool: &str, error: WorktreeError) -> ToolError {
        ToolError::Unavailable {
            tool: tool.to_string(),
            reason: error.to_string(),
        }
    }

    fn failed_read(tool: &str, error: WorktreeError) -> ToolError {
        ToolError::Unavailable {
            tool: tool.to_string(),
            reason: format!("{error} {READ_MISS_HINT}"),
        }
    }
}

/// What the model is shown for a listing, a search or a file. Everything
/// answers through this, so the caps and the wording cannot drift apart
/// between the four.
struct Answer<'a> {
    paths: &'a PathPolicy,
    limits: ToolLimits,
}

impl Answer<'_> {
    /// Denied paths are dropped rather than replaced with a placeholder: these
    /// are structured results, and a path the model can see is a path it will
    /// try to read. The extension whitelist is not applied here — that a
    /// `Makefile` exists is worth knowing, and reading it is refused later.
    fn listing(&self, found: Vec<String>, complete: bool) -> ToolOutput {
        let ceiling = self.limits.max_files_per_listing;
        let visible = self.paths.retain_visible(found.iter().map(String::as_str));
        let total = visible.len();
        let mut lines: Vec<String> = visible.into_iter().take(ceiling).collect();
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
        self.text(lines.join("\n"))
    }

    fn hits(&self, found: Vec<crate::worktree::SearchHit>, empty_note: Option<&str>) -> ToolOutput {
        let kept: Vec<&crate::worktree::SearchHit> = found
            .iter()
            .filter(|hit| !self.paths.is_denied(&hit.path))
            .collect();
        let ceiling = self.limits.max_hits_per_search;
        let mut lines: Vec<String> = kept
            .iter()
            .take(ceiling)
            .map(|hit| format!("{}:{}: {}", hit.path, hit.line, hit.text))
            .collect();
        if lines.is_empty() {
            lines.push("No matches.".to_string());
            if let Some(note) = empty_note {
                lines.push(note.to_string());
            }
        }
        if kept.len() > ceiling {
            lines.push(format!(
                "{} more matches; narrow the query or the glob.",
                kept.len() - ceiling
            ));
        }
        self.text(lines.join("\n"))
    }

    /// A file body, and the record that this run really read it.
    ///
    /// Nothing here is ever truncated. Part of a file reads exactly like all
    /// of it, and a caller who does not know a line is missing will conclude
    /// the thing on that line does not exist. So both ceilings answer with a
    /// refusal carrying the numbers, and choosing what to leave out is left
    /// to whoever asked: `stat_file` says how big the file is, and a line
    /// range says which part of it to hand back.
    fn file(
        &self,
        tool: &str,
        path: &str,
        range: Option<LineRange>,
        body: String,
    ) -> Result<ToolOutput, ToolError> {
        let stat = Stat::of(&body);
        let fetch_ceiling = self.limits.max_file_bytes;
        if stat.bytes > fetch_ceiling {
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

    /// What `stat_file` answers: the numbers needed to plan reads, and
    /// nothing else. Deliberately tiny, so asking is always cheaper than
    /// finding out by being refused.
    fn stat(&self, path: &str, body: &str) -> ToolOutput {
        let stat = Stat::of(body);
        let room = self.limits.max_output_bytes;
        let fetch_ceiling = self.limits.max_file_bytes;
        let verdict = match () {
            _ if stat.bytes > fetch_ceiling => format!(
                "past max_file_bytes ({fetch_ceiling}), so no read of this file can succeed, \
                 in whole or in part"
            ),
            _ if stat.bytes > room => format!(
                "over the {room} one read may return, so read it in line ranges of about {} lines",
                stat.suggested_window(room)
            ),
            _ => "small enough to read in one call".to_string(),
        };
        ToolOutput::new(format!("{}\n{verdict}", stat.describe(path)))
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

/// The three declarations the four content tools share. Written once each:
/// what the model is shown and what its call is checked against both come out
/// of these.
fn glob_signature() -> Signature {
    Signature::new(vec![Parameter::required(
        "glob",
        Shape::text(),
        "Repository-relative glob, same syntax as deny_paths, e.g. src/**/*.c",
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

pub struct ListFiles {
    context: WorktreeContext,
    /// Written from the worktree's reach and the limits rather than fixed, so
    /// what the model is told is what this run's answer will actually be.
    description: String,
    signature: Signature,
}

impl ListFiles {
    pub const NAME: &'static str = "list_files";

    pub fn new(context: WorktreeContext) -> Self {
        let description = desc_list_files(context.reach(), context.limits());
        Self {
            context,
            description,
            signature: glob_signature(),
        }
    }
}

impl Tool for ListFiles {
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

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let glob = check_glob(
            &self.context.paths,
            self.name(),
            checked.text("glob").unwrap_or_default(),
        )?;
        let listing = self
            .context
            .source
            .list_files(&glob)
            .map_err(|error| WorktreeContext::failed(self.name(), error))?;
        Ok(self
            .context
            .answer()
            .listing(listing.paths, listing.complete))
    }
}

/// Size without content. The body still has to be read to count lines — no
/// platform reports those — but only the numbers come back, so this stays
/// affordable for a file far too big to read.
pub struct StatFile {
    context: WorktreeContext,
    description: String,
    signature: Signature,
}

impl StatFile {
    pub const NAME: &'static str = "stat_file";

    pub fn new(context: WorktreeContext) -> Self {
        let description = desc_stat_file(context.reach(), context.limits());
        Self {
            context,
            description,
            signature: path_signature(false),
        }
    }
}

impl Tool for StatFile {
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

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let path = self
            .context
            .path(self.name(), checked.text("path").unwrap_or_default())?;
        let body = self
            .context
            .source
            .read_file(&path, None)
            .map_err(|error| WorktreeContext::failed_read(self.name(), error))?;
        Ok(self.context.answer().stat(&path, &body))
    }
}

pub struct ReadFile {
    context: WorktreeContext,
    description: String,
    signature: Signature,
}

impl ReadFile {
    pub const NAME: &'static str = "read_file";

    pub fn new(context: WorktreeContext) -> Self {
        let description = desc_read_file(context.reach(), context.limits());
        Self {
            context,
            description,
            signature: path_signature(true),
        }
    }
}

impl Tool for ReadFile {
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

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let path = self
            .context
            .path(self.name(), checked.text("path").unwrap_or_default())?;
        let range = range_of(self.name(), &checked)?;
        // The whole file first: `max_file_bytes` is a statement about the
        // file, and a range must not become a way to read a slice of one that
        // is too big to read.
        let body = self
            .context
            .source
            .read_file(&path, None)
            .map_err(|error| WorktreeContext::failed_read(self.name(), error))?;
        self.context.answer().file(self.name(), &path, range, body)
    }
}

/// `search_code`, whose description and empty-result note are written from the
/// worktree's reach when it is registered. The same name is a regular
/// expression search on one run and a keyword index on another, and the model
/// has no way to tell which one it got from the name alone.
pub struct SearchCode {
    context: WorktreeContext,
    description: String,
    empty_note: Option<String>,
    signature: Signature,
}

impl SearchCode {
    pub const NAME: &'static str = "search_code";

    /// A miss from a keyword index over the default branch says nothing about
    /// the branch under review, so an empty answer says that rather than just
    /// being empty.
    const EMPTY_KEYWORD_NOTE: &'static str = "A miss does not mean it is absent: this search used a keyword index that only covers \
         the default branch, so something new on this branch may not be indexed. To confirm \
         presence, use list_files or read_file.";

    pub fn new(context: WorktreeContext) -> Self {
        let reach = context.reach();
        let description = desc_search_code(reach, context.limits());
        let empty_note = match reach.search {
            Search::Keyword => Some(Self::EMPTY_KEYWORD_NOTE.to_string()),
            _ => None,
        };
        Self {
            context,
            description,
            empty_note,
            signature: search_signature(),
        }
    }
}

impl Tool for SearchCode {
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

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.signature.validate(self.name(), arguments)?;
        let query = checked.text("query").unwrap_or_default();
        let glob = match checked.text("glob") {
            Some(given) => Some(check_glob(&self.context.paths, self.name(), given)?),
            None => None,
        };
        let hits = self
            .context
            .source
            .search(query, glob.as_deref())
            .map_err(|error| WorktreeContext::failed(self.name(), error))?;
        Ok(self.context.answer().hits(hits, self.empty_note.as_deref()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::config::SecuritySettings;
    use crate::worktree::{Listing, SearchHit};

    /// Counts every question it is asked. That is the whole point of this
    /// fake: an out of bounds argument has to be refused *before* the
    /// worktree is reached, and only a counter can tell that from a worktree
    /// that was asked and happened to answer with nothing.
    struct CountingWorktree {
        calls: AtomicUsize,
        root: PathBuf,
        reach: Reach,
        listing: Listing,
        body: String,
        hits: Vec<SearchHit>,
    }

    impl CountingWorktree {
        fn empty(root: &Path) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                root: root.to_path_buf(),
                reach: Reach {
                    content: Content::Checkout,
                    search: Search::Regex,
                },
                listing: Listing {
                    paths: Vec::new(),
                    complete: true,
                },
                body: String::new(),
                hits: Vec::new(),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl WorktreeSource for CountingWorktree {
        fn root(&self) -> &Path {
            &self.root
        }

        fn reach(&self) -> Reach {
            self.reach
        }

        fn open_in(&self, _run_dir: &Path) -> Result<(), WorktreeError> {
            Ok(())
        }

        fn head_sha(&self) -> Result<Option<String>, WorktreeError> {
            Ok(Some("head".to_string()))
        }

        fn supply(&self, _path: &str) -> Result<(), WorktreeError> {
            Ok(())
        }

        fn list_files(&self, _glob: &str) -> Result<Listing, WorktreeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.listing.clone())
        }

        fn read_file(
            &self,
            _path: &str,
            _lines: Option<LineRange>,
        ) -> Result<String, WorktreeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.body.clone())
        }

        fn search(
            &self,
            _query: &str,
            _glob: Option<&str>,
        ) -> Result<Vec<SearchHit>, WorktreeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.hits.clone())
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
    };

    fn context(source: &Arc<CountingWorktree>, deny: &[&str]) -> WorktreeContext {
        let root = source.root().to_path_buf();
        WorktreeContext::new(
            Arc::clone(source) as Arc<dyn WorktreeSource>,
            policy(deny, Some(&root)),
            LIMITS,
        )
    }

    fn tools(source: &Arc<CountingWorktree>, deny: &[&str]) -> Vec<Box<dyn Tool>> {
        let context = context(source, deny);
        vec![
            Box::new(ListFiles::new(context.clone())),
            Box::new(StatFile::new(context.clone())),
            Box::new(ReadFile::new(context.clone())),
            Box::new(SearchCode::new(context)),
        ]
    }

    /// An argument that names a path or a pattern this tool may take, aimed
    /// somewhere it may not go.
    fn out_of_bounds(tool: &str) -> Value {
        match tool {
            ListFiles::NAME => json!({"glob": "../etc/**"}),
            ReadFile::NAME | StatFile::NAME => json!({"path": "../etc/passwd"}),
            _ => json!({"query": "password", "glob": "secrets/**"}),
        }
    }

    /// The boundary case for every one of the four, because the check being in
    /// the tool is the only thing holding the read boundary up: there is no
    /// type that a worktree refuses to be called with (§6 security).
    #[test]
    fn an_out_of_bounds_argument_is_refused_before_the_worktree_is_asked() {
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree::empty(root.path()));

        for tool in tools(&worktree, &["secrets/**"]) {
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
        assert_eq!(worktree.calls(), 0, "no read was attempted");
    }

    /// A denied path is dropped from a listing rather than turned into a
    /// placeholder, and the extension whitelist is not applied there at all:
    /// seeing `Makefile` tells the model what kind of project this is, and
    /// trying to read it is refused separately.
    #[test]
    fn listings_drop_denied_paths_and_keep_ones_that_cannot_be_read() {
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree {
            listing: Listing {
                paths: vec![
                    "src/parse.c".to_string(),
                    "secrets/deploy.toml".to_string(),
                    "Makefile".to_string(),
                ],
                complete: true,
            },
            ..CountingWorktree::empty(root.path())
        });
        let context = context(&worktree, &["secrets/**"]);

        let listed = ListFiles::new(context.clone())
            .execute(&json!({"glob": "**/*"}))
            .expect("listed");
        assert!(listed.text.contains("src/parse.c"), "{}", listed.text);
        assert!(listed.text.contains("Makefile"), "{}", listed.text);
        assert!(!listed.text.contains("secrets/"), "{}", listed.text);

        let refused = ReadFile::new(context)
            .execute(&json!({"path": "Makefile"}))
            .expect_err("no extension");
        assert!(matches!(refused, ToolError::Rejected { .. }), "{refused}");
        assert_eq!(
            worktree.calls(),
            1,
            "the refused read never reached the worktree"
        );
    }

    #[test]
    fn a_search_drops_hits_in_denied_files() {
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree {
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
            ..CountingWorktree::empty(root.path())
        });
        let output = SearchCode::new(context(&worktree, &["secrets/**"]))
            .execute(&json!({"query": "token"}))
            .expect("searched");
        assert!(output.text.contains("src/parse.c:12:"), "{}", output.text);
        assert!(!output.text.contains("secrets/"), "{}", output.text);
    }

    #[test]
    fn a_listing_past_the_ceiling_says_how_many_are_left() {
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree {
            listing: Listing {
                paths: (0..LIMITS.max_files_per_listing + 5)
                    .map(|index| format!("src/file{index:04}.c"))
                    .collect(),
                complete: true,
            },
            ..CountingWorktree::empty(root.path())
        });
        let output = ListFiles::new(context(&worktree, &[]))
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
    fn an_incomplete_listing_says_so_in_words_the_model_reads() {
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree {
            listing: Listing {
                paths: vec!["src/parse.c".to_string()],
                complete: false,
            },
            ..CountingWorktree::empty(root.path())
        });
        let output = ListFiles::new(context(&worktree, &[]))
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
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree {
            body: "one\ntwo\nthree\nfour\n".to_string(),
            ..CountingWorktree::empty(root.path())
        });
        let output = ReadFile::new(context(&worktree, &[]))
            .execute(&json!({"path": "src/parse.c", "first_line": 2, "last_line": 3}))
            .expect("read");

        assert_eq!(output.text, "two\nthree");
        let file = output.context_file.expect("a read is a context file");
        assert_eq!(file.path, "src/parse.c");
        assert_eq!((file.first_line, file.last_line), (2, 3));
        assert_eq!(file.body, "two\nthree");
    }

    fn with_limits(source: &Arc<CountingWorktree>, limits: ToolLimits) -> WorktreeContext {
        let root = source.root().to_path_buf();
        WorktreeContext::new(
            Arc::clone(source) as Arc<dyn WorktreeSource>,
            policy(&[], Some(&root)),
            limits,
        )
    }

    /// Past `max_file_bytes` the answer is a refusal with the size in it. A
    /// silent clip would read exactly like a short file.
    #[test]
    fn a_file_over_max_file_bytes_is_refused_rather_than_clipped() {
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree {
            body: "x".repeat(1_000),
            ..CountingWorktree::empty(root.path())
        });
        let context = with_limits(
            &worktree,
            ToolLimits {
                max_file_bytes: 100,
                ..LIMITS
            },
        );
        let error = ReadFile::new(context.clone())
            .execute(&json!({"path": "src/parse.c"}))
            .expect_err("over the ceiling");
        assert!(matches!(error, ToolError::Rejected { .. }), "{error}");
        assert!(error.to_string().contains("max_file_bytes"), "{error}");
        assert!(error.to_string().contains("1000"), "{error}");

        // A range is no way around it: reading part of a file means reading
        // all of it first, whichever shape the worktree has.
        assert!(
            ReadFile::new(context)
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
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree {
            body: "0123456789\n".repeat(400),
            ..CountingWorktree::empty(root.path())
        });
        let context = with_limits(
            &worktree,
            ToolLimits {
                max_output_bytes: 500,
                ..LIMITS
            },
        );

        let error = ReadFile::new(context.clone())
            .execute(&json!({"path": "src/parse.c"}))
            .expect_err("4400 bytes do not fit in 500");
        let said = error.to_string();
        assert!(matches!(error, ToolError::Rejected { .. }), "{said}");
        assert!(said.contains("400 lines"), "{said}");
        assert!(said.contains("4400 bytes"), "{said}");
        assert!(said.contains("line range"), "{said}");

        // And the range it was told to use comes back whole, not clipped.
        let output = ReadFile::new(context.clone())
            .execute(&json!({"path": "src/parse.c", "first_line": 1, "last_line": 30}))
            .expect("30 lines fit");
        assert_eq!(output.text.lines().count(), 30);
        assert_eq!(output.omitted_bytes, 0, "a read is never clipped");

        // A range that still does not fit is refused the same way rather than
        // clipped, or paging would silently stop being paging.
        let refused = ReadFile::new(context)
            .execute(&json!({"path": "src/parse.c", "first_line": 10, "last_line": 300}))
            .expect_err("291 lines do not fit either");
        assert!(refused.to_string().contains("lines 10-300"), "{refused}");
    }

    /// What `stat_file` is for: the size, and what to do about it, without
    /// paying for any of the content. A file too big to read at all says so,
    /// so the model does not spend a round finding out.
    #[test]
    fn stat_answers_with_the_size_and_a_plan_and_no_content() {
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree {
            body: "0123456789\n".repeat(400),
            ..CountingWorktree::empty(root.path())
        });
        let output = StatFile::new(with_limits(
            &worktree,
            ToolLimits {
                max_output_bytes: 500,
                ..LIMITS
            },
        ))
        .execute(&json!({"path": "src/parse.c"}))
        .expect("stat");
        assert!(
            output.text.contains("4400 bytes, 400 lines"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("line ranges of about"),
            "{}",
            output.text
        );
        assert!(
            !output.text.contains("0123456789"),
            "no content: {}",
            output.text
        );
        assert!(
            output.context_file.is_none(),
            "a size is not a file this run read"
        );

        let output = StatFile::new(with_limits(
            &worktree,
            ToolLimits {
                max_file_bytes: 100,
                ..LIMITS
            },
        ))
        .execute(&json!({"path": "src/parse.c"}))
        .expect("stat still answers for a file too big to read");
        assert!(output.text.contains("max_file_bytes"), "{}", output.text);
    }

    #[test]
    fn half_a_line_range_is_answered_rather_than_guessed_at() {
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree::empty(root.path()));
        let error = ReadFile::new(context(&worktree, &[]))
            .execute(&json!({"path": "src/parse.c", "first_line": 4}))
            .expect_err("half a range");
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error}"
        );
        assert_eq!(worktree.calls(), 0);
    }

    /// The names are fixed now, so the description is the only thing that can
    /// say what this run's worktree is and how far a search reaches. Every
    /// shape has to say it, because the model reads only the description.
    #[test]
    fn the_descriptions_say_which_worktree_this_is_and_how_far_it_reaches() {
        let root = tempfile::tempdir().expect("temp dir");
        let checkout = Arc::new(CountingWorktree::empty(root.path()));
        let read = ReadFile::new(context(&checkout, &[]));
        assert_eq!(read.name(), "read_file");
        assert!(read.description().contains("checkout under review"));
        assert!(
            read.description().contains("262144"),
            "the fetch ceiling is a number, not a word: {}",
            read.description()
        );
        assert!(
            SearchCode::new(context(&checkout, &[]))
                .description()
                .contains("regular expression")
        );

        let fetched = Arc::new(CountingWorktree {
            reach: Reach {
                content: Content::Fetched,
                search: Search::Keyword,
            },
            ..CountingWorktree::empty(root.path())
        });
        let read = ReadFile::new(context(&fetched, &[]));
        assert!(
            read.description().contains("started empty"),
            "{}",
            read.description()
        );
        assert!(
            read.description().contains("fetched"),
            "{}",
            read.description()
        );
        let search = SearchCode::new(context(&fetched, &[]));
        assert!(search.description().contains("keyword matching"));
        assert!(search.description().contains("metacharacters are literal"));
        assert!(search.description().contains("does not mean it is absent"));

        let listing = ListFiles::new(context(&fetched, &[]));
        assert!(
            listing.description().contains("whole repository"),
            "a listing is not a list of what has been fetched: {}",
            listing.description()
        );
    }

    /// An empty answer from a keyword index that only covers the default
    /// branch says nothing about the branch under review, so the answer says
    /// that instead of just being empty.
    #[test]
    fn an_empty_keyword_search_says_that_it_is_not_a_denial() {
        let root = tempfile::tempdir().expect("temp dir");
        let worktree = Arc::new(CountingWorktree {
            reach: Reach {
                content: Content::Fetched,
                search: Search::Keyword,
            },
            ..CountingWorktree::empty(root.path())
        });
        let output = SearchCode::new(context(&worktree, &[]))
            .execute(&json!({"query": "parse_token"}))
            .expect("searched");
        assert!(
            output.text.contains("does not mean it is absent"),
            "{}",
            output.text
        );
    }

    /// The real `Checkout`, because a symlink out of the tree has to be
    /// refused against a real filesystem rather than a fake.
    #[test]
    fn a_read_goes_through_the_worktree_and_stops_at_its_edge() {
        let root = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(root.path().join("src")).expect("src");
        std::fs::write(root.path().join("src/parse.c"), "int main(void)\n{\n}\n").expect("source");
        let worktree =
            crate::worktree::Checkout::open(root.path().to_path_buf()).expect("worktree");
        let context = WorktreeContext::new(
            Arc::new(worktree) as Arc<dyn WorktreeSource>,
            policy(&[], Some(root.path())),
            LIMITS,
        );

        let listed = ListFiles::new(context.clone())
            .execute(&json!({"glob": "**/*.c"}))
            .expect("listed");
        assert!(listed.text.contains("src/parse.c"), "{}", listed.text);

        let read = ReadFile::new(context.clone())
            .execute(&json!({"path": "src/parse.c"}))
            .expect("read");
        assert!(read.text.contains("int main(void)"), "{}", read.text);
        assert_eq!(
            read.context_file.expect("a read is a context file").path,
            "src/parse.c"
        );

        let hits = SearchCode::new(context)
            .execute(&json!({"query": r"int\s+main"}))
            .expect("searched");
        assert!(hits.text.contains("src/parse.c:1:"), "{}", hits.text);
    }
}
