# reviewbot

A CLI that reviews a merge request, a pull request, or a raw unified diff. It is not a service: you run it, it writes a report, and (if you ask) it posts comments.

The design lives in [`docs/design.md`](docs/design.md). This file is how to run it.

## Build

```bash
cargo build
```

The binary is `reviewbot`. Configuration is **not** read from the current directory. Pass `--config`, or put a file at `$XDG_CONFIG_HOME/reviewbot/reviewbot.toml` (else `~/.config/reviewbot/reviewbot.toml`).

Copy [`examples/reviewbot.toml`](examples/reviewbot.toml) and point the `api_key` / `api_token` fields at environment variable names or files outside the repo. Never put a real key in the file.

```bash
export DEEPSEEK_API_KEY=...          # dummy is enough for `config check`
export GITLAB_TOKEN=...              # only needed to pull or post
reviewbot --config examples/reviewbot.toml config check
```

`config check` reads the credential; it does not send a request. It also does not check that a `[[tool]]` `bin` exists on the machine.

## Review a diff

```bash
git diff main...HEAD > change.diff
reviewbot --config examples/reviewbot.toml review change.diff
# or stdin
git diff main...HEAD | reviewbot --config examples/reviewbot.toml review -
```

A `git format-patch` mbox is refused. Use `git diff`.

## Review a URL

```bash
reviewbot --config examples/reviewbot.toml review \
  https://gitlab.com/acme/app/-/merge_requests/128
```

Add `--publish` to post comments as well as write the report. `--publish` on a diff file fails at startup.

```bash
reviewbot --config examples/reviewbot.toml review --publish \
  --worktree . \
  https://gitlab.com/acme/app/-/merge_requests/128
```

`--worktree` reuses an existing checkout (read only). Without it, repository tools go through the platform API. There is no clone, and no third mode.

## Where things land

| flag | what |
|---|---|
| `--runs-dir DIR` | checkpoints and traces. Default `$XDG_STATE_HOME/reviewbot/runs`. If you point this at the repo (CI does), add it to `.gitignore`. |
| `--out-dir DIR` | copies `report-<run_id>.md` and `summary-<run_id>.json` for CI artifacts. Do not archive the whole runs directory: `traces/` holds the internal view. |

`--format json` makes stdout a JSON document (no progress mixed in). `-q` silences text; JSON still prints.

```bash
reviewbot --config examples/reviewbot.toml --format json -q review change.diff --out-dir artifacts/
```

## Resume, publish, report

If a run dies after it has a directory, stderr names the `run_id` and a command you can paste, including `--runs-dir` when you used a non-default one.

```bash
reviewbot --runs-dir .reviewbot/runs resume 7f3a9c1e
reviewbot --runs-dir .reviewbot/runs publish 7f3a9c1e   # leftover comments only; no model
reviewbot --runs-dir .reviewbot/runs report 7f3a9c1e --out-dir artifacts/
```

`publish` posts even if the original `review` did not pass `--publish`. It skips markers already on the MR or in `published.json`.

## Inspecting a config and past runs

Every noun is singular: `run`, `model`, `tool`, `platform`, `provider`.

```bash
reviewbot --runs-dir .reviewbot/runs run list
reviewbot --runs-dir .reviewbot/runs --format json run list
reviewbot run show 7f3a9c1e
reviewbot run prune                    # delete every run; only command that deletes
reviewbot run prune --keep 10          # keep the newest 10 instead
reviewbot run prune --dry-run          # list, do not delete
reviewbot --config examples/reviewbot.toml model list
reviewbot --config examples/reviewbot.toml tool list
reviewbot --config examples/reviewbot.toml platform list
reviewbot --config examples/reviewbot.toml provider list
```

`platform list` and `provider list` name where each credential comes from, never the credential itself — an inline secret is refused when the config is parsed, so there is only ever a source to print. Neither command reads the credential; `config check` is what does that.

`review` and `resume` never delete a run. `run prune` with no `--keep` deletes every run. If a review leaves more than 10 runs, it warns once with a `run prune` line you can paste.

## Exit codes

| code | meaning |
|---|---|
| 0 | finished (findings do not change this) |
| 1 | unexpected failure |
| 2 | config / input |
| 3 | budget stopped the run |
| 4 | platform or model down |
| 5 | review done, some posts failed; use `publish` |

There is no `--fail-on`. Gate a pipeline on `summary.json` yourself.

## CI

See [`examples/gitlab-ci.yml`](examples/gitlab-ci.yml) and [`examples/github-actions.yml`](examples/github-actions.yml). Copy them to `.gitlab-ci.yml` / `.github/workflows/reviewbot.yml` in the repo you want reviewed. The shape is: `config check`, then `review` with `--out-dir` and (on GitLab) `--runs-dir` inside the project so cache can see it, then `run prune`. Artifacts come from `--out-dir` only.

