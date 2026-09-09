//! GitHub: bearer token, one review request for all inline comments.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{Backoff, PlatformEntry, PlatformKind, Secret};
use crate::domain::Narrative;

use super::http::HttpClient;
use super::source::{LineRange, Listing, RepoSource, SearchHit};
use super::{
    Capabilities, ChangeRef, DiffRefs, ExistingComment, OutgoingComment, Platform, PlatformChange,
    PlatformError,
};

const API_VERSION: &str = "2022-11-28";
const ACCEPT_JSON: &str = "application/vnd.github+json";
const ACCEPT_DIFF: &str = "application/vnd.github.v3.diff";
const ACCEPT_RAW: &str = "application/vnd.github.raw";

/// How many directories the per-directory walk may visit when the recursive
/// tree came back truncated. Past this the listing is incomplete and says so.
const WALK_CEILING: usize = 32;

/// How many search hits are taken as candidates. Each one costs a file read at
/// `head_sha`, so the number of reads a single search can trigger is bounded.
const SEARCH_CANDIDATES: usize = 20;

pub struct GitHub {
    entry: PlatformEntry,
    token: Secret,
    http: Arc<HttpClient>,
    repo: Arc<GitHubRepo>,
}

impl GitHub {
    pub fn new(entry: PlatformEntry, token: Secret, backoff: Backoff) -> Self {
        let http = Arc::new(HttpClient::new(
            entry.base_url.clone(),
            entry.host.clone(),
            token.expose(),
            backoff,
        ));
        let repo = Arc::new(GitHubRepo {
            host: entry.host.clone(),
            http: Arc::clone(&http),
            token: token.clone(),
            commit: Mutex::new(None),
            whole_tree: Mutex::new(None),
            trees: Mutex::new(BTreeMap::new()),
            files: Mutex::new(BTreeMap::new()),
        });
        Self {
            entry,
            token,
            http,
            repo,
        }
    }

    /// `owner/repo` split, which the REST paths need separately.
    pub fn owner_and_repo(change: &ChangeRef) -> Option<(&str, &str)> {
        change.project.split_once('/')
    }

