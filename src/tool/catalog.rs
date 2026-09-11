use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use crate::config::Settings;
use crate::platform::{
    Capabilities, LineRange, Listing, PlatformError, Repo, RepoSource, SearchHit, SearchKind,
};
use crate::security::{EnvPolicy, PathPolicy};
use crate::worktree::Worktree;

use super::command::{CommandContext, CommandTool};
use super::content::{
    FetchRepoFile, ListLocalFiles, ListRepoFiles, ReadLocalFile, SearchLocalRegex,
    SearchRepoKeyword, SearchRepoRegex, SuggestLocalRead, ToolLimits,
};
use super::registry::Registry;
use super::submit::{FinishReview, SubmitComment, SubmitSummary};
use super::types::{Purpose, Round};

impl Registry {
    /// Every tool this config offers, in one registry, with no branch in sight.
    pub fn assemble(
        settings: &Settings,
        paths: PathPolicy,
        worktree: Arc<Worktree>,
        run_dir: &Path,
    ) -> Self {
        let mut registry = Self::new();
        // The three ways of handing something over: a finding, "I have none", and
        // the verdict. None of them touches the worktree, and `Round` keeps each
        // off the turns it does not belong to. "I have none" has to be as
        // available as filing something, or a model with nothing to file will file
        // something.
        registry.register(Box::new(SubmitComment::new()));
        registry.register(Box::new(FinishReview::new()));
        registry.register(Box::new(SubmitSummary::new()));

        let limits = ToolLimits::from_config(settings.config());
        registry.register(Box::new(ListLocalFiles::new(
            Arc::clone(&worktree),
            paths.clone(),
            limits,
        )));
        registry.register(Box::new(SuggestLocalRead::new(
            Arc::clone(&worktree),
            paths.clone(),
            limits,
        )));
        registry.register(Box::new(ReadLocalFile::new(
            Arc::clone(&worktree),
            paths.clone(),
            limits,
        )));
        registry.register(Box::new(SearchLocalRegex::new(
            Arc::clone(&worktree),
            paths.clone(),
            limits,
        )));
        registry.register(Box::new(ListRepoFiles::new(
            Arc::clone(&worktree),
            paths.clone(),
            limits,
        )));
        registry.register(Box::new(FetchRepoFile::new(
            Arc::clone(&worktree),
            paths.clone(),
            limits,
        )));
        registry.register(Box::new(SearchRepoRegex::new(
            Arc::clone(&worktree),
            paths.clone(),
            limits,
        )));
        registry.register(Box::new(SearchRepoKeyword::new(
            Arc::clone(&worktree),
            paths.clone(),
            limits,
        )));

        for entry in &settings.config().tools {
            registry.register(Box::new(CommandTool::new(
                entry.clone(),
                CommandContext::new(
                    EnvPolicy::default(),
                    paths.clone(),
                    Arc::clone(&worktree),
                    run_dir.to_path_buf(),
                    settings.options().backoff(),
                ),
                limits,
            )));
        }
        registry
    }

    /// The catalog, read off the real tools rather than described a second time.
    pub fn catalog(settings: &Settings) -> Result<Vec<ToolListing>, crate::config::ConfigError> {
        let worktree = Worktree::Local {
            root: PathBuf::from("/catalog"),
            repo: Some(Repo::new(
                Arc::new(CatalogSource) as Arc<dyn RepoSource>,
                Capabilities::all(),
            )),
        };
        let registry = Self::assemble(
            settings,
            PathPolicy::for_settings(settings)?,
            Arc::new(worktree),
            Path::new("/catalog"),
        );
        Ok(registry
            .all()
            .into_iter()
            .map(|tool| ToolListing {
                name: tool.name().to_string(),
                purpose: tool.purpose(),
                description: tool.description().to_string(),
                parameters: tool.signature().schema(),
                rounds: tool.rounds().to_vec(),
                preconditions: tool
                    .precondition()
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            })
            .collect())
    }
}

/// One row of `tool list`: the contract the model would be given.
#[derive(Clone, Debug, Serialize)]
pub struct ToolListing {
    pub name: String,
    pub purpose: Purpose,
    pub description: String,
    pub parameters: serde_json::Value,
    pub rounds: Vec<Round>,
    /// What a run's worktree has to be able to do before a call to the tool
    /// can be answered. Empty when nothing does. Never a reason for the tool
    /// to be missing: every tool is offered on every run.
    pub preconditions: Vec<String>,
}

/// A `RepoSource` with no run behind it. `tool list` still has to go through
/// `build`, and a `Repo` needs a source, so every method says there is
/// nothing to read rather than inventing an answer.
struct CatalogSource;

impl CatalogSource {
    fn no_run(operation: &'static str) -> PlatformError {
        PlatformError::Request {
            operation,
            host: "catalog".to_string(),
            reason: "there is no run behind this, so there is nothing to read".to_string(),
        }
    }
}

impl RepoSource for CatalogSource {
    fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
        Err(Self::no_run("listing repository files"))
    }

    fn read_file(&self, _path: &str, _lines: Option<LineRange>) -> Result<String, PlatformError> {
        Err(Self::no_run("reading a repository file"))
    }

    fn size(&self, _path: &str) -> Result<u64, PlatformError> {
        Err(Self::no_run("reading a file size"))
    }

    fn search(
        &self,
        _kind: SearchKind,
        _query: &str,
        _glob: Option<&str>,
    ) -> Result<Vec<SearchHit>, PlatformError> {
        Err(Self::no_run("searching the repository"))
    }
}

/// The catalog, read off the real tools rather than described a second time.
pub fn inventory(settings: &Settings) -> Result<Vec<ToolListing>, crate::config::ConfigError> {
    Registry::catalog(settings)
}
