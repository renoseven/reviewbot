//! The whole command line surface. Every subcommand and flag parses.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "reviewbot",
    version,
    about = "Review a merge request, a pull request or a raw diff"
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
    /// The config file. The only source for this path.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Where runs and checkpoints live. No config field for this.
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

    #[arg(long, global = true)]
    pub no_color: bool,

    /// Retries for transient failures.
    #[arg(long, global = true, default_value_t = 2, value_name = "N")]
    pub retries: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Format {
    Text,
    Json,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the five stages over a URL, a diff file, or `-` for stdin.
    Review(ReviewArgs),

    /// Continue a run from its first unfinished stage.
    Resume { run_id: String },

    /// Post comments that are final but not yet on the merge request.
    Publish { run_id: String },

    /// Render the report again from the checkpoint. Calls no model.
    Report(ReportArgs),

    #[command(subcommand)]
    Run(RunCommand),

    #[command(subcommand)]
    Config(ConfigCommand),

    #[command(subcommand)]
    Model(ModelCommand),

    #[command(subcommand)]
    Tool(ToolCommand),

    #[command(subcommand)]
    Platform(PlatformCommand),

    #[command(subcommand)]
    Provider(ProviderCommand),
}

#[derive(Debug, Args)]
pub struct ReviewArgs {
    /// `http(s)://...` is a platform URL, `-` is stdin, anything else is a
    /// diff file path.
    pub target: String,

    /// Which `[[model]]` entry to use. Changes the run id.
    #[arg(long, value_name = "NAME|ALIAS")]
    pub model: Option<String>,

    /// Reuse an existing checkout, read only.
    #[arg(long, value_name = "PATH")]
    pub worktree: Option<PathBuf>,

    /// Also post the comments back to the merge request.
    #[arg(long)]
    pub publish: bool,

    /// Override the computed run id. Still fingerprint checked.
    #[arg(long, value_name = "ID")]
    pub run_id: Option<String>,

    /// Export the report and the summary here, for CI artifacts.
    #[arg(long, value_name = "DIR")]
    pub out_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    pub run_id: String,

    #[arg(long, value_name = "DIR")]
    pub out_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum RunCommand {
    /// Runs in the runs directory, with that directory named up front.
    List,

    Show {
        run_id: String,
    },

    /// Keep the newest N runs and delete the rest. The only command that
    /// deletes a run. N defaults to 0: delete every run.
    Prune {
        #[arg(long, default_value_t = reviewbot::record::DEFAULT_KEEP, value_name = "N")]
        keep: usize,

        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Parse and validate, credential readability included. Sends nothing.
    Check,
}

#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Every `[[model]]` entry with its prices and window.
    List,
}

#[derive(Debug, Subcommand)]
pub enum ToolCommand {
    /// Every tool, registered or not, with why not.
    List,
}

#[derive(Debug, Subcommand)]
pub enum PlatformCommand {
    /// Every `[[platform]]` entry, its resolved kind and where its token
    /// comes from. Reads no credential.
    List,
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Every `[[provider]]` entry, its protocol, budget and where its key
    /// comes from. Reads no credential.
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
            vec!["reviewbot", "report", "7f3a9c1e", "--out-dir", "artifacts"],
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
