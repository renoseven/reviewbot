# review 变成唯一入口，外加状态屏与 run 内日志

## 将 --out-dir 重命名为 --output-dir

**意图**：纯重命名 CLI flag `--out-dir` → `--output-dir`，Rust 字段 `out_dir` → `output_dir`；行为、产物文件名、`deny_paths` 自动追加逻辑不变。这个 flag 要留着，只是名字写全。

**步骤**：
- 改 `src/cli/args.rs`、`src/cli/mod.rs`、`src/config/mod.rs`、`src/stage/publish.rs`、`src/tests.rs` 及若干注释（`lib.rs`、`layout.rs`、`fingerprint.rs`、`path.rs`）
- 改 `README.md`、`examples/gitlab-ci.yml`、`examples/github-actions.yml`、`docs/design.md` 中的 flag 字面量
- `cargo test` 全绿；`cargo clippy --all-targets -- -D warnings` 零警告；`rg` 在 `src README.md examples docs/design.md` 无残留

**决策**：
- clap derive 从字段名 `output_dir` 自动生成 `--output-dir`，不加手动 rename attribute
- `docs/conversations/*` 历史记录不改动
- 后续删掉 `report` 子命令时，这个 flag 就只属于 `review` 了，设计文档里那张「`review` / `report` 共用」的表随之并回 `review`

---

## run 目录锁改成内核持有的 flock

**意图**：注意那个文件锁——正常退出后 `lock` 文件会留在目录里，别让它变成一个要靠探活来收拾的残局。

**步骤**：
- 加依赖 `rustix`；`src/record/lock.rs` 改成在 `<run dir>/lock` 上做 `flock(LOCK_EX | LOCK_NB)`，拿不到就 `RecordError::LockHeld`，不等待不抢占
- 删掉探活、`--force`、unlock 那一路；文件里只写一行 `pid N` 当线索，没有人读它
- `src/lib.rs`：`LockedRun::create` 把「建目录 + 拿锁」收成一步，**放在 `adapters.open_worktree` 之前**
- 两条用例：残留的 `lock`（写着一个早死的 pid）拦不住下一个 run；第一个持有者还在时第二个当场被拒，释放后又进得来

**决策**：
- **锁归内核，文件只是线索。** 进程怎么死都释放（Ctrl-C、`SIGKILL`、panic、断电），于是「过期的锁」这个概念不存在，探活、`--force`、unlock 三样一起没了
- **干净退出也不删 `lock` 文件。** 删它一分钱不省，反倒开一道竞态：unlink 与 close 之间，一个进程握着旧 fd，另一个已经新建并握住了新的。文件跟 run 目录一起被 `run prune` 带走
- **锁在开 worktree 之前拿。** 两个进程都开过 worktree 之后再比谁拿到锁，字已经写下去了
- 为什么不是写 pid 再探活：pid 会复用，而且「那个进程还在不在」和「它还在不在写这个目录」是两个问题

---

## review 是进入一个 run 的唯一命令

**意图**：`review` 要能重新进入自己起的那个 run——同一条命令再跑一遍就接着跑，已经完成的阶段不重跑；`resume`、`publish`、`report` 三个子命令删掉。失败时印在 stderr 上的下一步，就印这次调用的原文。

**步骤**：
- `src/cli/args.rs`：删 `Resume` / `Publish` / `Report` 三个子命令；`--output-dir` 挪进 `ReviewArgs`；加一条用例断言这三个名字不再解析（不留别名）
- `src/lib.rs`：只剩 `review()` / `review_with()`，删 `resume()` / `publish_run()` / `render_report()` / `resume_with()`。`review_with` 先算 `run_id`、建目录拿锁，再 `peek_meta`：指纹对不上直接 `FingerprintMismatch`，对得上就跳过已完成的阶段
- `review_with` 每次都 `recorder.set_publish_intent(本次 --publish)`
- `src/cli/render.rs`：`failure()` 多收一个 `invocation`，从 `std::env::args_os()` 来，按 shell 规矩逐词引一次印在 `next:` 上，底下补一行说明这条命令接着跑那个 run；`main` 只在 `Command::Review` 时传它
- `cargo test` 全绿

