# worktree 收成本地缓存

## 本地与仓库拆成不同的工具

**意图**：把本地的实现与 repo 的实现写成不同的 tool。假设一个 workspace 上面有 PR 的相关信息，模型可以调用 repo 相关的工具先获取 repo 文件列表，逐个拉取需要的文件，然后再调用本地工具进行检查。如果 worktree 是外部传入的，任何模型以及工具都不应该对它进行写入操作。worktree 是本地缓存，也是跑外部检查器的基座。

**步骤**：
- `WorktreeSource` trait 与 `Checkout` / `FetchedWorktree` / `Widest` 三个实现删掉，收成 `enum Worktree { Empty, Local { root, repo: Option<Repo> }, Cache { root, repo } }`
- 内容工具按动作拆成八个：本地 `list_local_files` / `suggest_local_read` / `read_local_file` / `search_local_regex`，仓库 `list_repo_files` / `fetch_repo_file` / `search_repo_regex` / `search_repo_keyword`。`RESERVED_TOOL_NAMES` 11 条
- `read_local` 不再自动取回；取回与读是两个动作，缓存未命中就是未命中
- `CommandTool` 在 spawn 前对每个 `Shape::Path` 参数调一次 `fetch`；cwd 从 worktree 根改到 `<run_dir>/checks`；Path 参数展开成绝对路径，输出回模型前剥掉 worktree 根前缀。`cache/` 与 `checks/` 登记进 `record/layout.rs`
- `orient` 按变体选列举（`Local` 走 `list_local`，`Cache` 走 `list_repo`，`Empty` 无布局）；prompt 补「先看本地、缺了再去 repo 拉」

**决策**：
- **列仓库和列本地是两个动作，模型该被告知差别，而不是故意揉在一起。** 当年 §8 反对的是两个名字答同一个问题；八个动作各答各的，不是那件事
- **仓库那半没有「读」，本地那半没有「拉」。** 要正文就先 `fetch_repo_file` 再 `read_local_file`。两组不是镜像
- **为什么不是继续四个名字、差别只写进描述：** 那会把「仓库在评审 commit 上有什么」和「此刻磁盘上有什么」说成一件事，模型分不清
- **为什么不是「取回并直接回正文」的复合工具：** 等于把刚拆开的两个动作又粘回去，又变成两个名字答同一个问题
- **写盘要 root 与 repo 这一对，只有 `Cache` 握着。** 模块里唯一的 `fs::write` 落在 match 到 `Cache { root, repo }` 的那一臂；`Local` 与 `Empty` 凑不出这一对

---

## Worktree 收成一个 enum，中间词汇删掉

**意图**：需要讨论 Worktree 提供的能力和 tool 需要的能力。Worktree 直接改成 enum，跟 Kind 合并。struct Repo 也改成 enum 呢。search 时机上是 repo 的搜索能力，它们不能同时支持么。没传 workspace 参数的时候直接生成一个目录不就行了；run 自己开的那份改名成 cache。regex_search 与 keyword_search 改成 bitflags。变体从 RepoCache 改成 Cache。Search 改成 SearchKind。repo 为什么不直接放到 platform 上 `platform.repo() -> Repo`。WorktreeContext 不太喜欢。

**步骤**：
- 删掉 `Reach` / `Abilities` / `Content` / `Tool::needs()` / `WorktreeContext` / `worth_reporting`
- `Repo { source, capabilities }` 住在 `platform`；`Platform::repo()` 取代 `repo_source()` + `capabilities()`。`Capabilities` 改成位集 `REGEX_SEARCH` / `KEYWORD_SEARCH`（empty 即答不了）。`Search` 改名 `SearchKind`，成为 `RepoSource::search` 的 per-call 参数
- `RepoSource` 加 `size(path)`（GitHub 读缓存树的 `size`，GitLab 走 `HEAD .../files/:path` 的 `X-Gitlab-Size`）与 `cached_body(path) -> Option<String>`（手上有就给、绝不为此发请求）。`Listing` 条目改成 `File { path, bytes: Option<u64> }`
- 编排改成：建 platform → `Input::identify` 拿 `head_sha`（检出走关联函数 `Worktree::head_at`）→ `bind_repo` → 算 `run_id` 并上锁 → 建 `Worktree`（`Cache` 那支在这里建 `<run_dir>/cache`）→ 建工具与 `PathPolicy`。`Adapters` 缩成 `{ platform, protocol, redactor }`，锁之后的那半是 `stage::Equipment { worktree, tools }`。删 `OnceLock` 与 `open_worktree`
- 可用性落成 `availability.rs` 里五个收 `&Worktree` 的函数（`no_files` / `no_repo` / `no_regex_search` / `no_keyword_search` / `no_whole_tree`），每条挨着定义拒绝、描述短句、`tool list` 前置条件。报告用的「缺了什么」在 `Worktree::went_without`

