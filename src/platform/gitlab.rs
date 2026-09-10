//! GitLab: `PRIVATE-TOKEN` header, project id is the URL encoded full path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use globset::Glob;
use serde::Deserialize;
use serde_json::json;

use crate::common::{Backoff, Secret};
use crate::config::{PlatformEntry, PlatformKind};
use crate::domain::{DEV_NULL, Narrative};

use super::http::HttpClient;
use super::source::{LineRange, Listing, RepoSource, SearchHit};
use super::{
    Capabilities, ChangeRef, DiffRefs, ExistingComment, OutgoingComment, Platform, PlatformChange,
    PlatformError,
};

/// Keyset pages of the recursive tree. Big enough that an ordinary repository
/// is one or two round trips.
const TREE_PAGE: &str = "100";

pub struct GitLab {
    entry: PlatformEntry,
    token: Secret,
    http: Arc<HttpClient>,
    repo: Arc<GitLabRepo>,
}

impl GitLab {
    pub fn new(entry: PlatformEntry, token: Secret, backoff: Backoff) -> Self {
        let host = entry.host().unwrap_or("gitlab.com").to_string();
        let http = Arc::new(HttpClient::new(
            entry.base_url.clone(),
            host.clone(),
            token.expose(),
            backoff,
        ));
        let repo = Arc::new(GitLabRepo {
            host,
            http: Arc::clone(&http),
            token: token.clone(),
            commit: Mutex::new(None),
            tree: Mutex::new(None),
            files: Mutex::new(BTreeMap::new()),
        });
        Self {
            entry,
            token,
            http,
            repo,
        }
    }

    /// `group/sub/project` has to travel as one path segment.
    pub fn project_id(change: &ChangeRef) -> String {
        change.project.replace('/', "%2F")
    }

    fn auth(&self) -> Vec<(&str, String)> {
        vec![("PRIVATE-TOKEN", self.token.expose().to_string())]
    }

    /// `https://host/group/project/-/merge_requests/N` →
    /// `{base_url}/projects/group%2Fproject/merge_requests/N`.
    fn mr_url(&self, change: &ChangeRef, extra: &[&str]) -> Result<reqwest::Url, PlatformError> {
        let iid = change.number.to_string();
        let mut segments = vec!["projects", change.project.as_str(), "merge_requests", &iid];
        segments.extend_from_slice(extra);
        self.http.url(&segments)
    }

    fn apply_auth(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        request.header("PRIVATE-TOKEN", self.token.expose())
    }

    fn versions(&self, change: &ChangeRef) -> Result<DiffRefs, PlatformError> {
        let mut url = self.mr_url(change, &["versions"])?;
        url.query_pairs_mut().append_pair("per_page", "1");
        let headers = self.auth();
        let pages = self
            .http
            .get_pages("reading merge request versions", url, &headers)?;
        let latest = pages
            .into_iter()
            .next()
            .ok_or_else(|| PlatformError::Request {
                operation: "reading merge request versions",
                host: self.http.host().to_string(),
                reason: "the merge request has no diff versions".to_string(),
            })?;
        let version: GitlabVersion =
            serde_json::from_value(latest).map_err(|error| PlatformError::Request {
                operation: "reading merge request versions",
                host: self.http.host().to_string(),
                reason: error.to_string(),
            })?;
        Ok(DiffRefs {
            head_sha: version.head_commit_sha,
            base_sha: Some(version.base_commit_sha),
            start_sha: Some(version.start_commit_sha),
        })
    }

    fn diffs(&self, change: &ChangeRef) -> Result<String, PlatformError> {
        let mut url = self.mr_url(change, &["diffs"])?;
        url.query_pairs_mut().append_pair("per_page", "100");
        let headers = self.auth();
        let pages = self
            .http
            .get_pages("fetching the merge request diff", url, &headers)?;
        let mut files = Vec::new();
        for row in pages {
            let file: GitlabDiffFile =
                serde_json::from_value(row).map_err(|error| PlatformError::Request {
                    operation: "fetching the merge request diff",
                    host: self.http.host().to_string(),
                    reason: error.to_string(),
                })?;
            files.push(file);
        }
        Ok(unified_from_diffs(&files))
    }

