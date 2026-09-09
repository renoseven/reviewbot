//! The six builtin tools: three that read the repository through the platform
//! API and three that read a local checkout. Ordinary `Tool` implementations,
//! registered beside the configured commands, so nothing downstream has to ask
//! which kind of tool it is holding.
//!
//! Every one of them does the same three things in the same order: read the
//! model's arguments, put them through `PathPolicy`, and only then ask the
//! source. The check comes first because this is the only place the model can
//! reach either source, which is the whole of why the read boundary holds
//! (§6 security). The sources themselves take plain repository relative paths.
//!
//! The two groups are deliberately six names rather than three: the same
//! question is not equally answerable on a platform API and on a disk, and a
//! shared name would hide which strength the answer came from.

use std::sync::Arc;

use globset::Glob;
use serde_json::{Value, json};

use crate::config::Config;
use crate::platform::{Capabilities, LineRange as RepoRange, PlatformError, RepoSource};
use crate::record::ContextFile;
use crate::security::{PathPolicy, truncate};
use crate::worktree::{WorktreeError, WorktreeSource};

use super::{Origin, Tool, ToolError, ToolOutput};

/// Static facts about a builtin, for `tool list`. The real tools still
/// need a source before they can run; this is only what the catalog prints.
pub struct BuiltinSpec {
    pub name: &'static str,
    pub description: String,
    pub parameters: Value,
    pub requires_worktree: bool,
    /// `Some("code_search")` when the platform must advertise that capability.
    pub capability_gate: Option<&'static str>,
}

/// How much a content tool may fetch and how much it may hand back, all of it
/// from `[review]`. Carried as one value so the two symmetric groups cannot
/// drift apart in their caps, and so the descriptions quote the same numbers
/// the code enforces.
///
/// `max_read_bytes` lives here rather than on `PathPolicy` because it is not
/// a permission: the policy answers whether a path may be read at all, this
/// answers how much of it fits.
#[derive(Clone, Copy, Debug)]
pub struct ToolLimits {
    pub max_read_bytes: u64,
    pub max_output_bytes: u64,
    pub max_files_listed: usize,
    pub max_search_hits: usize,
}

impl ToolLimits {
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_read_bytes: config.review.max_read_bytes,
            max_output_bytes: config.review.max_tool_output_bytes,
            max_files_listed: config.review.max_files_listed as usize,
            max_search_hits: config.review.max_search_hits as usize,
        }
    }
}

const DESC_READ_REPO_FILE: &str = "Read a file on the reviewed commit (head_sha), via the platform API with no local checkout. Optional line range (first_line and last_line together); without one you get the whole file. Nothing is ever truncated: a read too big to return is refused with the file's size, so use stat_repo_file first when you do not know how big the file is, and page through a large one in line ranges. The path must be one the diff or a listing gave you: a guessed path that misses costs a whole round.";
const DESC_READ_WORKTREE_FILE: &str = "Read a file in the local checkout (worktree). Optional line range (first_line and last_line together); without one you get the whole file. Nothing is ever truncated: a read too big to return is refused with the file's size, so use stat_worktree_file first when you do not know how big the file is, and page through a large one in line ranges. The path must be one the diff or a listing gave you: a guessed path that misses costs a whole round.";
const DESC_STAT_REPO_FILE: &str = "Size of a file on the reviewed commit (head_sha), in bytes and lines, plus whether it fits in one read and how many lines to ask for at a time. Returns numbers only, never file content, so it is cheap. Ask this before reading a file whose size you do not know: it turns one refused read into a plan.";
const DESC_STAT_WORKTREE_FILE: &str = "Size of a file in the local checkout (worktree), in bytes and lines, plus whether it fits in one read and how many lines to ask for at a time. Returns numbers only, never file content, so it is cheap. Ask this before reading a file whose size you do not know: it turns one refused read into a plan.";
const EMPTY_KEYWORD_NOTE: &str = "A miss does not mean it is absent: this search used a keyword index that only covers the default branch, so something new on this branch may not be indexed. To confirm presence, use list_repo_files or read_repo_file.";

fn desc_list_repo_files(limits: ToolLimits) -> String {
    format!(
        "List file paths on the reviewed commit (head_sha) matching a glob, via the platform API. No local checkout. At most {}; overflow says how many remain. An incomplete listing from the platform says so.",
        limits.max_files_listed
    )
}

fn desc_list_worktree_files(limits: ToolLimits) -> String {
    format!(
        "List file paths in the local checkout (worktree) matching a glob. Scans the disk; the listing is complete. At most {}; overflow says how many remain.",
        limits.max_files_listed
    )
}

fn desc_search_repo() -> String {
    "Search the reviewed commit (head_sha). Registered only when the platform advertises code_search; otherwise this entry exists but is not in the model's tool list.".to_string()
}

fn desc_search_worktree(limits: ToolLimits) -> String {
    format!(
        "Search the local checkout (worktree). query is a regular expression over every file on disk, so a miss usually means it is not there. Optional glob to limit files. At most {} hits.",
        limits.max_search_hits
    )
}

fn desc_search_repo_regex(limits: ToolLimits) -> String {
    format!(
        "Search the reviewed commit (head_sha). query is a regular expression. Optional glob to limit files. At most {} hits.",
        limits.max_search_hits
    )
}

