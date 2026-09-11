//! Turning results and failures into text or JSON. Every string leaving here
//! has been through the redactor.

use std::ffi::OsString;
use std::path::Path;

use chrono::{DateTime, Utc};
use comfy_table::presets::ASCII_FULL_CONDENSED;
use comfy_table::{ContentArrangement, Table};

use reviewbot::config::Settings;
use reviewbot::domain::{Confidence, Severity};
use reviewbot::security::Redactor;
use reviewbot::{Error, RunResult};

use super::app::Failure;
use super::args::Format;

/// How far the run got. The stages before it are implied: they walk in
/// order, so the last one is the whole answer.
fn furthest_stage(stages: &[reviewbot::domain::Stage]) -> String {
    stages
        .last()
        .map(|stage| stage.name().to_string())
        .unwrap_or_default()
}

/// The widest label in a run summary. The live screen uses the same prefix,
/// because its temporary rows should not jump sideways when the final rows
/// replace them.
pub(super) const SUMMARY_LABEL_WIDTH: usize = 10;

pub(super) fn push_summary_line(out: &mut String, label: &str, value: &str) {
    out.push_str(&format!("{label:<SUMMARY_LABEL_WIDTH$} {value}\n"));
}

fn redact(text: &str) -> Result<String, Error> {
    Ok(Redactor::new()?.redact(text))
}

/// stdout for a finished run.
pub fn run_result(result: &RunResult, format: Format) -> Result<String, Error> {
    let text = match format {
        Format::Json => serde_json::to_string_pretty(result)
            .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}")),
        Format::Text => text_summary(result),
    };
    redact(&text)
}

fn text_summary(result: &RunResult) -> String {
    let mut out = String::new();
    push_summary_line(&mut out, "run_id", &result.run_id);
    push_summary_line(&mut out, "model", &result.model);
    // Just the verdict. Why there is none belongs at the end with the other
    // things that went wrong, not folded into the line a reader skims for
    // the answer.
    match result.overall_score {
        Some(score) => push_summary_line(
            &mut out,
            "overall",
            &format!("{score} / 100  (the model's judgement of what this run found)"),
        ),
        None => push_summary_line(&mut out, "overall", "not scored"),
    }
    let severity = Severity::ALL
        .iter()
        .map(|band| format!("{band} {}", result.count_severity(*band)))
        .collect::<Vec<_>>()
        .join(" / ");
    let bands = Confidence::ALL
        .iter()
        .map(|band| format!("{band} {}", result.count(*band)))
        .collect::<Vec<_>>()
        .join(" / ");
    push_summary_line(&mut out, "comments", &result.comments.len().to_string());
    push_summary_line(&mut out, "severity", &severity);
    push_summary_line(&mut out, "confidence", &bands);
    push_summary_line(
        &mut out,
        "skipped",
        &format!("{} files", result.skipped.len()),
    );
    if !result.unreviewed.is_empty() {
        push_summary_line(
            &mut out,
            "unreviewed",
            &format!("{} files", result.unreviewed.len()),
        );
    }
    match result.budget {
        Some(ceiling) => push_summary_line(
            &mut out,
            "budget",
            &format!("{:.4} / {:.4} {}", result.spent, ceiling, result.currency),
        ),
        None => push_summary_line(
            &mut out,
            "budget",
            &format!("{:.4} {} (no ceiling)", result.spent, result.currency),
        ),
    }
    push_summary_line(
        &mut out,
        "report",
        &result.report_path.display().to_string(),
    );
    push_summary_line(
        &mut out,
        "summary",
        &result.summary_path.display().to_string(),
    );
    if !result.published.is_empty() {
        push_summary_line(
            &mut out,
            "published",
            &format!("{} comments", result.published.len()),
        );
    }
    // Last, and once, in the same shape every other error takes. A run that
    // stopped says so and nothing else: the missing score follows from the
    // stop and does not earn a second line quoting the first one back.
    match (&result.stopped, &result.unscored_reason) {
        (Some(reason), _) | (None, Some(reason)) => out.push_str(&problem(reason)),
        (None, None) => {}
    }
    out
}

