# 调查轮次：不收紧上限，削派活

经验命令始终是 `./target/debug/reviewbot review /tmp/1.diff --worktree ~/syscare`。同一 `run_id` `5c1e10c145dccda4`（输入身份 + `head_sha`）prune 后再跑。不要另起一份叠着跑，除非明确要求。

## 轮次多是观光，不收紧上限

**意图**：看这次为什么轮次这么多。怎么压，不要收紧上限，也不让分片共享对话。不要写请早停。再看为什么超过次数、为什么跑了这么多 24。同一份再排查为什么超过次数。

**步骤**：
- 读那次 run 的 log、triage、四份 review trace
- `review.md`：「spend what is left deliberately」改成上限不是配额
- 删 `rounds-left.md`；普通轮次不再报余额，只在下一轮是最后一轮时发 `rounds-last.md`
- 同 `run_id` prune 后对照多盘：收紧查阅措辞时仍逛或撞顶；派活削完 overall 28，5 条，0.4219 CNY，`sys.rs` / `mod.rs` 仍打满调查额度

**决策**：
- **`rounds=24` 是每个 chunk 的调查上限，不是 triage 跑了 24 轮。** 窗口减去预留、按 `max_chunk_tokens` 能再装几份；1M 窗口这次就是 24。上限防死循环，花钱靠 `budget_per_run`。016 已否过把轮数变回旋钮
- **撞顶不是突破 24。** 调查打满后再加一轮收尾（log 上的 round 25，只挂 submit / finish）。状态栏 24/24 含最后一次 finish：交付不计入 `rounds`，发送前用 `rounds+1` 标号
- **轮次多是每轮一两个、顺着刚打开的文件往下走。** 派活削完这盘没有 C 检查器打 Rust。`sys.rs`：本文件 → 清单上的 `mod.rs` → `ioctl_dev.h`→`.c`→`patch_manage.c` / `patch_load.c`。`mod.rs`：清单 sibling、`manager`、`kpatch`、`kernel.rs`、`Cargo.toml`、`main.rs`。对照：`target.rs` 9 轮交 1 条并 finish，`config.rs` 3 轮 finish
- **直接相关没守住。** 定义或调用方才算；头文件背后的实现、清单 sibling、内核 ioctl，prompt 已写成不算。闸要模型自己每跳判，手续（搜到路径就读）比闸好执行
- **不写请早停。** 写过，总轮次还更高。要禁的是离开这次改动去观光
- **为什么不是半程再插「该收了」、也不是一次多路径读：** 猜一个数两边都贵；一轮本来就能并排打多个读

---

## 拿掉目录摘要

**意图**：全局项目背景给模型是怎么说的。是不是不太好，该怎么改；或者直接不包含目录。改。

**步骤**：
- 读 `orient.rs` 与那次 instructions：没有产品介绍，只有变更清单、目录摘要、worktree 一句
- 拿掉 `{{layout}}` 与 digest；`Orientation` 只留改动清单；删 `Preparing` / 「正在读仓库布局」
- README、`design.md` §7、相关断言跟着改

**决策**：
- **没有 README 式背景。** 本地 diff 也没有作者说明
- **目录摘要整块拿掉。** 按全仓文件数排名再让模型「对着摘要去列」，是一张旅游图。改动清单已经写出附近路径；两层附近目录不过是前缀再说一遍

---

## 削派活，闸留下

**意图**：先改提示词，禁止无意义的下一跳。`rounds-left.md` 不要重申。没有限制一个文件只能报一条吧？限制一下不要参考太多文件，看直接相关的。查阅从「涉及到」改成直接相关，加强、回退、再收紧消歧义。查阅这条是不是也删了比较好——不告诉模型查阅，它自己应该也会查阅。删除：先确认路径，不知大小先问，分片。顺序只写先本地、没有再 fetch，后面怎么做不要写。`worth fetching before you file anything` 要删。判断和全文是否矛盾就按路径把整份读出来这个呢。还有没有派活类的。miss 了就去搜仓也可以删，但要让模型知道本地是 repo cache。先读描述再调用删除。工具描述只陈述作用，不要提及缓存。§3 写成事实：轮次有上限、下一轮是最后一轮时会告诉你、用尽了就按已有的收。看一下 worktree 的描述是否符合当前设计；checkout、fetched、empty 这几个名字也跟着改。不要再让 C 检查器打到不是那种语言的文件上——约束写进工具描述，不要在代码里按扩展名闸，不要把 Rust 单独拎出来。先跑检查器改成先跑对应语言的，后又整句删掉。像这种指导模型做事的提示词还有哪些。

