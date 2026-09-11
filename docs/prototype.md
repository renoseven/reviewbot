# reviewbot 原型实现计划

这份计划把 [设计](design.md) 落成**可以本地跑通的原型**：先立骨架（每层可编译、假实现能空跑），再把 diff → 报告 → 发帖这条真链路打通，最后往回路里加工具与仓库内容来源。测试取「最小」档：每个模块留少量单元测试保证回归，不铺设计 §11 的四十组用例。

**M1–M8 已经做完。** 当前树就是这份计划的产物。现行行为以 [design.md](design.md) 为准；取舍以 [decisions.md](decisions.md) 为准。还没做的缺口和下一轮工作在 [README](../README.md) 的「当前缺陷 / 后续计划」，不要对着本文当路线图继续开里程碑。

后文是当时的目标形状与验收，留给对「原型是按什么顺序长出来的」有疑问的人。和现行实现打架的细节（例如顶层 `model list` / `tool list` 后来收进 `config info`），以 design 为准，本文不再改现行行为。

## 目标形状

```
src/
  lib.rs              # review() / RunResult + 模块声明与再导出，只有这些
  main.rs             # 只有 fn main()：起 tracing、调 cli 解析、分派、把错误收成退出码
  cli/                # 二进制这一侧，lib.rs 不声明它
    mod.rs            #   子命令分派 + lib 错误 → 退出码 0/1/2/3/4/5
    args.rs           #   clap derive：全局 / review / run prune 三组 flag
    render.rs         #   text 与 json 两种渲染：结果摘要、失败提示、runs/models/tools 列表
    status.rs         #   跑的时候那块状态屏，喂它的是 progress 那条事件通道
    logging.rs        #   tracing 的写出端：先攒着，run 目录一定下就追加进 <run dir>/log
  domain/             # 共享词汇，一类型一文件，不含逻辑
    mod.rs            #   只做 pub use 再导出
    changeset.rs      #   ChangeSet / FileChange / Hunk / 两个行集合（纯数据）
    comment.rs        #   Comment / CommentTarget
    confidence.rs     #   Confidence 四档枚举
  config/             # TOML 解析校验、model→provider→protocol 解析链、指纹、密钥来源
  security/           # 只提供检查：redactor、路径校验、子进程环境清洗与资源上限、输出截断
  budget/             # 钱与 token 的账
    mod.rs            #   Budget：冻结上限与货币、调用前检查、累计与结算
    estimate.rs       #   字符数 token 估算（plan 切分与 review 上下文检查共用同一个函数）
    price.rs          #   模型条目单价 → usage 换算，缓存命中走 cached_input_per_1m
  record/             # run_id、run 目录、lock、meta.json、checkpoint 原子写、Trace 两视图、Storage trait
  platform/           # Platform trait + RepoSource trait + gitlab / github + URL 解析 + capabilities()
  worktree/           # WorktreeSource trait 与实现（只有读方法）：遍历、读文件、正则匹配
  protocol/           # Request/Response + Protocol trait + openai
  tool/               # Tool trait + registry + command 实现 + 六个内建 tool（读取前调 security 的路径校验）
  progress.rs         # 类型化的事件通道：run 边跑边说自己在做什么，谁在看由调用方定
  stage/{input,plan,review,merge,report,publish}.rs
  prompts/{review.md,summary.md}   # include_str!，配置改不动
tests/                # 少量整装与 CLI 契约测试（assert_cmd）
```

依赖单向向下：`stage::*` → 适配器 → 设施 → `domain`；同层只有 `tool` 认 `platform` 的 `RepoSource` 与 `worktree` 的 `WorktreeSource` 这一条。顺序只写在 `lib.rs` 的 `review()` 里，不建 orchestrator 模块。

三处目录形状上的决定：

