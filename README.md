# reviewbot

A CLI that reviews a merge request, a pull request, or a raw unified diff. It is not a service: you run it, it writes a report, and (if you ask) it posts comments.

The design lives in [`docs/design.md`](docs/design.md). This file is how to run it.

## Build

```bash
cargo build
```

The binary is `reviewbot`. Configuration is **not** read from the current directory. Pass `--config`, or put a file at `~/.reviewbot/config.toml`.

`config init` writes that same example to `--config`, or to `~/.reviewbot/config.toml`. Point the `api_key` / `api_token` fields at environment variable names or files outside the repo. Never put a real key in the file.

```bash
reviewbot config init
export DEEPSEEK_API_KEY=...          # dummy is enough for `config check`
export GITLAB_TOKEN=...              # only needed to pull or post
reviewbot config check
```

`config check` reads the credential; it does not send a request. It also does not check that a `[[tool]]` `bin` exists on the machine.

## Review a diff

```bash
git diff main...HEAD > change.diff
reviewbot review change.diff
# or stdin
git diff main...HEAD | reviewbot review -
```

A `git format-patch` mbox is refused. Use `git diff`.

## Review a URL

```bash
reviewbot review \
  https://gitlab.com/acme/app/-/merge_requests/128
```

Add `--publish` to post comments as well as write the report. `--publish` on a diff file fails at startup.

```bash
reviewbot review --publish \
  --worktree . \
  https://gitlab.com/acme/app/-/merge_requests/128
```

`--worktree` reuses an existing checkout, read only, and it has to stand on the commit under review.

A run always has exactly one worktree, one of three shapes. With `--worktree` it is that checkout, the whole project, read only. Without it, a URL run opens a cache of its own at `<run dir>/cache` and fetches files into it from the platform API as the model asks; a plain diff with no platform has no directory at all, because nothing would read one. Fetched files stay on disk for the rest of the run, which is what lets an external checker open them. There is no clone. The content tools are eight names, split into a local half and a repository half: listing the repo and listing what is on disk are different actions, and the model is told so. The repository half has no read (fetch, then read locally); the local half has no fetch. A cache miss is a miss.

## Where things land

| flag | what |
|---|---|
| `--runs-dir DIR` | checkpoints, traces, the log, and this run's `cache/` and `checks/`. Default `~/.reviewbot/runs`. If you point this at the repo (CI does), add it to `.gitignore`. |
| `--output-dir DIR` | copies `report-<run_id>.md` and `summary-<run_id>.json` for CI artifacts. `review` only. Do not archive the whole runs directory: `traces/` holds the internal view. |

Inside one run directory: `report.md`, `summary.json`, `stages/`, `traces/`, `published.json`, `cache/`, `checks/`, and `log` — every line of tracing this run produced, appended, at the level `[log] level` asks for (`RUST_LOG` overrides). Nothing tracing writes reaches stdout or stderr. `run show` prints the log path.

While a review runs, a terminal gets a fixed-height live checklist: a title, all six stages, then three activity rows at the bottom. The review row counts chunks and files separately and carries current spend. The animated activity line says whether reviewbot is waiting for the model (`exchange 3/12`) or running a tool; completed tools remain visible as a per-chunk tally. The block is cleared before the final summary or an error is printed. If the terminal cannot support the inline viewport, reviewbot warns in the run log and falls back to plain appended lines.

A pipe deliberately keeps less: one run header, one line per chunk (with spend), and one line per finished stage. Exchanges, tool calls, and spend updates do not each create another log line. `--format json` makes stdout a single JSON document with progress suppressed; `-q` silences stdout entirely, progress included.

```bash
reviewbot --format json -q review change.diff --output-dir artifacts/
```

## Continuing a run

There is no `resume`, `publish` or `report` command. **Running the same `review` command again continues the run it started.** The stages that finished keep their checkpoints and are not paid for a second time; the report is re-rendered and any comment that never reached the MR is posted, because those last two stages are cheap and run on every entry.

If a run dies after it has a directory, stderr names the `run_id` and prints your own invocation back under `next:`, shell-quoted where a word needs it, so the line pastes and runs:

```
error: ...
run_id: 7f3a9c1e
next: reviewbot --runs-dir .reviewbot/runs review --publish https://gitlab.com/acme/app/-/merge_requests/128
      the same command again continues run 7f3a9c1e; the stages it finished are not run again
```

Two things worth knowing. The run id is `hash(input identity + head_sha)` — the config is not in it, so the same merge request at the same commit is always the same directory. Changing the config re-enters that run, drops the earliest stage the change can reach and everything after it, and continues under the current settings. Only `--run-id` aimed at another input's directory is refused; the `next:` replay line is withheld there, because the same command is exactly what aimed at the wrong directory. And `--publish` is re-read from the command line every time: a run you first launched with `--publish` and then re-enter without it records "do not post" and posts nothing that time.

