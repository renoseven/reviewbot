# 把 input 与 worktree 收成单文件

## `src/stage/input` 改成 `input.rs`，`src/worktree` 改成 `worktree.rs`

**意图**：`src/stage/input` 直接改成 `input.rs`；`src/worktree` 直接改成 `worktree.rs`。目录模块收成对应的单文件，不再保留子目录。

**步骤**

1. `src/stage/input.rs` 已是模块根，目录里只剩 `diff.rs`。把 unified diff 解析并进 `input.rs`，删掉 `src/stage/input/`。
2. `src/worktree/mod.rs` 改名为 `src/worktree.rs`，去掉目录。
3. 路径跟着收：`input::diff::DiffError` → `input::DiffError`；测试里 `stage::input::diff::UnifiedDiff` → `stage::input::UnifiedDiff`。
4. 312 + 12 + 19 个用例全过。

**决策**

- **解析器跟 input 同文件。** diff 本来就是 input 的事，再单开子模块只是目录形状。
- **不在 `input.rs` 里再留一个 `mod diff`。** 用户要的是单文件，不是把目录搬进文件里当子模块。
