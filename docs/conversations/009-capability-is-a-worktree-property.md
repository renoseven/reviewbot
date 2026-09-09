# 工具全量注册，能力归 worktree

## 把「这次能查什么」从注册与否改成 worktree 的属性

**意图**：这次 run 能查什么，现在靠「注册或不注册工具」来表达。应改成：工具全量注册，能力是 worktree 的属性，由模型决定何时调用哪个工具。随后追加四条：能力应该做成「worktree 提供什么」与「tool 需要什么」的匹配；能力表达用 bitflag；tool 不该区分 command 与 builtin，command 就是一种能跑外部命令的 builtin；配置要不要跟着改、prompt 该放在哪里。

> 这推翻的是 [007](007-one-worktree-and-tool-contract) 定下的「前置条件不满足就整条不注册」。

**步骤**

1. `worktree`：`Reach::provides() -> Abilities`（`bitflags`，三位 `CONTENT` / `SEARCH` / `CHECKOUT`）与 `Reach::unmet(needs)`。`worktree` 里不再有一个字是给模型看的。新增 `Widest`——提供全部能力、拒绝一切读取的桩，只给 `tool list` 用。
2. `tool/availability.rs`（新）：给模型看的措辞全在这里——描述用的短标记、调用被拒时的完整理由、`tool list` 的前置条件、报告里「这次少了什么」。一条用例遍历所有 worktree 形状，断言每句拒绝都带「这是本次 run 的限制，不是关于代码的答案」。
3. `Tool` trait：`requires_checkout()` / `requires_build()`（本来就没人调）换成 `needs() -> Abilities` 加 `unavailable()`，后者从前者派生。`Registry::execute` 在派发前拦下。`names_with_purpose` → `usable_with_purpose`。
4. `stage::build_tools` 搬成 `tool::build`，三处 `if` 全删。`tool::inventory` 改成在 `Widest` 上调 `tool::build`、再从 trait 上读契约，`BuiltinSpec` 整个删掉——`tool list` 现在印的就是评审真会注册的东西。`PathPolicy::for_settings` 收掉两处重复装配。
5. 改名：`tool/builtin.rs` → `tool/content.rs`，`BUILTIN_TOOL_NAMES` → `RESERVED_TOOL_NAMES`。
6. prompt：删 `capabilities-none.md`；`capabilities.md` 末尾加 `{{worktree}}` 槽，填 `worktree-{checkout,fetched,empty}.md` 三份模板之一。`review.md` 改掉「列出的等于能调的」「没列出的这次就不存在」，新增一段讲 `NOT AVAILABLE THIS RUN` 是本次 run 的限制、不是关于代码的答案。
7. 验证：310 个用例全过，clippy 零告警，fmt 已跑。`tool list` 输出与改造前逐字相同（证明统一没改契约）。
8. 文档：`design.md` §5 tools 边界、§6 可扩展（整段重写）、§6 指纹与拒绝理由、§7 triage 余量与 prompt 能力段、§8 那张表、§11 测试、§13 验收；`README.md` 三处；`examples/reviewbot.toml` 两条注释；`tool list` 帮助与 `--worktree` 长说明。

**决策**

