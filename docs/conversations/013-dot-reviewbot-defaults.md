# 默认路径改到 ~/.reviewbot

## 配置与 run 不再走 XDG

**意图**：修改 run 的路径与配置文件路径默认值为 `~/.reviewbot`。配置文件是 `~/.reviewbot/config.toml`，runs 目录是 `~/.reviewbot/runs`；仍是一个 flag、一个默认值、不从 cwd 查找。不再认 `XDG_CONFIG_HOME` / `XDG_STATE_HOME`。默认配置文件先写成了 `reviewbot.toml`，随后改成 `config.toml`。

**步骤**

1. `src/config/paths.rs`：`default_root()` 为 `$HOME/.reviewbot`；配置与 runs 落在其下。补一条单元测试。
2. 删掉 `common::paths::xdg_dir`，只留 `home_dir` 与 `expand_tilde`。
3. CLI 帮助、`Settings::load` 注释、README、`docs/design.md` 里凡写默认路径的地方改成上述两个值。
4. 默认配置文件从 `reviewbot.toml` 改成 `config.toml`；仓库示例仍叫 `examples/reviewbot.toml`。
5. 316 + 12 + 19 个用例全过，clippy `-D warnings` 零告警。

**决策**

- **根目录是 `~/.reviewbot`，文件名仍分开。** 配置是文件、run 是目录，不能共用同一个路径。配置叫 `config.toml`，runs 仍是 `runs/`。
- **不再读 XDG 环境变量。** 用户要的是固定的家目录默认，不是换一套 XDG 根。覆盖仍然只靠 `--config` / `--runs-dir`。
- **为什么不是把 `--config` 默认成 `~/.reviewbot` 这个文件：** 那会和 runs 目录抢同一条路径。
- **为什么不叫 `reviewbot.toml`：** 目录已经叫 `.reviewbot`，文件再重复一遍程序名没有信息量。
