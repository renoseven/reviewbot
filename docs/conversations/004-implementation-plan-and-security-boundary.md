# 实现计划与安全边界

## 按设计立实现计划

### 意图

- 根据 `docs/design.md` 完整实现所有功能
- 实施时读 todo 里标的模型：先全用 claude-opus-5-thinking-high，后又要求重新考虑每一步该用哪个
- 测试只做最小档，先把功能跑通
- `domain` 做成目录，`budget` 也做成目录；问 `worktree` 是否放进 `domain`
- 计划在 `docs/` 里也放一份

### 步骤

- 读 `docs/design.md` 与两条 Rust rule，确认仓库还是空的（只有 `fn main() {}`）
- 按 §12 立 M1–M8 八个 todo，每条标上模型名；写出目标目录形状与几条会反复用到的决定
- `domain/` 拆成 `change_set.rs` / `comment.rs` / `confidence.rs`，`mod.rs` 只做再导出
- `budget/` 拆成 `mod.rs`（冻结、调用前检查、结算）/ `estimate.rs`（token 估算，triage 切分与 review 上下文检查共用）/ `price.rs`（单价换算）
- `worktree` 留在适配器层并同样改成目录，与 `platform` 对称
- 按「错误多久才暴露」重排模型：M1 / M2 / M4 / M6 / M7 用 opus，M3 / M5 / M8 用 composer-2.5-fast
- 计划抄一份进 `docs/implementation-plan.md`，与 plan 文件同源
- `change_set.rs` 改名 `changeset.rs`；clap 那一侧拆成 `src/cli/{mod,args,render}.rs`，`main.rs` 只剩 `fn main()`

### 决策

- 测试取最小档：单元 + 一组整装 + CLI 契约，§11 其余四十组用例进 README 的 TODO
- `Confidence` 在 `domain` 里只是枚举，分数落档的映射写在 `merge`——`domain` 不含逻辑，而做这次映射的只有 `merge` 第 6 步
- `worktree` 不进 `domain`：它要读磁盘、要认设施层的校验与配置，放进最底层就是反向依赖
- 模型不再一刀切用 opus：判据是这一步的错误多久才暴露。形状与算法（分层定形、两个行集合、merge 七步、执行侧安全、按能力注册）返工要连着改数据结构，用 opus；照着设计已写死的端点、字段、表格落地的（协议实现、平台端点、CLI 与文档）错了当场编译不过或测试红，用 composer-2.5-fast
- 计划在 `docs/` 留一份而不是只放 plan 文件：plan 文件不进仓库，而模型分配与目录形状是要跟着代码走、评审人也要看的
- clap 拆成单独的 `src/cli/`：设计只要求 `main.rs` 不写业务逻辑，没要求它是一个文件。七个子命令 + 四组 flag + 两套渲染 + 六个退出码塞一处会到七八百行，而 flag 定义与输出渲染各自会变。这些文件只由 `main.rs` 声明，`lib.rs` 不认识，库的公开面没变

---

## 安全不做代码层面强制

### 意图

- 先问「`RepoSource` 与 `WorktreeSource` 为什么在 `security` 里面」
- 随后要求调整设计：不要在代码层面做安全性限制，这是实现保证的
- 两个来源 trait 各回主人；路径校验的调用点放在 `tool` 包装层

### 步骤

- 术语表：删掉 `ValidatedPath` 一行，换成「路径校验」——`security` 提供的一道检查，调用点在 `tool`
- 模块表：`security` 改成「只提供检查，不提供强制」；`platform` / `worktree` 各自抽出自己的来源 trait；`tool` 改成包装两个来源、路径校验加在这一层
- §2 补一句：同层唯一一条依赖是 `tool` → `platform` / `worktree`
- §6 安全「读取范围」加一段：校验是一道检查不是类型闸门，调用点固定在 `tool`，这条靠用例与 review 守
- §8「落到模块上」重写为 trait 跟着主人走 + 校验落在 `tool`
- §11 把 trybuild 那条编译失败用例换成「六个内建 tool 各喂一个越界路径，断言假来源一次请求都没收到」
- §13 那条「路径校验由类型强制」改写成「模型驱动的每一次读取都过校验」
- 「工作树只读」与 §14 两处去掉「编译期钉死」的说法，改成接口没有写方法 + 逐字节比对全树的用例
- `.cursor/rules/rust-coding.mdc`：「类型跟主人走」那条改成两个来源 trait 各在 `platform` / `worktree`、`tool` 认它们；「硬约束的代码形状」加一条「安全靠实现保证，不靠类型强制，校验点在 tool」

### 决策

- trait 跟着主人走：`RepoSource` → `platform`，`WorktreeSource` → `worktree`。它们当初放 `security` 的唯一理由就是「读取方法只收 `ValidatedPath`」，理由撤掉位置也就跟着回去
- 校验点只设一处，在 `tool`：模型能看见的读取全从内建 tool 进来，一道门覆盖得了；不在来源实现里再兵一次
- 这条推翻了 [002](002-reviewbot-design.md) 里「两个来源 trait 定义在 `security`，`ValidatedPath` 把强制力放到编译期」那一条，重写了设计

---

## 未决

- M1 尚未开工
