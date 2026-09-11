use std::path::Path;
use std::sync::Arc;

use crate::common::SecretSource;
use crate::config::{ConfigError, KNOWN_PROTOCOLS, PlatformKind, Settings};
use crate::platform::github::GitHub;
use crate::platform::gitlab::GitLab;
use crate::platform::{ChangeRef, Platform, PlatformError, Repo};
use crate::protocol::{OpenAi, Protocol, ProtocolError};
use crate::record::{InputIdentity, InputRecord};
use crate::security::{PathPolicy, Redactor};
use crate::tool::Registry;
use crate::worktree::Worktree;

use super::error::StageError;

/// What a run can reach before it has a directory of its own: the platform
/// the change comes from, the protocol the model is called over, and the
/// redactor holding this run's secrets. Built at startup and handed to every
/// stage, so tests can put fakes in the same slots.
///
/// The worktree and the tools are deliberately not here. Neither can exist
/// this early — the cache lives inside the run directory, and every tool's
/// description is written from the worktree it will read — so they are built
/// afterwards, as `Equipment`.
pub struct Adapters {
    platform: Option<Box<dyn Platform>>,
    protocol: Box<dyn Protocol>,
    /// Process redactor with this run's secrets already hidden.
    redactor: Redactor,
}

impl Adapters {
    /// The real ones. `host` comes from the input URL and decides which
    /// `[[platform]]` entry is used; diff input has none.
    pub fn real(settings: &Settings, host: Option<&str>) -> Result<Self, StageError> {
        let selection = settings.selection()?;
        let api_key = settings.selected_api_key()?;
        let mut secrets = vec![api_key.expose().to_string()];
        if let Some(host) = host {
            secrets.extend(platform_secrets(settings, host)?);
        }
        let redactor = Redactor::with_secrets(secrets)?;
        let backoff = settings.options().backoff();

        let protocol: Box<dyn Protocol> = match selection.provider.protocol.as_str() {
            OpenAi::NAME => Box::new(OpenAi::connect(
                selection.provider.base_url.clone(),
                api_key,
                backoff,
            )?),
            other => {
                return Err(StageError::Protocol(ProtocolError::Unknown {
                    protocol: other.to_string(),
                    known: KNOWN_PROTOCOLS.join(", "),
                }));
            }
        };

        let platform = match host {
            Some(host) => Some(platform_for(settings, host)?),
            None => None,
        };

        Ok(Self {
            platform,
            protocol,
            redactor,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        platform: Option<Box<dyn Platform>>,
        protocol: Box<dyn Protocol>,
        redactor: Redactor,
    ) -> Self {
        Self {
            platform,
            protocol,
            redactor,
        }
    }

    pub fn platform(&self) -> Option<&dyn Platform> {
        self.platform.as_deref()
    }

    pub fn protocol(&self) -> &dyn Protocol {
        self.protocol.as_ref()
    }

    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    /// Test fixtures swap the platform after construction. Product code binds
    /// through `bind` instead.
    #[cfg(test)]
    pub(crate) fn set_platform(&mut self, platform: Box<dyn Platform>) {
        self.platform = Some(platform);
    }

    /// The repository this run reads content from, when there is a platform
    /// to read it through. A plain diff has none, which is the single source
    /// of the `Option` the worktree carries.
    pub fn repo(&self) -> Option<Repo> {
        self.platform().map(|platform| platform.repo())
    }

    /// Point the repository reads at the commit under review. The head sha is
    /// settled before any stage runs — this run resolved it, or read it back
    /// out of the `meta.json` an earlier attempt left — and every repository
    /// read is by that sha.
    pub fn bind(&self, record: &InputRecord) -> Result<(), PlatformError> {
        if let Some(platform) = &self.platform
            && let InputIdentity::Platform {
                host,
                project,
                number,
            } = &record.identity
        {
            platform.bind_repo(
                &ChangeRef {
                    host: host.clone(),
                    project: project.clone(),
                    number: *number,
                },
                &record.head_sha,
            )?;
        }
        Ok(())
    }
}

/// The other half of the adapters: this run's worktree and the tools built
/// over it.
///
/// Apart from `Adapters` because of when it can be built. The cache a run
/// fills for itself sits inside the run directory, so it must not be opened
/// before that directory is locked; and every tool's description, refusal and
/// availability is written from the worktree, so the tools cannot be built
/// before it either. Both facts point at the same moment, which is after the
/// lock and before the first stage.
pub struct Equipment {
    /// Shared rather than owned: the content tools and the checkers hold it
    /// for the whole run.
    worktree: Arc<Worktree>,
    tools: Registry,
}

impl Equipment {
    pub fn real(
        settings: &Settings,
        adapters: &Adapters,
        run_dir: &Path,
    ) -> Result<Self, StageError> {
        let worktree = Arc::new(Worktree::open(
            settings.options().worktree.clone(),
            adapters.repo(),
            run_dir,
        )?);
        warn_about(&worktree);
        let tools = Registry::assemble(
            settings,
            PathPolicy::for_settings(settings)?,
            Arc::clone(&worktree),
            run_dir,
        );
        Ok(Self { worktree, tools })
    }

    pub fn worktree(&self) -> &Worktree {
        &self.worktree
    }

    pub fn tools(&self) -> &Registry {
        &self.tools
    }
}

fn platform_secrets(settings: &Settings, host: &str) -> Result<Vec<String>, ConfigError> {
    let Some(entry) = settings.config().platform(host) else {
        return Ok(Vec::new());
    };
    let field = format!("platform.{}.api_token", entry.host().unwrap_or("unknown"));
    let token = SecretSource::parse(&field, &entry.api_token)
        .and_then(|source| source.read(&field, settings.options().worktree.as_deref()))?;
    Ok(vec![token.expose().to_string()])
}

fn platform_for(settings: &Settings, host: &str) -> Result<Box<dyn Platform>, StageError> {
    let entry = settings
        .config()
        .platform(host)
        .ok_or_else(|| PlatformError::UnknownHost {
            host: host.to_string(),
        })?;
    let kind = entry.kind().ok_or_else(|| PlatformError::UnknownHost {
        host: host.to_string(),
    })?;
    let token = crate::platform::read_token(entry)?;
    let backoff = settings.options().backoff();
    Ok(match kind {
        PlatformKind::Gitlab => Box::new(GitLab::for_host(entry.clone(), token, backoff)?),
        PlatformKind::Github => Box::new(GitHub::for_host(entry.clone(), token, backoff)?),
    })
}

/// Say once, for whoever is watching the run, what this worktree cannot do.
/// The same sentences the report will carry, so the terminal and the report
/// cannot disagree; the model is told the same thing by every description it
/// is given.
fn warn_about(worktree: &Worktree) {
    for went_without in worktree.went_without() {
        tracing::warn!("this run {went_without}");
    }
    if worktree.is_empty() {
        tracing::warn!(
            "pass --worktree to point at a checkout, or review a merge request URL. Every tool \
             is still offered, and each one says it cannot answer"
        );
    }
}