    /// Title, description and commit subjects: two requests that the diff
    /// path does not otherwise make. Either one failing costs the model some
    /// context and nothing else, so neither one fails the run.
    fn narrative(&self, change: &ChangeRef) -> Narrative {
        let meta = match self.meta(change) {
            Ok(meta) => meta,
            Err(error) => {
                tracing::warn!("no merge request description for the review: {error}");
                GitlabMergeRequest::default()
            }
        };
        let messages = match self.commits(change) {
            Ok(messages) => messages,
            Err(error) => {
                tracing::warn!("no commit messages for the review: {error}");
                Vec::new()
            }
        };
        Narrative::new(meta.title, meta.description, messages)
    }

    fn meta(&self, change: &ChangeRef) -> Result<GitlabMergeRequest, PlatformError> {
        let url = self.mr_url(change, &[])?;
        let response = self.http.send("reading the merge request", || {
            self.apply_auth(self.http.get(url.clone()))
        })?;
        self.http.json("reading the merge request", &response.body)
    }

    /// One page, `Narrative::COMMITS + 1` deep, for the same reason the
    /// GitHub side asks for one more than it keeps: a branch longer than the
    /// cap has to be known to be longer.
    fn commits(&self, change: &ChangeRef) -> Result<Vec<String>, PlatformError> {
        let mut url = self.mr_url(change, &["commits"])?;
        url.query_pairs_mut()
            .append_pair("per_page", &(Narrative::COMMITS + 1).to_string());
        let response = self.http.send("listing the merge request commits", || {
            self.apply_auth(self.http.get(url.clone()))
        })?;
        let rows: Vec<GitlabCommit> = self
            .http
            .json("listing the merge request commits", &response.body)?;
        Ok(rows.into_iter().map(|row| row.message).collect())
    }

    fn post_one(
        &self,
        change: &ChangeRef,
        refs: &DiffRefs,
        comment: &OutgoingComment,
    ) -> Result<ExistingComment, PlatformError> {
        if comment.line.is_none() || comment.is_summary() {
            return self.post_note(change, comment, false);
        }
        match self.post_discussion(change, refs, comment) {
            Ok(posted) => Ok(posted),
            Err(PlatformError::Unprocessable { .. }) => {
                tracing::warn!(
                    path = comment.paths.display(),
                    "line is not commentable; posting as a file-level note"
                );
                self.post_note(change, comment, true)
            }
            Err(error) => Err(error),
        }
    }

    fn post_discussion(
        &self,
        change: &ChangeRef,
        refs: &DiffRefs,
        comment: &OutgoingComment,
    ) -> Result<ExistingComment, PlatformError> {
        let (Some(base_sha), Some(start_sha)) =
            (refs.base_sha.as_deref(), refs.start_sha.as_deref())
        else {
            return self.post_note(change, comment, true);
        };
        let url = self.mr_url(change, &["discussions"])?;
        let body = json!({
            "body": comment.body,
            "position": {
                "position_type": "text",
                "base_sha": base_sha,
                "start_sha": start_sha,
                "head_sha": refs.head_sha,
                "new_path": comment.paths.new_path,
                "old_path": comment.paths.old_path,
                "new_line": comment.line,
            }
        });
        let response = self.http.send("posting a discussion", || {
            self.apply_auth(self.http.post(url.clone()))
                .header("Content-Type", "application/json")
                .json(&body)
        })?;
        self.posted_from(&response.body, &comment.marker, false)
    }

    fn post_note(
        &self,
        change: &ChangeRef,
        comment: &OutgoingComment,
        degraded: bool,
    ) -> Result<ExistingComment, PlatformError> {
        let url = self.mr_url(change, &["notes"])?;
        let body = json!({ "body": comment.body });
        let response = self.http.send("posting a note", || {
            self.apply_auth(self.http.post(url.clone()))
                .header("Content-Type", "application/json")
                .json(&body)
        })?;
        self.posted_from(&response.body, &comment.marker, degraded)
    }

