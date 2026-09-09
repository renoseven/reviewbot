//! Subcommand dispatch and the mapping from the library's error type to an
//! exit code. No business logic lives here.

pub mod args;
pub mod render;

use std::io::Read;
use std::path::PathBuf;

use clap::Parser;

use reviewbot::config::{RunOptions, Settings, paths};
use reviewbot::{Error, Source};

use args::{
    Cli, Command, ConfigCommand, GlobalArgs, ModelCommand, PlatformCommand, ProviderCommand,
    ReviewArgs, RunCommand, ToolCommand,
};

/// Parse, set up tracing, dispatch, and turn whatever comes back into an
/// exit code. Successful runs write nothing to stderr.
pub fn run() -> i32 {
    let cli = Cli::parse();
    init_tracing(&cli.global);

    match dispatch(&cli) {
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

fn dispatch(cli: &Cli) -> Result<Finished, Error> {
    match &cli.command {
        Command::Review(review) => {
            let settings = load(&cli.global, review_options(&cli.global, review))?;
            let source = read_source(&review.target)?;
            let result = reviewbot::review(&settings, &source)?;
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

/// Progress and warnings are stdout; `-q` silences it completely. The only
/// thing that ever reaches stderr is the failure message printed above.
fn init_tracing(global: &GlobalArgs) {
    let level = match global.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("reviewbot={level}")));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_ansi(!global.no_color && std::io::IsTerminal::is_terminal(&std::io::stdout()));
    // JSON stdout must stay parseable: progress goes nowhere, same as `-q`.
    if global.quiet || global.format == args::Format::Json {
        let _ = builder.with_writer(std::io::sink).try_init();
    } else {
        let _ = builder.with_writer(std::io::stdout).try_init();
    }
}