- **推翻 007 的理由不是它错了，是它的代价藏在别处**。「模型看得见的永远等于真能调的」本身成立，但工具集随 run 变意味着：prompt 只能泛泛地讲工具；模型只能从一张无从核对的清单里反推自己的能力；而「这次不值得去看工程」这个判断本该由模型看着 diff 做。
- **provides / needs 匹配，而不是逐个工具写规则**：把「提不提供」（集合运算，可单独测）和「提供不了该怎么说」（另一张表）拆开。再加一位能力，改 `provides()` 一位加一条措辞，没有工具要回头改。
- **bitflag 用 `bitflags` crate**。我原本主张三位不值得加依赖、手写 `Abilities(u8)` 即可，用户要求引入，照办。
- **许可不是能力**：`requires_build` 要的是 `[security].allow_build_tools`，运维授予、启动即失败，worktree 既不提供也不收回。混进能力集合会让「这次做不到」和「你没批准」共用一套话。**因此配置不用改**——`requires_checkout` 是配置里唯一会用到的那一位，为一个永远只有一个元素的集合换成 `requires = [...]` 是负收益。
- **prompt 的归属边界一次讲死**：`src/prompts/` 装 reviewbot 对模型下的指令；工具自己的话（描述与拒绝）跟工具走；`worktree` 只报事实。所以措辞从 `Reach` 搬进 `tool/availability.rs`，而那段 worktree 说明——不属于任何工具、是模型整段读的散文——回到 `src/prompts/` 成为三份模板。
- **能力段模板从两份减到一份**：清单不再随 run 变，「什么都查不了」是同一张清单配另一段 worktree 说明，所以它是一个槽不是第二份模板。
- **`RESERVED_TOOL_NAMES` 留着**：它是安全边界（配置条目不许盖掉编译进去的名字），只是名字改成说清它是什么，而不是它从哪来。
- **模块改名到此为止**：`builtin.rs` → `content.rs` 是因为那个名字在维持一个不存在的分类；`command.rs` 照旧，它本来就是按「做什么」命名的。

---

## run `0e5f913bd60ec90f`：什么都没扫到，报告却像干净通过

**意图**：用户拿本地 diff 跑了一次，说「直接扫不到文件了」，要求一并修复。追问后确认口径：空 worktree 是正常流程，只审 diff 就行，但**不要因为没有能力读取文件就跳过模型检视**；只有 diff 切片时模型也不能完全不检视。

**步骤**

1. 先读痕迹再改代码。diff 读到了：5 个文件正常解析，4 个进评审，每片都拿到完整 diff 正文。缺的是 diff 之外的一切——四份 trace 的 `context_files` 全是 0，两个 C 检查器（这个 diff 还是 Rust）也跑不了。原因是输入 `/tmp/1.diff`、没给 `--worktree`、没有平台，worktree 是空目录。既有设计，不是这次改坏的。
2. 确认模型检视没被跳过：四个分片都发了模型调用，能力不足只影响布局摘要与「开工前把文件放进 worktree」那一步，不碰分片循环。
3. **报告说不清**——这是真病。`overall 100/100` 加一句 "appears safe to merge"，`unreviewed` / `unproduced` / `cut_short` 全空，`unused_checkers` 也空**而且是对的**（没有检查器答得上来，「有检查器却没调」天然不触发）。于是一次连一个文件都读不到的 run 和一次干净通过长得一模一样。`ReviewOutput` 加 `unavailable`，评审开始前记下，走进 `report.md`（印在模型那段话之前）与 `summary.json`。
4. **拒绝理由重复六遍**——我上一轮直接搞出来的回归：250 词的完整理由挂在六个工具的描述上。改成描述只留一句短话加 `NOT AVAILABLE THIS RUN`，完整理由在 worktree 那段说明里讲一次、在真被调用时再回一次。
5. **「确认不了」被读成了「不能提」**。真跑一次看模型输出：它把 `get_file`/`fput` 每条可见路径推了一遍、写了 35KB，最后写「Establishing them is required before reporting a leak」然后一条没提。查 prompt，出口有两处：第 3 段「或者干脆不提」、第 6 段「宁可漏报不要错报」。在只有 diff 的 run 上任何外部前提都永远确认不了，这两句合起来就是「什么都别提」。两处都改口径，空 worktree 模板里再说一遍。
6. 再跑一次（`--run-id blind-check-1`，因为 prompt 不进指纹、否则命中同一个 run）。模型明确接住了新指令（"instructions caution to not let tool unavailability stop filing where diff gives reason"），然后自己推翻了那个怀疑：可见路径上 `get_file`/`fput` 是配平的，拒绝臆造一个看不见的提前 return。**这个结论是对的**，0 条意见站得住。
7. 但这一轮暴露了新问题：CMakeLists 那片调 `submit_comment` 漏了 `confidence_score`，被拒后**直接改调 `finish_review`**，意见丢了。给 `submit_comment` / `submit_summary` 的参数拒绝补上「什么都没记下，带着同一条发现重发，别拿 `finish_review` 回答这个」，两条用例钉住。

