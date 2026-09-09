## 将 --out-dir 重命名为 --output-dir

**意图**：纯重命名 CLI flag `--out-dir` → `--output-dir`，Rust 字段 `out_dir` → `output_dir`；行为、产物文件名、deny_paths 自动追加逻辑不变。

**步骤**：
- 改 `src/cli/args.rs`、`src/cli/mod.rs`、`src/config/mod.rs`、`src/stage/publish.rs`、`src/tests.rs` 及若干注释（`lib.rs`、`layout.rs`、`fingerprint.rs`、`path.rs`）
- 改 `README.md`、`examples/gitlab-ci.yml`、`examples/github-actions.yml`、`docs/design.md` 中的 flag 字面量
- `cargo test` 316+12+19 全绿；`cargo clippy --all-targets -- -D warnings` 零警告
- `rg` 在 `src README.md examples docs/design.md` 无残留

**决策**：clap derive 从字段名 `output_dir` 自动生成 `--output-dir`，不加手动 rename attribute；`docs/conversations/*` 历史记录不改动
