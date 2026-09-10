# review 中断恢复，以及 allow_extensions 的生效位置

## review 阶段中断后只能从头开始

**意图**：review 阶段不能从 checkpoint 恢复，只能从头开始。Ctrl-C 停在 file 4/65 后再跑同一条命令，已经评过的文件又花一遍钱。恢复后续跑对了，但状态屏写成 file 1/57。

**步骤**：
1. 查清现状：`Review::run` 只在全部 chunk 结束时 `complete()`，中途不落盘；`completed()` 只认已标完成的阶段，所以半程等于没跑
2. `Recorder` 增加 `save` / `saved`：写同一份 checkpoint 文件但不标完成；解析失败当缺失，不标 incomplete
3. 每个 chunk 成功后立刻 `save`，并把未完成文件的 `pending_handoff` 一并写入；阶段结束再 `complete()` 并清掉 handoff
4. 再进入时 `saved()` 读回已完成的前缀，从下一个 chunk 接着跑；切开的文件靠盘上的 handoff 交接
5. `send_and_settle` 每次结算后 `record_spend`，中断再进时预算从 `meta.spent` 接着扣
6. 阶段测试：第二个文件脚本失败后再 queue 一条回复，断言只多送一次、第一份产物原样留下；切开的两片同理，后一片的 input 带上前一片的 finding
7. 状态屏 `files_seen` 原先数本进程见过的路径。改成用计划里的文件序号（切开的多片共享一个号）；`Event::Chunk.index` 也改成这个序号

**决策**：
- **失败点是分片，不是整个 review 阶段。** 设计里「只重试失败点及其下游」本来就是这个意思，只是实现把整个阶段当成原子单元
- **同一份 `3-review.json`，完成标记另走。** 不另开 in-progress 文件：原子 rename 已经保证读到的要么是上一份完整快照，要么是刚写完的这一份
- **handoff 必须进这份快照。** 它只活在内存里的话，切开的文件从第二片重跑会丢「前一片已经报过什么」
- **花费跟每次模型调用走，不跟 chunk 绑定。** 半片死掉时钱已经付给厂商，账本不能装没发生
- 为什么不是整阶段完成才写：那正是现在的 bug——65 个文件评到第 4 个被掐，前 3 个全丢
- **`file N/M` 是计划里的位置，不是本进程数过几个路径。** 恢复时已完成的文件不再发 Chunk 事件，按「见过的路径」计数会从 1 重来

---

## allow_extensions 没拦住 markdown 进评审

**意图**：`allow_extensions` 没有拦住 review 一些文件，比如 markdown；重新考虑它应用的位置与时机。示例名单不含 `md`，但 `README_zh.md` 仍被送到模型。

**步骤**：
1. 查清现状：白名单只在工具读文件时检查；review 送的是平台 API 给的 diff，不经过那道门。`deny_paths` 已经在 triage 跳过，`allow_extensions` 没有
2. `PathPolicy` 增加 `allows_extension`，`FileFilter::reason` 在 `deny_paths` 之后、`skip_paths` 之前检查 `new_path`；删除文件（`new_path` 为 `/dev/null`）跳过这道，仍报「整个文件被删」
3. 工具读、列表不按扩展名滤、布局摘要按扩展名滤，这三处不变
4. 阶段测试：`README.md` / `Makefile` 进跳过清单并点名 `allow_extensions`，删除的 `docs/old.md` 仍报 deleted，`src/kept.c` 进分片
5. 同步 `docs/design.md`、`README.md`、`examples/reviewbot.toml` 的口径

**决策**：
- **主闸在 triage，和 `deny_paths` 同一层。** 「不许碰」如果只挡补上下文、不挡把 diff 送给模型，名单去掉 `md` 仍然会给 markdown 花钱
- **只看 `new_path`。** 评审的是改完之后的那个文件；删掉的文件走已有的 deleted 理由，避免 `/dev/null` 被报成没有扩展名
- **工具读仍再拦一次。** 评 `.c` 时模型仍可能去读一篇 `.md`，那次拒绝留着
- **列表与搜索仍不按扩展名滤。** 存在与否本身是答案（`Makefile` 在那儿说明这是 make 工程）
- **input 不滤。** changeset 要完整，跳过的文件进报告，而不是假装没出现过
- 为什么不是让用户把 `**/*.md` 写进 `skip_paths`：那是成本策略，白名单已经在说「这类文件不许碰」

---

## 未决

无