- **`worktree` 不进 `domain`**：`domain` 是不含逻辑、不依赖任何东西的共享词汇，而 `worktree` 要读磁盘、要认 `security` 的校验与 `deny_paths`，放进最底层就是反向依赖。它与 `platform` 对称各占一个目录，并各自定义自己那个来源 trait。
- **安全靠实现保证**：`security` 只提供检查函数，调用点固定在 `tool`——模型能看见的每一次读取都从内建 tool 进来。六个内建 tool 每个都要在调用来源之前先校验路径。
- **`Confidence` 只是枚举**：分数→档位的映射写在 `merge` 第 6 步，`domain` 不含逻辑。
- **clap 拆进 `src/cli/`**：设计只说 `main.rs` 不写业务逻辑，没说它必须是一个文件。七个子命令、四组 flag、两套渲染、六个退出码塞一个文件会到七八百行，而「flag 长什么样」和「结果怎么印」是两件会各自变的事。这几个文件只由 `main.rs` 声明，`lib.rs` 不认识它们，库的公开面没多一个字。

## 里程碑

原型按这八步串起来。每步当时的验收写在条里；现在整条链路已经能在假适配器下空跑、也能对着真实模型和 GitHub PR 出报告。

**M1 分层骨架。** `Cargo.toml` 补齐 `tokio`/`reqwest`(rustls)/`serde`/`serde_json`/`toml`/`clap`/`sha2`/`regex`/`thiserror`/`tracing`/`tracing-subscriber`，另加 `globset`（`deny_paths` / `skip_paths` 的 glob 匹配，设计的依赖清单没列但绕不开），dev 加 `assert_cmd`。三个扩展点 trait（`Platform` / `Protocol` / `Tool`）与 `record::Storage` 先定死；两个来源 trait 跟着主人放在 `platform` 与 `worktree`，签名收普通的仓库相对路径。`stage::*` 六个空阶段 + 假适配器。验收是「假实现下空跑到底、正确落盘、把同一条命令再跑一遍能从任一阶段接上」。

**M2 input + plan。** unified diff 解析器（同时服务本地文件与平台 diff 端点，按内容判 mbox 并拒绝），每个文件建**可评论行**与**变更行**两个集合并随 `ChangeSet` 落盘；`plan` 做过滤、按变更行数降序、一文件一分片、超限按 hunk 再切，分片上限按 `context_window − max_output_tokens − 固定骨架 − 工具余量 − headroom` 算。仍用假 protocol。

**M3 review 主干。** `protocol::openai` 打 `POST {base_url}/responses`，无流式、无 `previous_response_id`；prompt 六段写进 `src/prompts/review.md` 用 `include_str!`，`instructions` 在整个 run 内逐字不变，`input` 只装一个文件的 diff；调用前预算检查 + usage 结算 + 脱敏 + 瞬时故障退避重试（500ms 起、翻倍、上限 8s、抖动，只对超时 / 5xx / 429 / 空 body）。不含工具。

**M4 merge + 报告。** 七步：解析 → 剔越界（越出文件、`evidence.lines` 一行都不落在变更行集合上即整条丢弃）→ 行号对齐（精确 → ±3 窗口 → `evidence.lines` 兜底 → 退化文件级）→ 核对标注 → 去重（同 `path`、区间相交、正文规范化后逐字相同）→ 定序统计 → 汇总打分（单独一次模型调用，预算不足或不合 schema 就 `overall_score = null` 并写明原因，禁止填 0）。`publish` 先只做写 `report.md` / `summary.json`；trace 留在 `traces/`，不折进报告。

**M5 platform + 发帖。** URL 解析 → host 匹配 `[[platform]]` → `kind` 由内置两条已知 host 或显式字段定，禁止从 `base_url` 形状反推；GitLab 逐条 discussions（带 `base_sha` / `start_sha` / `head_sha` 与 `new_line`），GitHub 一次 `POST .../reviews`；幂等标记 `<!-- reviewbot:{run_id}:{trace_id} -->`，汇总评论用 `{run_id}:summary`；发前拉已有评论比对，成功一条写一条 `published.json`；422 退化为文件级重试一次。流程到此走完。

**M6 工具与 function_call 循环。** `Tool` trait + registry + 通用 command 实现（argv 数组直接 `execve`、一个占位符一个元素、子进程环境剔 `*_API_KEY` / `*_TOKEN`、禁网、超时、stdout+stderr 合并后按 `max_tool_output_bytes` 截断、输出侧路径过 `deny_paths`）；循环每轮走预算与上下文两道检查；`requires_worktree` 不满足整条不注册并 `warn`；「注册了外部检查器但一次都没调」要留痕。引文核对与去重在这一步才真正生效。