    fn parts<'a>(
        &self,
        change: &'a ChangeRef,
    ) -> Result<(&'a str, &'a str, String), PlatformError> {
        let (owner, repo) = Self::owner_and_repo(change).ok_or_else(|| PlatformError::Request {
            operation: "reading the pull request",
            host: self.http.host().to_string(),
            reason: format!("project {:?} is not owner/repo", change.project),
        })?;
        Ok((owner, repo, change.number.to_string()))
    }

    /// `https://github.com/owner/repo/pull/N` → `{base_url}/repos/owner/repo/pulls/N`.
    fn pull_url(&self, change: &ChangeRef, extra: &[&str]) -> Result<reqwest::Url, PlatformError> {
        let (owner, repo, number) = self.parts(change)?;
        let mut segments = vec!["repos", owner, repo, "pulls", number.as_str()];
        segments.extend_from_slice(extra);
        self.http.url(&segments)
    }

    fn issues_url(
        &self,
        change: &ChangeRef,
        extra: &[&str],
    ) -> Result<reqwest::Url, PlatformError> {
        let (owner, repo, number) = self.parts(change)?;
        let mut segments = vec!["repos", owner, repo, "issues", number.as_str()];
        segments.extend_from_slice(extra);
        self.http.url(&segments)
    }

    fn apply_json(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        request
            .header("Authorization", format!("Bearer {}", self.token.expose()))
            .header("Accept", ACCEPT_JSON)
            .header("X-GitHub-Api-Version", API_VERSION)
    }

    fn json_headers(&self) -> Vec<(&str, String)> {
        vec![
            ("Authorization", format!("Bearer {}", self.token.expose())),
            ("Accept", ACCEPT_JSON.to_string()),
            ("X-GitHub-Api-Version", API_VERSION.to_string()),
        ]
    }

    fn pull(&self, change: &ChangeRef) -> Result<GithubPull, PlatformError> {
        let url = self.pull_url(change, &[])?;
        let response = self.http.send("reading the pull request", || {
            self.apply_json(self.http.get(url.clone()))
        })?;
        self.http.json("reading the pull request", &response.body)
    }

    /// Commit subjects, one page of them. `Narrative::COMMITS + 1` is asked
    /// for so a longer branch is *known* to be longer rather than silently
    /// cut, and paging past that would buy fixup messages at the price of a
    /// round trip each.
    fn commit_messages(&self, change: &ChangeRef) -> Vec<String> {
        match self.commits(change) {
            Ok(messages) => messages,
            Err(error) => {
                tracing::warn!("no commit messages for the review: {error}");
                Vec::new()
            }
        }
    }

    fn commits(&self, change: &ChangeRef) -> Result<Vec<String>, PlatformError> {
        let mut url = self.pull_url(change, &["commits"])?;
        url.query_pairs_mut()
            .append_pair("per_page", &(Narrative::COMMITS + 1).to_string());
        let response = self.http.send("listing the pull request commits", || {
            self.apply_json(self.http.get(url.clone()))
        })?;
        let rows: Vec<GithubCommit> = self
            .http
            .json("listing the pull request commits", &response.body)?;
        Ok(rows.into_iter().map(|row| row.commit.message).collect())
    }

    fn review_comment(comment: &OutgoingComment, file_level: bool) -> Value {
        let mut item = json!({
            "path": comment.paths.display(),
            "body": comment.body,
        });
        if file_level || comment.line.is_none() {
            item["subject_type"] = json!("file");
            return item;
        }
        item["side"] = json!("RIGHT");
        match (comment.line, comment.end_line) {
            (Some(start), Some(end)) if start != end => {
                item["start_line"] = json!(start);
                item["start_side"] = json!("RIGHT");
                item["line"] = json!(end);
            }
            (Some(line), _) => {
                item["line"] = json!(line);
            }
            _ => {}
        }
        item
    }

    fn post_review(
        &self,
        change: &ChangeRef,
        refs: &DiffRefs,
        comments: &[OutgoingComment],
        file_level: bool,
    ) -> Result<Vec<ExistingComment>, PlatformError> {
        let url = self.pull_url(change, &["reviews"])?;
        let payload = json!({
            "commit_id": refs.head_sha,
            "event": "COMMENT",
            "body": "reviewbot",
            "comments": comments
                .iter()
                .map(|comment| Self::review_comment(comment, file_level))
                .collect::<Vec<_>>(),
        });
        let response = self.http.send("submitting a review", || {
            self.apply_json(self.http.post(url.clone()))
                .header("Content-Type", "application/json")
                .json(&payload)
        })?;
        let value: Value = self.http.json("submitting a review", &response.body)?;
        let url = value
            .get("html_url")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(comments
            .iter()
            .map(|comment| ExistingComment {
                marker: comment.marker.clone(),
                url: url.clone(),
                degraded_to_file: file_level,
            })
            .collect())
    }

    fn post_issue_comment(
        &self,
        change: &ChangeRef,
        comment: &OutgoingComment,
    ) -> Result<ExistingComment, PlatformError> {
        let url = self.issues_url(change, &["comments"])?;
        let payload = json!({ "body": comment.body });
        let response = self.http.send("posting an issue comment", || {
            self.apply_json(self.http.post(url.clone()))
                .header("Content-Type", "application/json")
                .json(&payload)
        })?;
        let value: Value = self.http.json("posting an issue comment", &response.body)?;
        Ok(ExistingComment {
            marker: comment.marker.clone(),
            url: value
                .get("html_url")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            degraded_to_file: false,
        })
    }
}

impl Platform for GitHub {
    fn kind(&self) -> PlatformKind {
        PlatformKind::Github
    }

    fn host(&self) -> &str {
        &self.entry.host
    }