/// How a problem lands at the end of a summary: a blank line, then
/// `error:` and the sentence. The blank line separates it from the fields
/// above so it is not read as one more field.
fn problem(reason: &str) -> String {
    format!("\n{}", error_line(reason))
}

fn error_line(reason: &str) -> String {
    format!("error: {reason}\n")
}

/// stderr for a failure — every one of them, a command line that did not
/// parse included, because they all arrive here as one value. This stream
/// has printed nothing yet, so there is no blank line to set the sentence
/// off from; that blank belongs on stdout, after a summary's fields.
pub(super) fn failure(failure: &Failure) -> Result<String, Error> {
    let text = match failure {
        // clap wrote its own `error:` sentence, and the usage hint below it
        // is worth keeping, so the shape is all this has left to add.
        Failure::Usage(complaint) => complaint.render().to_string(),
        Failure::Command { error, invocation } => command_failure(error, invocation.as_deref()),
    };
    redact(&text)
}

/// Names the run, because a run that got as far as its own directory is the
/// thing the next attempt goes back into, and prints the command that goes
/// back in — which is this very invocation, now that re-entering a run is
/// running the same command again. `invocation` is `None` on the commands
/// that enter no run, and the replay is withheld from the one failure the
/// same command cannot get past — a directory recording another input,
/// which this command is what aimed at — so nothing here offers a line that
/// would only fail again.
fn command_failure(error: &Error, invocation: Option<&[OsString]>) -> String {
    let mut out = error_line(&error.to_string());
    if let Some(run_id) = error.run_id() {
        out.push_str(&format!("run_id: {run_id}\n"));
        if let Some(invocation) = invocation
            .filter(|words| !words.is_empty())
            .filter(|_| error.same_command_continues())
        {
            // Echoed rather than rebuilt, so `--runs-dir` and everything else
            // that decided which run this is comes back exactly as it went in.
            out.push_str(&format!("next: {}\n", shell_command(invocation)));
            out.push_str(&format!(
                "      the same command again continues run {run_id}; \
                 the stages it finished are not run again\n"
            ));
        }
    }
    out
}

