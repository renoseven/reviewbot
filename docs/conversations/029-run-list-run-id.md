# run list 表头 RUN_ID

## run list 的 RUN ID 改成 RUN_ID

### 意图

- `reviewbot run list` 这列头应该改成 `RUN_ID`
- 还有没有类似的问题
- 不用了（`Runs dir`、`config check` 的 Title Case、`NOT AVAILABLE THIS RUN` 都不改）

### 步骤

- `src/cli/render.rs` 表头 `RUN ID` → `RUN_ID`
- `tests/cli.rs` 的 `run_list_text_aligns_spent_with_currency_and_utc` 跟着改
- `cargo test --offline --test cli run_list_text_aligns_spent_with_currency_and_utc`
- 扫了 CLI 全部 `pad_table` / `catalog_table` 表头：`RUN ID` 是唯一带空格的 ALL CAPS 列

### 决策

- 多词 ALL CAPS 列用 `_`，与 `config info` 的 `BASE_URL` / `TOKEN_FROM` / `BUDGET_PER_RUN` / `KEY_FROM` / `CACHED_1M` / `MAX_OUT` 一致
- 为什么不是改 `Runs dir`、`config check` 的 `Path` / `Credential`、或 `NOT AVAILABLE THIS RUN`：不是表头；用户明确说不用改
