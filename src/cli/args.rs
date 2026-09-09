//! The whole command line surface. Every subcommand and flag parses, and every
//! one of them says what it does: a command nobody documented is a command
//! whose behaviour is only discoverable by running it, and one of these
//! deletes things.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "reviewbot",
    version,
    about = "Review a merge request, a pull request or a raw diff",
    long_about = "Review a merge request, a pull request or a raw unified diff with a language \
                  model, write a report, and optionally post the comments back.\n\n\
                  Nothing is read from the current directory: the config comes from --config or \
                  from ~/.reviewbot/config.toml. Runs, checkpoints and traces live under \
                  --runs-dir (default ~/.reviewbot/runs), and `run prune` is the only command \
                  that deletes any of it."
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
    /// The config file. The only source for this path. Default: ~/.reviewbot/config.toml
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Where runs and checkpoints live. No config field for this. Default: ~/.reviewbot/runs
    #[arg(long, global = true, value_name = "PATH")]
    pub runs_dir: Option<PathBuf>,

    /// How stdout is rendered.
    #[arg(long, global = true, value_enum, default_value_t = Format::Text)]
    pub format: Format,

    /// -v for debug, -vv for trace.
    #[arg(short = 'v', global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Write nothing at all to stdout. Errors still go to stderr.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// Never colour the progress output, whatever the terminal is.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Retries for transient failures.
    #[arg(long, global = true, default_value_t = 2, value_name = "N")]
    pub retries: u32,
}

/// How stdout is rendered.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Format {
    /// Aligned columns for a person.
    Text,
    /// One JSON document, with progress kept off stdout.
    Json,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the five stages over a URL, a diff file, or `-` for stdin.
    #[command(long_about = "Review a change and write a report.\n\n\
                      The target says what to review, and its shape decides how it is read:\n\n\
                      - `http(s)://...` is a merge request or pull request URL. Its host has to \
                      have a [[platform]] entry, and reviewbot fetches the diff, the description \
                      and the commit subjects through that platform's API.\n\
                      - `-` reads a unified diff from standard input.\n\
                      - anything else is a path to a file holding a unified diff. A `git \
                      format-patch` mbox is refused rather than half understood; use `git diff`.\n\n\
                      A URL is the only target that can be published back to, and the only one \
                      that comes with what the author wrote about the change.")]
    Review(ReviewArgs),

    /// Continue a run from its first unfinished stage.
    ///
    /// Uses the current config and the publish intent recorded in the run, and
    /// refuses a run whose config has changed since it started. Stages that
    /// already finished are not paid for again.
    Resume {
        /// The run to continue, as `run list` prints it.
        run_id: String,
    },

    /// Post comments that are final but not yet on the merge request.
    ///
    /// Calls no model: the comments come from the finished run. It posts even
    /// when the original review was not asked to, because asking now is the
    /// asking that counts, and it skips anything already on the request.
    Publish {
        /// The finished run whose comments should go out.
        run_id: String,
    },

    /// Render the report again from the checkpoint. Calls no model.
    #[command(
        long_about = "Write `report.md` and `summary.json` again from the checkpoints of a \
                      finished run. It calls no model and no platform, so it costs nothing and \
                      always agrees with what that run concluded."
    )]
    Report(ReportArgs),

    /// Inspect and prune past runs.
    #[command(subcommand)]
    Run(RunCommand),

    /// Check the config without sending anything.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// The model catalog this config declares.
    #[command(subcommand)]
    Model(ModelCommand),

    /// The tools this config could offer the model, and their contracts.
    #[command(subcommand)]
    Tool(ToolCommand),

    /// The code hosts this config can review.
    #[command(subcommand)]
    Platform(PlatformCommand),

    /// The vendors this config can call.
    #[command(subcommand)]
    Provider(ProviderCommand),
}

#[derive(Debug, Args)]
pub struct ReviewArgs {
    /// A merge request URL, a diff file, or `-` for stdin.
    pub target: String,

    /// Which `[[model]]` entry to use. Changes the run id.
    #[arg(long, value_name = "NAME|ALIAS")]
    pub model: Option<String>,

    /// Reuse an existing checkout, read only.
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

    /// Also post the comments back to the merge request.
    #[arg(long)]
    pub publish: bool,

    /// Override the computed run id. Still fingerprint checked.
    #[arg(long, value_name = "ID")]
    pub run_id: Option<String>,

    /// Export the report and the summary here, for CI artifacts.
    #[arg(long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    /// The run to render again.
    pub run_id: String,

