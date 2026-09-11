## 活动行改成 -ing 动词

**意图**：`waiting for model` 改成 `thinking`，`waiting for conclusion` 改成 `concluding`，`waiting for {name}` 改成 `executing {name}`，剩下的统一风格。`Stage::Report` 的 `writing` 再改成 `reporting`。`posting` 改成 `publishing`。

**步骤**：
- `activity_line` / `doing` 收成同一套单个 `-ing`：`starting` / `thinking` / `concluding` / `executing {name}` / `reading` / `planning` / `reviewing` / `merging` / `reporting` / `publishing`
- 屏幕测试、`status` 注释、`design.md` 例句与断言跟着改；文件行那个 `reviewing  path` 不动

**决策**：
- **一律单个动词。** 跟 `thinking` / `concluding` / `executing` 对齐，阶段自己干活的不再带宾语（`reading changes` → `reading`）
- **report / publish 跟阶段名对齐。** 写 `reporting`、`publishing`，不写 `writing`、`posting`
- **不写 `$`。** `executing ${name}` 是插值，屏幕上是 `executing search_repo`
- **为什么不是继续用 waiting for：** 用户要换句式，三种等待不再共用前缀

---

## triage 全改成 plan

**意图**：阶段名 `triage` 全改成 `plan`，屏幕、配置、落盘一个拼写。

**步骤**：
- `Stage::Plan`、`Outcome::Plan`、模块 `stage/plan.rs`、配置 `[plan]` / `PlanSettings`、指纹字段 `plan`、产物 `PlanOutput`
- 活文档（`design.md`、README、`example.toml`、夹具）一起改；历史对话记录不改写
- 清单上 `{:<9}` 对齐按 `plan` 四个字母重排
- 核对大小写：对外仍是小写 `plan`；`Triage skips` 被收成 `Plan skips` 的那处注释改回 `` `plan` skips ``

**决策**：
- **一个拼写。** 不能只改屏幕，checkpoint / `meta.json` / `[plan]` 一起换；旧 run 续跑会从这一阶段起重
- **产物叫 `PlanOutput`。** 跟 `ReviewOutput` 对齐；阶段执行者是 `Plan`，不跟产物同名
- **为什么不是只改屏幕上的阶段名：** 设计定过名字同时是落盘格式，分叉会漂

---

## run list 的 STAGES 只报最远

**意图**：`run list` 的 STAGES 列显示最高的一个就行了，不要整串 `input, triage, review, merge, report, publish`。

**步骤**：
- 文本渲染改成取 `completed_stages` 最后一个；`run show` 同一句
- JSON 仍是走完的那一串；CLI 断言改成有 `publish`、没有 `input, plan`

**决策**：
- **屏幕上看最远那个。** 阶段有固定顺序，前面的不必再念一遍
- **JSON 仍是列表。** 调用方不必自己推顺序；`meta.json` 还是一个 `completed_through`
