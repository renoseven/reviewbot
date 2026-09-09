# 未调用的检查器不进报告

## 报告不要列出「offered typecheck, cppcheck; none called」

**意图**：修复 run `6f9cc255c101e270`。报告里四份 Rust 文件（`sys.rs` / `mod.rs` / `target.rs` / `config.rs`）各自一条 `offered typecheck, cppcheck; none called`，这个不应该显示到报告中。

**步骤**

1. 读该 run 的 `report.md`：两条真实发现之后，四份 `.rs` 再各列一条未调用检查器；`sys.rs` 与 `config.rs` 因此同一文件出现两次。检查器是配置里的 C/C++ 工具，prompt 已要求模型按描述判断适用性、不适用就跳过。
2. `publish` 报告清单不再收录 `unused_checkers`；`PublishInput` 去掉该字段。评审阶段仍写入分片 trace 与 checkpoint。
3. 用例：徽标仍贴在分数旁；报告正文不含 `none called`。`design.md` §可观测改口。
4. 312 + 12 + 19 个用例全过，clippy `-D warnings` 零告警。`reviewbot report 6f9cc255c101e270` 重渲后报告只剩两条发现和被删的 `monitor.rs`，四条 `offered …; none called` 已不在。

**决策**

- **不进报告，仍进 trace。** 报告是给读这次改动的人看的；没调检查器是模型过程，不是改动上的缺口。C 检查器没打到 Rust 文件上，正是 prompt「If none apply, skip」要的行为。
- **不按扩展名过滤检查器。** `[[tool]]` 没有文件类型字段，适用性本来就交给模型看描述。为这条噪声加配置是另一件事。
- **不重跑评审。** checkpoint 里仍有 `unused_checkers`；改的是渲染，`report` 子命令即可修好这份 run。