**步骤**：
- 查阅闸改过几版（直接相关两案、变更出发点、清单优先）并同 `run_id` 复跑；观光还在
- `review.md`：删查阅派活、路径/大小/分片手续、「worth fetching」、「Read each description」、「Decide what to use」、「先跑检查器」；闸留下；「先本地，没有再 fetch」留下
- `split.md`：只留第几片、整份还能按路径读
- 八条内容工具描述只写做什么；`local_side` 删掉；cache / checkout 不写进每条描述
- worktree 三段按形状改准并改名：`worktree-local.md` / `worktree-cache.md` / `worktree-empty.md`
- §3 轮次三句改成事实；「ask for everything」只留在 `rounds-last.md`
- `cppcheck` / `typecheck` 描述补上只收 C/C++；用例锁住这两句，并断言没有 `Rust` / `.rs`。不写扩展名名单，也不在 `Path` / `CommandTool` 上闸
- `design.md`、README、断言跟着改

**决策**：
- **不派查阅活。** 模型自己会查。去取定义/调用方、「你只看到一个文件所以该你去查」，和「先跑检查器」一类。闸留下：只能看直接相关、两案、不是相关的、40–69 是替代
- **直接相关就是那两样。** 本文件变更行用到的符号定义，或这次新增/改过的函数的调用方。刚点到的文件不算。清单上的另一个文件是那一轮的事。打开头文件再去读实现，是二跳
- **这次只改提示词，不在工具里拒。** 搜到路径不是去读的理由；`use` / `mod` / 单独类型名不算「用到」；清单即使点了名也不从这里打开。不写最多 N 个文件、不写 “Do not follow”（会被读成不要顺着代码看）
- **顺序只写到 fetch。** 先确认路径、不知大小先问、分片怎么读、miss 了去搜仓，都不写
- **描述写清 cache，不派下一步。** 本地只覆盖已取回的、miss 不是仓里没有。cache 还是检出只写在 worktree 那段
- **worktree 三段只写这次是什么。** 检出：整仓在盘上，不说每个能力都能答。cache：列仓库走平台、列/搜本地只覆盖盘上已有的。空：检视类会拒，交付还在
- **§3 轮次只写事实。** 并排打完还要的，只在 `rounds-last.md` 派一次
- **变更清单不派取回。** 留下的是事实：清单上的文件在 `head_sha` 已带这次改动；清单用来判断手上这份，不是去评别的文件
- **余额整段删掉。** 报「用掉几轮」会被当成配额花掉
- **一文件可以报多条。** 「one at a time」是一条意见一次调用，不是配额
- **适用语言是描述的一部分。** 只写收什么，不点名 Rust。点名会把闸读成「这份仓库的例外」
- **不在代码里按扩展名拒检查器。** 闸若挂在 `Path` / `allow_extensions` 上，`.rs` 会读不到、`submit_comment.path` 也会被拒。真要模型发不出来，得从这次请求的 `tools` 里拿掉那个工具
- **「先跑」整句删掉。** 钉在对应语言上还是调了。义务句在逼第一轮硬凑；检查器仍在清单里，按需再调
- **派活削完，24 还在。** C 检查器没再打 `.rs`。`sys.rs` / `mod.rs` 照样撞顶：手续比闸好执行

---

## 收工只有 finish；submit 交意见

**意图**：正文写成评审、没调工具就当收工了。一次 review 以 `finish_review` 结尾，有缺陷才 `submit_comment`。`submit_comment` 单独一轮就被当成收工这个问题怎么解决。能不能把 finish 当作唯一结束语义，直接不接收空的 submit；submit 与 finish 这种流程控制也不应当记入 round。提示词中应表明完成时应当调用 finish，不要有歧义。`diff_lines` 写成了文本；`review.md` 里关于整数的措辞再改一下。在没有对应语言的外部工具确认编译问题的场景下，尽量少提编译相关的问题。同一份再跑（overall 72，1 条 minor），为什么把 Upatch 的笔误放过去了。

