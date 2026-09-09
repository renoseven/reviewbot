//! Turning results and failures into text or JSON. Every string leaving here
//! has been through the redactor.

use std::path::Path;

use reviewbot::config::{Settings, paths};
use reviewbot::domain::Confidence;
use reviewbot::security::Redactor;
use reviewbot::tool::Purpose;
use reviewbot::{Error, RunResult};

use super::args::Format;

/// stdout for a finished run.
pub fn run_result(result: &RunResult, format: Format) -> String {
    let text = match format {
        Format::Json => serde_json::to_string_pretty(result)
            .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}")),
        Format::Text => text_summary(result),
    };
    Redactor::new().redact(&text)
}

fn text_summary(result: &RunResult) -> String {
    let mut out = String::new();
    out.push_str(&format!("run_id     {}\n", result.run_id));
    out.push_str(&format!("model      {}\n", result.model));
    match (result.overall_score, &result.summary) {
        (Some(score), _) => out.push_str(&format!(
            "overall    {score} / 100  (the model's judgement of what this run found)\n"
        )),
        (None, _) => out.push_str(&format!(
            "overall    not scored  ({})\n",
            result
                .unscored_reason
                .as_deref()
                .unwrap_or("no reason given")
        )),
    }
    let bands = Confidence::ALL
        .iter()
        .map(|band| format!("{band} {}", result.count(*band)))
        .collect::<Vec<_>>()
        .join(" / ");
    out.push_str(&format!(
        "comments   {}  ({bands})\n",
        result.comments.len()
    ));
    out.push_str(&format!("skipped    {} files\n", result.skipped.len()));
    if !result.unreviewed.is_empty() {
        out.push_str(&format!("unreviewed {} files\n", result.unreviewed.len()));
    }
    if let Some(reason) = &result.stopped {
        out.push_str(&format!("stopped    {reason}\n"));
    }
    match result.budget {
        Some(ceiling) => out.push_str(&format!(
            "budget     {:.4} / {:.4} {}\n",
            result.spent, ceiling, result.currency
        )),
        None => out.push_str(&format!(
            "budget     {:.4} {} (no ceiling)\n",
            result.spent, result.currency
        )),
    }
    out.push_str(&format!("report     {}\n", result.report_path.display()));
    out.push_str(&format!("summary    {}\n", result.summary_path.display()));
    if !result.published.is_empty() {
        out.push_str(&format!("published  {} comments\n", result.published.len()));
    }
    out
}

/// stderr for a failure. Carries the run id and a command that can be copied
/// straight back into the shell.
pub fn failure(error: &Error, runs_dir: &Path) -> String {
    let mut out = format!("error: {error}\n");
    if let Some(run_id) = error.run_id() {
        out.push_str(&format!("run_id: {run_id}\n"));
        let runs_flag = if runs_dir == paths::default_runs_dir() {
            String::new()
        } else {
            format!("--runs-dir {} ", runs_dir.display())
        };
        out.push_str(&format!("next: reviewbot {runs_flag}resume {run_id}\n"));
        out.push_str(&format!("      reviewbot {runs_flag}publish {run_id}\n"));
        out.push_str(&format!("      reviewbot {runs_flag}report {run_id}\n"));
    }
    Redactor::new().redact(&out)
}

/// stdout for `config check`.
pub fn config_check(settings: &Settings, format: Format) -> Result<String, Error> {
    let selection = settings.selection()?;
    let text = match format {
        Format::Json => serde_json::json!({
            "config": settings.config_path.display().to_string(),
            "model": selection.model.name,
            "selected_by": selection.reason.as_str(),
            "provider": selection.provider.name,
            "protocol": selection.provider.protocol,
            "currency": selection.provider.currency,
            "budget_per_run": selection.provider.budget_per_run,
            "platforms": settings.config.platforms.len(),
            "tools": settings.config.tools.len(),
            "ok": true,
        })
        .to_string(),
        Format::Text => {
            let mut out = String::new();
            out.push_str(&format!("config     {}\n", settings.config_path.display()));
            out.push_str(&format!(
                "model      {}  (selected by {})\n",
                selection.model.name,
                selection.reason.as_str()
            ));
            out.push_str(&format!(
                "provider   {}  ({}, {} budget {})\n",
                selection.provider.name,
                selection.provider.protocol,
                selection.provider.currency,
                selection.provider.budget_per_run
            ));
            out.push_str(&format!("platforms  {}\n", settings.config.platforms.len()));
            out.push_str(&format!("tools      {}\n", settings.config.tools.len()));
            out.push_str("credential readable, no requests sent\n");
            out
        }
    };
    Ok(Redactor::new().redact(&text))
}