/// The invocation as one line a shell would read back the same way, which is
/// the point of printing it: a target with an `&` in it has to survive the
/// paste.
fn shell_command(invocation: &[OsString]) -> String {
    invocation
        .iter()
        .map(|word| shell_quote(&word.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single quotes are the whole of the escaping, because inside them every
/// character but `'` stands for itself, and `'\''` closes, escapes and reopens
/// for the one that does not. A word of nothing but characters no shell reads
/// as syntax is left bare, so the usual line still looks like the one that was
/// typed.
fn shell_quote(word: &str) -> String {
    let bare = !word.is_empty()
        && word
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_./:=@%+,".contains(character));
    match bare {
        true => word.to_string(),
        false => format!("'{}'", word.replace('\'', r"'\''")),
    }
}

/// stdout for `config check`.
pub fn config_check(settings: &Settings, format: Format) -> Result<String, Error> {
    let selection = settings.selection()?;
    let text = match format {
        Format::Json => serde_json::json!({
            "config": settings.config_path().display().to_string(),
            "model": selection.model.name,
            "selected_by": selection.reason.as_str(),
            "provider": selection.provider.name,
            "protocol": selection.provider.protocol,
            "currency": selection.provider.currency,
            "budget_per_run": selection.provider.budget_per_run,
            "platforms": settings.config().platforms.len(),
            "tools": settings.config().tools.len(),
            "ok": true,
        })
        .to_string(),
        Format::Text => {
            let mut out = String::new();
            out.push_str(&check_field("Path", settings.config_path().display()));
            out.push_str(&check_field("Provider", &selection.provider.name));
            out.push_str(&check_field("Credential", "readable"));
            out.push_str(&check_field(
                "Budget",
                budget(
                    selection.provider.budget_per_run,
                    &selection.provider.currency,
                ),
            ));
            out.push_str(&check_field("Platforms", settings.config().platforms.len()));
            out.push_str(&check_field("Models", settings.config().models.len()));
            out.push_str(&check_field("Tools", settings.config().tools.len()));
            out.push_str("ok\n");
            out
        }
    };
    redact(&text)
}

/// Widest `config check` label, including the colon: `Credential:`.
const CHECK_LABEL_WIDTH: usize = 11;

fn check_field(label: &str, value: impl std::fmt::Display) -> String {
    format!(
        "{label:<CHECK_LABEL_WIDTH$} {value}\n",
        label = format!("{label}:")
    )
}

/// stdout for `config init`.
pub fn config_init(path: &Path, format: Format) -> Result<String, Error> {
    let text = match format {
        Format::Json => serde_json::json!({ "config": path.display().to_string() }).to_string(),
        Format::Text => format!("wrote {}\n", path.display()),
    };
    redact(&text)
}

/// stdout for `config info`: the four catalogs as tables.
pub fn config_info(settings: &Settings, format: Format) -> Result<String, Error> {
    let tools = reviewbot::tool::inventory(settings)?;
    let text = match format {
        Format::Json => serde_json::json!({
            "config": settings.config_path().display().to_string(),
            "log": settings.config().log,
            "platforms": platform_values(settings),
            "providers": provider_values(settings),
            "models": model_values(settings),
            "plan": settings.config().plan,
            "review": settings.config().review,
            "security": settings.config().security,
            "tools": tools,
        })
        .to_string(),
        Format::Text => text_info(settings, &tools),
    };
    redact(&text)
}

fn text_info(settings: &Settings, tools: &[reviewbot::tool::ToolListing]) -> String {
    format!(
        "{}\n{}\n{}\n{}",
        platform_table(settings),
        provider_table(settings),
        model_table(settings),
        tool_table(tools),
    )
}

pub fn run_list(runs_dir: &Path, format: Format) -> Result<String, Error> {
    let runs = reviewbot::record::Runs::open(runs_dir).list()?;
    let text = match format {
        Format::Json => serde_json::json!({
            "runs_dir": runs_dir.display().to_string(),
            "runs": runs,
        })
        .to_string(),
        Format::Text => {
            let mut out = format!("Runs dir  {}\n", runs_dir.display());
            if runs.is_empty() {
                out.push_str("(no runs)\n");
                return redact(&out);
            }
            let rows: Vec<Vec<String>> = runs
                .iter()
                .map(|row| {
                    vec![
                        row.run_id.clone(),
                        truncate(&row.input, 40),
                        furthest_stage(&row.completed_stages),
                        money(row.spent, &row.currency, 4),
                        format_unix_utc(row.updated_at),
                    ]
                })
                .collect();
            out.push_str(&pad_table(
                &["RUN_ID", "INPUT", "STAGES", "SPENT", "UPDATED"],
                &rows,
            ));
            out
        }
    };
    redact(&text)
}

pub fn run_show(runs_dir: &Path, run_id: &str, format: Format) -> Result<String, Error> {
    let show = reviewbot::record::Runs::open(runs_dir).show(run_id)?;
    let text = match format {
        Format::Json => serde_json::to_string_pretty(&show)
            .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}")),
        Format::Text => {
            let mut out = String::new();
            out.push_str(&format!("run_id     {}\n", show.run_id));
            out.push_str(&format!("input      {}\n", show.input));
            out.push_str(&format!("model      {}\n", show.model));
            out.push_str(&format!(
                "stages     {}\n",
                furthest_stage(&show.completed_stages)
            ));
            let bands = show
                .comments
                .iter()
                .map(|row| format!("{} {}", row.band, row.count))
                .collect::<Vec<_>>()
                .join(" / ");
            out.push_str(&format!("comments   {bands}\n"));
            match show.budget {
                Some(ceiling) => out.push_str(&format!(
                    "budget     {:.4} / {:.4} {}\n",
                    show.spent, ceiling, show.currency
                )),
                None => out.push_str(&format!(
                    "budget     {:.4} {} (no ceiling)\n",
                    show.spent, show.currency
                )),
            }
            out.push_str(&format!("report     {}\n", show.report.display()));
            out.push_str(&format!("summary    {}\n", show.summary.display()));
            out.push_str(&format!("log        {}\n", show.log.display()));
            out.push_str(&format!("traces     {}\n", show.traces.display()));
            out
        }
    };
    redact(&text)
}

pub fn run_prune(report: &reviewbot::record::PruneReport, format: Format) -> Result<String, Error> {
    let pruned = report.deleted.len();
    let text = match format {
        Format::Json => {
            let mut value = serde_json::json!({
                "pruned": pruned,
                "keep_latest": report.keep,
                "deleted": report.deleted,
            });
            if report.dry_run {
                value["dry_run"] = serde_json::json!(true);
            }
            value.to_string()
        }
        Format::Text => format!(
            "{}\n",
            prune_sentence(pruned, report.kept.len(), report.dry_run)
        ),
    };
    redact(&text)
}

fn prune_sentence(pruned: usize, kept: usize, dry_run: bool) -> String {
    let prune = if dry_run { "would prune" } else { "pruned" };
    let deleted = match pruned {
        0 => "no runs".to_string(),
        1 => "1 run".to_string(),
        n => format!("{n} runs"),
    };
    let recent = match kept {
        0 => "none".to_string(),
        1 => "the most recent run".to_string(),
        n => format!("the {n} most recent"),
    };
    format!("{prune} {deleted}, retaining {recent}.")
}

/// Which API endpoints this config can review, and where each one's token
/// comes from. The token column is the *source*, never the credential: an
/// inline secret is refused at parse time, so there is nothing here to leak.
fn platform_values(settings: &Settings) -> Vec<serde_json::Value> {
    settings
        .config()
        .platforms
        .iter()
        .map(|platform| {
            serde_json::json!({
                "host": platform.host(),
                "kind": platform.kind().map(|kind| kind.as_str()),
                "base_url": platform.base_url,
                "api_token": platform.api_token,
            })
        })
        .collect()
}

fn platform_table(settings: &Settings) -> String {
    let rows: Vec<Vec<String>> = settings
        .config()
        .platforms
        .iter()
        .map(|platform| vec![platform.base_url.clone(), platform.api_token.clone()])
        .collect();
    catalog_table("PLATFORMS", &["BASE_URL", "TOKEN_FROM"], &rows)
}

/// Which vendors this config can call, and what each one is allowed to
/// spend. The budget is per run, not per month: `-1` is no ceiling and `0`
/// spends nothing, and both are worth being able to read off a table.
fn provider_values(settings: &Settings) -> Vec<serde_json::Value> {
    settings
        .config()
        .providers
        .iter()
        .map(|provider| {
            serde_json::json!({
                "name": provider.name,
                "protocol": provider.protocol,
                "base_url": provider.base_url,
                "currency": provider.currency,
                "budget_per_run": provider.budget_per_run,
                "api_key": provider.api_key,
            })
        })
        .collect()
}

fn provider_table(settings: &Settings) -> String {
    let rows: Vec<Vec<String>> = settings
        .config()
        .providers
        .iter()
        .map(|provider| {
            vec![
                provider.name.clone(),
                provider.protocol.clone(),
                provider.base_url.clone(),
                budget(provider.budget_per_run, &provider.currency),
                provider.api_key.clone(),
            ]
        })
        .collect();
    catalog_table(
        "PROVIDERS",
        &["NAME", "PROTOCOL", "BASE_URL", "BUDGET_PER_RUN", "KEY_FROM"],
        &rows,
    )
}

/// `-1` and `0` are both real settings and neither reads as an amount, so
/// they get words instead of a number.
fn budget(value: f64, currency: &str) -> String {
    if value < 0.0 {
        return "unlimited".to_string();
    }
    if value == 0.0 {
        return "none".to_string();
    }
    money(value, currency, 2)
}

fn model_values(settings: &Settings) -> Vec<serde_json::Value> {
    let default_name = default_model_name(settings);
    settings
        .config()
        .models
        .iter()
        .map(|model| {
            serde_json::json!({
                "name": model.name,
                "alias": model.alias,
                "provider": model.provider,
                "currency": currency_of(settings, &model.provider),
                "input_per_1m_tokens": model.input_per_1m_tokens,
                "cached_input_per_1m_tokens": model.cached_input_per_1m_tokens,
                "output_per_1m_tokens": model.output_per_1m_tokens,
                "context_window_tokens": model.context_window_tokens,
                "max_output_tokens": model.max_output_tokens,
                "reasoning_effort": model.reasoning_effort,
                "default": default_name == Some(model.name.as_str()),
            })
        })
        .collect()
}

fn model_table(settings: &Settings) -> String {
    let default_name = default_model_name(settings);
    let rows: Vec<Vec<String>> = settings
        .config()
        .models
        .iter()
        .map(|model| {
            let currency = currency_of(settings, &model.provider);
            let cached = model
                .cached_input_per_1m_tokens
                .map(|value| money(value, currency, 2))
                .unwrap_or_else(|| "-".to_string());
            let is_default = default_name == Some(model.name.as_str());
            vec![
                model.name.clone(),
                model.alias.clone().unwrap_or_else(|| "-".to_string()),
                model.provider.clone(),
                money(model.input_per_1m_tokens, currency, 2),
                cached,
                money(model.output_per_1m_tokens, currency, 2),
                model.context_window_tokens.to_string(),
                model.max_output_tokens.to_string(),
                if is_default { "yes" } else { "no" }.to_string(),
            ]
        })
        .collect();
    catalog_table(
        "MODELS",
        &[
            "NAME",
            "ALIAS",
            "PROVIDER",
            "IN_1M",
            "CACHED_1M",
            "OUT_1M",
            "CONTEXT",
            "MAX_OUT",
            "DEFAULT",
        ],
        &rows,
    )
}

fn default_model_name(settings: &Settings) -> Option<&str> {
    settings
        .config()
        .models
        .iter()
        .find(|model| model.default)
        .map(|model| model.name.as_str())
}

fn tool_table(tools: &[reviewbot::tool::ToolListing]) -> String {
    let rows: Vec<Vec<String>> = tools
        .iter()
        .map(|row| {
            let rounds: Vec<&str> = row.rounds.iter().map(|round| round.as_str()).collect();
            vec![
                row.name.clone(),
                row.purpose.as_str().to_string(),
                rounds.join(", "),
            ]
        })
        .collect();
    catalog_table("TOOLS", &["NAME", "PURPOSE", "ROUNDS"], &rows)
}

fn currency_of<'a>(settings: &'a Settings, provider: &str) -> &'a str {
    settings
        .config()
        .providers
        .iter()
        .find(|entry| entry.name == provider)
        .map(|entry| entry.currency.as_str())
        .unwrap_or("")
}