**步骤**：
- 空正文一次结束；有正文没调用的，按截断那样重问一次（`after-prose.md`）
- `run_round` 只在 `finished` 时结束分片；有内容的 `submit_comment` 继续
- 空 `submit_comment` 改成拒绝并回传 `finish_review`，不再 `.finishing()`
- 一轮里只有 Delivery 用途的工具时不 `rounds += 1`，也不因此撞顶 / 发最后一轮提醒
- `review.md` 与 `after-prose` / `after-truncate` / `conclude`、两条工具描述：做完必须调 `finish_review`（交过最后一条也要调）；要的是文件行号，不是那一行的正文，不写 integers
- `review.md` 第 2 段：没有对应语言的检查器能确认编译问题时，尽量少提编译相关意见。不设闸
- `submit.rs`：依据字段贴成 diff 正文就拒并 `resend`。语言闸（没有检查器引文就拒「编不过」）加过又删掉
- 读这次 report / log / 四份 trace：`mod.rs` 最后一轮正文写明看见了 `Kpatch:` 却不交

**决策**：
- **正文当收工是循环的洞。** 没有 function call 就 `break` 成空意见。重问一次
- **结束信号只有 `finish_review`。** 有内容的 submit 交一条意见，空 submit 拒。从前一轮只有交付就算收工，模型交完第一条分片就停
- **完成时必须调 finish，交过意见也不例外。** 「没发现才 finish / instead」会把 finish 读成空结果专用通道
- **submit / finish 不记入调查轮数。** 交意见和收工是流程控制，不该把剩下的查阅额度吃掉
- **`diff_lines`（后来的 `evidence.lines`）要的是文件行号。** 字段名像 hunk 正文，模型把 `+` 行贴进去。拒绝句写明改参数再交
- **不留语言闸，也不按类堆「第几条该交」。** 叠上去换的是假意见；整段「What is a finding」回退
- **放过的那条是 `get_patch_status` 的前缀。** `+ "Kpatch: Failed to get patch status, {}"`，同文件其余都是 `"Upatch: Failed to ..."`。看见了，正文写「cosmetic… does not meet the bar」，是第 2 节排序，不是编排漏了。不为这串字加规则（016 已否过）

---

## 不记没调检查器；Local 能搜不算缺口

**意图**：报告写「搜不了仓库」。C 检查器打到 Rust 上。有检查器没调要不要留痕。

**步骤**：
- `Worktree::Local` 的 `went_without` 改为空；Empty / Cache 的真缺口留下
- `unused_checkers` 从循环、checkpoint、`ReviewOutput` 删掉

**决策**：
- **完整检出没有平台搜索不是覆盖缺口。** 本地正则能搜整棵树。真缺口是 Empty 读不到文件，或 Cache 且平台也搜不了
- **`unused_checkers` 删掉。** 适用与否是模型按描述判断的；C 检查器没打到 Rust 上不是缺口，checkpoint 里那份名单没有人看

---

## waiting for conclusion 粘住

**意图**：后面 waiting for model 都变成 waiting for conclusion 了，排查相关问题。

**步骤**：
- 读 `status.rs` 的 `concluding` 与 `screen.rs` 等待行
- `Event::Chunk` 改为先 `forget_chunk()`（清掉收尾标志）；`Event::Round` 也把 `concluding` 置回 false
- 加跨文件断言；`design.md` §11 状态屏跟着写

**决策**：
- **循环没把后一个文件收尾，是状态栏把旗留着了。** 散文重问或撞顶会发 `Event::Concluding`；下一文件仍发 `Round`，但 `Chunk` / `Round` 都不清 `state.concluding`
- **收尾只属于那个文件。** 换文件或新的调查轮就要回到 `waiting for model`

---

## 挂点进报告，依据叫 evidence.lines

**意图**：`report.md` 里面为什么不是文件行数范围啊。这是不是个 bug。改。`diff_lines` 与 diff 对应的行号是不是一件事。先改成 `ref_lines`。挂点才是更重要的信息，为什么不在报告中。`line` / `end_line` 也容易混，写清楚。根据语义改名字。`ref_lines` 是不是叫 `src_lines` 更合适。依据就叫 `evidences`。`evidences` 是否是可数名词。`line` / `end_line` 改成成对的。

