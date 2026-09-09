# 一个 worktree、工具契约、prompt 模板化

## 按一份 46 条决策清单改造基线

**意图**：仓库已是基线，`.cursor/rules/` 已按最终口径改过、不要再动。给了一份编号 1–46 的决策清单（只写「是什么」和「为什么」，类型怎么切、文件怎么放、函数签名什么样由实施者按当前代码的形状定）。要求：先通读，出一张「做 / 跳过（基线已满足）」的对照表；然后**不按清单编号从头扫**，按「先钉住独立缺陷 → 再动内容来源这块地基 → 然后工具契约 → 然后 prompt → 最后收尾项」的顺序落地；验证先 `cargo test` 再 clippy，最后 fmt；文档、对话记录、逐项报告放最后。不要顺手做结构重构：不重排模块、不搬文件、不把自由函数改写成方法、不合并伴生类型、不动测试与渲染代码的组织方式。未轮到的条目不要预改，基线已满足的只在报告里写一句理由。

> 清单本身只存在于对话里，没有进仓库，所以下面不引用它的编号——每一块都按它做的事命名。对照表的结论是：30 条要做，16 条基线已满足。

**步骤**

1. 通读全部源码（约 24k 行）与 `docs/design.md`，出对照表。判为「跳过」的是基线已实现的那些：一文件一分片、切片留痕与交接、改动清单、布局摘要、拒绝「别的文件写了什么」式的摘要、整轮 prompt 逐字节相同、自述在入口规范化、argv 数组不过 shell、裸字符串参数启动即拒、外部命令就是一个 tool。
2. **先钉住两处独立缺陷**（有用例、不碰主干）：配置里那份内建工具保留名单与实际注册数了一遍是对得上的（原来 10 条），缺的只是那条防漂移用例，补上后现在双向相等；`SubmitSummary::read` 改成拒绝全空白 `summary`（原来去掉空白后当「没有总结」收下），`merge` 侧补两条用例——被拒后重问一次、两次都空白记为未打分。
3. **内容来源合成一个 worktree**（后面所有工具名与配置名的地基）。`worktree` 模块重写：`Checkout`（命令行给的检出，只读、完整、本地正则搜索）与 `FetchedWorktree`（run 目录下的 `worktree/`，缺文件时按 `head_sha` 从 `RepoSource` 取回落盘）；`Reach { Content, Search }` 决定注册与描述措辞。`Adapters.worktree` 从 `Option` 变必有，`open_worktree(run_dir)` 在 `review_with` / `resume_with` 里绑定目录。八个对称的内建 tool 合成四个：`list_files` / `stat_file` / `read_file` / `search_code`。同一批把配置项改齐：`requires_checkout`、`max_file_bytes`、`max_files_per_listing`、`max_hits_per_search`、`skip_files_over_bytes`、`budget_per_run`、`context_window_tokens`、三个 `_per_1m_tokens`，连带示例配置、测试 fixture、内嵌配置串、命令输出的 JSON 键、`README.md`、`docs/design.md`。
4. **工具契约**：新增 `src/tool/signature.rs`——参数声明（`Parameter` / `Shape`，支持嵌套对象与数组）是源头，给模型的 JSON Schema 与进来的校验都由它派生，全仓库只此一处写 schema；被字符串化的整数在任意深度按整数读。`Origin{Builtin,Config}` 换成 `Purpose{Content,Check,Delivery}`，新增 `Round{Investigation,Conclusion,Scoring}` 与 `Registry::schemas_for(round)`；`SubmitSummary` 也进 registry，`merge` 改从 registry 取打分轮的工具，不再手写一份 schema。`src/prompts/review.md` 删掉那批已不存在的工具名（`*_repo` / `*_worktree` / `stat_*_file`），补用例断言 prompt 本体不提任何未注册的工具名。
5. **prompt 模板化**：新增 `src/stage/prompt.rs`——`Template` 加 `{{槽}}`（未声明的槽与装配完仍空的槽都是 `PromptError`；空槽连它占的空行一起消失；模板里没有条件、循环、嵌套，能力段做成两份模板而不是一个 `if`），三个共用渲染件 `Fence`（正文里伪造结束行会被换掉）、`CappedList`（Keep::First/Last × Counted/Said/Silent）、`code_ref`。prompt 拆成 `src/prompts/` 下 12 份模板。作者自述那三句话原先在 `review.md` 与 `review.rs` 各写一遍，只留模板那一份。补用例断言两个分片的 `instructions` 与自述逐字节相同、且不含未填的 `{{`。
6. **收尾项**：`Trace.checks` 从 `Vec<String>` 变 `Vec<Check{stage,note}>`，`merge` 重跑只 `forget(NAME)`（原来清空全部，把 `review` 记的会话经过一起抹掉）；`tool list` 改印契约（用途分组、调用签名、逐参数一行、轮次、前置条件），不再印「是否注册」，`--format json` 同构，帮助文案跟着改；`args.rs` 补齐每个子命令与参数的帮助，四条给长说明，加一条遍历整棵命令树的用例；`ConfigError::Parse` 里的 `toml::de::Error` 装箱，`Error` 从 128 字节降到 104，10 条 `result_large_err` 告警清零，另修基线本来就有的 3 条告警（`sort_by_key`、8 参数函数收成 `Finished`、`needless_borrows`），全程零抑制，并补一条 `size_of::<Error>() < 128` 的用例。
7. 验证：324 个用例全过，`cargo clippy --all-targets` 零告警，`cargo fmt` 已跑。`docs/design.md` 的 §2 模块、§4 Trace、§5 配置、§6 可扩展与安全、§7 prompt 与汇总打分、§8（整节重写成「内容来源：一次 run 一个 worktree」）、§10 命令与帮助、§11 测试、§13 验收、§14 空白，以及 `README.md`，都改到最终状态。

