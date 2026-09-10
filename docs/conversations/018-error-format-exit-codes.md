# 错误只出现一次、统一 error: 格式、引用错误退出码 2

## 报错应该在最下面，最好使用 Result 传递

**意图**：预算改完再跑，0.1 那次仍不对——摘要里 `overall` 嵌套重复 `not scored:`，还把花费带进了不该有花费的报告。要求报错排在最下面，最好用 Result 传递。随后问「能不能正常显示 error: xxxx 前面空一行，整体错误都要这样」。最后一轮针对上面这次提交（`4d28ddd`）追加要求：「所有错误走同一个路径，走 Result 传递错误」——即 clap 那条自己打印后退出的支路也要收进 `Result`，不许再有第二条通往 stderr 的路。

**步骤**：
- merge：`Score` 结构换成 `type Scoring = Result<Scored, Unscored>`；`Unscored` 是枚举（`RunStopped` / `NothingReviewed` / `NoReadableAnswer` / `Unaffordable` / `Unreadable`），`Display` 是唯一变成句子的地方；`nothing_to_score` 返回 `Option<Unscored>`
- `render.rs`：`overall` 只写 `not scored`；停止/未打分理由挪到全部字段之后。统一成 `error:` 形式——新增 `problem(reason)` 输出「空行 + `error: 一句话`」，摘要末尾与 `failure()` 共用它
- 逐条排查其余错误路径，只有 clap 那三种漏网（先是让它自己 `print()`、前面手动 `eprintln!()` 补空行，后一轮改掉，见下）
- 同一轮排查发现三种「指着的东西不在」落在退出码 1：目标 diff 打不开、`run show` 的 run 号没写过、`--worktree` 不是检出。`Error::exit_code` 把 `StageError::UnreadableInput`、`RecordError::RunNotFound`、`WorktreeError::{NotADirectory,Unopenable,NoHead,HeadMismatch}` 一并归到 2；design.md 退出码表补一段说明 2 与 1 的分界
- 两条 CLI 契约用例：解析错误的 stderr 以 `\nerror: ` 开头且 `--help`/`--version` 不被误伤；三种不存在的引用都退出 2
- 收成一条路（`4d28ddd` 原地改）：`cli` 新增 `Failure`（`Usage(clap::Error)` / `Command { error, invocation }`，`Error` 装箱避掉 `result_large_err`），解析、装 tracing、dispatch 挪进 `execute() -> Result<Finished, Failure>`；`run()` 只剩两个分支——打 stdout，或交给 `render::failure`。`render::failure` 改成收 `&Failure`，run_id / `next:` 那段落进私有的 `command_failure`，`-q` 的静音变成 `Finished::silenced`
- `--help` / `--version` 由 `use_stderr()` 为假认出，走 `Ok(Finished)`：文本取 `render()`，stdout 是终端时用 `.ansi()` 保住 clap 的加粗，管道里不带转义（`script` 起伪终端与直接管道各验一次）
- 单元测试补一条：`Failure::Usage` 同样是「空行 + `error:`」且保留 `Usage:` 提示；design.md 补一段「每个错误都靠 `Result` 走回同一个渲染件」，并给失败示例补上那行空行
- `cargo fmt` / `clippy --all-targets` / `cargo test` 全绿（344 + 38 + 22）

**决策**：
- **未打分理由用类型传，字符串只在渲染时生成。** 从前四处拼字符串、两处再读回去，结果 `overall` 里嵌了一层 `not scored:`，还把花费带进了不该有花费的报告
- **停了就只说停的理由。** 没打分是停下来的结果，再单独说一遍等于把同一句话抄两遍
- **所有错误共用一个形状：空行 + `error: 一句话`。** 前面通常已经有十来个字段或一串进度行，没有那行空行 `error:` 会被读成又一个字段；两条通道用同一个渲染件，读的人不必先分辨这是哪种失败
- **只留一条通往 stderr 的路：所有错误都当值回到 `run()`。** 先前只给 clap 手动补了行空行，但它仍是自己打印后 `return`——两条路并存，形状一致靠人记着，下次谁改 clap 那支就又不一样了。现在 clap 的抱怨是 `Failure::Usage`，与库的 `Error` 并列进同一个 `Result`，`eprint!` 只出现在一处
- **clap 的句子与 `Usage:` 提示照原样留着**，不拆出来套自己的 `problem()`：那两行说的是「该敲什么」，我们没有更好的话讲；渲染件只补前面那行空行
- **`--help` / `--version` 不是失败，是输出。** 它们同样从 `try_parse` 的 `Err` 回来，用 `use_stderr()` 认出，变成 `Ok(Finished)` 走 stdout、退出 0、不加空行；终端上保留 clap 自己的加粗（`render().ansi()`），管道里不带转义——是不是终端是流的属性，所以在 `cli` 判、不在渲染件里判
- **「指着的东西不在」归退出码 2，不归 1。** 1 的含义是「reviewbot 自己出了事」，把用户敲错的路径混进去，CI 里靠退出码分流的人会把自己的笔误读成 reviewbot 崩了