**决策**：
- **三个补救命令没携带任何新信息。** `run_id` 是输入与配置算出来的，而这两样正写在那条 `review` 命令上，所以「续跑这个 run」和「再跑一次这条命令」本来就是同一件事。删掉它们同时删掉三份要维护的分歧（`resume` 不收 `--publish`、`publish` 不重渲染报告、`report` 不发帖）
- **配置改了再跑是开新 run，不是被拒。** 指纹进 `run_id`，改一个字算出来就是另一个哈希。从前靠拒绝保证「一份 checkpoint 不混两套设定」，现在靠身份保证
- **`--run-id` 仍受指纹校验。** 它是唯一能让命令行与目录对不上的入口，所以那道检查留在这里
- **`meta.publish` 每次按命令行重写。** 带过 `--publish` 的 run，下次不带着它再跑就清成「不发」。命令行上写着什么这次就做什么，一个上次的开关不该替这次做决定；要接着发就把整行抄全，而 `next:` 印的正是抄得全的那行
- **`next:` 回放 `argv` 而不是重新拼一条命令。** 拼的过程每漏一个 flag 就是一条粘上去跑不对的命令；回放不会掉 `--config` / `--runs-dir` / `--publish` / `--model`。只有 `review` 印这一行，别的子命令不进 run
- 为什么不把三个名字留成 `review` 的别名：同一个 run 会有两种说法

---

## 最后一个阶段拆成 report 与 publish

**意图**：把写报告和发帖拆开——渲染报告不该联网，发帖不该顺带重写报告。拆完这两步每次进入都跑，正好接掉删掉的 `report` 与 `publish` 命令。

**步骤**：
- 新建 `src/stage/report.rs`（5 号）：渲染 `report.md`、`summary.json` 与 `--output-dir` 的两份拷贝，只读前面几个阶段的 checkpoint，全程不碰网络；checkpoint 是那份 `Summary`
- `src/stage/publish.rs` 缩成 6 号：只发帖，数字直接用 `Summary`，不自己再数一遍
- `src/lib.rs` 的 `run_stages`：前四个阶段有 checkpoint 就跳过，后两个每次都跑；`publish` 不看自己的 checkpoint，该不该发由 MR 上的标记加 `published.json` 决定
- `StageError::PublishIncomplete` 的话改成「再跑一遍同一条 `review` 命令补发剩下的」
- `cargo test` 全绿

**决策**：
- **两件事对外部世界的要求相反，所以不能合。** 渲染只读盘、必须免费；发帖必须联网、必须幂等。合在一起时这一半的代价就是另一半的代价：想重渲染得进一个会摸 MR 的阶段，想补几条评论得连报告一起重写
- **正因为一个免费、一个幂等，这两步重跑没有代价**，于是「每次进入都跑」成立，删掉的那两个命令也就有了去处
- **`publish` 不看自己的 checkpoint。** 一个只差几条评论没发出去的 run，如果被自己的 checkpoint 说服「上次跑过了」，就再没有别的路补上
- 旧 run 目录里那份 `stages/5-publish.json` 不迁移：现在 5 号是 `report`、6 号是 `publish`，两个每次都重跑，根本没人去看它

---

## 终端上一块跟着长出来的状态屏

**意图**：跑的时候终端要有东西在动，字段就用最终摘要那几个字段，不要另造一套进度词汇。`--format json` 要保留，那种时候什么都别画。

**步骤**：
- 新建 `src/progress.rs`：`Event`（`RunStarted` / `StageStarted` / `StageFinished` / `Chunk` / `Round` / `Tool` / `Spend`）与 `Progress` trait，另有一个 `Silent`
- `src/stage/mod.rs`：`StageContext` 多一个 `progress`；`stage_started` / `stage_finished` 由知道顺序的 `lib.rs` 那一侧发，`Spend` 在 `send_and_settle` 里结算完就发
- `src/lib.rs`：`review()` 第三个参数收 `&dyn Progress`；每个阶段结束时那句 `detail` 用最终摘要的词，读自 checkpoint 的那次额外缀上 `, from checkpoint`
- 新建 `src/cli/status.rs`：TTY 上就地重画一块摘要形状的东西（只为已知字段留行）加一行活动行，非 TTY 改成一行抬头 + 每分片一行（带累计花费）+ 每个完成的阶段一行；`finish()` 把块抹掉让最终摘要落在同一处
- `src/cli/mod.rs`：`-q` 或 `--format json` 时传 `Silent`
- 标签宽度与用词跟 `render::run_result()` 共用（`SUMMARY_LABEL_WIDTH` / `push_summary_line`）