    /// The code search endpoint is always there. It indexes the default branch
    /// and takes keywords rather than expressions, so hits are candidate paths
    /// that get re-read at `head_sha` and rematched locally.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            code_search: true,
            regex_search: false,
        }
    }

    fn head_sha(&self, change: &ChangeRef) -> Result<String, PlatformError> {
        Ok(self.pull(change)?.head.sha)
    }

    fn fetch_change(&self, change: &ChangeRef) -> Result<PlatformChange, PlatformError> {
        let pull = self.pull(change)?;
        let url = self.pull_url(change, &[])?;
        let response = self.http.send("fetching the pull request diff", || {
            self.http
                .get(url.clone())
                .header("Authorization", format!("Bearer {}", self.token.expose()))
                .header("Accept", ACCEPT_DIFF)
                .header("X-GitHub-Api-Version", API_VERSION)
        })?;
        Ok(PlatformChange {
            diff: response.body,
            head_sha: pull.head.sha,
            base_sha: pull.base.map(|base| base.sha),
            start_sha: None,
            // The title and body came free with the pull object. Only the
            // commit subjects cost a request, and a run that cannot have
            // them still reviews the diff.
            narrative: Narrative::new(pull.title, pull.body, self.commit_messages(change)),
        })
    }

    fn existing_comments(&self, change: &ChangeRef) -> Result<Vec<ExistingComment>, PlatformError> {
        let mut found = Vec::new();
        let mut review_url = self.pull_url(change, &["comments"])?;
        review_url.query_pairs_mut().append_pair("per_page", "100");
        for row in
            self.http
                .get_pages("listing review comments", review_url, &self.json_headers())?
        {
            let comment: GithubComment =
                serde_json::from_value(row).map_err(|error| PlatformError::Request {
                    operation: "listing review comments",
                    host: self.http.host().to_string(),
                    reason: error.to_string(),
                })?;
            found.extend(ExistingComment::from_body(&comment.body, comment.html_url));
        }

        let mut issue_url = self.issues_url(change, &["comments"])?;
        issue_url.query_pairs_mut().append_pair("per_page", "100");
        for row in self
            .http
            .get_pages("listing issue comments", issue_url, &self.json_headers())?
        {
            let comment: GithubComment =
                serde_json::from_value(row).map_err(|error| PlatformError::Request {
                    operation: "listing issue comments",
                    host: self.http.host().to_string(),
                    reason: error.to_string(),
                })?;
            found.extend(ExistingComment::from_body(&comment.body, comment.html_url));
        }

        found.sort_by(|a, b| a.marker.cmp(&b.marker));
        found.dedup_by(|a, b| a.marker == b.marker);
        Ok(found)
    }

    fn post_comments(
        &self,
        change: &ChangeRef,
        refs: &DiffRefs,
        comments: &[OutgoingComment],
    ) -> Result<Vec<ExistingComment>, PlatformError> {
        let (summaries, inline): (Vec<_>, Vec<_>) =
            comments.iter().cloned().partition(|c| c.is_summary());
        let mut posted = Vec::new();
        if !inline.is_empty() {
            match self.post_review(change, refs, &inline, false) {
                Ok(done) => posted.extend(done),
                Err(PlatformError::Unprocessable { .. }) => {
                    tracing::warn!("review comments were rejected; retrying as file-level");
                    posted.extend(self.post_review(change, refs, &inline, true)?);
                }
                Err(error) => return Err(error),
            }
        }
        for comment in summaries {
            posted.push(self.post_issue_comment(change, &comment)?);
        }
        Ok(posted)
    }

    fn bind_repo(&self, change: &ChangeRef, head_sha: &str) {
        if let Some((owner, repo)) = Self::owner_and_repo(change) {
            self.repo.bind(owner, repo, head_sha);
        }
    }

    fn repo_source(&self) -> Arc<dyn RepoSource> {
        Arc::clone(&self.repo) as Arc<dyn RepoSource>
    }
}

/// The repository and commit every read goes against. There is nothing to read
/// before the head commit is known, which is why this starts out empty.
#[derive(Clone, Debug)]
struct Commit {
    owner: String,
    repo: String,
    sha: String,
}

/// One tree response. `truncated` is GitHub saying "this request could not
/// carry the whole tree", which is not the same as the tree being unknowable.
#[derive(Clone, Debug, Default)]
struct Tree {
    entries: Vec<TreeEntry>,
    truncated: bool,
}

#[derive(Clone, Debug)]
struct TreeEntry {
    path: String,
    sha: String,
    is_tree: bool,
}

/// Repository reads for one repository at one commit. Trees and file bodies
/// are cached because the sha cannot change inside a run.
struct GitHubRepo {
    host: String,
    http: Arc<HttpClient>,
    token: Secret,
    commit: Mutex<Option<Commit>>,
    /// Every blob path, set only when one recursive request carried the whole
    /// tree. From then on no glob costs a request.
    whole_tree: Mutex<Option<Vec<String>>>,
    /// Entries per tree sha, so the per-directory fallback does not walk the
    /// same directory twice across two listings.
    trees: Mutex<BTreeMap<String, Tree>>,
    files: Mutex<BTreeMap<String, String>>,
}

impl GitHubRepo {
    fn bind(&self, owner: &str, repo: &str, sha: &str) {
        *self.commit.lock().expect("commit") = Some(Commit {
            owner: owner.to_string(),
            repo: repo.to_string(),
            sha: sha.to_string(),
        });
    }

    fn commit(&self) -> Result<Commit, PlatformError> {
        self.commit
            .lock()
            .expect("commit")
            .clone()
            .ok_or_else(|| PlatformError::Request {
                operation: "reading the repository",
                host: self.host.clone(),
                reason: "the commit under review is not known yet".to_string(),
            })
    }

    fn get(&self, url: reqwest::Url, accept: &str) -> reqwest::blocking::RequestBuilder {
        self.http
            .get(url)
            .header("Authorization", format!("Bearer {}", self.token.expose()))
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", API_VERSION)
    }

