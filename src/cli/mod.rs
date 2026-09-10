//! Subcommand dispatch and the mapping from the library's error type to an
//! exit code. No business logic lives here.

pub mod args;
mod logging;
pub mod render;
mod screen;
mod status;

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use clap::Parser;

use reviewbot::config::{RunOptions, Settings, paths};
use reviewbot::progress::{Event, Progress, Silent};
use reviewbot::{Error, Source};

use args::{
    Cli, Command, ConfigCommand, GlobalArgs, ModelCommand, PlatformCommand, ProviderCommand,
    ReviewArgs, RunCommand, ToolCommand,
};
use logging::LogSink;
use status::Status;

/// Parse, set up tracing, dispatch, and turn whatever comes back into an
/// exit code. Successful runs write nothing to stderr.
pub fn run() -> i32 {
    let cli = Cli::parse();
    let log = init_tracing(cli.global.config.as_deref());

    match dispatch(&cli, &log) {
        Ok(finished) => {
            // `-q` silences text; `--format json` still prints (json wins).
            let show = !finished.output.is_empty()
                && (!cli.global.quiet || cli.global.format == args::Format::Json);
            if show {
                print!("{}", finished.output);
            }
            finished.exit_code
        }
        Err(error) => {
            // The environment is read here rather than in `render`, which only
            // formats. Only `review` enters a run, so it is the only command
            // whose failure can offer itself as the way back in.
            let invocation: Option<Vec<std::ffi::OsString>> =
                matches!(cli.command, Command::Review(_)).then(|| std::env::args_os().collect());
            eprint!("{}", render::failure(&error, invocation.as_deref()));
            error.exit_code()
        }
    }
}

/// What a subcommand produced. A run that the budget cut short still has a
/// summary worth printing, so the exit code travels beside the text instead
/// of only coming out of the error type.
struct Finished {
    output: String,
    exit_code: i32,
}

impl Finished {
    fn ok(output: String) -> Self {
        Self {
            output,
            exit_code: 0,
        }
    }
}

fn dispatch(cli: &Cli, log: &LogSink) -> Result<Finished, Error> {
    match &cli.command {
        Command::Review(review) => {
            let settings = load(&cli.global, review_options(&cli.global, review))?;
            let source = read_source(&review.target)?;
            let quiet = cli.global.quiet || cli.global.format == args::Format::Json;
            let result = if quiet {
                let progress = CliProgress::new(&Silent, log.clone());
                reviewbot::review(&settings, &source, &progress)
            } else {
                let status = match std::io::stdout().is_terminal() {
                    true => Status::terminal(!cli.global.no_color),
                    false => Status::pipe(Box::new(std::io::stdout())),
                };
                let result = {
                    let progress = CliProgress::new(&status, log.clone());
                    reviewbot::review(&settings, &source, &progress)
                };
                // The terminal goes back before anything else writes to it,
                // failures included: a message printed into a block that is
                // still being repainted leaves the cursor below a run that
                // appears to still be going.
                status.finish();
                result
            }?;
            Ok(Finished {
                output: render::run_result(&result, cli.global.format),
                exit_code: result.exit_code(),
            })
        }
        Command::Config(ConfigCommand::Check) => {
            let settings = load(&cli.global, base_options(&cli.global))?;
            // Reading the credential is part of the check; nothing is sent.
            settings.selected_api_key()?;
            render::config_check(&settings, cli.global.format).map(Finished::ok)
        }
        Command::Run(RunCommand::List) => {
            render::run_list(&runs_dir(&cli.global), cli.global.format).map(Finished::ok)
        }
        Command::Run(RunCommand::Show { run_id }) => {
            render::run_show(&runs_dir(&cli.global), run_id, cli.global.format).map(Finished::ok)
        }
        Command::Run(RunCommand::Prune { keep, dry_run }) => {
            let report = reviewbot::record::prune_runs(&runs_dir(&cli.global), *keep, *dry_run)?;
            Ok(Finished::ok(render::run_prune(&report, cli.global.format)))
        }
        Command::Model(ModelCommand::List) => {
            let settings = load(&cli.global, base_options(&cli.global))?;
            render::model_list(&settings, cli.global.format).map(Finished::ok)
        }
        Command::Tool(ToolCommand::List) => {
            let settings = load(&cli.global, base_options(&cli.global))?;
            render::tool_list(&settings, cli.global.format).map(Finished::ok)
        }
        Command::Platform(PlatformCommand::List) => {
            let settings = load(&cli.global, base_options(&cli.global))?;
            render::platform_list(&settings, cli.global.format).map(Finished::ok)
        }
        Command::Provider(ProviderCommand::List) => {
            let settings = load(&cli.global, base_options(&cli.global))?;
            render::provider_list(&settings, cli.global.format).map(Finished::ok)
        }
    }
}

fn load(global: &GlobalArgs, options: RunOptions) -> Result<Settings, Error> {
    Ok(Settings::load(global.config.as_deref(), options)?)
}

fn base_options(global: &GlobalArgs) -> RunOptions {
    RunOptions {
        runs_dir: runs_dir(global),
        retries: global.retries,
        ..RunOptions::default()
    }
}

fn review_options(global: &GlobalArgs, review: &ReviewArgs) -> RunOptions {
    RunOptions {
        output_dir: review.output_dir.clone(),
        model: review.model.clone(),
        worktree: review.worktree.clone(),
        publish: review.publish,
        run_id: review.run_id.clone(),
        ..base_options(global)
    }
}

fn runs_dir(global: &GlobalArgs) -> PathBuf {
    global
        .runs_dir
        .clone()
        .unwrap_or_else(paths::default_runs_dir)
}

/// The positional argument decides its own shape: a URL, standard input, or
/// a diff file. Telling a diff from an mbox is M2's job.
fn read_source(target: &str) -> Result<Source, Error> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(Source::Url(target.to_string()));
    }
    let content = if target == "-" {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|error| unreadable(target, error))?;
        buffer
    } else {
        std::fs::read_to_string(target).map_err(|error| unreadable(target, error))?
    };
    Ok(Source::Diff {
        origin: target.to_string(),
        content,
    })
}

fn unreadable(target: &str, error: std::io::Error) -> Error {
    Error::Stage(reviewbot::stage::StageError::UnreadableInput {
        reason: format!("cannot open {target}: {error}"),
    })
}

/// Keep rendering and log placement separate while listening for the one
/// event that establishes where this run's artifacts belong.
struct CliProgress<'a> {
    rendered: &'a dyn Progress,
    log: LogSink,
}

impl<'a> CliProgress<'a> {
    fn new(rendered: &'a dyn Progress, log: LogSink) -> Self {
        Self { rendered, log }
    }
}

impl Progress for CliProgress<'_> {
    fn emit(&self, event: Event) {
        if let Event::RunStarted { run_dir, .. } = &event {
            let _ = self.log.switch_to(run_dir);
        }
        self.rendered.emit(event);
    }
}

/// Build tracing before dispatch. The writer buffers until a run starts, and
/// commands without a run simply discard that buffer when the process exits.
fn init_tracing(config_path: Option<&Path>) -> LogSink {
    let level = reviewbot::config::log_level(config_path);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("reviewbot={level}")));
    let log = LogSink::default();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_ansi(false)
        .with_writer(log.clone())
        .try_init();
    log
}