fn desc_search_repo_keyword(limits: ToolLimits) -> String {
    format!(
        "Search the reviewed commit (head_sha). query is case-insensitive keyword matching; regex metacharacters are literal, not a regex. The platform index covers the default branch only, so hits are candidate paths: the file is re-fetched at head_sha and rematched locally. Paths, line numbers, and text all come from head_sha. A miss does not mean it is absent. At most {} hits.",
        limits.max_search_hits
    )
}

pub fn builtin_specs(limits: ToolLimits) -> [BuiltinSpec; 8] {
    [
        BuiltinSpec {
            name: "list_repo_files",
            description: desc_list_repo_files(limits),
            parameters: glob_schema(),
            requires_worktree: false,
            capability_gate: None,
        },
        BuiltinSpec {
            name: "stat_repo_file",
            description: DESC_STAT_REPO_FILE.to_string(),
            parameters: stat_schema(),
            requires_worktree: false,
            capability_gate: None,
        },
        BuiltinSpec {
            name: "read_repo_file",
            description: DESC_READ_REPO_FILE.to_string(),
            parameters: path_schema(),
            requires_worktree: false,
            capability_gate: None,
        },
        BuiltinSpec {
            name: "search_repo",
            description: desc_search_repo(),
            parameters: search_schema(),
            requires_worktree: false,
            capability_gate: Some("code_search"),
        },
        BuiltinSpec {
            name: "list_worktree_files",
            description: desc_list_worktree_files(limits),
            parameters: glob_schema(),
            requires_worktree: true,
            capability_gate: None,
        },
        BuiltinSpec {
            name: "stat_worktree_file",
            description: DESC_STAT_WORKTREE_FILE.to_string(),
            parameters: stat_schema(),
            requires_worktree: true,
            capability_gate: None,
        },
        BuiltinSpec {
            name: "read_worktree_file",
            description: DESC_READ_WORKTREE_FILE.to_string(),
            parameters: path_schema(),
            requires_worktree: true,
            capability_gate: None,
        },
        BuiltinSpec {
            name: "search_worktree",
            description: desc_search_worktree(limits),
            parameters: search_schema(),
            requires_worktree: true,
            capability_gate: None,
        },
    ]
}

/// Said when the platform could not hand over the whole tree. It has to be
/// said: a half list read as a whole one is how a reviewer concludes that
/// something is not there.
const INCOMPLETE: &str = "Note: this listing is incomplete. The platform would not hand \
                          over the whole tree at once, so a path not listed here may still \
                          exist. Narrow the glob to a specific directory and try again.";

/// Appended to a failed read. The platform's own words are usually a bare
/// 404, which does not tell the model what to do next; most misses are a path
/// it invented, and the next guess costs another round.
const READ_MISS_HINT: &str = "(If the path was wrong, list the files first to see what \
                              this source actually has, rather than guessing again.)";

/// What the three repository tools share. Cloned into each of them when they
/// are registered, so `Tool::execute` still takes nothing but arguments.
#[derive(Clone)]
pub struct RepoContext {
    source: Arc<dyn RepoSource>,
    paths: PathPolicy,
    limits: ToolLimits,
}

impl RepoContext {
    pub fn new(source: Arc<dyn RepoSource>, paths: PathPolicy, limits: ToolLimits) -> Self {
        Self {
            source,
            paths,
            limits,
        }
    }

    fn answer(&self) -> Answer<'_> {
        Answer {
            paths: &self.paths,
            limits: self.limits,
        }
    }

    /// API mode: normalize, deny list, extension whitelist. Nothing lands on
    /// disk, so there is no symlink or root question to ask.
    fn path(&self, tool: &str, given: &str) -> Result<String, ToolError> {
        self.paths
            .check_repo_path(given)
            .map_err(|rejection| ToolError::Rejected {
                tool: tool.to_string(),
                reason: rejection.to_string(),
            })
    }

    fn failed(tool: &str, error: PlatformError) -> ToolError {
        ToolError::Unavailable {
            tool: tool.to_string(),
            reason: error.to_string(),
        }
    }

    fn failed_read(tool: &str, error: PlatformError) -> ToolError {
        ToolError::Unavailable {
            tool: tool.to_string(),
            reason: format!("{error} {READ_MISS_HINT}"),
        }
    }
}

/// What the three disk tools share. The root comes from the source rather than
/// being carried twice.
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

    fn answer(&self) -> Answer<'_> {
        Answer {
            paths: &self.paths,
            limits: self.limits,
        }
    }

    /// The same three checks as the API side plus the symlink rule, and the
    /// resolved path still has to sit under the worktree root.
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

/// The optional line range, as the model wrote it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Range {
    first: u32,
    last: u32,
}

impl Range {
    /// The lines the model asked for, cut out of a body the size check has
    /// already cleared. Both sources define a range the same way, so either
    /// one of them can do the cutting for both.
    fn slice(self, text: &str) -> String {
        RepoRange {
            first: self.first,
            last: self.last,
        }
        .slice(text)
    }
}

/// One search hit, whichever source found it.
struct Hit {
    path: String,
    line: u32,
    text: String,
}

impl From<crate::platform::SearchHit> for Hit {
    fn from(hit: crate::platform::SearchHit) -> Self {
        Self {
            path: hit.path,
            line: hit.line,
            text: hit.text,
        }
    }
}