    /// One tree, by its own sha. `recursive` asks for the whole thing in one
    /// go; without it the answer is a single directory level, which is the
    /// only way past a truncated response.
    fn tree(&self, sha: &str, recursive: bool) -> Result<Tree, PlatformError> {
        let key = match recursive {
            true => format!("{sha}:recursive"),
            false => sha.to_string(),
        };
        if let Some(cached) = self.trees.lock().expect("trees").get(&key) {
            return Ok(cached.clone());
        }
        let commit = self.commit()?;
        let mut url =
            self.http
                .url(&["repos", &commit.owner, &commit.repo, "git", "trees", sha])?;
        if recursive {
            url.query_pairs_mut().append_pair("recursive", "1");
        }
        let response = self.http.send("listing the repository tree", || {
            self.get(url.clone(), ACCEPT_JSON)
        })?;
        let payload: GithubTree = self
            .http
            .json("listing the repository tree", &response.body)?;
        let tree = Tree {
            entries: payload
                .tree
                .into_iter()
                .filter(|entry| entry.kind == "blob" || entry.kind == "tree")
                .map(|entry| TreeEntry {
                    path: entry.path,
                    sha: entry.sha,
                    is_tree: entry.kind == "tree",
                })
                .collect(),
            truncated: payload.truncated,
        };
        self.trees.lock().expect("trees").insert(key, tree.clone());
        Ok(tree)
    }