    fn posted_from(
        &self,
        body: &str,
        marker: &str,
        degraded: bool,
    ) -> Result<ExistingComment, PlatformError> {
        let value: serde_json::Value = self.http.json("reading a post response", body)?;
        let url = value
            .pointer("/notes/0/web_url")
            .or_else(|| value.get("web_url"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(ExistingComment {
            marker: marker.to_string(),
            url,
            degraded_to_file: degraded,
        })
    }
}

impl Platform for GitLab {
    fn kind(&self) -> PlatformKind {
        PlatformKind::Gitlab
    }

    fn host(&self) -> &str {
        self.entry.host().unwrap_or("gitlab.com")
    }

    /// Blob search exists only with Advanced Search or Exact Code Search, and
    /// neither the host nor the `base_url` says which. Guessing would hand the
    /// model a search that silently returns nothing, so it stays off.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            code_search: false,
            regex_search: false,
        }
    }

    fn head_sha(&self, change: &ChangeRef) -> Result<String, PlatformError> {
        let url = self.mr_url(change, &[])?;
        let response = self
            .http
            .send("reading the merge request head commit", || {
                self.apply_auth(self.http.get(url.clone()))
            })?;
        let meta: GitlabMergeRequest = self
            .http
            .json("reading the merge request head commit", &response.body)?;
        if let Some(sha) = meta
            .sha
            .filter(|sha| !sha.is_empty())
            .or_else(|| meta.diff_refs.and_then(|refs| refs.head_sha))
        {
            return Ok(sha);
        }
        Ok(self.versions(change)?.head_sha)
    }

    fn fetch_change(&self, change: &ChangeRef) -> Result<PlatformChange, PlatformError> {
        let refs = self.versions(change)?;
        let diff = self.diffs(change)?;
        Ok(PlatformChange {
            diff,
            head_sha: refs.head_sha,
            base_sha: refs.base_sha,
            start_sha: refs.start_sha,
            narrative: self.narrative(change),
        })
    }