**决策**

- **修的是「说不清」，不是「扫不到」**。只审 diff 是正常跑法，退出码不动、分片不减；报告只是不许把它说成别的。所以没有默认拿 cwd 当 worktree，也没有改成启动即失败——那两条都是替用户做决定。
- **`unavailable` 印在模型那段话之前**，因为它框住那段话的全部；一个能看遍全部的 run 一个字都不印，不让常规路径为一个没什么可说明的说明买单。
- **缺内容只说最根本的那一条**：没内容必然连带没搜索、没检出，三句都说「这里没有代码」会把解释淹掉。
- **不确定由 `confidence_score` 承担，不由沉默承担**：diff 给了理由、只是某个前提这次核不了的，属于 40–69 那一档并点名核不了的是哪条；沉默只留给「压根没理由相信它有问题」。
- **指令要跟拒绝一起走**。「拒绝不是停止信号」写在 prompt 第 3 段，离模型读到拒绝隔着几千 token，实测压不住。放进拒绝理由本身才有用。
- **不给「模型该不该提这一条」设代码闸门**。这次丢掉的恰好是条垃圾意见，但那是运气；能改的只有措辞和分档，不能改的是替模型判断。

---

## 加一个严重程度，和置信度并列

**意图**：现在只有置信度，应该增加一个严重程度，也让模型决定；summary 的时候可以根据严重程度和置信度算最后的总分。确认口径时先答「reviewbot 算分但算法要科学一点、summary 还是模型给」、severity 用枚举，随后改为：整体流程跟置信度保持一致、severity 用自己的一套档位，分数也让模型给——反正本来全都是模型给的。

**步骤**

1. `domain/severity.rs`：`Severity{Trivial,Minor,Major,Critical}`，照 `confidence.rs` 的样子写。`Comment` 加 `severity` 与 `severity_score`。
2. `submit_comment` 的 schema 加 `severity_score`（必填，0–100），两条参数说明互相点名对方「那是另一个数」。
3. `merge`：解析 `severity_score`（走同一个 `whole_score`）、`severity_band()` 用同一套端点、`score_of` 加字段名参数（trace 里要看得出是哪个数缺了）、排序改成「先严重、再确定」、去重留 `(severity, confidence)` 靠前的那条、`findings_json` 两个数都带上。
4. `review.md` 第 6 段拆成两张区间表并写明「它们本来就该不一致」；`summary.md` 让打分轮权衡两者；报告标题改成 `## [major 85% / high 70%]`，图例跟着改；`summary.json` 加 `by_severity`；CLI 摘要分两行印。
5. 用例：一条 20/99 的琐事必须排在 95/45 的致命缺陷之后；打分轮的输入里两个数都在。312 个用例全过，clippy 零告警。
6. 真跑一次验证（`--run-id sev-check-1`）：`[major 85% / high 70%]`，两个数如期不一致，总分 45。

**决策**

- **两个数不合成一个**。用户先要「reviewbot 算总分」，后改成仍由模型给；后者也正好保住 `rust-coding.mdc` 那条「不改模型给的数」。理由记下来：公式看不出「三条中危其实是同一个设计问题的三个面」，而合成本身就是 reviewbot 替读的人做权衡，那个权衡因人因项目而异。reviewbot 只做排序，不做加权。
- **severity 用 0–100 而不是枚举**。用户先选枚举，后改口「分数也让模型给」。这样也更好：枚举要参与任何排序或权衡，都得由 reviewbot 给每档指定一个数——那就成了 reviewbot 发明的数。
- **两轴同一套区间端点（90/70/40），但档位名各用一套**。端点一致是为了只学一张表；名字分开是因为一条意见上出现两个「high」，读的人得先分清哪个是哪个。
- **排序主键换成严重程度**。这是加这一轴的全部意义：按置信度排，一条确凿的命名问题就压在一条不确定的内存越界前面，而忙的 MR 上只有开头几条会被读。
- **打分轮必须看见两个数**：只给置信度的话，「一条确凿的琐事」和「一条不确定的致命缺陷」在它眼里差不多。
