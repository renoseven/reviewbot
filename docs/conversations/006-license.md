# 仓库许可证

## 增加 LICENSE，尽量宽松且满足依赖

### 意图

- 根据现有代码与依赖，给仓库增加一个 LICENSE，要满足依赖的许可证要求
- 至少 MIT/Apache-2.0（不要 Unlicense / 0BSD / CC0 这类比 MIT 更松、也不带专利授权的许可）

### 步骤

- 用 `cargo metadata` 扫完整依赖树：直接依赖与传递依赖均为宽松许可（MIT、Apache-2.0、BSD-3-Clause、ISC、Unicode-3.0、CDLA-Permissive-2.0、Unlicense OR MIT 等），无 GPL / AGPL / 强制 copyleft
- 新增 `LICENSE-MIT`、`LICENSE-APACHE`；`Cargo.toml` 写 `license = "MIT OR Apache-2.0"`；README 末尾加 License 段

### 决策

- 本仓库采用 MIT OR Apache-2.0 双许可：接收方可任选其一。这是 Rust 生态默认组合，Apache-2.0 带专利授权，满足「至少 MIT/Apache-2.0」
- 为什么不是只 MIT：用户要求双许可下限
- 依赖不挡这条：`r-efi` 是 MIT OR Apache-2.0 OR LGPL-2.1-or-later，选前两者即可；`ring` 是 Apache-2.0 AND ISC，对源码许可无 copyleft 要求（再分发二进制时保留其 NOTICE 是分发方义务，不改变本仓库许可）