**M7 内建 tool 与两个内容来源。** `RepoSource`（平台 API 按 `head_sha` 列 / 读 / 搜，GitHub 树 `truncated` 时退到逐目录、搜索结果只当候选路径再按 sha 取回本地重做匹配）与 `WorktreeSource`（磁盘遍历、读、正则）；六个对称命名的内建 tool 按能力注册，`capabilities()` 说不支持搜索就不注册 `search_repo`；整棵树一个 run 只取一次，后续 glob 本地过滤。

**M8 收口与交付物。** `run list|show|prune`、查看类命令、`--format json` 的字段化输出（`run list` 的 runs 目录进顶层键），退出码 0/1/2/3/4/5，失败提示带 `run_id` 与可照抄的下一步命令（回放本次 `argv`，含非默认 `--runs-dir`）；README、示例配置、GitLab CI / GitHub Actions 片段。原型交到这里：本地能评 diff / URL、能出报告、能发帖、能加外部命令。后来 CLI 收成 `config` / `review` / `run`，目录与命令以 README 为准。

## 每步用哪个模型

判据是**这一步的错误多久才会暴露**：形状类与算法类的错误要到下游甚至真实 MR 上才看得见，返工要连着改数据结构；设计已经写清、但仍要跨章节对齐字段与行为的，用 grok 4.6。

| 里程碑 | 模型 | 为什么 |
|---|---|---|
| M1 | claude-opus-5-thinking-high | 分层与 trait 定形，后面七步全站在它上面 |
| M2 | claude-opus-5-thinking-high | 纯删除 hunk、上下文行这些边界错了，要到 M5 发帖 422 才暴露 |
| M3 | cursor-grok-4.6-high-fast | `/responses` 的字段与重试分类在 §9 与 §6 里已经列全，但仍要和预算、脱敏、checkpoint 对齐 |
| M4 | claude-opus-5-thinking-high | 七步是整份设计里算法最密的一段：对齐、去重、越界、打分降级 |
| M5 | cursor-grok-4.6-high-fast | 端点是样板，但 422 退化、幂等、三个 SHA 与可评论行对不上会反噬 M2/M4 |
| M6 | claude-opus-5-thinking-high | 子进程与执行侧安全约束，出错代价最高 |
| M7 | claude-opus-5-thinking-high | 「不注册」与「注册了但返回错误」是两回事，那张按能力注册的表容易做拧 |
| M8 | cursor-grok-4.6-high-fast | 子命令、渲染、README 与 CI 片段量大，容易漏 §10 的字段化约定 |

## 几条会反复用到的决定

- 错误：每模块 `thiserror` 定义可匹配类型，`lib` 出口一个总错误枚举供 `main.rs` 映射退出码；`anyhow` 只在 `main.rs`。
- 目录锁：`<run dir>/lock` 上做 `flock(LOCK_EX | LOCK_NB)`，拿不到直接失败不等待不抢占；锁在内核手上，所以没有过期的锁、也没有探活与 `--force`，文件本身正常结束后留在原地。锁要在开 worktree 之前拿。
- checkpoint：临时文件 + `rename`，读不出来就回退上一份完整快照，禁止 catch 后整次重跑。
- 全程串行，阶段与分片都不并发——并发会让调用前预算检查失效。
- 密钥读出后只在内存传递，不进 checkpoint / trace / 指纹 / 日志。

## 测试（最小档）

原型只保证这三档能离线回归。其余（真实模型调用后的 run 目录体积、prompt 对着真实 PR、CI artifact 矩阵）当时就标成未做，现在写在 README「当前缺陷」。

- 单元：diff 解析与两个行集合、行号对齐、引文比对、去重、glob、token 估算、脱敏、退避曲线。
- 整装：假适配器下 `review()` 跑完六阶段，再进入一次同一个 run 不重复调模型。
- CLI 契约：`assert_cmd` 验成功 run 不写 stderr、`--format json` 可解析、退出码。
