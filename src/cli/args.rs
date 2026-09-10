//! The whole command line surface. Every subcommand and flag parses, and every
//! one of them says what it does: a command nobody documented is a command
//! whose behaviour is only discoverable by running it, and one of these
//! deletes things.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "reviewbot",
    version,
    about = "Review a merge request, pull request, or unified diff",
    long_about = "Review a merge request, pull request, or unified diff with a language model, \
                  write a report, and optionally post the comments back.\n\n\
                  The config is --config. Runs live under --runs-dir. The current directory is \
                  never consulted. Only `run remove` and `run prune` delete anything."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Recognized by every subcommand.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Config file
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        default_value = "~/.reviewbot/config.toml"
    )]
    pub config: PathBuf,

    /// Directory for runs and checkpoints
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        default_value = "~/.reviewbot/runs"
    )]
    pub runs_dir: PathBuf,

    /// Output format
    #[arg(long, global = true, value_enum, default_value_t = Format::Text)]
    pub format: Format,

    /// Suppress stdout
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// Never colour progress output
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Retries for transient failures
    #[arg(long, global = true, default_value_t = 2, value_name = "N")]
    pub retries: u32,
}

/// How stdout is rendered.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Format {
    /// Aligned columns for a person.
    Text,
    /// One JSON document, with the status screen kept off stdout.
    Json,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage the config file
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Review a merge request, pull request, or unified diff
    #[command(long_about = "Review a change and write a report.\n\n\
                      The target says what to review, and its shape decides how it is read:\n\n\
                      - `http(s)://...` is a merge request or pull request URL. Its host has to \
                      have a [[platform]] entry, and reviewbot fetches the diff, the description \
                      and the commit subjects through that platform's API.\n\
                      - `-` reads a unified diff from standard input.\n\
                      - anything else is a path to a file holding a unified diff. A `git \
                      format-patch` mbox is refused rather than half understood; use `git diff`.\n\n\
                      A URL is the only target that can be published back to, and the only one \
                      that comes with what the author wrote about the change.\n\n\
                      Running the same command again continues the run it started rather than \
                      opening a second one: the stages that finished keep their checkpoints and \
                      are not paid for again, so a review that was interrupted, or one that has \
                      to post its comments after all, is the same command over again. A config \
                      that changed since the run started is refused instead of half applied.")]
    Review(ReviewArgs),

    /// Inspect and delete past runs
    #[command(subcommand)]
    Run(RunCommand),
}

#[derive(Debug, Args)]
pub struct ReviewArgs {
    /// Merge request URL, diff file, or `-` for stdin
    pub target: String,

    /// `[[model]]` name or alias
    #[arg(long, value_name = "NAME|ALIAS")]
    pub model: Option<String>,