    fn existing_comments(&self, change: &ChangeRef) -> Result<Vec<ExistingComment>, PlatformError> {
        let mut found = Vec::new();
        let mut discussions = self.mr_url(change, &["discussions"])?;
        discussions.query_pairs_mut().append_pair("per_page", "100");
        for row in self
            .http
            .get_pages("listing discussions", discussions, &self.auth())?
        {
            let discussion: GitlabDiscussion =
                serde_json::from_value(row).map_err(|error| PlatformError::Request {
                    operation: "listing discussions",
                    host: self.http.host().to_string(),
                    reason: error.to_string(),
                })?;
            for note in discussion.notes {
                found.extend(ExistingComment::from_body(&note.body, note.web_url));
            }
        }

        let mut notes = self.mr_url(change, &["notes"])?;
        notes.query_pairs_mut().append_pair("per_page", "100");
        for row in self.http.get_pages("listing notes", notes, &self.auth())? {
            let note: GitlabNote =
                serde_json::from_value(row).map_err(|error| PlatformError::Request {
                    operation: "listing notes",
                    host: self.http.host().to_string(),
                    reason: error.to_string(),
                })?;
            found.extend(ExistingComment::from_body(&note.body, note.web_url));
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
        let mut posted = Vec::new();
        for comment in comments {
            posted.push(self.post_one(change, refs, comment)?);
        }
        Ok(posted)
    }

    fn bind_repo(&self, change: &ChangeRef, head_sha: &str) {
        self.repo.bind(&change.project, head_sha);
    }

    fn repo_source(&self) -> Arc<dyn RepoSource> {
        Arc::clone(&self.repo) as Arc<dyn RepoSource>
    }
}

/// The project and commit every read goes against. There is nothing to read
/// before the head commit is known, which is why this starts out empty.
#[derive(Clone, Debug)]
struct Commit {
    project: String,
    sha: String,
}

/// Repository reads for one project at one commit. The whole tree is fetched
/// once and each file body once, because the sha cannot change inside a run:
/// otherwise every glob the model tries is another round trip.
struct GitLabRepo {
    host: String,
    http: Arc<HttpClient>,
    token: Secret,
    commit: Mutex<Option<Commit>>,
    tree: Mutex<Option<Vec<String>>>,
    files: Mutex<BTreeMap<String, String>>,
}

impl GitLabRepo {
    fn bind(&self, project: &str, sha: &str) {
        *self.commit.lock().expect("commit") = Some(Commit {
            project: project.to_string(),
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

    fn auth(&self) -> Vec<(&str, String)> {
        vec![("PRIVATE-TOKEN", self.token.expose().to_string())]
    }

    /// Every blob path at this commit, keyset paged to the end. `?page=N` is
    /// gone since GitLab 15.0, and the `Link: rel=next` header carries the
    /// `page_token` for the next page.
    fn tree(&self) -> Result<Vec<String>, PlatformError> {
        if let Some(cached) = self.tree.lock().expect("tree").clone() {
            return Ok(cached);
        }
        let commit = self.commit()?;
        let mut url = self
            .http
            .url(&["projects", &commit.project, "repository", "tree"])?;
        url.query_pairs_mut()
            .append_pair("ref", &commit.sha)
            .append_pair("recursive", "true")
            .append_pair("pagination", "keyset")
            .append_pair("per_page", TREE_PAGE);
        let rows = self
            .http
            .get_pages("listing the repository tree", url, &self.auth())?;
        let mut paths: Vec<String> = rows
            .iter()
            .filter(|row| row.get("type").and_then(|kind| kind.as_str()) == Some("blob"))
            .filter_map(|row| row.get("path").and_then(|path| path.as_str()))
            .map(str::to_string)
            .collect();
        paths.sort();
        paths.dedup();
        *self.tree.lock().expect("tree") = Some(paths.clone());
        Ok(paths)
    }

    fn body(&self, path: &str) -> Result<String, PlatformError> {
        if let Some(cached) = self.files.lock().expect("files").get(path) {
            return Ok(cached.clone());
        }
        let commit = self.commit()?;
        let mut url = self.http.url(&[
            "projects",
            &commit.project,
            "repository",
            "files",
            path,
            "raw",
        ])?;
        url.query_pairs_mut().append_pair("ref", &commit.sha);
        let response = self.http.send("reading a repository file", || {
            self.http
                .get(url.clone())
                .header("PRIVATE-TOKEN", self.token.expose())
        })?;
        self.files
            .lock()
            .expect("files")
            .insert(path.to_string(), response.body.clone());
        Ok(response.body)
    }
}

impl RepoSource for GitLabRepo {
    /// The tree is already in hand after the first call, so a second glob is
    /// filtered locally rather than fetched again.
    fn list_files(&self, glob: &str) -> Result<Listing, PlatformError> {
        let matcher = Glob::new(glob)
            .map_err(|error| PlatformError::Request {
                operation: "listing the repository tree",
                host: self.host.clone(),
                reason: format!("invalid glob {glob:?}: {error}"),
            })?
            .compile_matcher();
        let paths = self
            .tree()?
            .into_iter()
            .filter(|path| matcher.is_match(path))
            .collect();
        Ok(Listing {
            paths,
            complete: true,
        })
    }

    fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, PlatformError> {
        let body = self.body(path)?;
        Ok(match lines {
            Some(range) => range.slice(&body),
            None => body,
        })
    }

    /// Blob search exists only with Advanced Search or Exact Code Search, and
    /// nothing in the config says whether this instance has it. `capabilities`
    /// therefore reports no code search, so `search_code` refuses before
    /// reaching the worktree and this is never called.
    fn search(&self, _query: &str, _glob: Option<&str>) -> Result<Vec<SearchHit>, PlatformError> {
        Err(PlatformError::Unsupported {
            host: self.host.clone(),
            capability: "code search",
        })
    }
}

#[derive(Debug, Deserialize)]
struct GitlabVersion {
    head_commit_sha: String,
    base_commit_sha: String,
    start_commit_sha: String,
}

#[derive(Debug, Default, Deserialize)]
struct GitlabMergeRequest {
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    diff_refs: Option<GitlabDiffRefs>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitlabCommit {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct GitlabDiffRefs {
    #[serde(default)]
    head_sha: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitlabDiffFile {
    #[serde(default)]
    old_path: String,
    #[serde(default)]
    new_path: String,
    #[serde(default)]
    diff: String,
    #[serde(default)]
    new_file: bool,
    #[serde(default)]
    deleted_file: bool,
}

#[derive(Debug, Deserialize)]
struct GitlabDiscussion {
    #[serde(default)]
    notes: Vec<GitlabNote>,
}

#[derive(Debug, Deserialize)]
struct GitlabNote {
    #[serde(default)]
    body: String,
    #[serde(default)]
    web_url: Option<String>,
}

/// Turn GitLab's per-file JSON hunks into one unified diff the M2 parser
/// already accepts. The `diff` field is usually just `@@` hunks.
fn unified_from_diffs(files: &[GitlabDiffFile]) -> String {
    let mut out = String::new();
    for file in files {
        let text = file.to_unified();
        if text.is_empty() {
            continue;
        }
        out.push_str(&text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

impl GitlabDiffFile {
    fn git_names(&self) -> (&str, &str) {
        let old = if self.old_path.is_empty() {
            self.new_path.as_str()
        } else {
            self.old_path.as_str()
        };
        let new = if self.new_path.is_empty() {
            self.old_path.as_str()
        } else {
            self.new_path.as_str()
        };
        (old, new)
    }

    fn to_unified(&self) -> String {
        let diff = self.diff.trim_start_matches('\u{feff}');
        if diff.is_empty() && !self.new_file && !self.deleted_file {
            return String::new();
        }
        if diff.starts_with("diff --git ") {
            return self.diff.clone();
        }
        let (git_old, git_new) = self.git_names();
        if git_old.is_empty() && git_new.is_empty() {
            return String::new();
        }
        let mut out = format!("diff --git a/{git_old} b/{git_new}\n");
        if diff.starts_with("--- ") {
            out.push_str(diff);
            return out;
        }
        let minus = if self.new_file {
            DEV_NULL.to_string()
        } else {
            format!("a/{git_old}")
        };
        let plus = if self.deleted_file {
            DEV_NULL.to_string()
        } else {
            format!("b/{git_new}")
        };
        out.push_str(&format!("--- {minus}\n+++ {plus}\n"));
        if !diff.is_empty() {
            out.push_str(diff);
            if !diff.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage::input::UnifiedDiff;

    fn change() -> ChangeRef {
        ChangeRef {
            host: "gitlab.com".to_string(),
            project: "acme/app".to_string(),
            number: 128,
        }
    }

    fn gitlab(base_url: String) -> GitLab {
        GitLab::new(
            PlatformEntry {
                base_url,
                api_token: "GITLAB_TOKEN".to_string(),
            },
            Secret::from("test-gitlab-token".to_string()),
            Backoff::new(2),
        )
    }

    fn sample_hunk() -> &'static str {
        "@@ -10,3 +10,4 @@ static int parse(void)\n int before(void);\n-int gone(void);\n+int added(void);\n+int also_added(void);\n int after(void);\n"
    }

    fn diffs_json() -> serde_json::Value {
        serde_json::json!([{
            "old_path": "src/parse.c",
            "new_path": "src/parse.c",
            "new_file": false,
            "deleted_file": false,
            "diff": sample_hunk(),
        }])
    }

    fn mr_json() -> serde_json::Value {
        serde_json::json!({
            "sha": "head222222222222222222222222222222222222",
            "title": "bound the parser index",
            "description": "fixes the overflow reported in #12",
        })
    }

    fn commits_json() -> serde_json::Value {
        serde_json::json!([
            { "message": "bound the index\n\nthe loop ran one past the end" },
            { "message": "add a test for it" },
        ])
    }

    fn versions_json() -> serde_json::Value {
        serde_json::json!([{
            "id": 9,
            "base_commit_sha": "base111111111111111111111111111111111111",
            "head_commit_sha": "head222222222222222222222222222222222222",
            "start_commit_sha": "start33333333333333333333333333333333333",
        }])
    }

    fn refs() -> DiffRefs {
        DiffRefs {
            head_sha: "head222222222222222222222222222222222222".to_string(),
            base_sha: Some("base111111111111111111111111111111111111".to_string()),
            start_sha: Some("start33333333333333333333333333333333333".to_string()),
        }
    }

    fn inline(marker: &str) -> OutgoingComment {
        OutgoingComment {
            paths: super::super::DiffPaths {
                old_path: "src/parse.c".to_string(),
                new_path: "src/parse.c".to_string(),
            },
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
    fn nested_project_paths_are_url_encoded() {
        let change = ChangeRef {
            host: "gitlab.com".to_string(),
            project: "group/sub/project".to_string(),
            number: 1,
        };
        assert_eq!(GitLab::project_id(&change), "group%2Fsub%2Fproject");
    }

    #[test]
    fn a_gitlab_html_url_is_fetched_from_the_api_base() {
        let parsed = ChangeRef::parse("https://gitlab.com/acme/app/-/merge_requests/128").unwrap();
        let api = gitlab("https://gitlab.com/api/v4".to_string())
            .mr_url(&parsed, &[])
            .unwrap();
        assert_eq!(
            api.as_str(),
            "https://gitlab.com/api/v4/projects/acme%2Fapp/merge_requests/128"
        );
    }

    #[test]
    fn json_hunks_become_a_parseable_unified_diff() {
        let files = [GitlabDiffFile {
            old_path: "src/parse.c".to_string(),
            new_path: "src/parse.c".to_string(),
            diff: sample_hunk().to_string(),
            new_file: false,
            deleted_file: false,
        }];
        let text = unified_from_diffs(&files);
        assert!(
            text.contains("diff --git a/src/parse.c b/src/parse.c"),
            "{text}"
        );
        assert!(text.contains("--- a/src/parse.c"), "{text}");
        assert!(text.contains("+++ b/src/parse.c"), "{text}");
        assert!(text.contains("@@ -10,3 +10,4 @@"), "{text}");
        let parsed = UnifiedDiff::parse(&text).expect("the M2 parser accepts it");
        assert_eq!(parsed.files()[0].new_path, "src/parse.c");
        assert!(parsed.files()[0].changed_lines.contains(&11));
    }

    /// GitLab's diff path does not otherwise touch the merge request itself,
    /// so both the description and the subjects are extra requests here.
    #[tokio::test]
    async fn fetch_change_brings_back_what_the_author_wrote() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/versions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(versions_json()))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/diffs",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(diffs_json()))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(mr_json()))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/commits",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(commits_json()))
            .expect(1)
            .mount(&server)
            .await;

        let fetched = call({
            let uri = server.uri();
            let change = change();
            move || gitlab(uri).fetch_change(&change)
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
    }

    #[tokio::test]
    async fn fetch_change_reads_versions_and_json_diffs() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/versions",
            ))
            .and(wiremock::matchers::header(
                "PRIVATE-TOKEN",
                "test-gitlab-token",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(versions_json()))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/diffs",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(diffs_json()))
            .expect(1)
            .mount(&server)
            .await;

        let fetched = call({
            let uri = server.uri();
            let change = change();
            move || gitlab(uri).fetch_change(&change)
        })
        .await
        .expect("fetched");

        assert_eq!(fetched.head_sha, "head222222222222222222222222222222222222");
        assert_eq!(
            fetched.base_sha.as_deref(),
            Some("base111111111111111111111111111111111111")
        );
        assert_eq!(
            fetched.start_sha.as_deref(),
            Some("start33333333333333333333333333333333333")
        );
        assert!(
            fetched.diff.contains("--- a/src/parse.c"),
            "{}",
            fetched.diff
        );
        assert!(
            fetched.diff.contains("@@ -10,3 +10,4 @@"),
            "{}",
            fetched.diff
        );
        UnifiedDiff::parse(&fetched.diff).expect("parseable");
    }

    #[tokio::test]
    async fn an_inline_discussion_sends_new_line_both_paths_and_shas() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/discussions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "d1",
                "notes": [{"id": 1, "body": "ok", "web_url": "https://gitlab.com/acme/app/-/merge_requests/128#note_1"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let marker = "<!-- reviewbot:run1:trace-1 -->";
        let posted = call({
            let uri = server.uri();
            let change = change();
            let refs = refs();
            let comment = inline(marker);
            move || gitlab(uri).post_comments(&change, &refs, &[comment])
        })
        .await
        .expect("posted");
        assert_eq!(posted[0].marker, marker);
        assert!(!posted[0].degraded_to_file);

        let received = server.received_requests().await.expect("received");
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json");
        assert_eq!(body["position"]["position_type"], "text");
        assert_eq!(body["position"]["new_line"], 11);
        assert!(body["position"].get("old_line").is_none());
        assert_eq!(body["position"]["new_path"], "src/parse.c");
        assert_eq!(body["position"]["old_path"], "src/parse.c");
        assert_eq!(
            body["position"]["head_sha"],
            "head222222222222222222222222222222222222"
        );
        assert_eq!(
            body["position"]["base_sha"],
            "base111111111111111111111111111111111111"
        );
        assert_eq!(
            body["position"]["start_sha"],
            "start33333333333333333333333333333333333"
        );
        assert!(
            body["body"].as_str().unwrap().contains(marker),
            "{}",
            body["body"]
        );
    }

    #[tokio::test]
    async fn a_file_level_comment_uses_the_notes_endpoint() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/notes",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": 9,
                    "body": "file",
                    "web_url": "https://gitlab.com/note"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let comment = OutgoingComment {
            paths: super::super::DiffPaths::for_path("src/parse.c"),
            line: None,
            end_line: None,
            body: "file level <!-- reviewbot:run1:trace-2 -->".to_string(),
            marker: "<!-- reviewbot:run1:trace-2 -->".to_string(),
        };
        call({
            let uri = server.uri();
            let change = change();
            let refs = refs();
            move || gitlab(uri).post_comments(&change, &refs, &[comment])
        })
        .await
        .expect("posted");

        let received = server.received_requests().await.expect("received");
        assert_eq!(
            received[0].url.path(),
            "/projects/acme%2Fapp/merge_requests/128/notes"
        );
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json");
        assert!(body.get("position").is_none());
    }

    #[tokio::test]
    async fn http_422_retries_once_as_a_note() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/discussions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(422)
                    .set_body_string(r#"{"message":"line is not commentable"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/notes",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": 3,
                    "web_url": "https://gitlab.com/note"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let posted = call({
            let uri = server.uri();
            let change = change();
            let refs = refs();
            let comment = inline("<!-- reviewbot:run1:trace-1 -->");
            move || gitlab(uri).post_comments(&change, &refs, &[comment])
        })
        .await
        .expect("degraded");
        assert!(posted[0].degraded_to_file);
        assert_eq!(server.received_requests().await.expect("received").len(), 2);
    }

    #[tokio::test]
    async fn an_existing_marker_is_read_from_discussions() {
        let server = wiremock::MockServer::start().await;
        let marker = "<!-- reviewbot:run1:trace-1 -->";
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/discussions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "id": "d1",
                    "notes": [{"id": 1, "body": format!("already said\n{marker}")}]
                }])),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/notes",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let existing = call({
            let uri = server.uri();
            let change = change();
            move || gitlab(uri).existing_comments(&change)
        })
        .await
        .expect("listed");
        assert_eq!(existing[0].marker, marker);
    }

    #[tokio::test]
    async fn http_401_is_fatal_and_is_not_retried() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/versions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(401).set_body_string(r#"{"message":"401"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let error = call({
            let uri = server.uri();
            let change = change();
            move || gitlab(uri).fetch_change(&change)
        })
        .await
        .expect_err("401");
        assert!(
            matches!(error, PlatformError::Permission { status: 401, .. }),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("/projects/acme%2Fapp/merge_requests/128/versions"),
            "{error}"
        );
        assert_eq!(server.received_requests().await.expect("received").len(), 1);
    }

    /// `?page=N` is gone from the tree endpoint since GitLab 15.0, so the
    /// pages are followed by the cursor the `Link` header carries. The whole
    /// tree is fetched once per run: another glob is another local filter, not
    /// another round trip.
    #[tokio::test]
    async fn the_repository_tree_is_keyset_paged_and_fetched_once_per_run() {
        let server = wiremock::MockServer::start().await;
        let second_page = format!(
            "{}/projects/acme%2Fapp/repository/tree?pagination=keyset&page_token=src",
            server.uri()
        );
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/repository/tree",
            ))
            .and(wiremock::matchers::query_param_is_missing("page_token"))
            .and(wiremock::matchers::query_param("pagination", "keyset"))
            .and(wiremock::matchers::query_param("recursive", "true"))
            .and(wiremock::matchers::query_param(
                "ref",
                "head222222222222222222222222222222222222",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("Link", format!("<{second_page}>; rel=\"next\"").as_str())
                    .set_body_json(serde_json::json!([
                        {"type": "tree", "name": "src", "path": "src"},
                        {"type": "blob", "name": "parse.c", "path": "src/parse.c"},
                    ])),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/repository/tree",
            ))
            .and(wiremock::matchers::query_param("page_token", "src"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"type": "blob", "name": "readme.md", "path": "docs/readme.md"},
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let (sources, docs) = call({
            let uri = server.uri();
            let change = change();
            move || {
                let gitlab = gitlab(uri);
                gitlab.bind_repo(&change, "head222222222222222222222222222222222222");
                let repo = gitlab.repo_source();
                (
                    repo.list_files("src/**/*.c").expect("listed"),
                    repo.list_files("**/*.md").expect("listed"),
                )
            }
        })
        .await;

        assert_eq!(sources.paths, vec!["src/parse.c".to_string()]);
        assert!(
            sources.complete,
            "keyset paging reaches the end of the tree"
        );
        assert_eq!(docs.paths, vec!["docs/readme.md".to_string()]);
        assert_eq!(
            server.received_requests().await.expect("received").len(),
            2,
            "two pages of one tree; the second glob was filtered locally"
        );
    }