fn money(amount: f64, currency: &str, digits: usize) -> String {
    if currency.is_empty() {
        format!("{amount:.digits$}")
    } else {
        format!("{amount:.digits$} {currency}")
    }
}

/// Missing `meta.json` stores `updated_at = 0`; that is not a real time.
fn format_unix_utc(secs: u64) -> String {
    if secs == 0 {
        return "-".to_string();
    }
    let Ok(secs) = i64::try_from(secs) else {
        return format!("{secs}");
    };
    match DateTime::<Utc>::from_timestamp(secs, 0) {
        Some(when) => when.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => format!("{secs}"),
    }
}

fn column_widths(headers: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    widths
}

/// One catalog: the name, then a boxed grid of the column header and body.
fn catalog_table(title: &str, headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut table = Table::new();
    table
        .load_style(ASCII_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(headers)
        .add_rows(rows.iter().cloned());
    format!("{title}\n{table}\n")
}

fn pad_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let widths = column_widths(headers, rows);
    let mut out = String::new();
    push_padded_row(&mut out, headers.iter().copied(), &widths);
    for row in rows {
        push_padded_row(&mut out, row.iter().map(String::as_str), &widths);
    }
    out
}

fn push_padded_row<'a, I>(out: &mut String, cells: I, widths: &[usize])
where
    I: IntoIterator<Item = &'a str>,
{
    for (index, cell) in cells.into_iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(cell);
        if index + 1 < widths.len() {
            let pad = widths[index].saturating_sub(cell.chars().count());
            for _ in 0..pad {
                out.push(' ');
            }
        }
    }
    out.push('\n');
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_catalog_is_a_boxed_table() {
        let text = catalog_table(
            "T",
            &["A", "BB"],
            &[vec!["ccc".to_string(), "d".to_string()]],
        );
        assert_eq!(
            text,
            "T\n\
             +-----+----+\n\
             | A   | BB |\n\
             +==========+\n\
             | ccc | d  |\n\
             +-----+----+\n"
        );
    }

    #[test]
    fn prune_is_one_sentence_and_zero_is_nothing() {
        assert_eq!(
            prune_sentence(0, 0, false),
            "pruned no runs, retaining none."
        );
        assert_eq!(
            prune_sentence(1, 2, false),
            "pruned 1 run, retaining the 2 most recent."
        );
        assert_eq!(
            prune_sentence(3, 0, false),
            "pruned 3 runs, retaining none."
        );
        assert_eq!(
            prune_sentence(0, 2, false),
            "pruned no runs, retaining the 2 most recent."
        );
        assert_eq!(
            prune_sentence(1, 2, true),
            "would prune 1 run, retaining the 2 most recent."
        );
        assert_eq!(
            prune_sentence(0, 0, true),
            "would prune no runs, retaining none."
        );
    }

    /// The line is printed to be pasted, so a word the shell would read as
    /// syntax is quoted and everything else stays as it was typed: a hint the
    /// reader has to edit before it runs is a hint they have to think about.
    #[test]
    fn an_invocation_is_echoed_as_one_line_a_shell_reads_back_the_same_way() {
        let words = [
            "reviewbot",
            "--runs-dir",
            "/tmp/a b/runs",
            "review",
            "https://host/x?a=1&b=2",
        ]
        .map(OsString::from);
        assert_eq!(
            shell_command(&words),
            "reviewbot --runs-dir '/tmp/a b/runs' review 'https://host/x?a=1&b=2'"
        );
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    /// The run id is required of every failure that has one. The command beside
    /// it is the one that was just run, because running it again is what
    /// re-enters the run — and a command that enters no run is not offered
    /// back, since repeating it would continue nothing.
    #[test]
    fn a_failure_in_a_run_prints_the_run_id_and_the_command_that_goes_back_in() {
        let stopped = || Error::InRun {
            run_id: "7f3a9c1e".to_string(),
            source: Box::new(Error::PublishNeedsPlatform),
        };
        let words =
            ["reviewbot", "--runs-dir", "/tmp/runs", "review", "x.diff"].map(OsString::from);

        let text = failure(&Failure::Command {
            error: Box::new(stopped()),
            invocation: Some(words.to_vec()),
        })
        .expect("redacted");
        assert!(
            text.starts_with("error: "),
            "a failure on stderr is the whole of that stream: {text:?}"
        );
        assert!(text.contains("run_id: 7f3a9c1e\n"), "{text}");
        assert!(
            text.contains("next: reviewbot --runs-dir /tmp/runs review x.diff\n"),
            "{text}"
        );
        assert!(text.contains("continues run 7f3a9c1e"), "{text}");

        let elsewhere = failure(&Failure::Command {
            error: Box::new(stopped()),
            invocation: None,
        })
        .expect("redacted");
        assert!(elsewhere.contains("run_id: 7f3a9c1e\n"), "{elsewhere}");
        assert!(!elsewhere.contains("next:"), "{elsewhere}");
    }

    /// The one failure repeating the command cannot get past: the command is
    /// what pointed at the wrong directory. The run id still goes out — it
    /// names the run that is in the way, and what that run holds is in the
    /// sentence — but the replay does not, because it would only fail again.
    #[test]
    fn a_directory_recording_another_input_is_not_offered_the_command_back() {
        let words = [
            "reviewbot",
            "review",
            "--run-id",
            "7f3a9c1e",
            "https://host/x",
        ]
        .map(OsString::from);

        let text = failure(&Failure::Command {
            error: Box::new(Error::DifferentInput {
                run_id: "7f3a9c1e".to_string(),
                recorded: "gitlab.com/acme/app #99".to_string(),
            }),
            invocation: Some(words.to_vec()),
        })
        .expect("redacted");
        assert!(text.starts_with("error: "), "{text:?}");
        assert!(text.contains("records a different input"), "{text}");
        assert!(text.contains("gitlab.com/acme/app #99"), "{text}");
        assert!(text.contains("run_id: 7f3a9c1e\n"), "{text}");
        assert!(!text.contains("next:"), "{text}");
    }

    /// A command line that did not parse arrives as a value like every other
    /// failure, so it is set off the same way. clap's own sentence and the
    /// usage hint under it are kept: they say what to type instead.
    #[test]
    fn a_command_line_that_did_not_parse_takes_the_same_shape() {
        let complaint =
            <super::super::args::Cli as clap::Parser>::try_parse_from(["reviewbot", "review"])
                .expect_err("review needs something to review");

        let text = failure(&Failure::Usage(complaint)).expect("redacted");
        assert!(text.starts_with("error: "), "{text:?}");
        assert!(text.contains("Usage:"), "{text}");
    }

    #[test]
    fn pad_table_aligns_to_the_widest_cell() {
        let text = pad_table(&["A", "BB"], &[vec!["ccc".to_string(), "d".to_string()]]);
        assert_eq!(text, "A    BB\nccc  d\n");
    }

    #[test]
    fn unix_timestamps_print_as_utc() {
        assert_eq!(format_unix_utc(0), "-");
        assert_eq!(format_unix_utc(1_700_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(format_unix_utc(1_788_936_240), "2026-09-09 06:44:00 UTC");
    }

    /// `-1` and `0` are settings, not amounts, and printing them as money
    /// would read as "this provider may spend minus one".
    #[test]
    fn a_budget_of_minus_one_or_zero_reads_as_words() {
        assert_eq!(budget(-1.0, "CNY"), "unlimited");
        assert_eq!(budget(0.0, "CNY"), "none");
        assert_eq!(budget(10.0, "CNY"), "10.00 CNY");
    }

    #[test]
    fn money_puts_the_currency_after_the_amount() {
        assert_eq!(money(3.0, "CNY", 2), "3.00 CNY");
        assert_eq!(money(0.0316, "", 4), "0.0316");
    }

    #[test]
    fn check_fields_align_to_the_credential_label() {
        assert_eq!(
            check_field("Path", "/tmp/c.toml"),
            "Path:       /tmp/c.toml\n"
        );
        assert_eq!(
            check_field("Credential", "readable"),
            "Credential: readable\n"
        );
        assert_eq!(check_field("Tools", 2), "Tools:      2\n");
    }
}