impl From<crate::worktree::SearchHit> for Hit {
    fn from(hit: crate::worktree::SearchHit) -> Self {
        Self {
            path: hit.path,
            line: hit.line,
            text: hit.text,
        }
    }
}

/// What the model is shown for a listing, a search or a file. Both groups
/// answer through this, so the two halves of a symmetric pair cannot drift
/// apart in their caps or their wording.
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
        let ceiling = self.limits.max_files_listed;
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

    fn hits(&self, found: Vec<Hit>, empty_note: Option<&str>) -> ToolOutput {
        let kept: Vec<&Hit> = found
            .iter()
            .filter(|hit| !self.paths.is_denied(&hit.path))
            .collect();
        let ceiling = self.limits.max_search_hits;
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

    /// A file body, and the record that this run really fetched it.
    ///
    /// Nothing here is ever truncated. Part of a file reads exactly like all
    /// of it, and a caller who does not know a line is missing will conclude
    /// the thing on that line does not exist. So both ceilings answer with a
    /// refusal carrying the numbers, and choosing what to leave out is left
    /// to whoever asked: `stat_*_file` says how big the file is, and a line
    /// range says which part of it to hand back.
    fn file(
        &self,
        tool: &str,
        path: &str,
        range: Option<Range>,
        body: String,
    ) -> Result<ToolOutput, ToolError> {
        let stat = Stat::of(&body);
        let fetch_ceiling = self.limits.max_read_bytes;
        if stat.bytes > fetch_ceiling {
            return Err(ToolError::Rejected {
                tool: tool.to_string(),
                reason: format!(
                    "{path} is {} bytes over {} lines, past max_read_bytes ({fetch_ceiling}); \
                     no part of it can be read, because reading any part means fetching all of it. \
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

    /// What `stat_*_file` answers: the numbers needed to plan reads, and
    /// nothing else. Deliberately tiny, so asking is always cheaper than
    /// finding out by being refused.
    fn stat(&self, path: &str, body: &str) -> ToolOutput {
        let stat = Stat::of(body);
        let room = self.limits.max_output_bytes;
        let fetch_ceiling = self.limits.max_read_bytes;
        let verdict = match () {
            _ if stat.bytes > fetch_ceiling => format!(
                "past max_read_bytes ({fetch_ceiling}), so no read of this file can succeed, \
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

/// The model's arguments, read with the tool's name attached so a refusal says
/// which call it is answering.
struct Arguments<'a> {
    tool: &'a str,
    value: &'a Value,
}

impl<'a> Arguments<'a> {
    fn new(tool: &'a str, value: &'a Value) -> Self {
        Self { tool, value }
    }

    fn text(&self, name: &str) -> Result<&'a str, ToolError> {
        let given = self
            .value
            .get(name)
            .ok_or_else(|| self.invalid(format!("missing argument {name:?}")))?;
        let text = given
            .as_str()
            .ok_or_else(|| self.invalid(format!("argument {name:?} has to be a string")))?;
        if text.is_empty() {
            return Err(self.invalid(format!("argument {name:?} is empty")));
        }
        Ok(text)
    }

    fn optional_text(&self, name: &str) -> Result<Option<&'a str>, ToolError> {
        match self.value.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(_) => self.text(name).map(Some),
        }
    }

    /// Both ends of a range come together. Half a range would have to be
    /// guessed at, and a guess about how much of a file the model wanted is
    /// the kind of thing it cannot notice going wrong.
    fn range(&self) -> Result<Option<Range>, ToolError> {
        match (self.number("first_line")?, self.number("last_line")?) {
            (None, None) => Ok(None),
            (Some(first), Some(last)) if first >= 1 && last >= first => {
                Ok(Some(Range { first, last }))
            }
            (Some(first), Some(last)) => Err(self.invalid(format!(
                "first_line {first} and last_line {last} are not a range: \
                 lines start at 1 and last_line cannot be smaller than first_line"
            ))),
            _ => Err(self
                .invalid("first_line and last_line go together; give both or neither".to_string())),
        }
    }

    fn number(&self, name: &str) -> Result<Option<u32>, ToolError> {
        match self.value.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .map(Some)
                .ok_or_else(|| {
                    self.invalid(format!(
                        "argument {name:?} has to be a positive whole number"
                    ))
                }),
        }
    }

    fn invalid(&self, reason: impl Into<String>) -> ToolError {
        ToolError::InvalidArguments {
            tool: self.tool.to_string(),
            reason: reason.into(),
        }
    }
}

fn glob_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "glob": {
                "type": "string",
                "description": "Repository-relative glob, same syntax as deny_paths, e.g. src/**/*.c",
            },
        },
        "required": ["glob"],
        "additionalProperties": false,
    })
}

fn path_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Repository-relative path, e.g. src/parse.c"},
            "first_line": {"type": "integer", "minimum": 1, "description": "Start of the line range; supply with last_line"},
            "last_line": {"type": "integer", "minimum": 1, "description": "End of the line range; supply with first_line"},
        },
        "required": ["path"],
        "additionalProperties": false,
    })
}

/// A path and nothing else: a size is a fact about the whole file, so there
/// is no range to ask about.
fn stat_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Repository-relative path, e.g. src/parse.c"},
        },
        "required": ["path"],
        "additionalProperties": false,
    })
}

fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "What to search for"},
            "glob": {"type": "string", "description": "Optional. Limit the search to files matching this glob"},
        },
        "required": ["query"],
        "additionalProperties": false,
    })
}

pub struct ListRepoFiles {
    context: RepoContext,
    /// Written from the limits rather than fixed, so the ceiling the model is
    /// told about is the one the answer enforces.
    description: String,
}

impl ListRepoFiles {
    pub fn new(context: RepoContext) -> Self {
        let description = desc_list_repo_files(context.limits);
        Self {
            context,
            description,
        }
    }
}

impl Tool for ListRepoFiles {
    fn name(&self) -> &str {
        "list_repo_files"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        glob_schema()
    }

    fn origin(&self) -> Origin {
        Origin::Builtin
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let arguments = Arguments::new(self.name(), arguments);
        let glob = check_glob(&self.context.paths, self.name(), arguments.text("glob")?)?;
        let listing = self
            .context
            .source
            .list_files(&glob)
            .map_err(|error| RepoContext::failed(self.name(), error))?;
        Ok(self
            .context
            .answer()
            .listing(listing.paths, listing.complete))
    }
}

/// Size without content. The body still has to be fetched to count lines —
/// no platform reports those — but only the numbers come back, so this stays
/// affordable for a file far too big to read.
pub struct StatRepoFile {
    context: RepoContext,
}

impl StatRepoFile {
    pub fn new(context: RepoContext) -> Self {
        Self { context }
    }
}

impl Tool for StatRepoFile {
    fn name(&self) -> &str {
        "stat_repo_file"
    }

    fn description(&self) -> &str {
        DESC_STAT_REPO_FILE
    }

    fn parameters(&self) -> Value {
        stat_schema()
    }

    fn origin(&self) -> Origin {
        Origin::Builtin
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let arguments = Arguments::new(self.name(), arguments);
        let path = self.context.path(self.name(), arguments.text("path")?)?;
        let body = self
            .context
            .source
            .read_file(&path, None)
            .map_err(|error| RepoContext::failed_read(self.name(), error))?;
        Ok(self.context.answer().stat(&path, &body))
    }
}

pub struct ReadRepoFile {
    context: RepoContext,
}

impl ReadRepoFile {
    pub fn new(context: RepoContext) -> Self {
        Self { context }
    }
}

impl Tool for ReadRepoFile {
    fn name(&self) -> &str {
        "read_repo_file"
    }

    fn description(&self) -> &str {
        DESC_READ_REPO_FILE
    }

    fn parameters(&self) -> Value {
        path_schema()
    }

    fn origin(&self) -> Origin {
        Origin::Builtin
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let arguments = Arguments::new(self.name(), arguments);
        let path = self.context.path(self.name(), arguments.text("path")?)?;
        let range = arguments.range()?;
        // The whole file first: `max_read_bytes` is a statement about the
        // file, and a range must not become a way to read a slice of one that
        // is too big to read.
        let body = self
            .context
            .source
            .read_file(&path, None)
            .map_err(|error| RepoContext::failed_read(self.name(), error))?;
        self.context.answer().file(self.name(), &path, range, body)
    }
}

/// `search_repo`, whose description and empty-result note are written from
/// `capabilities()` when it is registered. The same name is a regular
/// expression search on one instance and a keyword index on another, and the
/// model has no way to tell which one it got from the name alone.
pub struct SearchRepo {
    context: RepoContext,
    description: String,
    empty_note: Option<String>,
}

impl SearchRepo {
    pub fn new(context: RepoContext, capabilities: Capabilities) -> Self {
        let (description, empty_note) = match capabilities.regex_search {
            true => (desc_search_repo_regex(context.limits), None),
            false => (
                desc_search_repo_keyword(context.limits),
                Some(EMPTY_KEYWORD_NOTE.to_string()),
            ),
        };
        Self {
            context,
            description,
            empty_note,
        }
    }
}

impl Tool for SearchRepo {
    fn name(&self) -> &str {
        "search_repo"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        search_schema()
    }

    fn origin(&self) -> Origin {
        Origin::Builtin
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let arguments = Arguments::new(self.name(), arguments);
        let query = arguments.text("query")?;
        let glob = match arguments.optional_text("glob")? {
            Some(given) => Some(check_glob(&self.context.paths, self.name(), given)?),
            None => None,
        };
        let hits = self
            .context
            .source
            .search(query, glob.as_deref())
            .map_err(|error| RepoContext::failed(self.name(), error))?;
        Ok(self.context.answer().hits(
            hits.into_iter().map(Hit::from).collect(),
            self.empty_note.as_deref(),
        ))
    }
}

pub struct ListWorktreeFiles {
    context: WorktreeContext,
    description: String,
}

impl ListWorktreeFiles {
    pub fn new(context: WorktreeContext) -> Self {
        let description = desc_list_worktree_files(context.limits);
        Self {
            context,
            description,
        }
    }
}

impl Tool for ListWorktreeFiles {
    fn name(&self) -> &str {
        "list_worktree_files"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        glob_schema()
    }

    fn origin(&self) -> Origin {
        Origin::Builtin
    }

