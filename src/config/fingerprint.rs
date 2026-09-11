//! The identity of a set of settings, cut into one slice per stage that
//! reads settings of its own. A run re-entered under a changed config drops
//! the earliest stage the change can reach, and everything after it, instead
//! of being refused or landing in a directory of its own.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::Stage;

use super::file::Config;

/// Three digests, each named after the earliest stage that can read what
/// went into it. A change to a slice invalidates that stage and every stage
/// after it, so the earliest slice that differs is the whole answer.
///
/// There is no `merge` slice: everything merge scores with also reaches
/// `review`, which is earlier. `report` and `publish` have none because they
/// run on every entry regardless.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fingerprint {
    pub input: String,
    pub triage: String,
    pub review: String,
}

impl Fingerprint {
    /// Each slice beside the stage it is named after, in the order the run
    /// walks them — which is what makes "the earliest one that changed" a
    /// `find` rather than a comparison somebody has to keep sorted.
    fn slices(&self) -> [(Stage, &str); 3] {
        [
            (Stage::Input, self.input.as_str()),
            (Stage::Triage, self.triage.as_str()),
            (Stage::Review, self.review.as_str()),
        ]
    }

    /// The earliest stage whose slice differs, or `None` when the two sets
    /// of settings are the same as far as any stage can tell. One value says
    /// both which slice changed and where the rerun starts, because a slice
    /// carries the name of the stage it belongs to.
    pub fn earliest_change(&self, other: &Self) -> Option<Stage> {
        self.slices()
            .into_iter()
            .zip(other.slices())
            .find(|((_, mine), (_, theirs))| mine != theirs)
            .map(|((stage, _), _)| stage)
    }
}

/// `model` is the `--model` value as given on the command line, `worktree`
/// is whether `--worktree` was given at all (the content source mode).
///
/// Secret *values* never appear here: the config only holds the env var name
/// or the path a credential is read from, and an inline secret is refused at
/// parse time. Neither do artifact locations (`--output-dir`, `--runs-dir`)
/// or operational settings (`[log]`, `--retries`, `-q`, `--format`,
/// `--publish`), none of which changes what a run concludes.
pub fn fingerprint(config: &Config, model: Option<&str>, worktree: bool) -> Fingerprint {
    // Destructured field by field rather than read through `config.x`: a
    // section added to `Config` stops compiling here until somebody names
    // the slice it belongs to, so a new setting cannot fall out of all three
    // by being forgotten.
    let Config {
        // Diagnostics do not change what a review concludes, and a run that
        // threw away its checkpoints because someone asked for debug logging
        // would be charging for the question rather than the answer.
        log: _,
        review,
        triage,
        security,
        providers,
        models,
        platforms,
        tools,
    } = config;
    Fingerprint {
        input: digest(&(platforms, worktree)),
        triage: digest(triage),
        review: digest(&(review, security, providers, models, tools, model)),
    }
}