**决策**：
- **状态屏的字段就是最终摘要的字段。** 另起一套进度词汇的话，跑完那一刻屏幕上会换一套词，读的人得自己把两套对上
- **只为已知的字段留行，不先摆一张全是 `-` 的空表。** 空表等于一上来就宣称有十行值得看，而其中八行此刻什么都不知道
- **`, from checkpoint` 是必须的。** 一个看不出差别的观察者会把没人干过的活报成干过了——两次跑的数字一模一样
- **非 TTY 不画块，改成只增不减的事件行。** 光标移动只在有光标时有意义，而 CI 日志的价值恰恰在每个完成的阶段各留一行
- **最终摘要仍然只由 `render::run_result()` 产出**，状态屏没有第二套渲染
- **`progress` 不是扩展点。** 扩展点是配置能启用的 `platform` / `protocol` / `tool` 三个 trait；这个由调用方在代码里挑，配置里看不见。`Silent` 必须是一个完整答案——**没有哪件事可以取决于谁在看**
- 为什么不搭在 `tracing` 上：日志是给事后读的散文，事件是关于一个还在跑的 run 的类型化事实，两条通路说同样的时刻、用不同的词，谁也不从谁那儿派生

---

## 日志改写进 run 目录，级别由配置定

**意图**：`tracing` 的输出挪进 run 目录，级别用配置里的一个字段挑；`-v` 删掉。

**步骤**：
- `src/config/file.rs`：加 `[log]` 段与 `LogSettings` / `LogLevel`（`error|warn|info|debug|trace`，默认 `info`），`#[serde(default, skip_serializing)]` 把它挡在指纹之外
- `src/config/mod.rs`：加 `log_level(config_path)`，在真正加载配置之前宽松地读一次（文件缺失 / 读不动 / 解析失败都退回 `info`，不在这里失败）
- 新建 `src/cli/logging.rs`：`LogSink` 先把启动阶段的诊断攒在内存，`Event::RunStarted` 一到就 `switch_to(run_dir)`，连同缓冲一起**追加**写进 `<run dir>/log`
- `src/cli/args.rs` 删 `-v` / `-vv`（留一条用例断言它们不再解析）；`record/layout.rs` 加 `LOG`；`run show` 印出日志路径
- `examples/reviewbot.toml` 补一段带注释的 `[log]`
- `cargo test` 325 + 21 + 20 全绿；clippy `-D warnings` 零警告；`cargo fmt --check` 干净

**决策**：
- **日志一个字节都不上 stdout / stderr。** 混在 stdout 上时，「这次 run 的经过」是否完整取决于当时谁在接管道：管道断了就没了，`-q` 一给就没了，`--format json` 为了不弄脏 JSON 也得全压掉——而最需要日志的正是这种非交互、事后才看的场合
- **追加写。** 同一个 run 再进来一次，前一次那半程还在，而那半程正是解释「为什么会有第二次」的东西
- **级别进配置、不进指纹。** 它属于「这台机器怎么配」，不属于「这一次调用」；一旦按调用给，就得在「进指纹」和「配置里有字段不进指纹」之间选，前者意味着想看细一点日志就作废掉全部 checkpoint。所以 `[log]` 是唯一被 `skip_serializing` 摘出去的一段
- **`RUST_LOG` 仍然覆盖它**，那是临时排查的旋钮，不写进任何文件
- **`-q` 保留但改了辖域**：它现在压的是终端（状态屏与摘要），run 目录里那份日志照写
- 为什么不留 `-v`：日志既然只往 run 目录写、事后随时翻得到，就没有「这一次要不要打开它」这个问题

---

## 把文档拉回和代码一致

**意图**：代码全部落地且绿了，散文一个字没动，把 `docs/design.md`、`README.md`、`examples/reviewbot.toml` 和这份对话记录补上；`design.md` 是论证体，照它的语气改，不要摊成变更清单。