pub fn run_list(runs_dir: &Path, format: Format) -> Result<String, Error> {
    let runs = reviewbot::record::list_runs(runs_dir)?;
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
                return Ok(Redactor::new().redact(&out));
            }
            let rows: Vec<Vec<String>> = runs
                .iter()
                .map(|row| {
                    vec![
                        row.run_id.clone(),
                        truncate(&row.input, 40),
                        row.completed_stages.join(", "),
                        money(row.spent, &row.currency, 4),
                        format_unix_utc(row.updated_at),
                    ]
                })
                .collect();
            out.push_str(&pad_table(
                &["RUN ID", "INPUT", "STAGES", "SPENT", "UPDATED"],
                &rows,
            ));
            out
        }
    };
    Ok(Redactor::new().redact(&text))
}

pub fn run_show(runs_dir: &Path, run_id: &str, format: Format) -> Result<String, Error> {
    let show = reviewbot::record::show_run(runs_dir, run_id)?;
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
                show.completed_stages.join(", ")
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
            out.push_str(&format!("traces     {}\n", show.traces.display()));
            out
        }
    };
    Ok(Redactor::new().redact(&text))
}

pub fn run_prune(report: &reviewbot::record::PruneReport, format: Format) -> String {
    let text = match format {
        Format::Json => serde_json::to_string_pretty(report)
            .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}")),
        Format::Text => {
            let mut out = format!("runs_dir  {}\n", report.runs_dir.display());
            out.push_str(&format!("keep      {}\n", report.keep));
            if report.dry_run {
                out.push_str("dry_run   true (nothing deleted)\n");
            }
            out.push_str(&format!("kept      {}\n", report.kept.join(", ")));
            if report.deleted.is_empty() {
                out.push_str("deleted   (none)\n");
            } else {
                out.push_str(&format!("deleted   {}\n", report.deleted.join(", ")));
            }
            out
        }
    };
    Redactor::new().redact(&text)
}