**决策**：
- **变体只在带不同数据或允许不同操作时才值得。** `Worktree` 两条都占（`Empty` 连 root 都没有，只有 `Cache` 写得了盘），所以是 enum；`Repo` 一条都不占（始终是同一个 `Arc<dyn RepoSource>`），所以留作 struct
- **两种搜索引擎不再坍缩成一个。** 今天两边都开时取 regex、把关键词索引丢掉；GitLab 上那是两个不同的索引、覆盖面不一样
- **`SearchKind` 是枚举不是位。** 这一次调用恰好一个引擎，位集表达不了
- **删除 `Reach` / `Abilities`。** 它们是为「一套词汇两边用、能不能答是一次集合相减」存在的；变体定下来之后条件直接对着它们说，中间那层影子词汇没有用处，而且位集表达不了「或」
- **`Repo` 跟主人走，落到 `Platform::repo()`。** 两个访问器永远一起用，分开就留了个能把某个平台的 source 配上不属于它的 `Capabilities` 的缝
- **为什么不是 `Option` 字段加 `expect` 糊编排：** 那只是把晚绑定从 root 挪到别处。后一半整体不存在，用 `Equipment` 表达
- **为什么不是留 `WorktreeContext`：** 它没有一个方法维护跨三个字段的不变式，是个穿透层；拒绝与描述进 `availability`，三个字段在八个 struct 里重复认下来
- **假实现只落在 `RepoSource`。** worktree 不是扩展点；契约用例盖真 `Worktree` 在临时目录上

---

## 用什么工具省轮次

**意图**：还可以给模型提供什么本地 / repo 工具，减少调用。`list_repo_files` 到底要不要加大小，要不要把 `stat_local_file` 拆成两个调用。既然 `stat_local_file` 的语义变了，返回内容和名字也改一下，叫 `suggest_local_read`。仓库还有什么可以提高效率的工具。本地呢，还有什么优化。

**步骤**：
- `fetch_repo_file` 收路径数组、逐项成功或失败，上限走必填的 `[review].max_files_per_fetch`；超限文件先问 `size` 再下载，挡住「先下完再拒」那个洞
- 两个列表工具尽力带字节数：本地遍历本来就 stat 了；仓库那边平台一次给的才带（GitHub 递归树有，GitLab 的 tree 没有）
- `suggest_local_read` 三种结论：整份读得下 / 分段并给出切好的区间 / 超过 `max_file_bytes` 读不了。行数超限时不读，`stat_local` 的 `lines` 为 `None`
- 三个搜索工具的命中按文件分组加计数，被截掉的文件连名字带条数；计数在 `deny_paths` 之后算
- 仓库搜索过完路径校验后只暖 `cached_body` 回 `Some` 的那些，写入走 `Worktree::warm`（只 `Cache` 写盘）
- `search_local` 跳过非文本（与落盘共用前几 KB 出现 NUL 那条），答案里报跳过数；`read_local` / `stat_local` 从 `fs::metadata` 判上限，超了不读正文

**决策**：
- **列表带了大小就是平台一次给的；没带不代表量不出来。** 模型也不依赖它才安全——`fetch_repo_file` 内部已经在下载前挡住超限文件
- **`suggest_local_read` 不拆成「只报字节数」另两个工具。** 拆出来的那个没人会调，省下的那一轮已经被列表省掉了
- **为什么不是 blame：** GitLab 有 REST、GitHub 只在 GraphQL，两边形状不一样，`Capabilities` 还得再加一位，这轮不值得
- **为什么不是在平台层暖缓存：** 那样会绕过 `deny_paths` 那个唯一校验点。也绝不能因此偷偷下载：`cached_body` 回 `None` 就是这个优化在这个平台上不生效
- **为什么不是 `fetch_repo_file` 兼收 glob：** 能省一对 list→fetch，但一次拉一整棵子树的代价和「模型点名要哪些文件」对不上，没做

---

## 掏字段的自由函数收回到实体上

**意图**：下面这些是典型的反模式，把函数实现放到对应实体上，排查 tools 与 worktree。`tool_schemas(context.tools)` / `concluding_tool_schemas(context.tools)`；`went_without(worktree)` 再转一层；`worktree_paragraph` / `repository` / `paths_of` 在外面 match 变体或掏 `root`。

**步骤**：
- `Worktree` 补 `repo` / `is_empty` / `is_checkout` / `is_cache` / `list_project` / `went_without`；`cached_body` 与内部 `repository` 走 `repo()`
- 报告用的「这次 run 缺了什么」从 `availability::went_without` 挪到 `Worktree::went_without`；模型读的拒绝句仍留在 `availability`，改问方法不再 match 变体
- `Registry::request_schemas` 给出请求用的 schema；删掉 review / merge 里的转发函数
- `Prompts::worktree` 按形状选模板；`orient` 改调 `list_project`
- 八个内容工具的描述收到各自 `describe`；测试里的 `paths_of` 删掉

**决策**：
- **变体只在 `Worktree` 自己的方法里 match。** 外面问事实，不拆字段
- **报告句跟 worktree 走，拒绝句跟 tool 走。** 前者给人看、是这次 run 的事实；后者只有模型读，继续集中在 `availability`
- **为什么不是 `Worktree` 去选 prompt 文件：** 那会让适配器依赖阶段；模板主人是 `Prompts`，形状判断仍问 worktree
- **为什么不是再做一个 context 把 tools 和 worktree 捆起来：** 那就是刚删的 `WorktreeContext`
