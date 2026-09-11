# 编码规范与测试规范

## 整份换成新稿

### 意图

- 更新编码规范与测试规范，两份都给了完整新稿，按稿覆盖现有 `.cursor/rules/rust-coding.mdc` 与 `rust-testing.mdc`

### 步骤

- 对照旧稿，按用户正文整份覆盖两条 rule
- 记进本对话

### 决策

- 编码规范不再钉分层、类型归属、模块别名、prompt `include_str!`、安全靠实现；改成通用形状：文件切分（含 `mod.rs` / 单文件模块）、封装分块、独立实体必须实例化、成熟库、禁止 `unwrap` / `expect`、硬约束收成原则（外来的数、静默降级、串行、密钥、argv、瞬时重试、原子写）
- 测试规范五档改成通用名：阶段→组合、CLI 契约→CLI；去掉 Worktree / RepoSource / 五阶段等产品细节；「不要测」收成判不了、合法空输出、退出码不含业务结论
- 为什么不是并进 `003`：那是另一次对话；本轮两份都给了新稿，单独成套
- 为什么整份换而不是补丁：新稿已自洽，旧分层与产品硬约束会和「必须实例化 / 通用硬约束」抢焦点

---

## 按编码规范收 `src/` 形状

### 意图

- 按整份编码规范收 `src/`：独立实体改成构造再调方法、依赖一次注入；`mod.rs` 收成桶；字段私有；产品代码去掉 `unwrap`/`expect`
- 公开入口 `review()` / `review_with()` 与六阶段顺序仍只出现在 `lib.rs`，不另起编排类型
- 不改 `review()` 签名、不新增 `Run`/`pipeline`/`orchestrator`、不改配置字段名、不改测试分档
- 对象化做完后继续改仍不合的：`Merge::align` 改实例方法、Preamble 三个自由函数收回去、`Orientation.change` 私有、publish locator 缺失就失败、厂商没给 usage 当错误、手写日期换成 chrono
- 删掉 `StageContext::new`，字段全部改 `pub`；字段公开后访问器也删掉
- 再按顺序改：`platform_secrets` 读失败外传、`function_call` 缺字段当 Malformed、handoff 缺行号用路径不填 0

### 步骤

- 构造：`Checkout` / `Opening` / `Preamble::assemble` / `Redactor::with_secrets` / `Recorder::open` 一次注入 / `Registry::assemble` / `Adapters::bind` / `Runs` / `FileFilter` 持 `PathPolicy`；`OpenAi::connect`、`GitHub::for_host`、`GitLab::for_host`
- 六阶段改成 `Type::new(context).run(...)`；`Review` 用私有 `ChunkWork`；`Narrative` 规范化离开 `domain`，由 `Input` 组 changeset 时做
- 八个桶拆文件：`protocol`/`record`/`platform`/`budget`/`stage`/`tool`/`config`/`cli` 的 `mod.rs` 只留文档、`mod`、`pub use`
- 封装：`Adapters`/`Equipment`/`StageContext`/`Trace`/`Request`/`Settings` 字段私有，跨模块走访问器与方法；`RunResult`/`ToolOutput`/`MergeOutput` 本轮保持公开
- 产品路径去掉 `unwrap`/`expect`：正则与 HTTP 构造返回 `Result`，mutex 中毒映成 `PlatformError`，fingerprint 序列化失败外传，`lib.rs` 用结构消掉 preamble 的 `expect`，diff 解析器用 `get_or_insert`；测试模块的 `unwrap`/`expect` 留下
- `Merge::align` 改成 `&self` 方法，`run_chunk` / `findings_from` / `finding_from` 一并改实例方法；对齐单测先构造 `Merge`
- `assemble_instructions` / `narrative_preface` / `prompt_tokens` 收到 `Preamble` 上；测试走 `from_parts`
- `Orientation.change` 私有，对外 `change()`；测试用 `with_change`
- `Publish::publish_target`：host / project / number / head_sha 缺了就 `UnreadableInput`；补一条缺 locator 的用例
- `usage_from` 没 usage 或没 `input_tokens` / `output_tokens` 返回 `Malformed`；cached 仍可省略当 0
- `cli/render.rs` 的 `format_unix_utc` 改用 chrono，删掉手写 `civil_from_days`
- `cargo test --offline` 绿（403 + 46 + 24）
- 删 `StageContext::new`，字段改 `pub`；`lib.rs` 与 `StageFixture::context` 改成立面构造
- 删 `StageContext` 访问器，调用方直接读字段；`stage_started` 等行为方法留下
- `platform_secrets` 读 token 失败走 `ConfigError`，不再当没密钥；配置里没有这个 host 仍是空列表
- `function_call` 缺 `call_id` / `name` / `arguments` 返回 `Malformed`，不再补 `""` / `"{}"`
- handoff 缺 `start_line` 用 `code_span` 只写路径，不填 `0`

### 决策

- 不另起 `Run`/`orchestrator`：阶段顺序仍只写在 `lib.rs`
- `Adapters::bind` 需要未绑定 platform 才能问 `head_sha`，绑定是第二种形态；对外只暴露 `Adapters::bind`，`Platform::bind_repo` 留 trait
- `StageContext` 字段公开、立面构造，不再用 9 参数 `new` 或访问器；行为方法留下；`recorder`/`budget` 本身是 `&mut`，调用方直接用字段
- `Settings` 只包 `config()`/`options()`/`config_path()`，TOML 内层仍是纯数据
- `Redactor::new`/`with_secrets`、`Client::new`、平台 `for_host` 改为 `Result`，失败可匹配，不把静态正则或 TLS 客户端当成不可能失败
- 测试夹具 `StageFixture::with_*` 保持 builder，不按产品构造约束收
- 对象化那轮不换依赖；补规范时只加 chrono 换手写公历，CLI 渲染用
- cached tokens 省略仍当 0：厂商常不报 cache details，那不是「没计费」
- 配置里没有对应 `[[platform]]` 时 `platform_secrets` 仍回空列表：那时没有 token 可读，失败发生在 `platform_for`
- 为什么不是一次改完再测：按 plan 五段落地，每段独立可编译

---

## 源码中文注释改成英文

### 意图

- 仓库中有一些中文注释，改成英文

### 步骤

- 全库搜 CJK：源码里只有 `platform/types.rs` 两处 rustdoc 是中文
- `Capabilities` / `Repo` 的注释改成英文
- `budget/estimate.rs`、`common/truncate.rs` 里的「数组越界」是测 CJK 切分的夹具，不是注释，留下

### 决策

- 只改源码注释。`docs/`、`.cursor/rules/` 本身就是中文文档，不在这句「注释」里
- 为什么不是改测试字符串：那些字是被测输入，换成英文就测不到 CJK
