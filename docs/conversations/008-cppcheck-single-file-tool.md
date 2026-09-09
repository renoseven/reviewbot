# 为本机配置单文件 C 检查器

## 配置可独立检查 C 源码的外部工具，并写成示例

**意图**：在 `~/.config/reviewbot/reviewbot.toml` 配一个能独立检查 C 源码文件的外部工具，并说明要执行什么命令。独立指单文件即可跑、不依赖完整工程检出。随后要求把这份 `cppcheck` 写进仓库示例，并补上题目点名的 `typecheck`。指出对话记录写成了 `010-…`，编号不对。`tests/fixtures/valid.toml` 也要跟着改。

**步骤**

1. 读本机配置与 `[[tool]]` 契约：已有 `gcc -fsyntax-only`，但 `requires_checkout = true`，无 `--worktree` 时整条不注册。
2. 本机无 `cppcheck`，`apt` 装了 2.13.0，路径 `/usr/bin/cppcheck`。
3. 在本机配置追加 `[[tool]]` `cppcheck`：`--enable=warning,style --template=gcc --quiet -- {path}`，`requires_checkout = false`，`requires_build = false`。
4. `reviewbot config check` 通过；`tool list` 里 `cppcheck` 的前置条件只有「worktree 有可读代码」。
5. `examples/reviewbot.toml` 启用同一段 `cppcheck`。
6. 本机与示例里原来的 `gcc` 改名为 `typecheck`（仍是 `gcc -fsyntax-only`，`requires_checkout = true`），作为题目点名的「加一段 `[[tool]]`、不重新编译」演示。
7. `README.md`「Add a tool」段改成示例同时交付 `cppcheck` 与 `typecheck`。
8. 示例与本机 `config check` 均为 tools 2；`the_example_config_*` 与 `tool_list_*` 用例通过。
9. `tests/fixtures/valid.toml` 同步成同一对 `cppcheck` / `typecheck`；`tool list` 用例改断言这两个名字。

**决策**

- 独立单文件检查用 `cppcheck`，不用把编译器的 `requires_checkout` 关掉：缺工程头文件会刷一屏 missing include，比不跑更糟。
- `typecheck` 是题目点的名字，实现仍是 `gcc -fsyntax-only`：它做的是类型/语法检查，不是再装一个二进制。`requires_checkout = true`，无 `--worktree` 时不注册。
- 示例与夹具两份都启用：`cppcheck` 演示「单文件即可」；`typecheck` 演示「再加一段 `[[tool]]`」。