    /// Existing checkout to reuse, read-only
    #[arg(
        long,
        value_name = "PATH",
        long_help = "Use this checkout as the run's worktree. It must stand on the commit under \
                     review, and reviewbot never writes to it.\n\n\
                     Without it the run opens an empty worktree of its own under the run \
                     directory and fetches files into it from the platform API as they are \
                     asked for. That worktree is not a whole project, so a checker declaring \
                     requires_checkout refuses on it, saying so to the model rather than \
                     reporting the project files it cannot find."
    )]
    pub worktree: Option<PathBuf>,

    /// Post comments back to the merge request
    #[arg(long)]
    pub publish: bool,

    /// Override the computed run id
    #[arg(long, value_name = "ID")]
    pub run_id: Option<String>,

    /// Directory for the report and summary
    #[arg(long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum RunCommand {
    /// List recorded runs
    List,

    /// Show one run
    Show {
        /// Run id
        run_id: String,
    },

    /// Delete one run
    #[command(long_about = "Delete one run directory by id.\n\n\
                      The run takes its report, its summary, its log, its traces and its \
                      worktree with it. A missing id is an error, not a no-op. Success prints \
                      nothing.")]
    Remove {
        /// Run id
        run_id: String,
    },

    /// Delete old runs, retaining the newest
    #[command(long_about = "Delete run directories, newest first retained.\n\n\
                      `--keep-latest N` counts from the newest run by modification time and \
                      deletes everything older. 0 deletes every run in the directory. A \
                      deleted run takes its report, its summary, its log, its traces and its \
                      worktree with it.\n\n\
                      Success prints one sentence. `--dry-run` reports what would happen and \
                      deletes nothing.")]
    Prune {
        /// Newest runs to retain
        #[arg(long = "keep-latest", default_value_t = reviewbot::record::DEFAULT_KEEP, value_name = "N")]
        keep_latest: usize,

        /// Report what would be deleted and delete nothing
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Write the example config
    #[command(long_about = "Write a complete, valid example config and stop.\n\n\
                      The file is the one that ships in the binary as src/config/example.toml: \
                      every required number is filled in, and `config check` will accept it \
                      once the credential names it points at are readable. It will not \
                      overwrite a file that is already there; pass `--config` to choose \
                      another path.")]
    Init,

    /// Validate the config
    #[command(long_about = "Validate the config and stop.\n\n\
                      Everything it does is local: the file is parsed, every rule that spans two \
                      tables is checked, the selected model resolves to a provider, and the \
                      selected provider's credential is read from wherever it says to read it. \
                      No request is sent to a model or to a platform, so this is safe to run in \
                      any pipeline and costs nothing.\n\n\
                      It does not check that a [[tool]] `bin` exists on this machine: that is a \
                      fact about the runner, and a review that calls the tool is where it \
                      matters.")]
    Check,

    /// List platforms, providers, models, and tools
    #[command(
        long_about = "Print the platforms, providers, models, and tools in the loaded config \
                      as tables.\n\n\
                      Column titles are uppercase. A title of more than one word is joined \
                      with `_`. The tool table is name, purpose, and rounds; the rest of \
                      the contract is in `--format json`.\n\n\
                      Credentials are named by source, never printed. This command does not \
                      read them; `config check` is what does that."
    )]
    Info,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_surface_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// Every command and every argument says what it is. Walked rather than
    /// listed, so a command added later cannot land without help text: the
    /// only way to find out what an undocumented flag does is to run it, and
    /// one of these commands deletes runs.
    #[test]
    fn every_command_and_argument_carries_help() {
        fn walk(command: &clap::Command, path: &str) {
            let named = match path.is_empty() {
                true => command.get_name().to_string(),
                false => format!("{path} {}", command.get_name()),
            };
            assert!(
                command.get_about().is_some() || command.get_long_about().is_some(),
                "{named} has no help text"
            );
            for argument in command.get_arguments() {
                assert!(
                    argument.get_help().is_some() || argument.get_long_help().is_some(),
                    "{named}: --{} has no help text",
                    argument.get_id()
                );
            }
            for sub in command.get_subcommands() {
                walk(sub, &named);
            }
        }
        walk(&Cli::command(), "");
    }

    /// These need more than a line, because the short version cannot carry
    /// what a reader has to know before running them.
    #[test]
    fn the_commands_that_need_spelling_out_have_a_long_help() {
        let command = Cli::command();
        for path in [
            // What a target may be, and what each shape can and cannot do.
            vec!["review"],
            // How `--keep-latest` counts, and what success prints.
            vec!["run", "prune"],
            // Deletes one run and prints nothing.
            vec!["run", "remove"],
            // Local from start to finish; it sends nothing.
            vec!["config", "check"],
            // Where it writes, and that it will not overwrite.
            vec!["config", "init"],
            // The four catalogs; the tool table is the short columns.
            vec!["config", "info"],
        ] {
            let mut found = &command;
            for name in &path {
                found = found
                    .get_subcommands()
                    .find(|sub| sub.get_name() == *name)
                    .unwrap_or_else(|| panic!("{path:?} exists"));
            }
            assert!(
                found.get_long_about().is_some(),
                "{path:?} needs a long help"
            );
        }
    }

    #[test]
    fn the_full_surface_parses() {
        for arguments in [
            vec![
                "reviewbot",
                "review",
                "https://gitlab.com/a/b/-/merge_requests/1",
            ],
            vec![
                "reviewbot",
                "review",
                "--publish",
                "--worktree",
                ".",
                "x.diff",
            ],
            vec![
                "reviewbot",
                "review",
                "--run-id",
                "7f3a9c1e",
                "--output-dir",
                "artifacts",
                "x.diff",
            ],
            vec!["reviewbot", "run", "list"],
            vec!["reviewbot", "run", "show", "7f3a9c1e"],
            vec!["reviewbot", "run", "remove", "7f3a9c1e"],
            vec!["reviewbot", "run", "prune"],
            vec![
                "reviewbot",
                "run",
                "prune",
                "--keep-latest",
                "5",
                "--dry-run",
            ],
            vec!["reviewbot", "config", "check"],
            vec!["reviewbot", "config", "init"],
            vec!["reviewbot", "config", "info"],
            vec!["reviewbot", "-q", "--format", "json", "run", "list"],
        ] {
            Cli::try_parse_from(&arguments)
                .unwrap_or_else(|error| panic!("{arguments:?} should parse: {error}"));
        }
    }

    #[test]
    fn commands_are_listed_in_semantic_order() {
        fn names_of(command: &clap::Command) -> Vec<String> {
            command
                .get_subcommands()
                .map(|sub| sub.get_name().to_string())
                .filter(|name| name != "help")
                .collect()
        }
        let root = Cli::command();
        assert_eq!(names_of(&root), ["config", "review", "run"]);
        assert_eq!(
            names_of(root.find_subcommand("config").expect("config")),
            ["init", "check", "info"]
        );
        assert_eq!(
            names_of(root.find_subcommand("run").expect("run")),
            ["list", "show", "remove", "prune"]
        );
    }

    #[test]
    fn help_shows_defaults_the_way_clap_does() {
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("[default: ~/.reviewbot/config.toml]"),
            "{help}"
        );
        assert!(help.contains("[default: ~/.reviewbot/runs]"), "{help}");
        assert!(help.contains("[default: text]"), "{help}");
        assert!(help.contains("[default: 2]"), "{help}");
        assert!(
            !help.contains("Default:"),
            "defaults belong to clap, not the help prose: {help}"
        );
    }

    #[test]
    fn retries_defaults_to_two() {
        let cli = Cli::try_parse_from(["reviewbot", "config", "check"]).unwrap();
        assert_eq!(cli.global.retries, 2);
        assert_eq!(cli.global.format, Format::Text);
    }

    #[test]
    fn verbosity_flags_no_longer_parse() {
        assert!(Cli::try_parse_from(["reviewbot", "-v", "config", "check"]).is_err());
        assert!(Cli::try_parse_from(["reviewbot", "-vv", "config", "check"]).is_err());
    }

    /// The nouns are singular, and the old plurals are gone rather than
    /// kept as aliases: two spellings for one command is a surface to
    /// document and a thing to get wrong.
    #[test]
    fn the_old_plural_nouns_no_longer_parse() {
        for arguments in [
            vec!["reviewbot", "runs", "list"],
            vec!["reviewbot", "models", "list"],
            vec!["reviewbot", "tools", "list"],
        ] {
            assert!(
                Cli::try_parse_from(&arguments).is_err(),
                "{arguments:?} should no longer parse"
            );
        }
    }

    /// The three commands that existed only to re-enter a run are gone, and
    /// not kept as aliases of `review`: `review` re-enters the run itself, and
    /// a second spelling would be a second way to describe the same run.
    #[test]
    fn the_commands_that_only_re_entered_a_run_no_longer_parse() {
        for arguments in [
            vec!["reviewbot", "resume", "7f3a9c1e"],
            vec!["reviewbot", "publish", "7f3a9c1e"],
            vec!["reviewbot", "report", "7f3a9c1e"],
        ] {
            assert!(
                Cli::try_parse_from(&arguments).is_err(),
                "{arguments:?} should no longer parse"
            );
        }
    }

    #[test]
    fn prune_keep_latest_defaults_to_zero() {
        let cli = Cli::try_parse_from(["reviewbot", "run", "prune"]).unwrap();
        match cli.command {
            Command::Run(RunCommand::Prune {
                keep_latest,
                dry_run,
            }) => {
                assert_eq!(keep_latest, reviewbot::record::DEFAULT_KEEP);
                assert!(!dry_run);
            }
            other => panic!("expected prune, got {other:?}"),
        }
    }

    /// These used to be top-level nouns with a `list` verb. The same
    /// information now lives under `config info`, and the old spellings
    /// are gone rather than kept as aliases.
    #[test]
    fn the_old_catalog_nouns_no_longer_parse() {
        for arguments in [
            vec!["reviewbot", "model", "list"],
            vec!["reviewbot", "tool", "list"],
            vec!["reviewbot", "platform", "list"],
            vec!["reviewbot", "provider", "list"],
            vec!["reviewbot", "run", "prune", "--keep", "5"],
        ] {
            assert!(
                Cli::try_parse_from(&arguments).is_err(),
                "{arguments:?} should no longer parse"
            );
        }
    }
}