**决策**

- **worktree 是唯一来源，平台 API 降级成它的一个属性**。三条理由：工具名不能随模式变（否则 prompt 讲的名字在某一次并不注册，而模型会把「调不到的工具」读成「仓库里没有这个东西」）；同一能力写两遍必漂，而两遍的真实差别只有「这个文件在不在本地」；不落盘就等于在线模式下外部检查器永远跑不了。取文件那一层只管安全（不越界、不留执行位、二进制拒绝），**不设大小上限**——上限是读的人的事。
- **临时 worktree 不冒充检出**：它上面的列目录与搜索一律转发平台（用本地内容回答会把「取过什么」说成「仓库里有什么」）；「要一份完整检出」是独立前置条件，编译型检查器在它上面不注册；worktree 里什么都没有时（纯 diff 输入且无平台）内容工具与检查器全部不注册。
- **有检查器注册时，分片开工前先把待评文件放进 worktree**。否则 prompt 要求的「第一轮先跑检查器」在临时 worktree 上必然报「文件不存在」，而模型会把它读成关于代码的结论。
- **worktree 根路径 late-bound，而不是重排 `lib.rs` 的编排**：注册哪些工具只取决于平台能力，与文件将落在哪儿无关，所以沿用既有 `bind_repo` 那种「run 目录就绪后绑定」的形状。
- **`Origin` 换成 `Purpose`**：运行时要区分的是「它的答案意味着什么」（注册了却一次没被调的检查器要留痕，没发生的文件读什么都不说明），不是「这段代码写在哪」。
- **两份能力段模板，而不是模板里写条件**：「什么都查不了」需要的是另一句话，不是一个空列表。
- **阶段名做字段不做正文前缀**：靠前缀匹配来清理，等于把阶段名变成正文措辞的一部分，下次改一个字就清不掉了。
- **错误体积用装箱解决，不用 `#[allow]`**：这个仓库一处抑制都没有，是刻意的。
- 判为「跳过」里有两条值得记下理由：**收尾由谁发起**——轮数、窗口、回复被截断三种收尾都已本地发起、不等厂商报错，而预算不够时连收尾那一次调用也付不起（输入更大），只能停下并列出未评审；**没有发现就短回复结束**——当时看到空回复已经直接结束分片、空参数的提交调用也被收下不记，判断是回退没带走这项。**这一条判错了**，见下一套。

---

## 「No defect found in this change.」还是发出去了

**意图**：用户拿真实项目跑了一次（run `198be6c0b8b2fcb0`），报告里还是有「No defect found in this change.」这样的意见，要求严格按清单里「问题修复」那一段执行——也就是上一套里被我判成「跳过」的那条结束信号。

**步骤**

1. 先读那次 run 的痕迹，而不是直接改代码。`traces/review-syscared_src_config.rs.json` 里只有一次工具调用：`submit_comment {"body":"No defect found in this change.","suggestion":"N/A","confidence_score":95,"evidence":{"diff_lines":[67]},"path":"syscared/src/config.rs"}` → `recorded`。每一道校验都过了：字段非空、证据落在一行真的改动行上，于是它成了一条正式意见、带着 95 的置信度进了报告与打分。模型自己的回复里还写着「我在这个 run 里读不到别的文件」——那是纯 diff 输入，内容工具按设计一个都没注册，所以它手上**只有 `submit_comment` 一个工具**。
2. 新增 `FinishReview`（`finish_review`，无参数、用途是提交、轮次与 `submit_comment` 相同），执行结果带一个「结束」信号而不产出意见；空参数的 `submit_comment` 也归到同一个信号上。
3. `ToolOutput` 加 `finished` 标志，review 循环的结束判定改读这个信号，顺手去掉了原来 `name == SubmitComment::NAME` 那处按名字认工具的判断；分片以「没有要提的」结束时在 trace 记一句，读的人能把它和「什么都没产出」分开。
4. prompt 三处跟着改：能力清单那一段（「没有发现是一个真实结论，它有自己的结束方式」）、输出契约那一段（点名不要用占位正文、不要把 `suggestion` 填成 N/A、不要空调用）、以及收尾轮与「回复被截断」那两段提示——收尾轮从前只剩一个工具，那正是模型去填占位意见的地方。
5. 补一条回归用例，把这次真实失败的形状钉住：模型调 `finish_review` 时分片零意见、不标「被掐断」、trace 有那句话。保留名单加到 7 条，`tool list` 的 delivery 组现在印三条契约。294 个用例全过，clippy 仍零告警。

**决策**

- **修的是结束信号，不是提交工具的正文闸门。** 清单原话是「改结束信号，不要改合并阶段」，理由同样适用于工具本身：「这里没问题」在语义上没有形状可认，任何在正文上设的闸都是靠猜，而猜错的代价是丢掉一条措辞恰好听着让人放心的真发现。所以没有任何一处去匹配「no defect」这类字样。
- **「没有发现」必须和「有发现」一样容易表达**。会 function call 的模型不把「回一段纯文本」当成终止动作；只给它一个工具再要它收尾，它就会调那个工具。所以 `finish_review` 出现在 `submit_comment` 出现的每一轮上，尤其是收尾轮。
- **新工具无参数**。想说的话写在回复里，trace 本来就记；给它一个参数就是给它一个「把刚说没有的那条发现写进去」的地方。
- 描述里明写「不要用它提前离场」：它不制造新的失败模式（纯文本回复本来就能提前结束），只是给一个已经存在的动作起了名字。