    /// Also copy the report and the summary here.
    #[arg(long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum RunCommand {
    /// Runs in the runs directory, with that directory named up front.
    List,

    /// Everything recorded about one run: stages, counts, spend, artifacts.
    Show {
        /// The run to describe.
        run_id: String,
    },

    /// Keep the newest N runs and delete the rest.
    #[command(long_about = "Delete run directories, newest first kept.\n\n\
                      This is the only command that deletes anything. `--keep N` counts from the \
                      newest run by modification time and deletes everything older; the default \
                      is 0, which deletes every run in the directory. A deleted run takes its \
                      report, its summary, its traces and its worktree with it, and nothing else \
                      in reviewbot ever removes them.\n\n\
                      `--dry-run` prints exactly what would go, and deletes nothing.")]
    Prune {
        /// How many of the newest runs to keep. 0 deletes every run.
        #[arg(long, default_value_t = reviewbot::record::DEFAULT_KEEP, value_name = "N")]
        keep: usize,

        /// List what would be deleted and delete nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Parse and validate, credential readability included.
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
}

#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Every `[[model]]` entry with its prices and window.
    List,
}

#[derive(Debug, Subcommand)]
pub enum ToolCommand {
    /// Every tool's contract: purpose, call, rounds, preconditions.
    #[command(
        long_about = "Print the contract of every tool this config could offer the model, \
                      grouped by what the tool is for.\n\n\
                      Each row is the call as the model writes it, the description the model \
                      reads, one line per declared argument, the rounds the tool is offered on, \
                      and what a run's worktree has to be able to do before a call can be \
                      answered.\n\n\
                      Every tool here is offered to the model on every run; what varies is \
                      whether that run's worktree can answer it, and a run that cannot says so \
                      in the description and again in the refusal. Which worktree a review got \
                      is a fact about that review, and this command reviews nothing. The content \
                      descriptions here are written for the widest worktree there is, a whole \
                      checkout with a platform that can match expressions; the preconditions are \
                      what say when a run would get less."
    )]
    List,
}

#[derive(Debug, Subcommand)]
pub enum PlatformCommand {
    /// Every `[[platform]]` entry, its kind, and where its token comes from.
    List,
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Every `[[provider]]` entry, its protocol, budget and key source.
    List,
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

    /// Four of them need more than a line, because the short version cannot
    /// carry what a reader has to know before running them.
    #[test]
    fn the_commands_that_need_spelling_out_have_a_long_help() {
        let command = Cli::command();
        for path in [
            // What a target may be, and what each shape can and cannot do.
            vec!["review"],
            // The only command that deletes anything, and how `--keep` counts.
            vec!["run", "prune"],
            // Local from start to finish; it sends nothing.
            vec!["config", "check"],
            // It prints contracts, not what this invocation would register.
            vec!["tool", "list"],
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
            vec!["reviewbot", "resume", "7f3a9c1e"],
            vec!["reviewbot", "publish", "7f3a9c1e"],
            vec![
                "reviewbot",
                "report",
                "7f3a9c1e",
                "--output-dir",
                "artifacts",
            ],
            vec!["reviewbot", "run", "list"],
            vec!["reviewbot", "run", "show", "7f3a9c1e"],
            vec!["reviewbot", "run", "prune"],
            vec!["reviewbot", "run", "prune", "--keep", "5", "--dry-run"],
            vec!["reviewbot", "config", "check"],
            vec!["reviewbot", "model", "list"],
            vec!["reviewbot", "tool", "list"],
            vec!["reviewbot", "platform", "list"],
            vec!["reviewbot", "provider", "list"],
            vec!["reviewbot", "-q", "--format", "json", "run", "list"],
        ] {
            Cli::try_parse_from(&arguments)
                .unwrap_or_else(|error| panic!("{arguments:?} should parse: {error}"));
        }
    }

    #[test]
    fn retries_defaults_to_two() {
        let cli = Cli::try_parse_from(["reviewbot", "config", "check"]).unwrap();
        assert_eq!(cli.global.retries, 2);
        assert_eq!(cli.global.format, Format::Text);
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

    #[test]
    fn prune_keep_defaults_to_zero() {
        let cli = Cli::try_parse_from(["reviewbot", "run", "prune"]).unwrap();
        match cli.command {
            Command::Run(RunCommand::Prune { keep, dry_run }) => {
                assert_eq!(keep, reviewbot::record::DEFAULT_KEEP);
                assert!(!dry_run);
            }
            other => panic!("expected prune, got {other:?}"),
        }
    }
}