**步骤**：
- 起一个临时目录，把 `tests/fixtures/valid.toml` 的 `budget_per_run` 改成 0 跑一次真实的 `review`，取回非 TTY 的逐行输出、再进入一次的 `, from checkpoint` 那一版、以及指纹不符时 stderr 上的 `next:` 三段，照抄进 `design.md`（原来那段 `[1/5]` 是手编的）
- `docs/design.md`：§2 那段「`publish` 兼写报告与发帖是正当的」论证被拆分推翻，重写成「两件事对外部世界的要求相反」；§1/§3/§6/§7/§8/§10/§11/§12/§13 里约二十处 `resume` 逐条改写成「再跑一次同一条命令」，保住每处原本在论证的东西；§5 补 `[log]`；§6 可恢复补内核锁与 run 目录里的 `log`；§6 可观测重写成 traces / log / 终端三条通路；§10 删三个子命令、删 `-v`/`-vv`、`--output-dir` 并回 `review` 那张表、改退出码 5 的下一步、改 `lib.rs` 只剩一个入口
- `README.md`：`Resume, publish, report` 一节换成 `Continuing a run`，flag 表与产物清单补 `log`
- `examples/reviewbot.toml` 补 `[log]`；`tests/fixtures/valid.toml` 不动
- `rg -n "resume|reviewbot publish|reviewbot report|-vv|--out-dir|五个阶段|五阶段" docs README.md examples src` 扫一遍，剩下的都是明写「这个已经删了」的句子
- `cargo test` 325 + 21 + 20 全绿；`cargo clippy --all-targets -- -D warnings` 零警告；`cargo fmt --check` 干净

**决策**：
- **探二进制一律带 `--config` 与 `--runs-dir` 指向临时目录。** 此前有一次探测走了默认路径，把用户真实的 runs 目录删掉了；这条从此是硬规矩，不碰 `~/.reviewbot`，也不删仓库以外的任何东西
- **用真跑出来的输出，不手编示例。** 文档里那段 `[1/5]` 从来没有对应过任何一次真实运行，而 `[N/6]` 这次是从临时目录里抄回来的
- **`design.md` 里那段被推翻的论证是重写不是打补丁。** 它原本论证的是「`publish` 兼写报告与发帖是正当的」，而拆分恰恰是反过来的结论；留着原句改几个词只会得到一段自相矛盾的话
- **`tests/fixtures/valid.toml` 不补 `[log]`。** 它不写这一段，CLI 用例就顺带覆盖了「整段缺失退回 `info`」这条路径；`examples/reviewbot.toml` 是给人抄的文档，那份要写全
- `docs/conversations/001-015` 是历史记录，不动

---

## 状态屏定型，阶段进度收成前缀

**意图**：
- 状态输出要有标题；要说清共有多少文件、已经看了多少；明显循环的动画放在底部活动区
- 裸写 `round 3/12` 让人看不懂，工具调用也不能继续隐形；有充分理由可以采用 TUI 框架
- 读代码时把 stage 改成 enum；阶段既然有固定顺序，`completed_stages: Vec<Stage>` 不是正确形状，`number()` 也不该写六臂 `match`

**步骤**：
- 代码把编号与名字合并为 `domain::Stage`；六个显式判别值保持 checkpoint 编号，序列化保持原阶段名，调用点不再传一对可能配错的参数
- `meta.json` 改记 `completed_through: Option<Stage>`；给人看的完成清单按需从前缀展开，旧 run 缺字段时按未完成重算
- progress 事件改为携带类型化 `Outcome`、worktree 与 tool 完成耗时；终端不再从英文句子反解析状态
- TTY 改为 ratatui inline viewport：标题、六阶段、三活动行固定十行，由只负责绘制的线程约每 100ms 刷新；pipe 只保留 run header、每个 chunk 与每个完成阶段
- `docs/design.md` 重写输出、串行预算约束、可恢复状态与依赖/测试说明，并换入真实 PTY 帧；`README.md` 补齐终端与 pipe 行为

