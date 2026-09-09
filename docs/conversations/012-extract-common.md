# 把基础设施抽到 common

## 横切能力不再寄居在 config / security / platform

**意图**：把代码里的基础设施提取出来，放到 `common` 模块。先前已定模块名用 `common` 而不是 `util`：退避曲线、路径展开、输出截断、HTTP 重试这类没有单一功能主人的能力，不要继续放在 `config`、`security` 或 `platform` 里。密钥为什么还在 config——配置只该存指针，读密钥、拒内联、600 权限、永不打印都不是 TOML 解析。

**步骤**

1. 新增 `src/common/`：`backoff`（原 `config/retry.rs`）、`paths`（`home` / `~` / 绝对路径 / XDG 根）、`truncate`（原 `security/truncate.rs`）、`http`（共用客户端、重试循环、瞬时故障判定、`Retry-After`、错误正文截断）。
2. `config/paths.rs` 只留本程序的两个默认路径（`reviewbot.toml`、runs 目录）。`config` / `security` 再导出 `Backoff` 与 `truncate`，公开路径不变。
3. `platform/http.rs` 收成平台形态：拼 URL、翻页、401/403/422 映射、幂等标记。`protocol/openai.rs` 改走同一套 `common::http::Client`。两边删掉各自复制的重试循环。
4. HTTP 客户端不持有 `Redactor`：脱敏仍在调用方，`common` 不依赖 `security`。
5. `Secret` / `SecretSource` / `SecretError` 从 `config/secret.rs` 迁到 `common/secret.rs`。`config` 只保留 TOML 里的指针字段，校验和读取走 `SecretSource`；`ConfigError` 透明包装 `SecretError`，退出码仍是 2。
6. 删掉自制的 `common::paths::absolute`，调用点改用 `std::path::absolute`。拿不到 cwd 时 `load` / `written_paths` 走 `ConfigError::Unreadable`，worktree 警告拿不到就跳过。`expand_tilde` 留下，标准库没有 `~`。
7. 315 + 12 + 19 个用例全过，`clippy -D warnings` 零告警。

**决策**

- **`common` 只收没有主人的横切能力。** 脱敏、路径策略、子进程清洗仍是 `security`。
- **密钥进 `common`，不进 `security`。** `security` 已经依赖 `config`（`PathPolicy` 读 Settings）；密钥再放 `security` 会让 `config` 反过来依赖它，同层成环。`Secret` 被 config、platform、protocol、stage 共用，没有单一主人。
- **HTTP 传输与平台语义分开。** 重试政策只有一份；GitLab 的 `%2F`、分页、422 退化留在 `platform`。
- **为什么不是把整个 `security` 并进 `common`：** 那些是安全策略，有明确主人；`common` 不是杂物抽屉。
- **相对路径用 `std::path::absolute`，不用 `canonicalize`。** 不要求路径存在、不跟符号链接。cwd 失败不再悄悄留下相对路径。`~` 仍手写，不拉 `shellexpand`。

## 未决

- 编码规范分层列表还没写上 `common`。