fn digest<T: Serialize>(value: &T) -> String {
    let canonical = serde_json::to_vec(value).expect("config is serializable");
    format!("{:x}", Sha256::digest(&canonical))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::file::{Model, PlatformEntry, Provider, ToolEntry};

    const MINIMAL: &str = r#"
[review]
max_files_per_listing = 200
max_hits_per_search = 50
max_files_per_fetch = 20
max_file_bytes = 262144
max_tool_output_bytes = 32768
max_rounds = 100

[triage]
max_chunk_tokens = 24000
skip_files_over_bytes = 262144

[security]
allow_extensions = ["rs"]

[[provider]]
name = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com"
api_key = "DEEPSEEK_API_KEY"
currency = "CNY"
budget_per_run = 10.0

[[model]]
name = "deepseek-v4-flash"
default = true
provider = "deepseek"
input_per_1m_tokens = 2.0
output_per_1m_tokens = 3.0
context_window_tokens = 131072
max_output_tokens = 4096

[[platform]]
base_url = "https://gitlab.com/api/v4"
api_token = "GITLAB_TOKEN"
"#;

    fn config() -> Config {
        toml::from_str(MINIMAL).expect("valid config")
    }

    fn baseline() -> Fingerprint {
        fingerprint(&config(), None, false)
    }

    /// The whole point of the split: which stage a change reaches is read
    /// off the slice that changed, and the slice is named after that stage.
    #[test]
    fn the_earliest_slice_that_differs_names_the_stage_the_rerun_starts_from() {
        let base = baseline();
        assert_eq!(base.earliest_change(&base), None);

        let mut later = base.clone();
        later.review = "other".to_string();
        assert_eq!(base.earliest_change(&later), Some(Stage::Review));

        later.triage = "other".to_string();
        assert_eq!(base.earliest_change(&later), Some(Stage::Triage));

        later.input = "other".to_string();
        assert_eq!(
            base.earliest_change(&later),
            Some(Stage::Input),
            "all three changed, and the earliest is the one that decides"
        );
    }

    #[test]
    fn a_platform_entry_and_the_worktree_flag_are_the_input_slice() {
        let base = baseline();

        let mut config = config();
        config.platforms.push(PlatformEntry {
            base_url: "https://api.github.com".to_string(),
            api_token: "GITHUB_TOKEN".to_string(),
        });
        let changed = fingerprint(&config, None, false);
        assert_eq!(base.earliest_change(&changed), Some(Stage::Input));

        let with_worktree = fingerprint(&self::config(), None, true);
        assert_eq!(base.earliest_change(&with_worktree), Some(Stage::Input));
    }

    #[test]
    fn the_triage_table_is_the_triage_slice() {
        let mut config = config();
        config.triage.max_chunk_tokens = 12_000;
        let changed = fingerprint(&config, None, false);

        assert_eq!(baseline().earliest_change(&changed), Some(Stage::Triage));
        assert_eq!(
            baseline().input,
            changed.input,
            "the input slice is untouched, so that checkpoint survives"
        );
    }

    /// Everything a model call is made of lands in one slice, because
    /// `review` is the earliest stage that reads any of it.
    #[test]
    fn settings_a_model_call_is_made_of_are_all_the_review_slice() {
        let base = baseline();
        let unchanged = |changed: &Fingerprint| {
            assert_eq!(base.input, changed.input, "input is untouched");
            assert_eq!(base.triage, changed.triage, "triage is untouched");
            assert_eq!(base.earliest_change(changed), Some(Stage::Review));
        };

        let mut review = config();
        review.review.max_files_per_fetch = 5;
        unchanged(&fingerprint(&review, None, false));

        let mut security = config();
        security.security.follow_symlinks = true;
        unchanged(&fingerprint(&security, None, false));

        let mut provider = config();
        provider.providers.push(Provider {
            name: "other".to_string(),
            protocol: "openai".to_string(),
            base_url: "https://other.example.com".to_string(),
            api_key: "OTHER_KEY".to_string(),
            currency: "USD".to_string(),
            budget_per_run: 1.0,
        });
        unchanged(&fingerprint(&provider, None, false));

        let mut model = config();
        model.models.push(Model {
            name: "deepseek-v4-pro".to_string(),
            alias: None,
            default: false,
            provider: "deepseek".to_string(),
            input_per_1m_tokens: 4.0,
            cached_input_per_1m_tokens: None,
            output_per_1m_tokens: 12.0,
            context_window_tokens: 131_072,
            max_output_tokens: 8_192,
            reasoning_effort: None,
        });
        unchanged(&fingerprint(&model, None, false));

        let mut tool = config();
        tool.tools.push(ToolEntry {
            name: "cppcheck".to_string(),
            description: "static analysis".to_string(),
            bin: std::path::PathBuf::from("/usr/bin/cppcheck"),
            args: Vec::new(),
            params: Default::default(),
            requires_checkout: false,
            requires_build: false,
            timeout_ms: 60_000,
        });
        unchanged(&fingerprint(&tool, None, false));

        unchanged(&fingerprint(&config(), Some("deepseek-v4-pro"), false));
    }

    /// A credential is a pointer in the config and a value only in memory,
    /// so nothing here can carry one — but the pointer is fingerprinted,
    /// because pointing at another account is a different run.
    #[test]
    fn a_fingerprint_carries_the_pointer_to_a_credential_and_never_its_value() {
        let base = baseline();
        let mut config = config();
        config.providers[0].api_key = "OTHER_KEY".to_string();
        let changed = fingerprint(&config, None, false);

        assert_eq!(base.earliest_change(&changed), Some(Stage::Review));
        for slice in [&changed.input, &changed.triage, &changed.review] {
            assert!(!slice.contains("KEY"), "a digest, not the text: {slice}");
        }
    }
}