**决策**：
- **用 ratatui inline viewport，不手写 ANSI 重绘。** 屏幕状态、布局与终端接管交给已有抽象，测试可用 `TestBackend` 直接核帧
- **高度固定。** ratatui 创建 inline viewport 后不能调整高度（ratatui#984），所以十行从第一帧就保留；不调用 `insert_before`，因为连续重画时窗口 resize 会把 viewport 重复进 scrollback（ratatui#2666）
- **动画由 draw-only 线程驱动。** 模型调用会阻塞流水线几十秒，事件驱动画面会在最需要反馈时冻结；绘制线程不运行阶段、不调用模型、不碰预算或 checkpoint，因此不违反为保护调用前预算检查而定的串行规则
- **claim 失败退回 pipe。** inline viewport 需要查询光标位置，有些伪终端不回答；这时在 run 日志记 `warn` 并输出追加行，不能让运行过程失声
- **看着屏幕又提了四处，一并改了**：耗时对齐到固定表格列（原来右对齐到终端边缘，时间离自己那行半屏远）；`review` 行改成按文件计数，`piece 2/3` 只在文件真被切开时出现（原来 `chunk 1/4 · file 1/4` 两个数常常相同，读的人只会困惑）；抬头能一行放下就一行、放不下才两行；**收尾那一轮不再报成一轮**——它本来会打出 `round 13/12`，这是四条里唯一的功能 bug，补了一条阶段档用例钉住它
- **抬头以下一律顶到第 0 列，抬头后固定空一行。** 高度按抬头用满两行预留（viewport 锚定后改不了高度，ratatui#984）
- **高度随内容变，靠原地重新锚定。** ratatui 不给已锚定的 inline viewport 改高（#984），但可以把旧块抹掉、光标放回它的第一行、再要一个新高度的 viewport——锚点不变，块不会越改越往下。实测 8 → 9 → 12 行，三帧同一起点。中间试过两版都不够：一版按最高情形恒定预留（`starting` 时挂着满屏空行），一版把余量塞进中间那道空隙（状态行浮在屏幕中段）。问光标位置每次重锚都要做一次，终端不答就留着旧高度
- **第一帧不能只有一个版本号。** run 要先取回 MR 与 diff 才算得出 `run_id`，而 CLI 早就知道模型、目标与检出（配置是它加载的）；这三样先填上，活动行写 `starting` 并计时，且**这一刻不画阶段清单**——还没开始的 run 底下摆六行「未开始」像是卡住了。库那边为此多发一个 `Opening` 事件：那几秒本来在事件流里根本不存在。措辞先试过「正在弄清这是哪次改动」，被否掉了：它在解释实现，而这一刻要的只是一个短词加一个在走的秒表
- **一句话 + `...` + 括号里的秒表（只有等别人时才有秒表）**，是这一行唯一的形状；秒表不用 `·` 隔开——那个分隔号在别处隔的是两件平级的事，时间却是前面那件事的时长；阶段在跑时不带秒表，它的耗时在清单那一行上
- **没有等待可言时，那一行说这个阶段在做什么**（`reading changes...` / `planning the review...` / `merging findings...` / `writing the report...` / `posting comments...`），不重复阶段名——名字在清单里，写两遍等于没答「正在做什么」
- **`waiting for model` / `waiting for cppcheck` / `waiting for conclusion` 是同一个句式**，只有「在等谁」不同（原来写的是 `waiting for the model`，去掉冠词才跟工具那边对齐；中间一版缩成了单个 `waiting`，被否掉）
- **`starting` 那一刻的块就是三行**：抬头、空行、那句 `starting`。中间试过在清单的位置摆「这次 run 打算做什么」（publish / budget / checkers / runs），被否掉——那几行是为了填补固定高度留下的空白而存在的，高度改成随内容变之后它们就没有理由了
- **阶段之间那几秒也要有人认领。** `input` 完成到 `triage` 开始之间，`lib.rs` 在装配两阶段共用的 preamble，布局摘要是一次目录树请求（大项目好几秒）。它不属于任何阶段、清单上没有行，屏幕原来停在「一个阶段完成、底下空着」，看着像卡死。给它一个 `Preparing` 事件，活动行写「正在读仓库布局」
- **「正在启动」从有屏幕那一刻算起，不等库发事件。** 否则最初几帧先画一张全是「未开始」的清单，等 run 开口再收掉，屏幕闪一下
- **跑完不写 `done`。** 那一行只活到整块被抹掉之前的十分之一秒，而紧接着落在同一处的最终摘要把这件事说得更全
- **spinner 后面一个空格。** 一度读到 `⠴awaiting`，怀疑是盲文点字按双宽渲染吃掉了那一格，于是改成两个；抓字节流看过空格一直都在，回过头仍按一个空格定稿
- **`running $tool` 改成 `waiting for $tool`。** 那一行从头到尾只回答一个问题：这次 run 在等什么；`waiting for the model` 是句话，`running cppcheck` 是个标签，两者并排读着像两套语法
- **`round` 只属于等待行。** 它表达模型与工具的一次往返，分母是撞上就提前结束 chunk 的上限，不是预计轮数；正在跑的工具放 spinner 行，已完成工具按 chunk 汇总
- **完成状态只能是前缀。** `completed_through` 排除了不可能的离散集合，并让 `mark_incomplete` 真正把损坏阶段及其全部下游一起作废；后续结论依赖那份不可读 checkpoint，不能单独留下
- **编号取 `#[repr(u8)]` 的显式判别值。** `number()` 直接转换，声明顺序同时给出 `Ord`；名字仍单独映射并作为序列化格式，从而不改旧 trace 与 checkpoint 文件名