**步骤**：
- 读这次 `report.md`、`4-merge.json`、各 trace 的 `submit_comment` 与 merge 备注
- 证据字段先后叫过 `diff_lines`、`ref_lines`、`evidences`，现改成 `evidence.lines`；历史对话文件不改
- 挂点改为必填的 `start_line` + 可选的 `end_line`；`review.md` 与工具描述写明这一对是报告标题和 MR 线程，并和 `evidence.lines` 对举
- `code_span`：有更靠后的 `end_line` 写成 `path:88-90`；报告标题改走它。merge 仍用 `evidence.lines` 做范围核与对齐兜底；报告不印依据
- 测试与 `design.md` 跟着改

**决策**：
- **标题不印 `end_line` 是漏了。** 类型就是区间，发 MR 多行评论会用，报告只拼 `:{line}`
- **不能拿依据的最小最大当区间。** `[52,53,104,158]` 是四处同一问题，不是 52–158 一段。这次五条看起来像单行，是模型没交挂点，merge 拿第一条依据顶上
- **`start_line` / `end_line` 是挂点，`evidence.lines` 是依据。** 挂点可以是上下文行；依据至少一行必须落在变更行上，否则整条丢。`start_line` 必填，省掉它标题就会退化成依据
- **挂点字段成对，不加第三个。** GitHub 发帖仍从这一对映射：多行时他们的 `start_line` 是我们的起点、他们的 `line` 是我们的 `end_line`
- **`evidence` 不可数，可数的是行。** 不叫 `evidences`。`diff_lines` 像 hunk 正文，`ref_lines` 像引用，`src_lines` 只标编号会和挂点并回去
- **编号同一套，职责不是一件事。** `evidence.lines` 要的是新文件里的行号，和 hunk `+` 侧数出来的是同一个数，不是 diff 文本第几行，也不是 `+` 那一行的正文

---

## 删掉 cut_short

**意图**：`cut_short` 这个功能直接删了。`why: "the tool loop reached its ceiling of 6 rounds"` 这个还有地方显示么。

**步骤**：
- 去掉 `CutShort`、`ReviewOutput.cut_short`、`Summary.cut_short`、报告里的「investigation cut short」
- 撞顶和截断仍走收尾轮；`Event::Concluding { why }` 还带着这句话，状态栏丢掉它，只显示 `waiting for conclusion...`
- `design.md` 与断言跟着改
- 查过剩余出口：屏幕和 `report.md` / `summary.json` 不印；`conclude()` 的 `tracing::warn!` 与该分片 review trace notes 还写同一句

**决策**：
- **收尾还在，名单不要了。** 调查用尽仍要模型按已有的交；那不是一份覆盖缺口。未评审只列一次都没打开过的文件

---

## prompts 保持 markdown 惯用法；按职责归一

**意图**：prompts 需要是 markdown 格式么——是保持 md 写法好，还是纯文本好。不是问用什么引擎。现在有些在编码里面、有些在文件里面，而且有很多文件，怎么归一。改一下。

**步骤**：
- `split-findings` / `split-note` / `split-handoff` 的说明并回 `split.md`；槽只填列表和那一句话
- 空槽要连说明一起消失：同一份模板里用 `{{#槽}}` … `{{/槽}}` 标界线，不是条件句
- 工具描述、拒绝句、Fence / CappedList 不动；扩展名仍是 `.md`

**决策**：
- **长指令保持现在这点 markdown 惯用法。** 编号小标题、硬规则加粗、分数表、字段反引号。这是给模型划层次，不是文档格式。收成纯散文，六段和四档会糊成一堵墙
- **短通知保持一句散文。** `conclude` / `rounds-last` / worktree 不必再套加粗或标题
- **不加** `#` 标题、斜体、`---`、围栏。围栏只给作者自述，走 `Fence`
- **按职责分，不收成一种载体。** 文章和通知在 `src/prompts/`；工具契约跟工具走；形状留在 `prompt.rs`。文件多是「模板里不放 if」的代价；只把已经是 `split.md` 槽位的说明写回去

---

## 未决

- 观光还在：闸仍要模型每跳自判，手续比闸好执行。不收紧 24，不在工具里拒下一跳
- ceiling 那句还在 log WARN 与 trace notes；屏幕和报告不印。没要求再删
