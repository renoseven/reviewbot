# reviewbot 详细设计

Code Review Agent：输入 GitLab MR / GitHub PR / 原始 `git diff`，输出带 trace 的 review comments。生产形态是 CI 流水线里的一步。

本文描述现行实现。取舍记录见 [设计讨论](conversations/002-reviewbot-design.md)。

| 节 | 内容 |
|---|---|
| [术语](#术语) | 后文用到的词 |
| [1. 范围](#1-范围) | 做什么、不做什么 |
| [2. 主流程与模块划分](#2-主流程与模块划分) | 六阶段、三层、扩展点 |
| [3. 一次完整的 run](#3-一次完整的-run) | 按时间顺序 |
| [4. 核心数据模型](#4-核心数据模型) | ChangeSet、Comment、Trace、RunResult |
| [5. 配置](#5-配置) | reviewbot.toml、命令行、密钥 |
| [6. Run 与落盘](#6-run-与落盘) | run_id、目录、锁、checkpoint、重入 |
| [7. 阶段](#7-阶段) | input、plan、review、merge、report、publish |
| [8. 适配器](#8-适配器) | platform、worktree、protocol、tool |
| [9. 预算、安全、重试](#9-预算安全重试) | 花费闸、路径与子进程、瞬时故障 |
| [10. 库与 CLI](#10-库与-cli) | 命令、flag、输出、退出码 |
| [11. 测试](#11-测试) | 五档 |

## 术语

| 词 | 含义 |
|---|---|
| 变更 / `ChangeSet` | 一次评审对象的规范化结果：文件、diff、两个行集合、定位用的 SHA、作者自述 |
| 可评论行 | 平台允许挂评论的行：新增行 + 上下文行 |
| 变更行 | 这次改动的行：新增行 + 纯删除处紧邻的那行。范围校验用它，对齐用可评论行 |
| 分片 / chunk | `review` 一次送给模型的单位。一个分片就是一个文件；单文件超限才按 hunk 再切 |
| `Comment` | 可发布的一条意见：`target` / `body` / `suggestion` / `severity` / `severity_score` / `confidence` / `confidence_score` / `trace_id` |
| `severity_score` / `confidence_score` | 模型给的两个 0–100 整数，原样发布 |
| `severity` / `confidence` | 分数落进区间后的档位别名，由 `merge` 算出 |
| 工具扫出 | 引文核对通过后贴的标签，字面是 `found by tool`；未通过是 `quote unverified`。不改分数 |
| `Trace` | 一条意见怎么来的。落盘是 internal 视图；published 视图去掉文件正文 |
| run | 一次 `review` 的执行。身份是 `run_id`，状态落在 run 目录。同一条命令再跑一遍进同一个 run |
| `run_id` | `hash(输入标识 + head_sha)` 的前 16 位小写十六进制。配置不在里面 |
| 配置指纹 | 解析后的配置加上会影响结论的命令行参数，切成 `input` / `plan` / `review` 三片 |
| checkpoint | `stages/<stage>.json`。原子写入（临时文件 + rename） |
| provider / model / protocol | 配置三层：厂商账号与预算 → 模型条目与单价 → 请求线协议 |
| `Repo` | `Platform::repo()` 交出的 `{ source, capabilities }` |
| `Worktree` | 这次 run 读代码的地方，一个 enum：`Empty` / `Local` / `Cache` |

## 1. 范围

**做**：拉取变更 → 用受控 tool 补上下文 → 调模型 → 分级 → 出 Markdown 报告；给了 `--publish` 再把结论和 `trace_id` 发回 MR/PR。

**不做**：结对编程、改用户代码、自动合并 MR、常驻服务、自己克隆仓库、把评审结论写进退出码。只在 Linux（含 WSL）上编和跑。

两个去处共用同一套 comment/trace：本地 Markdown 报告（总是生成），以及 MR/PR 回帖（`--publish` 才发）。

## 2. 主流程与模块划分

```
input → plan → review ⇄ tools → merge → report → publish
```

六个阶段固定，顺序只写在 `lib.rs` 的 `review()` 里。阶段互不依赖，谁也不知道自己前后是谁。每个阶段结束写一份 checkpoint。

`report` 与 `publish` 每次进入都重跑。前四个阶段有还有效的 checkpoint 就跳过。

分三层，依赖单向向下：阶段 → 适配器 → 设施 → `domain`。同层之间只有 `worktree` 认 `platform` 的 `RepoSource`。

**阶段**

| 模块 | 职责 |
|---|---|
| `stage::input` | MR/PR URL 或原始 diff → `ChangeSet`。取数据委托给 `platform` |
| `stage::plan` | 过滤、按变更行数排序、一文件一分片（超限按 hunk 切） |
| `stage::review` | 拼 prompt、调模型、跑工具循环，按分片产出 `{"comments":[...]}` |
| `stage::merge` | 解析、剔越界、行号对齐、核对引文、去重、定序、汇总打分 |
| `stage::report` | 渲染 `report.md`、`summary.json` 与 `--output-dir` 的两份拷贝。不出网 |
| `stage::publish` | 给了 `--publish` 才把 comment 发回 MR/PR |

`stage::orient` 不是第七个阶段。`Preamble::assemble()` 用它生成改动清单，填进 `{{change}}`。

没有编排模块。`run_id`、run 目录、目录锁、`meta.json`、checkpoint 在 `record`；配置指纹在 `config`；顺序在 `review()`。

**适配器**（trait + 内置实现）。扩展点是其中三个：平台、协议、tool。`worktree` 不是扩展点。

| 模块 | 职责 |
|---|---|
| `platform` | GitLab / GitHub：URL 解析、host 匹配、鉴权、拉 diff、发评论、查已有评论；`Platform::repo()` 交出 `Repo` |
| `worktree` | 这次 run 的 `Worktree`：本地半读磁盘，仓库半问平台 |
| `protocol` | `Request` / `Response`；按 `protocol` 值选实现。厂商 JSON 停在这一层 |
| `tool` | Tool trait + registry + 通用外部命令 + 八个内容工具 + 三个提交工具 |

**设施**

| 模块 | 职责 |
|---|---|
| `domain` | `ChangeSet`、`Comment`、`Severity`、`Confidence`、`Stage`。不依赖任何模块，不含逻辑 |
| `config` | 解析校验 TOML，解析 `model` → `provider` → `protocol`，算指纹 |
| `security` | 脱敏、路径校验、子进程环境清洗、输出截断。只提供检查，调用点在 `tool` |
| `budget` | 冻结、调用前估算、usage 累计与结算 |
| `record` | `run_id`、run 目录、锁、`meta.json`、checkpoint、`Trace` 两视图。存储走 trait，当前实现是本地文件系统 |
| `common` | HTTP 客户端与退避、密钥来源、截断 |
| `progress` | 类型化事件通道。调用方决定谁在看 |

`Request` / `Response` 归 `protocol`，`Trace` 归 `record`，`RunResult` 归 `lib.rs`。

## 3. 一次完整的 run

**启动。** `main.rs` 解析命令行，`config` 读 `reviewbot.toml`。校验失败即退出，不带着半份配置往下走。先建 platform / protocol / redactor。`Opening::identify` 解析出 `head_sha`（检出那一支走 `Checkout::head`，此时还没有 worktree 实例）。`bind_repo` 之后算出 `run_id`。在 runs 目录下建（或打开）那个目录并立刻拿目录锁，然后才看 `meta.json`：

- 输入身份或 `head_sha` 对不上 → 失败（退出码 2）
- 对得上再逐片比对指纹，最早对不上的那一片对应的阶段及其之后作废，删掉那些阶段的 checkpoint，用当前配置接着跑
- 没有 `meta.json`（或解析失败）→ 当作新 run，冻结预算写入 `meta.json`

拿到锁之后才建 `Worktree` 和工具。

**`input`。** 平台按 URL 的 host 匹配 `[[platform]]`，取 MR 元信息、diff 和定位 SHA。规范化成 `ChangeSet`，为每个文件建可评论行与变更行。原始 diff 输入走同一个出口，跳过 `platform`，`Narrative` 为空。

**`plan`。** 过滤噪声、按变更行数降序、一文件一分片。分片上限扣掉已装配好的 preamble（instructions + 调查工具 schema + 自述）。被跳过的文件记进清单。

**`review`。** 每个分片一轮对话。意见经 `submit_comment` 收下，结束信号是 `finish_review`。每看完一个分片写一次进行中的 checkpoint；再进来从下一个未完成的分片接着跑。

**`merge`。** 七步收成可发布列表，再调一次模型要总体评分。

**`report`。** 只读 checkpoint，写 `report.md` 与 `summary.json`。每次进入都重跑。

**`publish`。** 只有 `meta.publish == true` 才发帖。发之前拉已有评论，命中标记就跳过。每次进入都重跑，不看自己的 checkpoint 决定该不该发。

贯穿全程：`record` 在阶段边界原子落盘；`budget` 在每次模型调用前后各动一次；`security` 在数据往外走时脱敏，在执行往里进时限制路径与子进程。

## 4. 核心数据模型

### ChangeSet

`input` 的出口。后面的阶段认它，不认 URL 或原始 diff。

| 字段 | 含义 |
|---|---|
| `locator` | URL 输入：`host` / `project` / `number` / `head_sha`；GitLab 另有 `base_sha` / `start_sha`。diff 输入：给了 `--worktree` 才有检出 HEAD |
| `files` | 每个文件：`old_path` / `new_path`、`hunks`、`commentable_lines`、`changed_lines`、`binary` |
| `narrative` | `title`、`description`、`commits`（主题行，最多 20 条）、`more_commits`。diff 输入为空 |

`Hunk.text` 含 `@@` 头，可以拼回一段合法 unified diff。

两个行集合在 `input` 建好并随 `ChangeSet` 落盘。

### Comment、严重程度与置信度

`Comment` 缺任一字段不得发出：`target`（`path` + 可选 `start_line` / `end_line`）、`body`、`suggestion`、`severity`、`severity_score`、`confidence`、`confidence_score`、`trace_id`。

`body` 只写问题。`suggestion` 只写改法。`overall_score` / `summary` 不是 comment 的字段。

分数是模型给的 0–100 整数，原样保留。档位由 `merge` 按同一套端点算出：

| 分数 | `severity` | `confidence` |
|---|---|---|
| 90–100 | `critical` | `certain` |
| 70–89 | `major` | `high` |
| 40–69 | `minor` | `medium` |
| 0–39 | `trivial` | `low` |

区间写在 prompt 里，不可配置。reviewbot 不改分数。引文核对只贴 `found by tool` 或 `quote unverified`。

### Trace

同一 `trace_id` 两个视图。落盘是 internal。`run trace` 读 internal。

| 字段 | internal | published |
|---|---|---|
| tools | 名称、输入、输出、耗时、成败 | 输入/输出截断到 `max_tool_output_bytes` |
| diff | 本次 changeset 完整 diff | 同左 |
| 其他文件 | 完整正文 | 只有路径 + 行区间 |
| prompt | 脱敏后完整文本 | 嵌入的上下文正文换成占位 |
| 模型输出 | 原始 | 原始 |
| reasoning | 有 | 无 |
| 后处理 | `checks[]`：`stage` + `note` | 同左 |
| usage | tokens、单价、花费 | 同左 |

`trace_id` 与 `run_id` 同形（16 位小写十六进制）：

- 整文件：`hex_id("review:{path}")`
- 第 n 片（1-based）：`hex_id("review:{path}:{n}")`
- 汇总打分：`hex_id("merge:summary")`

每条记录带写它的 `Stage`。重跑一个阶段只清它自己那些 `checks`。

### RunResult

`review()` 的返回值。`summary.json` 与 stdout 摘要都从这里渲染。

| 字段 | 含义 |
|---|---|
| `run_id` / `model` | 这次 run |
| `comments` | `merge` 定稿后的列表 |
| `overall_score` / `summary` | `merge` 第 7 步；未打分时为 `null`，另有 `unscored_reason` |
| `skipped` / `unreviewed` | `plan` 跳过的，以及没评到的 |
| `stopped` | 预算截断等原因；有值时退出码 3 |
| `published` | 这次发出去的评论 |
| `spent` / `budget` / `currency` | 账目。`budget` 为 `None` 表示无上限 |
| `report_path` / `summary_path` | run 目录里那两份 |

### 协议契约

核心只认 `protocol::Request` / `protocol::Response`（`instructions`、`input` items、`output` items、`usage`）。厂商 JSON 不进 checkpoint、comment、tool 接口。

`InputItem`：`Message` / `FunctionCall` / `FunctionCallOutput`。`OutputItem`：`Message` / `FunctionCall` / `Reasoning`。

## 5. 配置

单一 TOML。路径只有 `--config`，默认 `~/.reviewbot/config.toml`。那个路径上没有文件就失败，并报出取到的路径。不从 cwd 读，不逐级往上找。

数组段名一律单数：`[[provider]]`、`[[model]]`、`[[platform]]`、`[[tool]]`。`deny_unknown_fields`。`--format json` 的顶层键仍是复数（`models` 等）。

完整示例随二进制走，是 `src/config/example.toml`，`config init` 写出去的就是它。已有文件不覆盖。

### 段与字段

`[log]`

| 字段 | 规则 |
|---|---|
| `level` | `error` / `warn` / `info` / `debug` / `trace`，默认 `info`。不进指纹。`RUST_LOG` 盖得过它 |

`[review]` — 下列数值必填且须 `> 0`，代码里没有兜底

| 字段 | 管什么 |
|---|---|
| `max_files_per_listing` | 一次列文件最多回多少条 |
| `max_hits_per_search` | 一次检索最多回多少条 |
| `max_files_per_fetch` | 一次 `fetch_repo_file` 最多拉多少个路径 |
| `max_file_bytes` | 一份文件最多取回来多少。超了整份拒，不截断 |
| `max_tool_output_bytes` | 一次工具回复的上限 |
| `max_rounds` | 一个文件最多几轮调查 |

`[plan]`

| 字段 | 规则 |
|---|---|
| `max_chunk_tokens` | 必填 `> 0`。一次评审看多少 diff |
| `skip_paths` | glob，默认 `[]` |
| `skip_generated` | 默认 `true`。命中 `@generated` / `Code generated by` |
| `skip_files_over_bytes` | 必填 `> 0` |

`[security]` — 只装权限，尺寸在 `[review]`

| 字段 | 规则 |
|---|---|
| `deny_paths` | glob，默认 `[]`。内置另加 `.git` / `.git/**`、runs 目录、`--output-dir` |
| `follow_symlinks` | 默认 `false` |
| `allow_extensions` | 必填、非空。裸名、不带点。整段不写或 `[]` 是配置错误 |
| `allow_build_tools` | 默认 `false`。`requires_build` 的 tool 还要它为 `true` |

`[[provider]]` — 全部必填

| 字段 | 规则 |
|---|---|
| `name` | 表内唯一 |
| `protocol` | 目前只认 `openai` |
| `base_url` | 拼接用的前缀 |
| `api_key` | 密钥来源，不是密钥本身 |
| `currency` | 显示用标签 |
| `budget_per_run` | `-1` 无上限；`0` 一分不花；`> 0` 上限；其余负数启动失败 |

`[[model]]`

| 字段 | 规则 |
|---|---|
| `name` | 发给厂商 API 的真实模型名。与 `alias` 共处同一命名空间，整体唯一 |
| `alias` | 可选 |
| `default` | 至多一条为 `true` |
| `provider` | 须匹配某个 `[[provider]].name` |
| `input_per_1m_tokens` / `output_per_1m_tokens` | 必填 |
| `cached_input_per_1m_tokens` | 可选 |
| `context_window_tokens` / `max_output_tokens` | 必填。须 `context_window > max_output + 4096` |
| `reasoning_effort` | 可选。取值 `none` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max`。不写则请求里不带该字段 |

`[[platform]]` — 没有 `name` / `host` / `kind`

| 字段 | 规则 |
|---|---|
| `base_url` | host 只认 `gitlab.com` 或 `api.github.com`。其余启动失败。同一网页 host 不能配两条 |
| `api_token` | 与 `api_key` 同一套来源规则 |

`[[tool]]` — 只装外部命令。内建 tool 不在这里出现，也关不掉

| 字段 | 规则 |
|---|---|
| `name` | 表内唯一，且不得与 11 个内建名重名 |
| `description` | 必填 |
| `bin` | 绝对路径，不得指向 worktree 内 |
| `args` | argv 数组，直接 `execve`。`{placeholder}` 须在 `params` 里声明，或是内置的 `worktree` |
| `params.<key>` | `type` = `path` / `string` / `integer` / `number` / `boolean`。`string` 必须有 `pattern` 或 `enum` |
| `requires_checkout` | 默认 `false` |
| `requires_build` | 默认 `false`。为 `true` 时须 `allow_build_tools` |
| `timeout_ms` | 默认 `60000` |

没有 `enabled` / `schedule` / `skippable`。不写这段 = 一个外部命令都不启用。

保留名：`submit_comment`、`finish_review`、`submit_summary`、`list_local_files`、`suggest_local_read`、`read_local_file`、`search_local_regex`、`list_repo_files`、`fetch_repo_file`、`search_repo_regex`、`search_repo_keyword`。

### 什么进配置，什么进命令行

一个设定只有一个来源。

- **只在配置里**：`[[provider]]`、`[[model]]`、`[[tool]]`、`[[platform]]`、`[review]`、`[plan]`、`[security]`。`[security]` 禁止命令行覆盖。
- **只在命令行**：位置参数、`--model`、`--publish`、`--output-dir`、`--format`、`-q`、`--no-color`、`--retries`、`--run-id`、`--worktree`、`--config`、`--runs-dir`、`run prune --keep-latest`。
- **在配置里、不进指纹**：`[log].level`。

`--model` 在配置里没有对应字段；`[[model]].default = true` 是「默认用哪条」。runs 目录没有配置字段，默认写死为 `~/.reviewbot/runs`。

### 选哪个模型

`--model`（匹配 `name` 或 `alias`）> 标了 `default = true` 的条目 > 唯一条目。三者都问不出结果 → 启动失败并列出可选项。至多一个 `default`。不允许运行时换便宜模型。

启动时只校验当前选中模型所属 provider 的密钥是否可读。输入 URL 的 host 必须匹配到某个 `[[platform]]`。

### 密钥来源

配置里只写来源。按值的形态判断：

- 以 `/`、`./`、`~` 开头 → 凭据文件。`~` 展开，去掉尾部换行，Unix 权限须为 600，路径必须在仓库外。
- 其余 → 环境变量名。长得像密钥本身（`sk-` / `sk_` / `ghp_` / `gho_` / `glpat-` / `github_pat_` 前缀，或长度 ≥ 32 且高熵）→ 拒绝启动。
- 不支持用命令取密钥。

密钥读出后只在内存传递，不进 checkpoint / trace / 指纹 / 日志。`--config` 指向被评审检出里的文件时 `warn` 一次，不硬失败。

### prompt

prompt 本体在 `src/prompts/`，`include_str!`，配置改不动。没有 `review.guidelines` 一类注入点。

模板规则：`{{槽}}` 未声明或装配完仍空是错误；`{{#槽}}…{{/槽}}` 在槽为空时整段（含空行）不出现。模板里没有条件、循环、嵌套。送给模型的指令、能力段、工具描述、拒绝理由一律英文。

### 指纹

三片 SHA-256（规范化 JSON）的小写十六进制：

| 片 | 内容 | 作废从哪起 |
|---|---|---|
| `input` | `[[platform]]` + 是否给了 `--worktree` | `input` |
| `plan` | 整个 `[plan]` | `plan` |
| `review` | `[review]`、`[security]`、`[[provider]]`、`[[model]]`、`[[tool]]`、`--model` | `review` |

没有 merge / report / publish 片。排除：密钥值、`--output-dir`、`--runs-dir`、`--retries`、`-q`、`--format`、`--publish`、`[log].level`。

## 6. Run 与落盘

### 身份

```
run_id = hex_id("{identity_key}\n{head_sha}")
```

`hex_id` = SHA-256 的前 16 位小写十六进制。

| 输入 | `identity_key` | `head_sha` |
|---|---|---|
| MR/PR URL | `platform:{host}:{project}:{number}` | 平台给的 head commit |
| diff | `diff:{content_sha256}` | 给了 `--worktree` 就取检出 HEAD，否则空串 |

diff 认内容不认路径。`--run-id` 覆盖算出来的目录名；重入仍比对 `meta` 里的输入身份与 `head_sha`。

### 目录

默认 `<runs_dir>/<run_id>/`，`--runs-dir` 覆盖。全部 JSON 明文。

```
meta.json
stages/<stage>.json          # input / plan / review / merge / report / publish
traces/<trace_id>.json
published.json
report.md
summary.json
log                          # tracing 追加写
cache/                       # Cache worktree
checks/                      # 检查器的可写 cwd
lock
```

`--output-dir` 另写 `report-<run_id>.md` 与 `summary-<run_id>.json`。run 目录整体不当 CI artifact；外发只用这两份。

`meta.json`：`run_id`、`input`、`model`、`provider`、`fingerprint`、`budget_limit`、`currency`、`price`、`spent`、`publish`、`completed_through`、`created_at`、`updated_at`。

`completed_through` 是一个 `Stage` 前缀。旧 run 缺这个字段当作一个阶段也没完成。某阶段文件读不出来，退到它的前一阶段。

`--publish` 每次进入按本次命令行重写 `meta.publish`。

### 锁

在 `<run dir>/lock` 上 `flock(LOCK_EX | LOCK_NB)`。拿不到直接失败，不等待、不抢占。锁在建 `Worktree` 之前拿。干净退出也把文件留在原地（里面一行 `pid N`）。内核释放锁，没有过期、没有 `--force`、没有 unlock 命令。

同一 `run_id` 同时只允许一个进程写。

### 重入

重新进入一个 run 的办法是把同一条命令再敲一遍。没有 `resume` / `publish` / `report` 子命令。

| 情况 | 动作 |
|---|---|
| 输入身份或 `head_sha` 不同 | 失败，不动 meta |
| 指纹相同 | 接着跑，checkpoint 保留 |
| 某一片不同 | `mark_incomplete(from)`，删掉 `from` 及之后的 checkpoint，`spent` 保留，用当前配置继续 |
| `meta.json` 坏了 | `warn`，当作新 run |

前四个阶段：`completed_through >= 该阶段` 且 checkpoint 能解析 → 跳过。`review` 未标完成时，`saved(Review)` 可从已写完的分片接着跑。

后两个阶段每次都跑，仍写 checkpoint。

### 清理

只有 `run remove` 和 `run prune` 删 run。`review` 对 runs 目录只写不删。

- `run remove <run_id>`：删这一个。成功时不打字。id 对不上是错误。
- `run prune`：按修改时间留最新 N 个，其余整个删掉。N 默认 0。`--dry-run` 只报个数。成功打一句。
- run 数超过 10 时，`review` 收尾 `warn` 一条，给出可复制的 `run prune` 命令。

### 终态

| 终态 | 含义 | 退出码 |
|---|---|---|
| 已完成 | 走完了 `merge`，报告已落盘 | 0；预算截断 3；发布部分失败 5 |
| 失败或未完成 | 没走到那一步 | 1、4，以及没有退出码的残留 |

退出码 2（配置错误，或 `--run-id` 指到另一个输入）不在这张表里。

## 7. 阶段

### 输入规范化（`input`）

`Source`：`Url(String)` 或 `Diff { origin, content }`。位置参数自己认：`http(s)://` 当 URL，`-` 是 stdin，其余当文件路径。按内容拒 `git format-patch` mbox。

`Opening::identify()` 在 run 目录建立之前跑，产出 `InputRecord`（算 `run_id`、校验检出 HEAD）。URL 那支此时只取 `head_sha`；给了 `--worktree` 则 HEAD 必须等于 `head_sha`。

`Input::run`：

1. URL：`fetch_change()` → 解析 unified diff → `ChangeSet`（含 locator 与 narrative）
2. Diff：解析内容 → `ChangeSet`（narrative 空；`head_sha` 来自 meta）
3. `normalize_narrative()`：去空白、commit 只留第一行、截到 20 条并设 `more_commits`

每个文件两个行集合：

| 集合 | 含哪些行 | 给谁用 |
|---|---|---|
| 可评论行 | 新增行 + 上下文行 | `merge` 对齐 |
| 变更行 | 新增行 + 纯删除处紧邻的那行 | `merge` 范围校验 |

标题 ≤ 200 字符，描述 ≤ 2000，commit 主题 ≤ 120。取不到自述不失败 run。

### 筛选与切分（`plan`）

过滤顺序：`deny_paths`（新旧路径）→ `allow_extensions`（`new_path`，删除文件除外）→ `skip_paths` → 二进制 → 纯删除（`new_path == /dev/null`）→ 空 hunks → 生成标记 → `skip_files_over_bytes`。跳过的进报告。

剩下的按 `changed_lines.len()` 降序，同分按 `new_path`。

一个分片一个文件。超限才按 hunk 边界再切，不腰斩 hunk。

窗口：

```
reserved = max_output_tokens + max(量出的 preamble, 4096) + 1024
available = context_window_tokens − reserved
chunk_tokens = min(available, max_chunk_tokens)
```

`available < 2000` 时 `plan` 失败。没有可用的调查工具时窗口侧轮数是 1；否则 `(available / chunk_tokens) − 1`，至少 1。调查循环的硬顶是 `[review].max_rounds`，每轮开工时再按当时对话长度查窗口。

一轮工具输出合计上限 = `bytes_for_ascii_tokens(chunk_tokens)`（ASCII 约 4 字符/token，乘 1.2）。

Preamble 在 `plan` 或 `review` 将要跑时装配一次，两阶段对着同一份字节。两个阶段都已完成时不再装配。

改动清单（`Orientation`）：文件数 < 2 则整段省略；否则最多 60 条，超出报数。每条写路径、`added` / `deleted` / `renamed`、是否二进制、变更行数（删除不写行数）。改名写成 `old -> new`。

### prompt 与输出契约（`review`）

`instructions` 在整个 run 内逐字不变，装不随分片变化的部分。`input` 装这个文件的 diff，以及自述、分片交接、收尾提示。

`src/prompts/`：

| 文件 | 槽 | 用处 |
|---|---|---|
| `review.md` | `change`、`capabilities` | 每个分片的 instructions，六段 |
| `capabilities.md` | `investigation`、`delivery`、`worktree` | 能力清单 |
| `worktree-local.md` / `worktree-cache.md` / `worktree-empty.md` | 无 | 填 `worktree` 槽 |
| `narrative.md` | `material` | `input` 第一条：围栏里的作者自述 |
| `split.md` | `pieces`、`piece`、`findings`、`note`、`handoff` | 被切开的文件，diff 之前 |
| `conclude.md` / `after-truncate.md` / `after-prose.md` / `rounds-last.md` | 无或 `reason` | 收尾、截断、散文作废、最后一轮提醒 |
| `summary.md` / `rescore.md` | 无 / `reason` | `merge` 打分 |

`review.md` 六段：任务与范围；判据与优先级；能力与用法；检查器输出怎么用；输出契约；两个分数。工具名与参数 schema 走请求的 `tools` 字段，与 prompt 能力段同源。`submit_comment` 与 `finish_review` 在 prompt 里点名。

`submit_comment` 参数：

| 字段 | 规则 |
|---|---|
| `path` | 可省，默认本分片文件 |
| `start_line` | 必填。挂点第一行 |
| `end_line` | 单行可省 |
| `body` / `suggestion` | 必填。都空则拒，回传去调 `finish_review` |
| `severity_score` / `confidence_score` | 0–100 整数。带引号的整数按整数读；`"92.5"` / `"high"` / `101` 拒 |
| `evidence.lines` | 文件行号数组，至少一个须是变更行。填 diff 正文则拒 |
| `evidence.external_files` | 可选 |
| `evidence.tool_quote` | 可选：`tool`、`text`、`note` |

一条意见一次调用。聊天正文里的 JSON 不算意见。没有 function call 但有正文 → 丢弃，塞 `after-prose.md` 再要一次。空正文直接结束。

`finish_review` 无参数，不产出意见。`submit_comment` 不是结束信号。

分片交接（`pieces > 1`）：第几片/共几片；前面已提交意见摘要（最多 12 条、每条 200 字符）；上一片留下的一句话（≤ 1200 字符）。整文件只有一片时这段不出现。

### 工具循环（`review`）

三种轮次，registry 按轮次生成 `tools`：

| 轮次 | 给哪些工具 |
|---|---|
| Investigation | 内容、检查器、`submit_comment`、`finish_review` |
| Conclusion | 只剩两条交付 |
| Scoring | 只有 `submit_summary`（`merge` 用） |

一轮可以并排多个 `function_call`。`chat.rounds` 只在这一轮含非交付调用时加一。交付轮不占 `max_rounds`。

每轮进模型前：上下文检查 + 预算检查。超窗或预算不够：开局那一轮失败；中途改走收尾（撤调查工具）。输出被截断 → `after-truncate.md` 再要一轮收尾。下一轮仍装得进且是最后一轮 → `rounds-last.md`。撞 `max_rounds` → `conclude.md`。

`raw_output` 是收下的 `submit_comment` 序列化成的 `{"comments":[...]}`。每分片结束后 `save`；整阶段完成才 `complete`。

预算耗尽：已收尾的分片算评审过；后面没打开的进 `unreviewed`，`stopped` 写原因，继续走 merge / report / publish。

`unavailable` 在开工前从 `worktree.went_without()` 记下，走进报告。

有检查器可用时，分片开工前先把本文件 `fetch` 进 worktree。

### 分片归并（`merge`）

七步。前六步不调模型。

1. **解析** `{"comments":[...]}`。读不出 → 该分片进 `unproduced`，不影响其他分片。
2. **剔除**：`path` 不是本分片文件；缺 `body` / `suggestion` / 任一分数；分数不是 0–100 整数；`evidence.lines` 一行都不落在变更行上。整条丢弃，记进 trace。
3. **对齐**到可评论行：空行或只含 `}` / `)` / `]` 的行（可带 `;` `,`）不算挂点。反引号只看 `body`：窗口内唯一命中、或证据行上的唯一命中，优先于更长但出现多次的名字；否则最长片段，唯一或距挂点最近（等距放弃）。≥6 且只出现一行的短片段也算。挂点已是非弱行且不含该片段 → 按上面钉；否则留下。挂点是弱行 → 同上反引号，否则证据第一行非弱可评论行，否则文件级。挂点不在可评论集合 → ±3 最近非弱行，否则证据，否则反引号，否则文件级。不往回猜。偏移记进 trace，分数不动。
4. **引文核对**：有 `tool_quote` 时，在发给模型的那份工具输出里做空白与路径规范化后的逐字比对，再查目标文件与 ±3 行号。全过贴 `found by tool`，否则 `quote unverified`。`external_files` 里从未取回过的文件只进 trace。分数不动。
5. **去重**：同一 `path`、对齐后区间相交、正文规范化后逐字相同（折叠空白、去掉数字）。留下 `(severity_score, confidence_score)` 较高的，证据取并集。跨文件不合。
6. **定序**：`severity_score` 降序，同分 `confidence_score` 降序，再 `path`，再行号（文件级当 `u32::MAX`）。算出两轴档位计数。
7. **汇总打分**：`summary.md` + 定稿清单，只挂 `submit_summary`。空清单仍打。跳过打分：`review.stopped`、一个分片都没有、或空评论且有 `unproduced`。预算不够或连着两次读不出 → `overall_score = null` 并写原因，不填 0。不合 schema 重问一次（`rescore.md`）。

重跑 `merge` 前清它自己写的 trace notes。

### 报告（`report`）

标题 `Reviewbot report`。顺序：`run` / `model` / `overall` → `unavailable` 覆盖说明 → 模型的 `summary` 与图例 → 一份清单（发现、未产出、跳过、未评审）。发现标题是 `[severity N% / confidence N%] badge \`path:line\``，然后问题、`suggestion:`、`trace: <id>`。花费不进报告。

`summary.json` 字段：`run_id`、`model`、`overall_score`、`summary`、`unscored_reason`、`comments`（按 confidence）、`by_severity`、`skipped`、`unreviewed`、`stopped`、`unproduced`、`unavailable`、`spent`、`budget`、`currency`。

### 发布（`publish`）

`meta.publish == false` 时不碰平台。diff 输入加 `--publish` 启动失败。

只发 `has_valid_comments` 为真的路径上的 comment。幂等标记 `<!-- reviewbot:{run_id}:{trace_id} -->`。汇总评论用 `{run_id}:summary`。发前合并平台已有标记与 `published.json`。成功一条写一条。部分失败 → `PublishIncomplete`（退出码 5），已发出的留在 `published.json`。

行内评论正文：

```
**[{severity} {n}% / {confidence} {n}%]**{badge} {body}

suggestion:
{suggestion}

run: `{run_id}`
trace: `{trace_id}`
<!-- reviewbot:{run_id}:{trace_id} -->
```

上限 65536 字节；超出截 finding，指向 `traces/`。422 退化为文件级，重试一次，trace 记一句，分数不动。

GitLab：逐条 discussions（`new_line`，带三个 SHA）；文件级走 notes。GitHub：先一批 inline 进一次 review（`event: COMMENT`），再发 issue comment 作汇总。

## 8. 适配器

### 平台

URL 解析出 host、项目、编号，按 host 匹配 `[[platform]]`。匹配不到就失败。HTTP 打在该条目的 `base_url` 上。`User-Agent: reviewbot/<version>`。

| | GitLab | GitHub |
|---|---|---|
| 鉴权 | `PRIVATE-TOKEN` | `Authorization: Bearer`，`Accept: application/vnd.github+json`，`X-GitHub-Api-Version: 2022-11-28` |
| 项目 | URL 编码路径 | `owner/repo` |
| SHA | `head` / `base` / `start` | `head` / `base`，`start` 空 |
| Capabilities | 空集 | `KEYWORD_SEARCH` |
| diff | versions + diffs JSON → unified | 同一 PR 端点，`Accept: application/vnd.github.v3.diff` |
| 树 | keyset 翻页递归 | 递归树；`truncated` 时逐目录走，上限 32 层 |
| 搜索 | 不支持 | `/search/code`，最多 20 个候选路径，再按 `head_sha` 取回本地匹配 |
| 发帖 | 一条一个 discussion；422 → note | 一批进 review；422 → `subject_type: file` |

`RepoSource`：`list_files`、`read_file`、`size`、`search`、`cached_body`。`Capabilities` 位集：`REGEX_SEARCH`、`KEYWORD_SEARCH`。

整棵树一个 run 只取一次，后续 glob 本地过滤。列表不完整须写进返回。`Cache` 上列仓库与搜仓库转发平台，不用盘上已有文件回答。

### Worktree

`Worktree::open(checkout, repo, run_dir)`：

| 条件 | 变体 |
|---|---|
| 给了 `--worktree` | `Local { root, repo }`。URL 输入时 HEAD 须等于 `head_sha`。只读 |
| 没给、有平台 | `Cache { <run_dir>/cache, repo }` |
| 没给、没平台 | `Empty` |

写盘只有 `Cache` 那一臂。`fetch` 先问 `size`，超 `max_file_bytes` 不下载；二进制不落盘；unix 模式 0644。`Local` 上缺失文件不自动取回。

`went_without()`：`Empty` 报读不到文件；`Cache` 报不是完整检出，平台搜不了再加一条。`Local` 为空。

reviewbot 自己不克隆。需要完整检出由调用方准备，再 `--worktree`。

### 协议

`Protocol::send(&Request) -> Response`。当前实现 `openai`：`POST {base_url}/responses`，超时 600s。请求字段：`model`、`instructions`、`input`、`tools`（`type: function`）、`max_output_tokens`、可选 `reasoning.effort`。不发 `stream`、`previous_response_id`、`store`。无状态，多轮自己拼 `input`。

用量：`input_tokens`、`output_tokens`（含思维链）、缓存命中从 `input_tokens_details.cached_tokens` 等字段读。思维链只进 trace。

### 工具

全部注册，答不答得上来对着这次 `Worktree`。不可用的描述标 `NOT AVAILABLE THIS RUN`；调用时 registry 拦下，回完整理由。拒绝发生在派发前。

五个条件（`tool/availability.rs`）：`no_files`、`no_repo`、`no_regex_search`、`no_keyword_search`、`no_whole_tree`。`requires_build` 是配置许可，不进这五条。

**内容工具**（Investigation）

| 名字 | 参数 | 行为 |
|---|---|---|
| `list_local_files` | `glob` | 此刻磁盘上的路径 |
| `suggest_local_read` | `path` | 只回尺寸与建议，不回正文 |
| `read_local_file` | `path`，可选行区间 | 先取整份再切片。超限拒，不截断 |
| `search_local_regex` | `query`，可选 `glob` | 本地正则，命中按文件分组 |
| `list_repo_files` | `glob` | 评审 commit 上的路径 |
| `fetch_repo_file` | `paths[]` | 取回落盘，不回正文。整批上限 `max_files_per_fetch` |
| `search_repo_regex` | `query`，可选 `glob` | 平台正则 |
| `search_repo_keyword` | `query`，可选 `glob` | 平台关键词 |

列表与搜索结果先过 `deny_paths`（剔掉，不当占位符）。扩展名白名单不在列表里滤，真读时再拦。超 listing / search 上限则截断并报还剩多少。

`read_local_file` 装不下就拒绝，理由带字节数、行数、一次最多多少。`suggest_local_read` 给出整份 / 分段区间 / 读不了三种结论。

**提交工具**

| 名字 | 轮次 |
|---|---|
| `submit_comment` | Investigation、Conclusion |
| `finish_review` | 同上 |
| `submit_summary` | Scoring |

**外部命令**：argv 数组、一个占位符一个元素、不经 shell。cwd = `<run_dir>/checks`。环境白名单（`PATH` / `HOME` / `LANG` / `LC_ALL` / `TMPDIR` / `TZ`），剔除 `*_API_KEY` / `*_TOKEN`。stdout+stderr 合并，按 `max_tool_output_bytes` 截断，输出里的路径过 `deny_paths`。路径参数先校验再 `fetch`，展开成 worktree 根下的绝对路径，回给模型前剥前缀。非零退出是结果，不重试。超时与被信号杀死才重试；耗尽后把失败当工具输出回给模型，阶段不失败。

参数校验：被字符串化的整数按整数读。JSON Schema 由参数声明派生，全仓库一处。

一轮内所有工具输出共享 `window.round_bytes()`，超出按调用顺序截断。

## 9. 预算、安全、重试

### 预算

顺着选中模型的 provider 冻结 `budget_per_run` 与 `currency`，写入 `meta.json`。中途只消耗。单价来自该 `[[model]]`。一次 run 单币种。

调用前：`allow(上限, 预留) = min(模型上限, 剩余÷输出单价 − 预留)`，结果写入请求的 `max_output_tokens`。输入不预扣。允许输出低于 1024 token（且调用方不是主动要更短）→ 拒。`-1` 恒过；`0` 恒不过。

开局那一轮不预留收尾额度。之后预留 `min(max_output_tokens, 4096)`。不够则走收尾路径。

结算用厂商返回的 `usage`。缓存命中走 `cached_input_per_1m_tokens`。`output_tokens` 已含思维链，不把 `reasoning_tokens` 再加一遍。重入保留已花费。

token 估算（ASCII 约 4 字符/token，CJK 约 1，×1.2）只服务切分与上下文检查。

全程串行：阶段与分片都不并发。状态屏另有只读绘制线程，不碰预算。

### 安全

本地磁盘只写 run 目录与 `--output-dir`。命令行给的检出只读。

**脱敏**：进模型的文本先过 redactor（PEM、Bearer、`sk-`、`glpat-`、GitHub token 形、`*API_KEY` / `*TOKEN` 赋值，以及本次读到的密钥字面值）。CLI 全部输出也过 redactor。

**路径校验**（调用点在 `tool`）：

- 仓库相对、规范化、禁止绝对路径与 `..`
- `deny_paths`（含内置）
- `allow_extensions`（列表不过这道）
- worktree 路径另查符号链接：`follow_symlinks = false` 时任一段是 symlink 即拒；`true` 时解析后仍须在根内

changeset 里的文件不豁免：命中 `deny_paths` 或不在白名单，进跳过清单。diff 本身不经这道。

没有扩展名的文件过不了白名单。

**子进程**：绝对路径、不在仓库内、argv 数组、环境清洗、cwd = `checks/`、超时。禁写检出靠部署隔离。`requires_build` 须 `allow_build_tools`。reviewbot 自己不跑 `npm install` / `cargo fetch`。

### 重试

`--retries` 默认 2（最多 3 次）。曲线：500ms 起、翻倍、上限 8s、全抖动；尊重 `Retry-After`。

只对瞬时错误重试：超时、连接重置、5xx、429、模型空 body / 截断 JSON。401/403/400/422、schema 失败、路径越界、预算不足首次即放弃。

| 场景 | 策略 |
|---|---|
| 模型 / 平台 HTTP | 上列曲线。平台 422 的文件级退化不算网络重试 |
| tool | 只对超时与被杀重试 |
| 打分解析失败 | 不退避，带理由重问一次 |
| 阶段失败 | 落盘退出，交给下一次同一条 `review` |

平台 HTTP 超时 60s，协议 600s。

## 10. 库与 CLI

`src/lib.rs` 对外入口是 `review(settings, source, progress) -> RunResult`。起一个 run 和重新进一个 run 是同一个函数。`progress::Silent` 不改变 run 行为。

`src/main.rs` 只起 tracing、调 `cli::run`、把结果收成退出码。业务在 lib。`src/cli/` 不由 `lib.rs` 声明。

### 命令

```
reviewbot review <URL | diff 文件 | ->
reviewbot review --publish <URL>
reviewbot run list | show <id> | trace <id> [--trace-id <id>]
reviewbot run remove <id>
reviewbot run prune [--keep-latest N] [--dry-run]
reviewbot config init | check | info
```

名词单数。没有顶层 `model` / `tool` / `platform` / `provider` / `trace` / `resume` / `publish` / `report`。

**全局 flag**

| flag | 默认 |
|---|---|
| `--config` | `~/.reviewbot/config.toml` |
| `--runs-dir` | `~/.reviewbot/runs` |
| `--format` | `text`（`text` / `json`） |
| `-q` | 关 |
| `--no-color` | 关（非 TTY 自动） |
| `--retries` | 2 |

**`review`**：`--model`、`--worktree`、`--publish`、`--run-id`、`--output-dir`。

`config check` 只做本地校验，不发请求，不查 `[[tool]].bin` 是否存在。`config info` 四张表：PLATFORMS / PROVIDERS / MODELS / TOOLS。凭据只报名来源。tool 表文本只印名字、用途、轮次；JSON 带完整契约。

### 输出

| 通路 | 去处 |
|---|---|
| 状态屏 + 结果摘要 | stdout |
| 致命错误 | stderr：`error:` 一句，可选 `run_id:`，可选 `next:`（本次 `argv` 原文）。`--run-id` 指错目录不印 `next:` |
| `tracing` | `<run dir>/log`。stdout / stderr 上没有 tracing |

`-q` 清 stdout（状态屏也不出），除非 `--format json`（JSON 照出）。`--format` 管 stdout，`--output-dir` 管文件。

TTY：一块随已知量变高的摘要，字段与最终摘要同一套词，底下活动行。约 100ms 一帧。不用 raw mode。非 TTY / 画不了 TUI：每行一条，只增不减（抬头、每个分片、每个阶段结束）。`-q` 与 `--format json` 用 `Silent`。

最终摘要字段：`run_id`、`model`、`overall`、`comments`、`severity`、`confidence`、`skipped`、`unreviewed`、`budget`、`report`、`summary`、`published`。问题写在最后，且只写一次。

`run list` 的 JSON 是 `{"runs_dir": "...", "runs": [...]}`。

### 退出码

| 码 | 何时 |
|---|---|
| 0 | 跑完且未截断 |
| 1 | 未分类错误、锁被持有 |
| 2 | 配置、输入、`--run-id` 指错、host 未配、mbox、worktree 对不上、run/trace 不存在 |
| 3 | 预算截断或预算错误 |
| 4 | 平台或模型服务失败 |
| 5 | 发布部分失败 |

退出码不编码评审结论。

### CI

GitLab 的 cache 只收项目内路径，流水线写 `--runs-dir`。artifact 只收 `--output-dir`。示例在 `examples/gitlab-ci.yml` 与 `examples/github-actions.yml`。

## 11. 测试

crate 同时产出 `lib` 与 `bin`。业务逻辑测 lib。测试离线，适配器换成假实现。同一行为只在够得着它的最低那档测。

| 档 | 测什么 |
|---|---|
| 单元 | 纯算法：行号对齐、引文比对、去重、glob、token 估算、脱敏、退避 |
| 契约 | trait 约定；真假实现跑同一组用例 |
| 阶段 | 六个阶段各自的输入输出，上下游 fixture |
| 整装 | `review()` 六阶段、重入、指纹、预算耗尽 |
| CLI | 输出流、退出码、落盘位置（`assert_cmd`） |