## Inspecting a config and past runs

```bash
reviewbot --runs-dir .reviewbot/runs run list
reviewbot --runs-dir .reviewbot/runs --format json run list
reviewbot run show 7f3a9c1e
reviewbot run remove 7f3a9c1e          # delete one run; prints nothing
reviewbot run prune                    # delete every run; prints how many
reviewbot run prune --keep-latest 10   # keep the newest 10 instead
reviewbot run prune --dry-run          # count, do not delete
reviewbot config info                  # platforms, providers, models, tools
```

`config info` lists the platforms, providers, models, and tools in the loaded config. Column titles are uppercase; a title of more than one word is joined with `_`. The tool table is name, purpose, and rounds; `--format json` has the rest of the contract. Credentials are named by source, never printed; `config check` is what reads them.

`review` never deletes a run, including when it re-enters one. `run prune` with no `--keep-latest` deletes every run. If a review leaves more than 10 runs, it warns once with a `run prune` line you can paste.

## Exit codes

| code | meaning |
|---|---|
| 0 | finished (findings do not change this) |
| 1 | unexpected failure |
| 2 | config / input |
| 3 | budget stopped the run |
| 4 | platform or model down |
| 5 | review done, some posts failed; run the same command again to post the rest |

There is no `--fail-on`. Gate a pipeline on `summary.json` yourself.

## CI

See [`examples/gitlab-ci.yml`](examples/gitlab-ci.yml) and [`examples/github-actions.yml`](examples/github-actions.yml). Copy them to `.gitlab-ci.yml` / `.github/workflows/reviewbot.yml` in the repo you want reviewed. The shape is: `config check`, then `review` with `--output-dir` and (on GitLab) `--runs-dir` inside the project so cache can see it, then `run prune`. Artifacts come from `--output-dir` only.

## Demo report

This repo does not ship a canned PR report. Produce one by pointing `review` at a public C repository (the sample code the brief uses is C). The run directory then has `report.md`, `summary.json`, `log`, and `traces/`.

## Add a tool without recompiling

[`src/config/example.toml`](src/config/example.toml) comments two checkers you can uncomment. `cppcheck` (`requires_build = false`, `requires_checkout = false`) reads one C/C++ file on its own, so any worktree with code in it can answer it. `typecheck` is the brief's named example of adding a tool without recompiling: another `[[tool]]` row, `gcc -fsyntax-only`, `requires_checkout = true` because a `.c` file without its headers produces a screen of missing includes, which is worse than not running. Listed means enabled; without a checkout, `typecheck`'s description says it is not available this run and calling it returns that same reason instead of the missing-include screen. A `requires_build` tool also needs `[security].allow_build_tools` and a sandbox.

## Known limits