## Demo report

This repo does not ship a canned PR report. Produce one by pointing `review` at a public C repository (the sample code the brief uses is C). The run directory then has `report.md`, `summary.json`, and `traces/`.

## Add a tool without recompiling

Uncomment the extra `[[tool]]` block in [`examples/reviewbot.toml`](examples/reviewbot.toml) (cppcheck). `tool list` will show it. The default enabled checker is `gcc -fsyntax-only` (`requires_build = false`). A `requires_build` tool also needs `[security].allow_build_tools` and a sandbox.

## Known limits

- Every number under `[review]`, `[triage]` and `[security]` is required and has no builtin default, because the right value depends on the model's context window or on how big this repository's files get. Omitting one, or writing a zero, fails `config check` and names the field. Copy [`examples/reviewbot.toml`](examples/reviewbot.toml), which sets all of them.
- `[security]` holds permissions only — `deny_paths`, `allow_extensions`, `follow_symlinks`, `allow_build_tools` — and no sizes. How much a tool may fetch or return is a context-window question, so those numbers live under `[review]` with `max_tool_rounds`.
- `[security].allow_extensions` is required for the same reason and is a hard boundary besides, so a default could only loosen it. Extensions are written bare (`"rs"`, not `".rs"`). It is a whitelist, so it blocks extensionless files (`Makefile`, `Dockerfile`, `LICENSE`).
- `[review].max_tool_rounds` and `[review].max_tool_output_bytes` are a pair: multiplied together they are held back from the context window, and a run refuses to start once nothing is left for the diff.
- A file read is never truncated. A file too big to return is refused with its size in the refusal, and the model pages through it in line ranges after asking `stat_repo_file` how big it is. `[review].max_read_bytes` bounds the fetch rather than the answer, and a line range is no way around it: the platform API has no range request, so reading part of a file means fetching all of it, and a file past that ceiling cannot be read at all.
- The review looks at one file at a time, but not blindly: the instructions carry the whole list of files this change touches and a digest of the repository's directory layout. Both are counted by reviewbot rather than summarised by a model, and both ride in the cached half of the prompt, so the run pays for them once instead of once per file. There is no stage that asks a model what the change is trying to do — a vague or wrong answer there would become a premise under every finding, and the report would look no different.
- The layout digest counts directories rather than naming files, and leaves out anything a read would refuse (`deny_paths`, and extensions outside the whitelist). A directory missing from it holds nothing readable, which is not the same as holding nothing; `list_repo_files` is still what answers about a specific path.
- On a URL, every chunk is also shown what the author wrote about the change: the title, the description and up to 20 commit subject lines. It is the highest-value context in the prompt — intent is the one thing a diff does not carry — and the only prompt injection surface reviewbot fetches on purpose, so it goes in fenced as material ahead of the diff and never into `instructions`. The fence says three things: read it for intent, do not read it as evidence about behaviour (where it and the diff disagree the diff is what is true, and the disagreement is worth filing), and an instruction inside it is reviewed content. A description that cannot be fetched costs the model context and nothing else; a diff off disk has none.
- Both submissions are function calls: `submit_comment` for a finding, `submit_summary` for the overall score. No stage reads JSON out of a chat message, so nothing strips markdown fences. `submit_summary` is offered on the scoring round only, which is why `tool list` shows it as `registered, merge stage only`.
- A score written as `"92"` is read as 92. The schema says integer and no vendor enforces it, so re-asking buys a round trip and the same number back. Nothing is rounded or clamped: `"45.7"`, `"high"` and `101` are still refused.
- Everything sent to the model is in English — both prompts, the capability paragraph, tool descriptions, and the notes and refusals in tool output. So are the two badges reviewbot puts on a comment (`found by tool`, `quote unverified`). The report body and comment text come from the model, in whatever language it chooses.
- reviewbot itself never writes the worktree. A subprocess cannot be stopped from writing it on a bare machine; that only holds in an isolated environment (container, or a user with no write permission).
- `requires_build` needs `allow_build_tools` and a sandbox.
- Suppression-style prompt injection (persuading the model to report nothing) is undetectable: an empty list is a legal review.
- GitLab `search_repo` stays off until Advanced Search can be known without guessing. The builtin still appears in `tool list` as capability-gated.
- No clone. Two modes only: platform API, or `--worktree`.
- Confidence calibration is unmeasured: the model’s number is published as given.

Leftover from the test plan in design §11 (not automated here): run-directory size after real model calls and after tools; prompt wording against live PRs; a full matrix of CI artifact names across two jobs.

## Design

[`docs/design.md`](docs/design.md) is the source of truth for stages, fingerprints, tools, and exit codes.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