---

## 轮数上限交给窗口算，并把余额告诉模型

**意图**：
- 一次真实 run 里四个文件有三个撞上 `max_tool_rounds = 12` 被掐断，问清楚模型在那 12 轮里做了什么、怎么解决
- 提高配置里的限制并同步示例、测试与本机配置；最好能**删掉这几个配置项**，让代码算出最佳值（交给模型规划也行）
- 告诉模型轮数限制

**步骤**

1. 查 run `e5a2bfb5d0b08ecd` 的 trace 与日志：模型每轮只发 1～2 个调用，12 轮换来 15 次查阅，全是正当调查（读本文件、列同目录、读兄弟模块与 kpatch 对照实现、搜被删方法的调用者）；70 次调用里 9 次空手而回，最大单次输出 29KB、平均 3KB；整场花 ¥0.44/¥10，输入 token 87% 命中缓存
2. `stage::triage` 的 `ChunkLimit` 改成 `Window`：`可用 = 窗口 − 输出额度 − 量出的骨架 − headroom`，`工作大小 = min(可用, max_chunk_tokens)`，`单轮上限 = 工作大小`，`轮数 = 可用 / 工作大小 − 1`；没有检视类工具答得上来时轮数为 1
3. `Window` 随 `TriagePlan` 进 checkpoint，`review` 读它而不再读配置——两边从此不可能算出不同的数
4. 删 `[review].max_tool_rounds`；`max_tool_output_bytes` 留下但改口径为「一次工具回复的上限」，因为 registry 在窗口划分之前就要这个数
5. 每轮结束往 `input` 追加 `{used} of {total} rounds used.`，下一轮是最后一轮时再加一句；`src/prompts/review.md` 里原来只说「用完会通知你」，改成「每轮都会告诉你用掉几轮」
6. 同步 `examples/reviewbot.toml`、`tests/fixtures/valid.toml`、各处内联配置与 `~/.reviewbot/config.toml`；`docs/design.md` §5 的必填论证、§7 的窗口公式、§11 的测试清单与 `README.md` 一并改写
7. 拿本机配置实测：带检出时算出 **24 轮、单轮 80KB**；不带检出（diff only）是 1 轮。336 + 32 + 20 全绿，clippy 与 fmt 干净

**决策**

- **轮数上限是防死循环的护栏，不是成本闸门。** 钱由 `budget_per_run` 在每次调用前拦住，所以把轮数定紧一分钱也省不下，只会让评审看得更少——那次撞顶的 run 只花了预算的 4%。护栏既然只防死循环，就可以从窗口算，不必让人猜
- **diff 先拿够，剩下的全换轮数，单轮上限等于一个分片的量。** 于是「要更多轮数就把分片切小」，两者在同一个窗口里竞争，关系说得清；而从前那对配置项相乘扣走窗口，没人能说清抬一个的代价
- **`max_tool_output_bytes` 不删。** 它是关于工具的事实（一份诊断读多少才有用），换个模型也是同一个答案，而且 registry 构造时窗口还没划分。删掉它等于把一个说得清的数也变成推导
- **不交给模型规划轮数。** 它同样不知道该几轮，还得多花一次调用去问，而问出来的数一样是猜的
- **`Window` 走 checkpoint 而不是各自重算。** 「`triage` 预留的和 `review` 花的必须是同一份」从一条注释里的嘱咐变成了结构上的事实
- **推翻了 design.md §5 的一条旧论证。** 原文说每个数值项都必填、代码不留默认值，理由是正确取值取决于代码看不见的东西；轮数恰恰相反——它只受 `[[model]].context_window_tokens` 约束，而那就写在配置里，代码看得见。所以那一段是重写，不是打补丁
- **每轮报余额。** prompt 一直要求模型省着用轮数，却从没说过余额是多少，等于让它盲花一笔看不见的预算。这句话进 `input` 不进 `instructions`：后者要逐字相同以命中厂商缓存
