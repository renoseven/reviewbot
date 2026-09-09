## 示例配置去掉 md、补代码扩展名

**意图**：修改 `examples/reviewbot.toml`，从 `allow_extensions` 去掉 `md`，并增加其它代码文件扩展名。随后补上 HTML 相关扩展名。再把这份名单同步到 `~/.reviewbot/config.toml` 和 `tests/fixtures/valid.toml`。

**步骤**：
1. `examples/reviewbot.toml`：删 `md`；名单改成多行数组，补 C/C++ 变体、JVM/C#/Swift/Zig、ObjC、脚本、JS/TS 生态、样式、SQL/proto/Dart/Elixir/Haskell、以及 toml/cmake/tf/nix。
2. `cargo test --lib the_example_config_sets_everything_that_is_required` 通过。
3. 补 `html` `htm` `xhtml`，以及常见 HTML 模板 `hbs` `ejs` `njk` `twig` `erb` `pug`。
4. `tests/fixtures/valid.toml` 的 `allow_extensions` 同样换成这份名单。

**决策**：名单改示例、本机配置和 CLI 夹具 `tests/fixtures/valid.toml`；不改 `docs/design.md` 里的配置样例，也不改各阶段测试里那份更短的清单。不收 `json`/`yaml`/`yml`：不是源码，还容易夹密钥。HTML 与模板按后话收入；`vue`/`svelte` 已有，不再单列。本机 `max_tool_output_bytes = 65536` 等与示例不同的项保持原样。