    fn requires_worktree(&self) -> bool {
        true
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let arguments = Arguments::new(self.name(), arguments);
        let glob = check_glob(&self.context.paths, self.name(), arguments.text("glob")?)?;
        let found = self
            .context
            .source
            .list_files(&glob)
            .map_err(|error| WorktreeContext::failed(self.name(), error))?;
        Ok(self.context.answer().listing(found, true))
    }
}

pub struct StatWorktreeFile {
    context: WorktreeContext,
}

impl StatWorktreeFile {
    pub fn new(context: WorktreeContext) -> Self {
        Self { context }
    }
}

impl Tool for StatWorktreeFile {
    fn name(&self) -> &str {
        "stat_worktree_file"
    }

    fn description(&self) -> &str {
        DESC_STAT_WORKTREE_FILE
    }

    fn parameters(&self) -> Value {
        stat_schema()
    }

    fn origin(&self) -> Origin {
        Origin::Builtin
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let arguments = Arguments::new(self.name(), arguments);
        let path = self.context.path(self.name(), arguments.text("path")?)?;
        let body = self
            .context
            .source
            .read_file(&path, None)
            .map_err(|error| WorktreeContext::failed_read(self.name(), error))?;
        Ok(self.context.answer().stat(&path, &body))
    }
}

pub struct ReadWorktreeFile {
    context: WorktreeContext,
}

impl ReadWorktreeFile {
    pub fn new(context: WorktreeContext) -> Self {
        Self { context }
    }
}

impl Tool for ReadWorktreeFile {
    fn name(&self) -> &str {
        "read_worktree_file"
    }

    fn description(&self) -> &str {
        DESC_READ_WORKTREE_FILE
    }

    fn parameters(&self) -> Value {
        path_schema()
    }

    fn origin(&self) -> Origin {
        Origin::Builtin
    }

    fn requires_worktree(&self) -> bool {
        true
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let arguments = Arguments::new(self.name(), arguments);
        let path = self.context.path(self.name(), arguments.text("path")?)?;
        let range = arguments.range()?;
        let body = self
            .context
            .source
            .read_file(&path, None)
            .map_err(|error| WorktreeContext::failed_read(self.name(), error))?;
        self.context.answer().file(self.name(), &path, range, body)
    }
}

pub struct SearchWorktree {
    context: WorktreeContext,
    description: String,
}

impl SearchWorktree {
    pub fn new(context: WorktreeContext) -> Self {
        let description = desc_search_worktree(context.limits);
        Self {
            context,
            description,
        }
    }
}

impl Tool for SearchWorktree {
    fn name(&self) -> &str {
        "search_worktree"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        search_schema()
    }

    fn origin(&self) -> Origin {
        Origin::Builtin
    }

