# 未调用检查器不再 warn

## 删掉「no external checker was called」那句 warn

**意图**：`review.rs` 里 `note_unused_checkers` 的 `tracing::warn!(…, "no external checker was called")` 能不能删。

**步骤**

1. 对照注释与 `010`：未调用检查器是 trace note，不是报告项，也不是失败；C 检查器没打到 Rust 文件上是 prompt「If none apply, skip」要的行为。
2. 删掉该 `warn`。分片 trace 与 checkpoint 的 `unused_checkers` 仍写。

**决策**

- **删 warn，留 trace。** 同文件其它 `warn` 对应预算耗尽、文件不在 worktree、被迫收尾、截断再问——都是过程出了问题。检查器没调是模型按描述做的判断，warn 会按分片刷屏。
