# 两个停条件：窗口 + max_rounds

经验命令始终是 `./target/debug/reviewbot review /tmp/1.diff --worktree ~/syscare`。同一 `run_id` `5c1e10c145dccda4`（输入身份 + `head_sha`）prune 后再跑。不要另起一份叠着跑，除非明确要求。

## 两个停条件：窗口 + max_rounds

**意图**：推出来的 24 不对，文件之间不叠对话，每轮也不填满一份 dump。两个条件：对话剩余 context，以及把轮数限制写成 `[review]` 配置、先给 100。`round 8/32` 后面的数就写配置限制。日志里的 why 要写真实原因：是 context 不足，还是硬限制到了。

**步骤**：
- `[review].max_rounds` 必填，示例与夹具 100；循环按文件计，交付不占轮次
- 屏幕 `of` 就是 `max_rounds`；下一轮仍装得进且是配置上限的最后一轮才发 `rounds-last`
- 窗口停：`not enough context left`；撞配置：`the configured limit was reached`（log WARN 与 trace notes）
- `design.md`、README、相关断言跟着改

**决策**：
- **停有两道，都不按整次 run。** 下一轮装不进窗口，或调查轮次碰到 `max_rounds`
- **`of` 只报配置。** 不拿当时窗口还能再装几份去改分母
- **why 分两种。** context 不足与硬限制到了各写一句，不混成 ceiling，也不报 token 数
- **为什么不是继续用 `可用 / 工作大小 − 1`：** 那是假定每轮填满一份 dump，1M 上每个文件都是 24

---

## 未决

- why 还在 log WARN 与 trace notes；屏幕和报告不印。没要求再删