    fn requires_worktree(&self) -> bool {
        true
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let arguments = Arguments::new(self.name(), arguments);
        let query = arguments.text("query")?;
        let glob = match arguments.optional_text("glob")? {
            Some(given) => Some(check_glob(&self.context.paths, self.name(), given)?),
            None => None,
        };
        let hits = self
            .context
            .source
            .search(query, glob.as_deref())
            .map_err(|error| WorktreeContext::failed(self.name(), error))?;
        Ok(self
            .context
            .answer()
            .hits(hits.into_iter().map(Hit::from).collect(), None))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::config::SecuritySettings;
    use crate::platform::{Listing, SearchHit as RepoHit};
    use crate::worktree::{LineRange as DiskRange, SearchHit as DiskHit};

    /// Counts every question it is asked. That is the whole point of these
    /// fakes: an out of bounds argument has to be refused *before* the source
    /// is reached, and only a counter can tell that from a source that was
    /// asked and happened to answer with nothing.
    struct CountingRepo {
        calls: AtomicUsize,
        listing: Listing,
        body: String,
        hits: Vec<RepoHit>,
    }

    struct CountingDisk {
        calls: AtomicUsize,
        root: PathBuf,
        paths: Vec<String>,
        body: String,
        hits: Vec<DiskHit>,
    }

    impl CountingRepo {
        fn empty() -> Self {
            Self {
                calls: AtomicUsize::new(0),
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

    impl CountingDisk {
        fn empty(root: &Path) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                root: root.to_path_buf(),
                paths: Vec::new(),
                body: String::new(),
                hits: Vec::new(),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl RepoSource for CountingRepo {
        fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.listing.clone())
        }

        fn read_file(
            &self,
            _path: &str,
            _lines: Option<RepoRange>,
        ) -> Result<String, PlatformError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.body.clone())
        }

        fn search(&self, _query: &str, _glob: Option<&str>) -> Result<Vec<RepoHit>, PlatformError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.hits.clone())
        }
    }

    impl WorktreeSource for CountingDisk {
        fn root(&self) -> &Path {
            &self.root
        }

        fn head_sha(&self) -> Result<String, WorktreeError> {
            Ok("head".to_string())
        }

        fn list_files(&self, _glob: &str) -> Result<Vec<String>, WorktreeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.paths.clone())
        }

        fn read_file(
            &self,
            _path: &str,
            _lines: Option<DiskRange>,
        ) -> Result<String, WorktreeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.body.clone())
        }

        fn search(&self, _query: &str, _glob: Option<&str>) -> Result<Vec<DiskHit>, WorktreeError> {
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
        max_read_bytes: 262_144,
        max_output_bytes: 65_536,
        max_files_listed: 200,
        max_search_hits: 50,
    };

    fn repo_context(source: &Arc<CountingRepo>, deny: &[&str]) -> RepoContext {
        RepoContext::new(
            Arc::clone(source) as Arc<dyn RepoSource>,
            policy(deny, None),
            LIMITS,
        )
    }

    fn disk_context(source: &Arc<CountingDisk>, deny: &[&str]) -> WorktreeContext {
        let root = source.root().to_path_buf();
        WorktreeContext::new(
            Arc::clone(source) as Arc<dyn WorktreeSource>,
            policy(deny, Some(&root)),
            LIMITS,
        )
    }

    fn repo_tools(source: &Arc<CountingRepo>, deny: &[&str]) -> Vec<Box<dyn Tool>> {
        let context = repo_context(source, deny);
        vec![
            Box::new(ListRepoFiles::new(context.clone())),
            Box::new(ReadRepoFile::new(context.clone())),
            Box::new(SearchRepo::new(context, Capabilities::default())),
        ]
    }

    fn disk_tools(source: &Arc<CountingDisk>, deny: &[&str]) -> Vec<Box<dyn Tool>> {
        let context = disk_context(source, deny);
        vec![
            Box::new(ListWorktreeFiles::new(context.clone())),
            Box::new(ReadWorktreeFile::new(context.clone())),
            Box::new(SearchWorktree::new(context)),
        ]
    }

    /// An argument that names a path or a pattern this tool may take, aimed
    /// somewhere it may not go.
    fn out_of_bounds(tool: &str) -> Value {
        match tool {
            "list_repo_files" | "list_worktree_files" => json!({"glob": "../etc/**"}),
            "read_repo_file" | "read_worktree_file" => json!({"path": "../etc/passwd"}),
            _ => json!({"query": "password", "glob": "secrets/**"}),
        }
    }

    /// The boundary case for every one of the six, because the check being in
    /// the tool is the only thing holding the read boundary up: there is no
    /// type that a source refuses to be called with (§6 security).
    #[test]
    fn an_out_of_bounds_argument_is_refused_before_either_source_is_asked() {
        let repo = Arc::new(CountingRepo::empty());
        let root = tempfile::tempdir().expect("temp dir");
        let disk = Arc::new(CountingDisk::empty(root.path()));

        for tool in repo_tools(&repo, &["secrets/**"]) {
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
        assert_eq!(repo.calls(), 0, "no repository read was attempted");

        for tool in disk_tools(&disk, &["secrets/**"]) {
            let error = tool
                .execute(&out_of_bounds(tool.name()))
                .expect_err(tool.name());
            assert!(
                matches!(error, ToolError::Rejected { .. }),
                "{}: {error}",
                tool.name()
            );
        }
        assert_eq!(disk.calls(), 0, "no disk read was attempted");
    }

    /// A denied path is dropped from a listing rather than turned into a
    /// placeholder, and the extension whitelist is not applied there at all:
    /// seeing `Makefile` tells the model what kind of project this is, and
    /// trying to read it is refused separately.
    #[test]
    fn listings_drop_denied_paths_and_keep_ones_that_cannot_be_read() {
        let repo = Arc::new(CountingRepo {
            listing: Listing {
                paths: vec![
                    "src/parse.c".to_string(),
                    "secrets/deploy.toml".to_string(),
                    "Makefile".to_string(),
                ],
                complete: true,
            },
            ..CountingRepo::empty()
        });
        let context = repo_context(&repo, &["secrets/**"]);

        let listed = ListRepoFiles::new(context.clone())
            .execute(&json!({"glob": "**/*"}))
            .expect("listed");
        assert!(listed.text.contains("src/parse.c"), "{}", listed.text);
        assert!(listed.text.contains("Makefile"), "{}", listed.text);
        assert!(!listed.text.contains("secrets/"), "{}", listed.text);

        let refused = ReadRepoFile::new(context)
            .execute(&json!({"path": "Makefile"}))
            .expect_err("no extension");
        assert!(matches!(refused, ToolError::Rejected { .. }), "{refused}");
        assert_eq!(repo.calls(), 1, "the refused read never reached the source");
    }

    #[test]
    fn a_search_drops_hits_in_denied_files() {
        let repo = Arc::new(CountingRepo {
            hits: vec![
                RepoHit {
                    path: "src/parse.c".to_string(),
                    line: 12,
                    text: "int token = 5;".to_string(),
                },
                RepoHit {
                    path: "secrets/deploy.toml".to_string(),
                    line: 3,
                    text: "token = \"real\"".to_string(),
                },
            ],
            ..CountingRepo::empty()
        });
        let output = SearchRepo::new(
            repo_context(&repo, &["secrets/**"]),
            Capabilities::default(),
        )
        .execute(&json!({"query": "token"}))
        .expect("searched");
        assert!(output.text.contains("src/parse.c:12:"), "{}", output.text);
        assert!(!output.text.contains("secrets/"), "{}", output.text);
    }

    #[test]
    fn a_listing_past_the_ceiling_says_how_many_are_left() {
        let repo = Arc::new(CountingRepo {
            listing: Listing {
                paths: (0..LIMITS.max_files_listed + 5)
                    .map(|index| format!("src/file{index:04}.c"))
                    .collect(),
                complete: true,
            },
            ..CountingRepo::empty()
        });
        let output = ListRepoFiles::new(repo_context(&repo, &[]))
            .execute(&json!({"glob": "src/**/*.c"}))
            .expect("listed");
        assert_eq!(output.text.lines().count(), LIMITS.max_files_listed + 1);
        assert!(output.text.contains("5 more paths"), "{}", output.text);
    }

    /// A truncated tree is not an empty directory, and the difference has to
    /// reach the model: a half list read as a whole one is how it decides
    /// something does not exist.
    #[test]
    fn an_incomplete_listing_says_so_in_words_the_model_reads() {
        let repo = Arc::new(CountingRepo {
            listing: Listing {
                paths: vec!["src/parse.c".to_string()],
                complete: false,
            },
            ..CountingRepo::empty()
        });
        let output = ListRepoFiles::new(repo_context(&repo, &[]))
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
        let repo = Arc::new(CountingRepo {
            body: "one\ntwo\nthree\nfour\n".to_string(),
            ..CountingRepo::empty()
        });
        let output = ReadRepoFile::new(repo_context(&repo, &[]))
            .execute(&json!({"path": "src/parse.c", "first_line": 2, "last_line": 3}))
            .expect("read");

        assert_eq!(output.text, "two\nthree");
        let file = output.context_file.expect("a read is a context file");
        assert_eq!(file.path, "src/parse.c");
        assert_eq!((file.first_line, file.last_line), (2, 3));
        assert_eq!(file.body, "two\nthree");
    }

    /// Past `max_read_bytes` the answer is a refusal with the size in it. A
    /// silent clip would read exactly like a short file.
    #[test]
    fn a_file_over_max_read_bytes_is_refused_rather_than_clipped() {
        let repo = Arc::new(CountingRepo {
            body: "x".repeat(1_000),
            ..CountingRepo::empty()
        });
        let context = RepoContext::new(
            Arc::clone(&repo) as Arc<dyn RepoSource>,
            policy(&[], None),
            ToolLimits {
                max_read_bytes: 100,
                ..LIMITS
            },
        );
        let error = ReadRepoFile::new(context)
            .execute(&json!({"path": "src/parse.c"}))
            .expect_err("over the ceiling");
        assert!(matches!(error, ToolError::Rejected { .. }), "{error}");
        assert!(error.to_string().contains("max_read_bytes"), "{error}");
        assert!(error.to_string().contains("1000"), "{error}");
    }

    /// `max_read_bytes` bounds the fetch, not the answer, so a range is no
    /// way around it: slicing a file means downloading all of it first, and
    /// the platform API has no way to ask for part of a blob.
    #[test]
    fn a_line_range_does_not_get_an_oversized_file_past_the_ceiling() {
        let repo = Arc::new(CountingRepo {
            body: "x".repeat(1_000),
            ..CountingRepo::empty()
        });
        let context = RepoContext::new(
            Arc::clone(&repo) as Arc<dyn RepoSource>,
            policy(&[], None),
            ToolLimits {
                max_read_bytes: 100,
                ..LIMITS
            },
        );
        assert!(
            ReadRepoFile::new(context)
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
        let repo = Arc::new(CountingRepo {
            body: "0123456789\n".repeat(400),
            ..CountingRepo::empty()
        });
        let context = RepoContext::new(
            Arc::clone(&repo) as Arc<dyn RepoSource>,
            policy(&[], None),
            ToolLimits {
                max_output_bytes: 500,
                ..LIMITS
            },
        );

        let error = ReadRepoFile::new(context.clone())
            .execute(&json!({"path": "src/parse.c"}))
            .expect_err("4400 bytes do not fit in 500");
        let said = error.to_string();
        assert!(matches!(error, ToolError::Rejected { .. }), "{said}");
        assert!(said.contains("400 lines"), "{said}");
        assert!(said.contains("4400 bytes"), "{said}");
        assert!(said.contains("line range"), "{said}");

        // And the range it was told to use comes back whole, not clipped.
        let output = ReadRepoFile::new(context)
            .execute(&json!({"path": "src/parse.c", "first_line": 1, "last_line": 30}))
            .expect("30 lines fit");
        assert_eq!(output.text.lines().count(), 30);
        assert_eq!(output.omitted_bytes, 0, "a read is never clipped");
    }

    /// A range that still does not fit is refused the same way rather than
    /// clipped, or paging would silently stop being paging.
    #[test]
    fn a_range_too_big_to_return_is_refused_too() {
        let repo = Arc::new(CountingRepo {
            body: "0123456789\n".repeat(400),
            ..CountingRepo::empty()
        });
        let context = RepoContext::new(
            Arc::clone(&repo) as Arc<dyn RepoSource>,
            policy(&[], None),
            ToolLimits {
                max_output_bytes: 500,
                ..LIMITS
            },
        );
        let error = ReadRepoFile::new(context)
            .execute(&json!({"path": "src/parse.c", "first_line": 10, "last_line": 300}))
            .expect_err("291 lines do not fit either");
        assert!(error.to_string().contains("lines 10-300"), "{error}");
    }

    /// What `stat` is for: the size, and what to do about it, without paying
    /// for any of the content. A file too big to read at all says so, so the
    /// model does not spend a round finding out.
    #[test]
    fn stat_answers_with_the_size_and_a_plan_and_no_content() {
        let repo = Arc::new(CountingRepo {
            body: "0123456789\n".repeat(400),
            ..CountingRepo::empty()
        });
        let context = RepoContext::new(
            Arc::clone(&repo) as Arc<dyn RepoSource>,
            policy(&[], None),
            ToolLimits {
                max_output_bytes: 500,
                ..LIMITS
            },
        );
        let output = StatRepoFile::new(context)
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

        let tight = RepoContext::new(
            Arc::clone(&repo) as Arc<dyn RepoSource>,
            policy(&[], None),
            ToolLimits {
                max_read_bytes: 100,
                ..LIMITS
            },
        );
        let output = StatRepoFile::new(tight)
            .execute(&json!({"path": "src/parse.c"}))
            .expect("stat still answers for a file too big to read");
        assert!(output.text.contains("max_read_bytes"), "{}", output.text);
    }

    #[test]
    fn half_a_line_range_is_answered_rather_than_guessed_at() {
        let repo = Arc::new(CountingRepo::empty());
        let error = ReadRepoFile::new(repo_context(&repo, &[]))
            .execute(&json!({"path": "src/parse.c", "first_line": 4}))
            .expect_err("half a range");
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error}"
        );
        assert_eq!(repo.calls(), 0);
    }

    /// The names are the ones `config` reserves, and `search_repo` describes
    /// the instance it was registered for rather than the group it is in: the
    /// same name is a regular expression on one platform and a keyword index
    /// on another, and the model reads only the description.
    #[test]
    fn the_searches_describe_the_strength_they_actually_have() {
        let repo = Arc::new(CountingRepo::empty());
        let keyword = SearchRepo::new(
            repo_context(&repo, &[]),
            Capabilities {
                code_search: true,
                regex_search: false,
            },
        );
        assert_eq!(keyword.name(), "search_repo");
        assert!(keyword.description().contains("keyword matching"));
        assert!(
            keyword
                .description()
                .contains("regex metacharacters are literal")
        );
        assert!(keyword.description().contains("does not mean it is absent"));

        let expressions = SearchRepo::new(
            repo_context(&repo, &[]),
            Capabilities {
                code_search: true,
                regex_search: true,
            },
        );
        assert!(
            expressions.description().contains("regular expression")
                && !expressions.description().contains("literal"),
            "{}",
            expressions.description()
        );

        let root = tempfile::tempdir().expect("temp dir");
        let disk = Arc::new(CountingDisk::empty(root.path()));
        let local = SearchWorktree::new(disk_context(&disk, &[]));
        assert!(local.description().contains("regular expression"));
    }

    /// An empty answer from a keyword index that only covers the default
    /// branch says nothing about the branch under review, so the answer says
    /// that instead of just being empty.
    #[test]
    fn an_empty_keyword_search_says_that_it_is_not_a_denial() {
        let repo = Arc::new(CountingRepo::empty());
        let output = SearchRepo::new(
            repo_context(&repo, &[]),
            Capabilities {
                code_search: true,
                regex_search: false,
            },
        )
        .execute(&json!({"query": "parse_token"}))
        .expect("searched");
        assert!(
            output.text.contains("does not mean it is absent"),
            "{}",
            output.text
        );
    }

    /// The disk tools are the ones that need a checkout, and they say so, so
    /// `build_tools` has something to go on beyond knowing their names.
    #[test]
    fn only_the_disk_group_declares_that_it_needs_a_worktree() {
        let repo = Arc::new(CountingRepo::empty());
        let root = tempfile::tempdir().expect("temp dir");
        let disk = Arc::new(CountingDisk::empty(root.path()));
        for tool in repo_tools(&repo, &[]) {
            assert!(!tool.requires_worktree(), "{}", tool.name());
            assert_eq!(tool.origin(), Origin::Builtin);
        }
        for tool in disk_tools(&disk, &[]) {
            assert!(tool.requires_worktree(), "{}", tool.name());
            assert_eq!(tool.origin(), Origin::Builtin);
        }
    }

    /// The disk group reads a real checkout through the real `LocalWorktree`,
    /// which is what a symlink out of the tree has to be refused against.
    #[test]
    fn a_worktree_read_goes_through_the_checkout_and_stops_at_its_edge() {
        let root = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(root.path().join("src")).expect("src");
        std::fs::write(root.path().join("src/parse.c"), "int main(void)\n{\n}\n").expect("source");
        let worktree =
            crate::worktree::LocalWorktree::open(root.path().to_path_buf()).expect("worktree");
        let context = WorktreeContext::new(
            Arc::new(worktree) as Arc<dyn WorktreeSource>,
            policy(&[], Some(root.path())),
            LIMITS,
        );

        let listed = ListWorktreeFiles::new(context.clone())
            .execute(&json!({"glob": "**/*.c"}))
            .expect("listed");
        assert!(listed.text.contains("src/parse.c"), "{}", listed.text);

        let read = ReadWorktreeFile::new(context.clone())
            .execute(&json!({"path": "src/parse.c"}))
            .expect("read");
        assert!(read.text.contains("int main(void)"), "{}", read.text);
        assert_eq!(
            read.context_file.expect("a read is a context file").path,
            "src/parse.c"
        );

        let hits = SearchWorktree::new(context)
            .execute(&json!({"query": r"int\s+main"}))
            .expect("searched");
        assert!(hits.text.contains("src/parse.c:1:"), "{}", hits.text);
    }
}