    /// The fallback for a truncated recursive tree: descend one directory at a
    /// time, and only into directories the glob's literal prefix can still
    /// reach. `src/**/*.c` therefore never asks about `docs`.
    fn walk(
        &self,
        root: &str,
        prefix: &str,
        matcher: &GlobMatcher,
    ) -> Result<Listing, PlatformError> {
        let mut pending = VecDeque::from([(root.to_string(), String::new())]);
        let mut paths = Vec::new();
        let mut visited = 0usize;
        let mut complete = true;
        while let Some((sha, directory)) = pending.pop_front() {
            if visited >= WALK_CEILING {
                complete = false;
                break;
            }
            visited += 1;
            for entry in self.tree(&sha, false)?.entries {
                let path = match directory.is_empty() {
                    true => entry.path.clone(),
                    false => format!("{directory}/{}", entry.path),
                };
                if entry.is_tree {
                    if reachable(prefix, &path) {
                        pending.push_back((entry.sha, path));
                    }
                } else if matcher.is_match(&path) {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        paths.dedup();
        Ok(Listing { paths, complete })
    }

    fn body(&self, path: &str) -> Result<String, PlatformError> {
        if let Some(cached) = self.files.lock().expect("files").get(path) {
            return Ok(cached.clone());
        }
        let commit = self.commit()?;
        let mut segments = vec![
            "repos",
            commit.owner.as_str(),
            commit.repo.as_str(),
            "contents",
        ];
        segments.extend(path.split('/'));
        let mut url = self.http.url(&segments)?;
        url.query_pairs_mut().append_pair("ref", &commit.sha);
        let response = self.http.send("reading a repository file", || {
            self.get(url.clone(), ACCEPT_RAW)
        })?;
        self.files
            .lock()
            .expect("files")
            .insert(path.to_string(), response.body.clone());
        Ok(response.body)
    }

    fn matcher(&self, glob: &str) -> Result<GlobMatcher, PlatformError> {
        Ok(Glob::new(glob)
            .map_err(|error| PlatformError::Request {
                operation: "listing the repository tree",
                host: self.host.clone(),
                reason: format!("invalid glob {glob:?}: {error}"),
            })?
            .compile_matcher())
    }

    /// The paths the search index offered, in order, deduplicated and cut to
    /// the number of reads one search is allowed to cost.
    fn candidates(
        &self,
        query: &str,
        glob: Option<&GlobMatcher>,
    ) -> Result<Vec<String>, PlatformError> {
        let commit = self.commit()?;
        let mut url = self.http.url(&["search", "code"])?;
        url.query_pairs_mut().append_pair(
            "q",
            &format!("{query} repo:{}/{}", commit.owner, commit.repo),
        );
        let response = self.http.send("searching the repository", || {
            self.get(url.clone(), ACCEPT_JSON)
        })?;
        let payload: GithubSearch = self.http.json("searching the repository", &response.body)?;
        let mut paths = Vec::new();
        for item in payload.items {
            if paths.len() >= SEARCH_CANDIDATES {
                break;
            }
            if glob.is_some_and(|matcher| !matcher.is_match(&item.path)) {
                continue;
            }
            if !paths.contains(&item.path) {
                paths.push(item.path);
            }
        }
        Ok(paths)
    }
}

impl RepoSource for GitHubRepo {
    /// One recursive request first, which is where nearly every repository
    /// stops. A truncated answer is not a missing tree: it means this request
    /// shape cannot carry it, so the walk takes over.
    fn list_files(&self, glob: &str) -> Result<Listing, PlatformError> {
        let matcher = self.matcher(glob)?;
        if let Some(cached) = self.whole_tree.lock().expect("whole tree").clone() {
            return Ok(Listing {
                paths: cached
                    .into_iter()
                    .filter(|path| matcher.is_match(path))
                    .collect(),
                complete: true,
            });
        }
        let commit = self.commit()?;
        let tree = self.tree(&commit.sha, true)?;
        if !tree.truncated {
            let mut all: Vec<String> = tree
                .entries
                .iter()
                .filter(|entry| !entry.is_tree)
                .map(|entry| entry.path.clone())
                .collect();
            all.sort();
            *self.whole_tree.lock().expect("whole tree") = Some(all.clone());
            return Ok(Listing {
                paths: all
                    .into_iter()
                    .filter(|path| matcher.is_match(path))
                    .collect(),
                complete: true,
            });
        }
        self.walk(&commit.sha, &literal_prefix(glob), &matcher)
    }

    fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, PlatformError> {
        let body = self.body(path)?;
        Ok(match lines {
            Some(range) => range.slice(&body),
            None => body,
        })
    }

    /// The index covers the default branch, and the change under review is on
    /// another one. So a hit is only a candidate path: the file is read again
    /// at `head_sha` and the match redone there, which is where the line
    /// numbers and the text in the answer come from.
    fn search(&self, query: &str, glob: Option<&str>) -> Result<Vec<SearchHit>, PlatformError> {
        let matcher = glob.map(|pattern| self.matcher(pattern)).transpose()?;
        let needle = query.to_lowercase();
        let mut hits = Vec::new();
        for path in self.candidates(query, matcher.as_ref())? {
            // A candidate that is gone at `head_sha` is not an error: the
            // index is simply describing a different commit.
            let Ok(body) = self.body(&path) else {
                continue;
            };
            for (index, line) in body.lines().enumerate() {
                if line.to_lowercase().contains(&needle) {
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

/// The leading path of a glob that holds no metacharacter. It is the boundary
/// of what the pattern can ever match, so the walk needs nothing outside it.
fn literal_prefix(glob: &str) -> String {
    let mut segments = Vec::new();
    for segment in glob.split('/') {
        if segment.contains(['*', '?', '[', '{', '}', ']']) {
            break;
        }
        segments.push(segment);
    }
    // The last literal segment may be the file name rather than a directory,
    // and a directory that does not exist simply yields nothing.
    if segments.len() == glob.split('/').count() && !segments.is_empty() {
        segments.pop();
    }
    segments.join("/")
}

/// Whether a directory is still on the way to `prefix` or already inside it.
fn reachable(prefix: &str, directory: &str) -> bool {
    let wanted: Vec<&str> = prefix.split('/').filter(|s| !s.is_empty()).collect();
    let walked: Vec<&str> = directory.split('/').filter(|s| !s.is_empty()).collect();
    let shared = wanted.len().min(walked.len());
    wanted[..shared] == walked[..shared]
}

#[derive(Debug, Deserialize)]
struct GithubPull {
    head: GithubSha,
    #[serde(default)]
    base: Option<GithubSha>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubCommit {
    commit: GithubCommitBody,
}

#[derive(Debug, Deserialize)]
struct GithubCommitBody {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct GithubSha {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GithubComment {
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubTree {
    #[serde(default)]
    tree: Vec<GithubTreeEntry>,
    /// GitHub's own word for "this response could not carry the whole tree".
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, Deserialize)]
struct GithubTreeEntry {
    #[serde(default)]
    path: String,
    #[serde(default)]
    sha: String,
    #[serde(default, rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct GithubSearch {
    #[serde(default)]
    items: Vec<GithubSearchItem>,
}

#[derive(Debug, Deserialize)]
struct GithubSearchItem {
    #[serde(default)]
    path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::DiffPaths;

    fn change() -> ChangeRef {
        ChangeRef {
            host: "github.com".to_string(),
            project: "acme/app".to_string(),
            number: 42,
        }
    }

    fn github(base_url: String) -> GitHub {
        GitHub::new(
            PlatformEntry {
                kind: Some(PlatformKind::Github),
                host: "github.com".to_string(),
                base_url,
                api_token: "GITHUB_TOKEN".to_string(),
            },
            Secret::from("test-github-token".to_string()),
            Backoff::new(2),
        )
    }

    fn pull_json() -> serde_json::Value {
        serde_json::json!({
            "head": { "sha": "headaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
            "base": { "sha": "basebbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" },
            "title": "bound the parser index",
            "body": "fixes the overflow reported in #12"
        })
    }

    fn commits_json() -> serde_json::Value {
        serde_json::json!([
            { "commit": { "message": "bound the index\n\nthe loop ran one past the end" } },
            { "commit": { "message": "add a test for it" } }
        ])
    }

    const UNIFIED: &str = "\
diff --git a/src/parse.c b/src/parse.c
--- a/src/parse.c
+++ b/src/parse.c
@@ -10,3 +10,4 @@ static int parse(void)
 int before(void);
-int gone(void);
+int added(void);
+int also_added(void);
 int after(void);
";

    fn refs() -> DiffRefs {
        DiffRefs {
            head_sha: "headaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            base_sha: Some("basebbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
            start_sha: None,
        }
    }

    fn inline(marker: &str) -> OutgoingComment {
        OutgoingComment {
            paths: DiffPaths::for_path("src/parse.c"),
            line: Some(11),
            end_line: None,
            body: format!("the index is a constant 5\n{marker}"),
            marker: marker.to_string(),
        }
    }

    async fn call<T, F>(f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        tokio::task::spawn_blocking(f).await.expect("join")
    }

    #[test]
    fn owner_and_repo_come_from_the_project_path() {
        let change = ChangeRef {
            host: "github.com".to_string(),
            project: "acme/app".to_string(),
            number: 42,
        };
        assert_eq!(GitHub::owner_and_repo(&change), Some(("acme", "app")));
    }

    #[test]
    fn a_github_html_url_is_fetched_from_the_api_base() {
        let parsed = ChangeRef::parse("https://github.com/acme/app/pull/42").unwrap();
        let api = github("https://api.github.com".to_string())
            .pull_url(&parsed, &[])
            .unwrap();
        assert_eq!(
            api.as_str(),
            "https://api.github.com/repos/acme/app/pulls/42"
        );
    }

    #[tokio::test]
    async fn fetch_change_uses_the_diff_accept_header() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42"))
            .and(wiremock::matchers::header("Accept", ACCEPT_JSON))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer test-github-token",
            ))
            .and(wiremock::matchers::header(
                "User-Agent",
                crate::platform::http::USER_AGENT,
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(pull_json()))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42"))
            .and(wiremock::matchers::header("Accept", ACCEPT_DIFF))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(UNIFIED))
            .expect(1)
            .mount(&server)
            .await;

        let fetched = call({
            let uri = server.uri();
            let change = change();
            move || github(uri).fetch_change(&change)
        })
        .await
        .expect("fetched");

        assert_eq!(fetched.head_sha, "headaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(
            fetched.base_sha.as_deref(),
            Some("basebbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(fetched.diff, UNIFIED);
        assert!(fetched.diff.contains("diff --git"));
        assert!(fetched.diff.contains("@@ "));
    }

    /// The title and body ride along on the pull object; the commit subjects
    /// are the one extra request. Subjects only: a commit body repeats the
    /// description often enough that carrying both pays twice.
    #[tokio::test]
    async fn fetch_change_brings_back_what_the_author_wrote() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42"))
            .and(wiremock::matchers::header("Accept", ACCEPT_JSON))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(pull_json()))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42"))
            .and(wiremock::matchers::header("Accept", ACCEPT_DIFF))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(UNIFIED))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42/commits"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(commits_json()))
            .expect(1)
            .mount(&server)
            .await;

        let fetched = call({
            let uri = server.uri();
            let change = change();
            move || github(uri).fetch_change(&change)
        })
        .await
        .expect("fetched");

        assert_eq!(
            fetched.narrative.title.as_deref(),
            Some("bound the parser index")
        );
        assert_eq!(
            fetched.narrative.description.as_deref(),
            Some("fixes the overflow reported in #12")
        );
        assert_eq!(
            fetched.narrative.commits,
            vec!["bound the index", "add a test for it"]
        );
        assert!(!fetched.narrative.more_commits);
    }

    /// A description that cannot be read costs the model context and nothing
    /// else. It must never be what fails a run that could still be reviewed.
    #[tokio::test]
    async fn commits_that_cannot_be_listed_leave_the_diff_reviewable() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42"))
            .and(wiremock::matchers::header("Accept", ACCEPT_JSON))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(pull_json()))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42"))
            .and(wiremock::matchers::header("Accept", ACCEPT_DIFF))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(UNIFIED))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42/commits"))
            .respond_with(wiremock::ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let fetched = call({
            let uri = server.uri();
            let change = change();
            move || github(uri).fetch_change(&change)
        })
        .await
        .expect("the diff still came back");

        assert_eq!(fetched.diff, UNIFIED);
        assert!(fetched.narrative.commits.is_empty());
        assert_eq!(
            fetched.narrative.title.as_deref(),
            Some("bound the parser index"),
            "the part that came free is still there"
        );
    }

    #[tokio::test]
    async fn a_review_is_one_comment_event_without_position() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42/reviews"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": 80,
                    "html_url": "https://github.com/acme/app/pull/42#pullrequestreview-80"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/repos/acme/app/issues/42/comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": 1,
                    "html_url": "https://github.com/acme/app/pull/42#issuecomment-1"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let marker = "<!-- reviewbot:run1:trace-1 -->";
        let summary = OutgoingComment {
            paths: DiffPaths::for_path(""),
            line: None,
            end_line: None,
            body: "overall 54 / 100\n<!-- reviewbot:run1:summary -->".to_string(),
            marker: "<!-- reviewbot:run1:summary -->".to_string(),
        };
        call({
            let uri = server.uri();
            let change = change();
            let refs = refs();
            let comment = inline(marker);
            move || github(uri).post_comments(&change, &refs, &[comment, summary])
        })
        .await
        .expect("posted");

        let received = server.received_requests().await.expect("received");
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].url.path(), "/repos/acme/app/pulls/42/reviews");
        let review: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json");
        assert_eq!(review["event"], "COMMENT");
        assert_eq!(
            review["commit_id"],
            "headaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(review["comments"][0]["side"], "RIGHT");
        assert_eq!(review["comments"][0]["line"], 11);
        assert_eq!(review["comments"][0]["path"], "src/parse.c");
        assert!(review["comments"][0].get("position").is_none());
        assert!(
            review["comments"][0]["body"]
                .as_str()
                .unwrap()
                .contains(marker)
        );
        assert_eq!(received[1].url.path(), "/repos/acme/app/issues/42/comments");
        let issue: serde_json::Value = serde_json::from_slice(&received[1].body).expect("json");
        assert!(
            issue["body"]
                .as_str()
                .unwrap()
                .contains("<!-- reviewbot:run1:summary -->")
        );
    }

    const HEAD: &str = "headaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn bound(uri: String) -> GitHub {
        let github = github(uri);
        github.bind_repo(&change(), HEAD);
        github
    }

    fn tree_path() -> String {
        format!("/repos/acme/app/git/trees/{HEAD}")
    }

    /// The recursive form is one request and covers nearly every repository, so
    /// after it every glob is a local filter.
    #[tokio::test]
    async fn a_whole_recursive_tree_is_fetched_once_and_later_globs_filter_it_locally() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(tree_path()))
            .and(wiremock::matchers::query_param("recursive", "1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "truncated": false,
                    "tree": [
                        {"path": "src", "type": "tree", "sha": "srctree"},
                        {"path": "src/parse.c", "type": "blob", "sha": "b1"},
                        {"path": "docs/readme.md", "type": "blob", "sha": "b2"},
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let (sources, docs) = call({
            let uri = server.uri();
            move || {
                let repo = bound(uri).repo_source();
                (
                    repo.list_files("src/**/*.c").expect("listed"),
                    repo.list_files("**/*.md").expect("listed"),
                )
            }
        })
        .await;

        assert_eq!(sources.paths, vec!["src/parse.c".to_string()]);
        assert!(sources.complete);
        assert_eq!(docs.paths, vec!["docs/readme.md".to_string()]);
        assert_eq!(
            server.received_requests().await.expect("received").len(),
            1,
            "one tree request answered both globs"
        );
    }

    /// `truncated: true` means this request shape could not carry the tree, not
    /// that the tree is unknowable. The fallback walks one directory level at a
    /// time, and only into directories the glob's literal prefix can reach:
    /// `src/**/*.c` never asks about `docs`.
    #[tokio::test]
    async fn a_truncated_tree_walks_only_the_directories_the_glob_can_reach() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(tree_path()))
            .and(wiremock::matchers::query_param("recursive", "1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "truncated": true,
                    "tree": [{"path": "src", "type": "tree", "sha": "srctree"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(tree_path()))
            .and(wiremock::matchers::query_param_is_missing("recursive"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "truncated": false,
                    "tree": [
                        {"path": "src", "type": "tree", "sha": "srctree"},
                        {"path": "docs", "type": "tree", "sha": "docstree"},
                        {"path": "README.md", "type": "blob", "sha": "b0"},
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/repos/acme/app/git/trees/srctree",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "truncated": false,
                    "tree": [
                        {"path": "parse.c", "type": "blob", "sha": "b1"},
                        {"path": "parse.h", "type": "blob", "sha": "b2"},
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/repos/acme/app/git/trees/docstree",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let listing = call({
            let uri = server.uri();
            move || bound(uri).repo_source().list_files("src/**/*.c")
        })
        .await
        .expect("listed");

        assert_eq!(listing.paths, vec!["src/parse.c".to_string()]);
        assert!(
            listing.complete,
            "the walk finished, so the list really is the whole answer"
        );
        assert_eq!(
            server.received_requests().await.expect("received").len(),
            3,
            "recursive, then the root level, then src"
        );
    }

    /// A walk that runs into its ceiling gives back what it has and says the
    /// list is not the whole answer.
    #[tokio::test]
    async fn a_walk_that_hits_its_ceiling_reports_the_listing_as_incomplete() {
        let server = wiremock::MockServer::start().await;
        let crowded: Vec<serde_json::Value> = (0..WALK_CEILING + 8)
            .map(|index| {
                serde_json::json!({
                    "path": format!("dir{index}"),
                    "type": "tree",
                    "sha": format!("subtree{index}"),
                })
            })
            .collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(tree_path()))
            .and(wiremock::matchers::query_param("recursive", "1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"truncated": true, "tree": []})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(tree_path()))
            .and(wiremock::matchers::query_param_is_missing("recursive"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"truncated": false, "tree": crowded})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(
                r"^/repos/acme/app/git/trees/subtree\d+$",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"truncated": false, "tree": []})),
            )
            .mount(&server)
            .await;

        let listing = call({
            let uri = server.uri();
            move || bound(uri).repo_source().list_files("**/*.c")
        })
        .await
        .expect("listed");

        assert!(
            !listing.complete,
            "the ceiling was reached, so the list is not the whole answer"
        );
    }

    /// The index covers the default branch and the review is of another one, so
    /// a hit is a candidate path and nothing more: the file is read again at
    /// `head_sha` and the match redone there. What comes back — path, line and
    /// text — is all from `head_sha`.
    #[tokio::test]
    async fn search_hits_are_candidates_that_get_rematched_at_head_sha() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/search/code"))
            .and(wiremock::matchers::query_param(
                "q",
                "parse_token repo:acme/app",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "total_count": 2,
                    "items": [
                        {"path": "src/parse.c", "line": 999, "text": "from the default branch"},
                        {"path": "src/gone.c"},
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/repos/acme/app/contents/src/parse.c",
            ))
            .and(wiremock::matchers::query_param("ref", HEAD))
            .and(wiremock::matchers::header("Accept", ACCEPT_RAW))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string("#include <x.h>\nint parse_token(void)\n{\n}\n"),
            )
            .expect(1)
            .mount(&server)
            .await;
        // Gone at `head_sha`: the index is describing a different commit, which
        // is not an error and does not sink the whole search.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/repos/acme/app/contents/src/gone.c",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_string(r#"{"message":"Not Found"}"#),
            )
            .mount(&server)
            .await;

        let hits = call({
            let uri = server.uri();
            move || bound(uri).repo_source().search("parse_token", None)
        })
        .await
        .expect("searched");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/parse.c");
        assert_eq!(hits[0].line, 2, "the line number comes from head_sha");
        assert_eq!(hits[0].text, "int parse_token(void)");
    }

    #[test]
    fn code_search_is_available_but_is_not_a_regular_expression_search() {
        let capabilities = github("https://api.github.com".to_string()).capabilities();
        assert!(capabilities.code_search);
        assert!(
            !capabilities.regex_search,
            "the index takes keywords, and search_repo's description has to say so"
        );
    }

    #[test]
    fn a_glob_reaches_no_further_up_the_tree_than_its_literal_prefix() {
        assert_eq!(literal_prefix("src/**/*.c"), "src");
        assert_eq!(literal_prefix("**/*.c"), "");
        assert_eq!(literal_prefix("src/lib/parse.c"), "src/lib");
        assert!(reachable("src", ""), "the root is on the way to src");
        assert!(reachable("src", "src/lib"), "inside the prefix");
        assert!(!reachable("src", "docs"));
        assert!(
            !reachable("src", "srcfoo"),
            "the comparison is by path segment, not by characters"
        );
    }

    #[tokio::test]
    async fn http_401_is_fatal_and_is_not_retried() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42"))
            .respond_with(
                wiremock::ResponseTemplate::new(401)
                    .set_body_string(r#"{"message":"Bad credentials"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let error = call({
            let uri = server.uri();
            let change = change();
            move || github(uri).head_sha(&change)
        })
        .await
        .expect_err("401");
        assert!(
            matches!(error, PlatformError::Permission { status: 401, .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("/repos/acme/app/pulls/42"),
            "{error}"
        );
        assert!(
            error.to_string().contains("Bad credentials"),
            "what the platform said has to survive into the message: {error}"
        );
        assert_eq!(server.received_requests().await.expect("received").len(), 1);
    }

    /// GitHub replies 403 to a REST call with no `User-Agent`, which reads
    /// exactly like a token without the right scope.
    #[tokio::test]
    async fn every_request_carries_a_user_agent() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/acme/app/pulls/42"))
            .and(wiremock::matchers::header_exists("User-Agent"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(pull_json()))
            .expect(1)
            .mount(&server)
            .await;

        let sha = call({
            let uri = server.uri();
            let change = change();
            move || github(uri).head_sha(&change)
        })
        .await
        .expect("head sha");

        assert_eq!(sha, "headaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let sent = server.received_requests().await.expect("received");
        assert_eq!(
            sent[0]
                .headers
                .get("User-Agent")
                .map(|value| value.to_str().expect("ascii")),
            Some(crate::platform::http::USER_AGENT)
        );
    }
}
