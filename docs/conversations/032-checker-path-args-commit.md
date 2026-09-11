# 提交检查器路径参数修正

## 只提交 src/config 与 tests，并 signoff

**意图**：将 `src/config` 与 `tests` 提交 commit，并 signoff。未要求提交其余文档或 lock 改动。

**步骤**

1. 工作区里与范围相关的是三份文件：`src/config/example.toml`、`src/config/settings.rs`、`tests/fixtures/valid.toml`。cppcheck / gcc 的 `args` 去掉 `--`，`{path}` 作为最后一个位置参数；`settings.rs` 增加用例，断言示例与夹具都不再写 `"--", "{path}"`。
2. 只 add 这三份，提交 `4225a94`（`bugfix: pass shipped checker paths as positional arguments`，带 Signed-off-by）。`Cargo.lock`、`README.md`、`docs/` 未纳入。

**决策**

- 只提交用户点名的 `src/config` 与 `tests`。`--` 不能留给这两条 shipped checker：它们把 `--` 当成未知选项，而 `{path}` 已是绝对 worktree 路径，作末位位置参数即可。