/// Which hosts this config can review, and where each one's token comes
/// from. The token column is the *source*, never the credential: an inline
/// secret is refused at parse time, so there is nothing here to leak.
pub fn platform_list(settings: &Settings, format: Format) -> Result<String, Error> {
    let platforms: Vec<serde_json::Value> = settings
        .config
        .platforms
        .iter()
        .map(|platform| {
            serde_json::json!({
                "host": platform.host,
                "kind": platform.resolved_kind().map(|kind| kind.as_str()),
                "base_url": platform.base_url,
                "api_token": platform.api_token,
            })
        })
        .collect();
    let text = match format {
        Format::Json => serde_json::json!({ "platforms": platforms }).to_string(),
        Format::Text => {
            let rows: Vec<Vec<String>> = settings
                .config
                .platforms
                .iter()
                .map(|platform| {
                    vec![
                        platform.host.clone(),
                        platform
                            .resolved_kind()
                            .map(|kind| kind.as_str().to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        platform.base_url.clone(),
                        platform.api_token.clone(),
                    ]
                })
                .collect();
            pad_table(&["HOST", "KIND", "BASE URL", "TOKEN FROM"], &rows)
        }
    };
    Ok(Redactor::new().redact(&text))
}

/// Which vendors this config can call, and what each one is allowed to
/// spend. The budget is per run, not per month: `-1` is no ceiling and `0`
/// spends nothing, and both are worth being able to read off a table.
pub fn provider_list(settings: &Settings, format: Format) -> Result<String, Error> {
    let providers: Vec<serde_json::Value> = settings
        .config
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
        .collect();
    let text = match format {
        Format::Json => serde_json::json!({ "providers": providers }).to_string(),
        Format::Text => {
            let rows: Vec<Vec<String>> = settings
                .config
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
            pad_table(
                &["NAME", "PROTOCOL", "BASE URL", "BUDGET/RUN", "KEY FROM"],
                &rows,
            )
        }
    };
    Ok(Redactor::new().redact(&text))
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

pub fn model_list(settings: &Settings, format: Format) -> Result<String, Error> {
    let default_name = settings
        .config
        .models
        .iter()
        .find(|model| model.default)
        .map(|model| model.name.as_str());
    let models: Vec<serde_json::Value> = settings
        .config
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
        .collect();
    let text = match format {
        Format::Json => serde_json::json!({ "models": models }).to_string(),
        Format::Text => {
            let rows: Vec<Vec<String>> = settings
                .config
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
            pad_table(
                &[
                    "NAME",
                    "ALIAS",
                    "PROVIDER",
                    "IN/1M",
                    "CACHED/1M",
                    "OUT/1M",
                    "CONTEXT",
                    "MAX OUT",
                    "DEFAULT",
                ],
                &rows,
            )
        }
    };
    Ok(Redactor::new().redact(&text))
}

/// The contract of every tool this config could offer: what it is for, how it
/// is called, what each argument is, which rounds it appears on, and what has
/// to be true of a run before it exists.
///
/// It does not say whether a tool is registered. Registration is a fact about
/// one invocation of `review` — which worktree it got, which platform, what the
/// config asked for — and this command reviews nothing and calls nothing, so it
/// has no invocation to report on. It printed one anyway, which read as a
/// verdict on the tool itself.
pub fn tool_list(settings: &Settings, format: Format) -> Result<String, Error> {
    let rows = reviewbot::tool::inventory(settings);
    let text = match format {
        Format::Json => serde_json::json!({ "tools": rows }).to_string(),
        Format::Text => {
            let mut out = String::new();
            for purpose in [Purpose::Content, Purpose::Check, Purpose::Delivery] {
                out.push_str(&format!("{}\n\n", purpose.as_str()));
                let group: Vec<_> = rows.iter().filter(|row| row.purpose == purpose).collect();
                if group.is_empty() {
                    out.push_str("(none)\n\n");
                }
                for row in group {
                    out.push_str(&format_tool_row(row));
                }
            }
            out
        }
    };
    Ok(Redactor::new().redact(&text))
}

fn format_tool_row(row: &reviewbot::tool::ToolListing) -> String {
    let mut out = format!("{}({})\n", row.name, call_arguments(&row.parameters));
    out.push_str(&format!("  {}\n", row.description));
    let rounds: Vec<&str> = row.rounds.iter().map(|round| round.as_str()).collect();
    out.push_str(&format!("  rounds    {}\n", rounds.join(", ")));
    if !row.preconditions.is_empty() {
        out.push_str(&format!("  needs     {}\n", row.preconditions.join("; ")));
    }
    let declared = parameters_of(&row.parameters);
    // Padded to the widest name in this tool, so a long one still gets a gap
    // rather than running into its own type.
    let width = declared
        .iter()
        .map(|parameter| parameter.name.chars().count())
        .max()
        .unwrap_or(0);
    for parameter in &declared {
        out.push_str(&format!(
            "  {:<width$}  {}{}\n",
            parameter.name,
            parameter.kind,
            match parameter.description.is_empty() {
                true => String::new(),
                false => format!("  {}", parameter.description),
            }
        ));
    }
    out.push('\n');
    out
}

/// The call as the model writes it: required names, then optionals in brackets.
fn call_arguments(parameters: &serde_json::Value) -> String {
    let declared = parameters_of(parameters);
    let required: Vec<&str> = declared
        .iter()
        .filter(|parameter| parameter.required)
        .map(|parameter| parameter.name.as_str())
        .collect();
    let optional: Vec<&str> = declared
        .iter()
        .filter(|parameter| !parameter.required)
        .map(|parameter| parameter.name.as_str())
        .collect();
    match (required.is_empty(), optional.is_empty()) {
        (true, true) => String::new(),
        (false, true) => required.join(", "),
        (true, false) => format!("[{}]", optional.join(", ")),
        (false, false) => format!("{}, [{}]", required.join(", "), optional.join(", ")),
    }
}

/// One row per declared argument, read back out of the schema the tool
/// published rather than out of a second list kept here.
struct DeclaredParameter {
    name: String,
    kind: String,
    required: bool,
    description: String,
}

fn parameters_of(parameters: &serde_json::Value) -> Vec<DeclaredParameter> {
    let Some(properties) = parameters
        .get("properties")
        .and_then(|value| value.as_object())
    else {
        return Vec::new();
    };
    let required: Vec<&str> = parameters
        .get("required")
        .and_then(|value| value.as_array())
        .map(|items| items.iter().filter_map(|item| item.as_str()).collect())
        .unwrap_or_default();
    let declare = |name: &String| DeclaredParameter {
        name: name.clone(),
        kind: properties
            .get(name)
            .map(describe_kind)
            .unwrap_or_else(|| "value".to_string()),
        required: required.iter().any(|need| need == name),
        description: properties
            .get(name)
            .and_then(|property| property.get("description"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
    };
    // Required ones first, in the order the tool declared them, so the rows
    // read in the same order as the call signature above them.
    let mut rows: Vec<DeclaredParameter> = required
        .iter()
        .filter(|name| properties.contains_key(**name))
        .map(|name| declare(&name.to_string()))
        .collect();
    rows.extend(
        properties
            .keys()
            .filter(|name| !required.iter().any(|need| need == *name))
            .map(declare),
    );
    rows
}

fn describe_kind(property: &serde_json::Value) -> String {
    let kind = property
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("value");
    let inner = match kind {
        "array" => property
            .get("items")
            .map(|items| format!(" of {}", describe_kind(items))),
        _ => None,
    };
    let bounds = match (property.get("minimum"), property.get("maximum")) {
        (Some(low), Some(high)) => Some(format!(" {low}-{high}")),
        (Some(low), None) => Some(format!(" {low} or more")),
        _ => None,
    };
    format!(
        "{kind}{}{}",
        inner.unwrap_or_default(),
        bounds.unwrap_or_default()
    )
}

fn currency_of<'a>(settings: &'a Settings, provider: &str) -> &'a str {
    settings
        .config
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
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Civil date from days since Unix epoch. Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

fn pad_table(headers: &[&str], rows: &[Vec<String>]) -> String {
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
    use reviewbot::tool::{Round, ToolListing};
    use serde_json::json;

    /// The row is the tool's contract: how it is called, what each argument
    /// is, which rounds offer it, and what a run needs before it exists.
    /// Never whether it is registered — that is a fact about one review, and
    /// this command reviews nothing.
    #[test]
    fn a_tool_row_prints_the_contract_rather_than_a_registration_verdict() {
        let row = ToolListing {
            name: "search_code".to_string(),
            purpose: Purpose::Content,
            description: "Search the code of this run's worktree.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What to search for"},
                    "glob": {"type": "string"},
                },
                "required": ["query"],
            }),
            rounds: vec![Round::Investigation],
            preconditions: vec!["the worktree can answer a search".to_string()],
        };
        let text = format_tool_row(&row);
        assert!(text.starts_with("search_code(query, [glob])"), "{text}");
        assert!(
            text.contains("Search the code of this run's worktree."),
            "{text}"
        );
        assert!(text.contains("rounds    investigation"), "{text}");
        assert!(
            text.contains("needs     the worktree can answer a search"),
            "{text}"
        );
        assert!(text.contains("query"), "{text}");
        assert!(text.contains("What to search for"), "{text}");
        assert!(!text.contains("registered"), "{text}");
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
}