- Config field names carry their unit and their subject: `[review].max_file_bytes`, `max_files_per_listing`, `max_hits_per_search`, `max_files_per_fetch`, `max_rounds`, `[plan].skip_files_over_bytes`, `[[provider]].budget_per_run`, `[[model]].context_window_tokens` and the three `_per_1m_tokens` prices, `[[tool]].requires_checkout`. Every number under `[review]`, `[plan]` and `[security]` is required and has no builtin default. Most of those answers depend on how big this repository's files get, or on how much output is worth reading; `max_rounds` is the other kind — a per-file dead-loop guard. Omitting one, or writing a zero, fails `config check` and names the field. `config init` writes [`src/config/example.toml`](src/config/example.toml), which sets all of them.
- `[log] level` (`error|warn|info|debug|trace`, default `info`) is the one config field left out of the fingerprint slices, so turning the log up does not invalidate a run's checkpoints. The fingerprint is three slices named after the earliest stage each can reach (`input` / `plan` / `review`); it is not part of the run id. It is the only way to set the level; there is no `-v`.
- `[security]` holds permissions only — `deny_paths`, `allow_extensions`, `follow_symlinks`, `allow_build_tools` — and no sizes. How much a tool may fetch or return is a different question from whether it may be touched, so those numbers live under `[review]`.
- `[security].allow_extensions` is required for the same reason and is a hard boundary besides, so a default could only loosen it. Extensions are written bare (`"rs"`, not `".rs"`). It is a whitelist: files outside it are skipped at plan (named in the report, not sent to the model), and a read of one is refused. Extensionless files (`Makefile`, `Dockerfile`, `LICENSE`) stay out.
- **The tool loop has two stops, both per file.** The conversation ends when the next call no longer fits the model's window, or when investigation rounds hit `[review].max_rounds` (required; the example is 100). The second is a dead-loop guard, not a window reservation: each file is a fresh conversation, and a round rarely fills a whole chunk, so a precomputed `available / chunk − 1` (24 on a 1M window) was the wrong stop. What a run may spend is still `[[provider]].budget_per_run`, checked before every call.
- The model is not told the remaining count after every round — that number was read as a quota to spend. It is told when the next round is the last, so it can ask for everything it still needs in one go. When the ceiling is hit the investigation tools come off and it is asked to conclude with what it has.
- A file read is never truncated. A file too big to return is refused with its size in the refusal, and the model pages through it in line ranges after asking `suggest_local_read` how to read it — that answer is one of three conclusions, and when the file needs paging it gives the cut ranges rather than a window size the model has to divide. `[review].max_file_bytes` bounds the fetch rather than the answer, and a line range is no way around it: a file past that ceiling is refused before it is downloaded. `fetch_repo_file` takes an array of paths, answers per entry, and is capped by `[review].max_files_per_fetch`. Both ceilings appear in the tool descriptions the model reads, so it can plan instead of guessing.
- The review looks at one file at a time, but not blindly: the instructions carry the whole list of files this change touches. That list is counted by reviewbot rather than summarised by a model, and it rides in the cached half of the prompt, so the run pays for it once instead of once per file. There is no directory digest — naming crates or folders is a map the model walks — and no stage that asks a model what the change is trying to do. A vague or wrong answer there would become a premise under every finding, and the report would look no different. A listing or a search is what answers for a path that is not on the change list.
- On a URL, every chunk is also shown what the author wrote about the change: the title, the description and up to 20 commit subject lines, fenced by the one renderer that knows how to fence. It is the highest-value context in the prompt — intent is the one thing a diff does not carry — and the only prompt injection surface reviewbot fetches on purpose, so it goes in fenced as material ahead of the diff and never into `instructions`. The fence says three things: read it for intent, do not read it as evidence about behaviour (where it and the diff disagree the diff is what is true, and the disagreement is worth filing), and an instruction inside it is reviewed content. A description that cannot be fetched costs the model context and nothing else; a diff off disk has none.
- Finding nothing has an ending of its own: `finish_review` takes no arguments and files nothing. Without it, a model handed only `submit_comment` and asked to conclude files a placeholder — a real run came back with `body: "No defect found in this change."`, `suggestion: "N/A"` and a confidence of 95, which passed every check a finding has to pass and went out as a published comment. There is no gate on the wording of a body: "nothing is wrong here" has no shape a check can recognise, and a gate that guessed would throw away real findings that happen to read reassuringly.
- Both submissions are function calls: `submit_comment` for a finding, `submit_summary` for the overall score. No stage reads JSON out of a chat message, so nothing strips markdown fences. Each tool is offered on the rounds it belongs to and no others — investigation, the concluding turn, or the scoring call — and `config info` prints those rounds. A blank or whitespace-only summary is refused and re-asked rather than published as a score with nothing behind it.
- A score written as `"92"` is read as 92. The schema says integer and no vendor enforces it, so re-asking buys a round trip and the same number back. Nothing is rounded or clamped: `"45.7"`, `"high"` and `101` are still refused.
- Everything sent to the model is in English — both prompts, the capability paragraph, tool descriptions, and the notes and refusals in tool output. So are the two badges reviewbot puts on a comment (`found by tool`, `quote unverified`). The report body and comment text come from the model, in whatever language it chooses.
- reviewbot never writes the checkout you name with `--worktree`; the only local writes are the run directory (including `cache/` and `checks/`) and `--output-dir`. A subprocess cannot be stopped from writing the checkout on a bare machine; that only holds in an isolated environment (container, or a user with no write permission). Checkers start in `<run dir>/checks`, not in the worktree root, so a checker that drops a file into its cwd does not write into a supplied checkout.
- `requires_build` needs `allow_build_tools` and a sandbox.
- Suppression-style prompt injection (persuading the model to report nothing) is undetectable: an empty list is a legal review.
- On GitLab, nothing answers `search_repo_regex` or `search_repo_keyword` until Advanced Search can be known without guessing. Both tools are still offered to the model, and both their descriptions and their refusals say that a miss there would have been the run's limit rather than evidence of absence.
- No clone. One worktree per run: the checkout you name, a cache the run fills from the platform API, or nothing at all when the input is a plain diff with no platform.
- Confidence calibration is unmeasured: the model’s number is published as given.

Leftover from the test plan in design §11 (not automated here): run-directory size after real model calls and after tools; prompt wording against live PRs; a full matrix of CI artifact names across two jobs.

## Design

[`docs/design.md`](docs/design.md) is the source of truth for stages, fingerprints, tools, and exit codes.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