    /// Always by the bound sha, and the same `(path, sha)` only once: a second
    /// read of the same file is answered out of the cache.
    #[tokio::test]
    async fn a_file_is_read_at_the_bound_sha_and_only_once() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/repository/files/src%2Fparse.c/raw",
            ))
            .and(wiremock::matchers::query_param(
                "ref",
                "head222222222222222222222222222222222222",
            ))
            .and(wiremock::matchers::header(
                "PRIVATE-TOKEN",
                "test-gitlab-token",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("one\ntwo\nthree\n"))
            .expect(1)
            .mount(&server)
            .await;

        let (whole, ranged) = call({
            let uri = server.uri();
            let change = change();
            move || {
                let gitlab = gitlab(uri);
                gitlab.bind_repo(&change, "head222222222222222222222222222222222222");
                let repo = gitlab.repo_source();
                (
                    repo.read_file("src/parse.c", None).expect("read"),
                    repo.read_file("src/parse.c", Some(LineRange { first: 2, last: 3 }))
                        .expect("read"),
                )
            }
        })
        .await;

        assert_eq!(whole, "one\ntwo\nthree\n");
        assert_eq!(ranged, "two\nthree");
        assert_eq!(server.received_requests().await.expect("received").len(), 1);
    }

    /// Blob search needs Advanced Search or Exact Code Search and nothing here
    /// can tell whether this instance has it. So the capability is off, which
    /// is what keeps `search_repo` out of the list the model is shown.
    #[test]
    fn code_search_is_not_claimed_without_knowing_the_instance_has_it() {
        let gitlab = gitlab("https://gitlab.com/api/v4".to_string());
        assert!(!gitlab.capabilities().code_search);
        assert!(matches!(
            gitlab.repo_source().search("token", None),
            Err(PlatformError::Unsupported { .. })
        ));
    }

    #[tokio::test]
    async fn posting_uses_the_entry_base_url_not_a_guessed_host() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/projects/acme%2Fapp/merge_requests/128/notes",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": 1
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let comment = OutgoingComment {
            paths: super::super::DiffPaths::for_path("src/parse.c"),
            line: None,
            end_line: None,
            body: "note".to_string(),
            marker: "<!-- reviewbot:run1:summary -->".to_string(),
        };
        call({
            let uri = server.uri();
            let change = ChangeRef {
                host: "git.example.com".to_string(),
                project: "acme/app".to_string(),
                number: 128,
            };
            let refs = refs();
            move || gitlab(uri).post_comments(&change, &refs, &[comment])
        })
        .await
        .expect("posted");

        let received = server.received_requests().await.expect("received");
        assert_eq!(received.len(), 1, "the entry's base_url received the post");
        assert_ne!(
            received[0].url.host_str(),
            Some("gitlab.com"),
            "posts go to the entry's host, not gitlab.com"
        );
    }
}
