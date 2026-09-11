# `trace` 命令与十六进制 trace_id

## `run trace` 与十六进制 trace_id

### 意图

- 实现 `reviewbot run trace $run_id`，参数为 `--trace-id`；无参数时按时间展示所有对话，有参数时按该 id 筛选
- 对话内容只要发给模型的 prompt、这份 diff、工具调用（名字、输入、输出、耗时、成败）、模型回复、思维链、用量
- `trace_id` 改成和 `run_id` 一样的十六位十六进制；publish 的时候也要用这个十六进制的 `trace_id`
- 等一下，是不是改成 `run trace` 更好
- trace 按照时间排序

### 步骤

- `Trace::for_review` / `for_summary` 用与 `run_id` 相同的 `hex_id` 生成十六位小写十六进制；路径与分片号记在 JSON 里，不再写进文件名
- `review` / `merge` 打分走命名构造；报告、MR 评论、幂等标记 `<!-- reviewbot:{run_id}:{trace_id} -->` 用的是同一份 id
- `Runs::traces` 读 `traces/`，按 JSON 的 mtime 从早到晚排，时间相同再按 `trace_id`；`--trace-id` 对不上是 `TraceNotFound`（退出码 2）
- 先做成顶层 `trace`，再收进 `run trace`，和 `run show` 并列；顶层 `trace` 不再解析
- text 按对话打印上述字段，json 是 `{ run_id, traces }`
- 用例：hash 稳定性、listing 按 mtime、CLI 全文与 `--trace-id`、生产路径上的 id 不再是 `review-src_parse.c`
- README、design.md 的命令面与落盘说明跟着改

### 决策

- **trace 仍是一份对话，不是整个 run 的流水账。** 一次 run 多份 trace；run 级过程在 `log`
- **展示只出那六样。** notes、context 正文、published 视图留在盘上，不进这条命令
- **id 由路径（加分片）哈希，稳定可复入。** 和 `run_id` 同形，靠出现的位置区分，不靠长得不一样
- **publish 不另造一套 id。** comment 上的 `trace_id` 就是生成时那份，标记和可见的 `trace: …` 跟着走
- **汇总评论的标记仍是 `{run_id}:summary`。** 那条没有对话 trace，固定后缀只为幂等
- **为什么不是继续用 `review-<path>`：** 用户要和 `run_id` 同形；路径改记在 trace 里，列出时按落盘时间
- **列出按 JSON 的 mtime 从早到晚。** 和 `run list` 同一只钟，不往 Trace 里加时间字段；时间相同按 `trace_id`。为什么不是按路径：用户要按评审发生的顺序看
- **命令是 `run trace`，不另起顶层 `trace`。** 它看的是某一个 run 的对话，和 `run show` 同一组；顶层再加一个名词是第二套表面

---

## `run trace` 的文本

### 意图

- 优化一下 `run trace` 的输出

### 步骤

- 抬头与 `run show` 同形：`run_id`，每份对话再出 `trace_id` / `piece` / `usage`
- 段落之间空一行；模型回复改叫 `reply`；思维链放在回复前面
- 工具的 input / output 若是 JSON 就 pretty-print
- 渲染用例改成整段比对；CLI 契约跟着改标签
- 标题不够明显、trace 之间分隔不够：路径改成带下划线的标题，段落标签全大写，两份对话中间加一行 `=`
- `REASONING`、`REPLY` 这种标签用方括号框起来，后面再空一行
- `trace_id` / `usage` 做成表头，用横线分割
- 手写 `ruled_table` 改走已有的 `comfy-table`
- 文件名放进表里；对话之间的 `========================================` 删掉
- `TOOL` 字段行改成 `[TOOL]`，和其余段落同一套
- 工具的参数和返回值改叫 `[ARGUMENTS]` / `[RESULT]`，空的也占位

### 决策

- **路径是 `FILE` 列。** 不挂在表外当标题；打分那份格子写 `scoring`
- **段落标签是 `[REASONING]`。** 方括号把标签从正文里抠出来，后面空一行再跟内容；工具调用是 `[TOOL]`，参数 `[ARGUMENTS]`，返回值 `[RESULT]`。空参数印 `(none)`，免得看起来像没记
- **对话之间不再画 `=`。** 那条是当时加的分隔，表框自己已经有线
- **抬头走 `comfy-table`。** 表本身没有名字，列是 `TRACE_ID` / `FILE` / `IN` / `CACHED` / `OUT`（有分片再加 `PIECE`）
