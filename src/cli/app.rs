use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use clap::Parser;

use reviewbot::config::{RunOptions, Settings, paths};
use reviewbot::progress::{Event, Progress, Silent};
use reviewbot::{Error, Source};

use super::args::{Cli, Command, ConfigCommand, GlobalArgs, ReviewArgs, RunCommand};
use super::logging::LogSink;
use super::render;
use super::status::Status;

/// Turn whatever the command produced into a stream and an exit code. There
/// are two arms and no more: text this process printed, or the one failure
/// that ended it. Successful runs write nothing to stderr.
pub fn run() -> i32 {
    match execute() {
        Ok(finished) => {
            print!("{}", finished.output);
            finished.exit_code
        }
        Err(failure) => {
            eprint!(
                "{}",
                render::failure(&failure).unwrap_or_else(|error| format!("{error}\n"))
            );
            failure.exit_code()
        }
    }
}

/// Parse, set up tracing, dispatch. Everything that can go wrong leaves here
/// as `Err`, the command line included, so no failure reaches a terminal
/// except through the one renderer above.
fn execute() -> Result<Finished, Failure> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // `--help` and `--version` come back as clap errors as well, and they
        // are not failures: they are the whole of what those two print. So
        // they leave as this invocation's output, exactly as clap wrote it.
        Err(asked) if !asked.use_stderr() => {
            return Ok(Finished {
                output: help_text(&asked),
                exit_code: asked.exit_code(),
            });
        }
        Err(complaint) => return Err(Failure::Usage(complaint)),
    };
    let log = init_tracing(Some(&cli.global.config));
    let finished = dispatch(&cli, &log).map_err(|error| Failure::Command {
        error: Box::new(error),
        // The environment is read here rather than in `render`, which only
        // formats. Only `review` enters a run, so it is the only command
        // whose failure can offer itself as the way back in.
        invocation: matches!(cli.command, Command::Review(_))
            .then(|| std::env::args_os().collect()),
    })?;
    Ok(finished.silenced(&cli.global))
}

/// What `--help` and `--version` print, as clap styles it. Whether the
/// escapes belong in the stream is a fact about the terminal, not about the
/// text, which is why it is decided here and not where the string is built.
fn help_text(asked: &clap::Error) -> String {
    match std::io::stdout().is_terminal() {
        true => asked.render().ansi().to_string(),
        false => asked.render().to_string(),
    }
}

/// Why the process is ending badly. clap's complaint is one of these because
/// it used to print itself and exit before dispatch could return anything,
/// which left the command line as the only failure that did not travel as a
/// `Result` — and so the only one that could quietly stop looking like the
/// rest.
pub(super) enum Failure {
    /// The command line did not parse. clap wrote both the sentence and the
    /// usage hint under it.
    Usage(clap::Error),
    /// A command ran and failed. `invocation` is the argument list as the
    /// process received it, and `None` on the commands that enter no run.
    Command {
        error: Box<Error>,
        invocation: Option<Vec<std::ffi::OsString>>,
    },
}

impl Failure {
    fn exit_code(&self) -> i32 {
        match self {
            Failure::Usage(complaint) => complaint.exit_code(),
            Failure::Command { error, .. } => error.exit_code(),
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

    /// `-q` silences text; `--format json` still prints (json wins). The exit
    /// code is not a stream and stays whatever the command decided.
    fn silenced(mut self, global: &GlobalArgs) -> Self {
        if global.quiet && global.format != super::args::Format::Json {
            self.output.clear();
        }
        self
    }
}

fn dispatch(cli: &Cli, log: &LogSink) -> Result<Finished, Error> {
    match &cli.command {
        Command::Review(review) => {
            let settings = load(&cli.global, review_options(&cli.global, review))?;
            let source = read_source(&review.target)?;
            let quiet = cli.global.quiet || cli.global.format == super::args::Format::Json;
            let result = if quiet {
                let progress = CliProgress::new(&Silent, log.clone());
                reviewbot::review(&settings, &source, &progress)
            } else {
                let status = match std::io::stdout().is_terminal() {
                    true => Status::terminal(!cli.global.no_color),
                    false => Status::pipe(Box::new(std::io::stdout())),
                }
                // Resolving a URL takes seconds, and the run cannot name
                // itself until it is done. What is already known says what
                // those seconds are for.
                .about(
                    &review.target,
                    settings.selection().ok().map(|it| it.model.name.as_str()),
                    settings.options().worktree.as_deref(),
                );
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
                output: render::run_result(&result, cli.global.format)?,
                exit_code: result.exit_code(),
            })
        }
        Command::Config(ConfigCommand::Check) => {
            let settings = load(&cli.global, base_options(&cli.global))?;
            // Reading the credential is part of the check; nothing is sent.
            settings.selected_api_key()?;
            render::config_check(&settings, cli.global.format).map(Finished::ok)
        }
        Command::Config(ConfigCommand::Init) => {
            let path = reviewbot::config::init_config(Some(&cli.global.config))?;
            render::config_init(&path, cli.global.format).map(Finished::ok)
        }
        Command::Config(ConfigCommand::Info) => {
            let settings = load(&cli.global, base_options(&cli.global))?;
            render::config_info(&settings, cli.global.format).map(Finished::ok)
        }
        Command::Run(RunCommand::List) => {
            render::run_list(&runs_dir(&cli.global), cli.global.format).map(Finished::ok)
        }
        Command::Run(RunCommand::Show { run_id }) => {
            render::run_show(&runs_dir(&cli.global), run_id, cli.global.format).map(Finished::ok)
        }
        Command::Run(RunCommand::Trace { run_id, trace_id }) => render::run_traces(
            &runs_dir(&cli.global),
            run_id,
            trace_id.as_deref(),
            cli.global.format,
        )
        .map(Finished::ok),
        Command::Run(RunCommand::Remove { run_id }) => {
            reviewbot::record::Runs::open(runs_dir(&cli.global)).remove(run_id)?;
            Ok(Finished::ok(String::new()))
        }
        Command::Run(RunCommand::Prune {
            keep_latest,
            dry_run,
        }) => {
            let report = reviewbot::record::Runs::open(runs_dir(&cli.global))
                .prune(*keep_latest, *dry_run)?;
            render::run_prune(&report, cli.global.format).map(Finished::ok)
        }
    }
}

fn load(global: &GlobalArgs, options: RunOptions) -> Result<Settings, Error> {
    Ok(Settings::load(Some(&global.config), options)?)
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
    paths::expand_user(&global.runs_dir)
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
