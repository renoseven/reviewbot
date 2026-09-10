# CLI：config init/info、删掉 catalog 命令、run remove 与 prune

## 命令面改成 config 两段加 run remove，catalog 顶层命令去掉

**意图**：增加 `config init`，从模板生成合法空白或示例 config，合法即可。增加 `config info`，分段显示全部配置字段，参考之前的 list 实现。删除 `platform` / `provider` / `model` / `tool` 命令。增加 `run remove $run_id`，成功时不输出。修改 `run prune` 为 `--keep-latest N`（默认 0），成功时输出一句话：多少个 run 被 prune 了。

**步骤**：
- `config`：`EXAMPLE_CONFIG` 读 `src/config/example.toml`；`init_config` 写到 `--config` 或 `~/.reviewbot/config.toml`，已有文件报 `AlreadyExists`，不覆盖；路径解析与 `Settings::load` 共用 `resolve_config_path`
- `record`：`remove_run` 删一个 run 目录，id 不是单层目录名或对不上都是 `RunNotFound`
- CLI：`config init` / `config info`；`run remove`；`run prune --keep-latest`；顶层 `model` / `tool` / `platform` / `provider` 不再解析，`--keep` 也不再解析
- `config info` 按文件分段：标量表（log / triage / review / security）一行一个字段；数组表沿用原先 list 的列与 tool 契约
- `run remove` 成功空输出；`run prune` 成功打 `N runs pruned`（dry-run 为 `would be pruned`）；JSON 仍带 `pruned` / `deleted` / `keep_latest`
- README、design.md 命令面与 CLI 契约用例跟着改；`cargo fmt` / `clippy -D warnings` / `cargo test` 全绿（350 + 40 + 23）

**决策**：
- **init 写示例配置，不另做一份「空白」。** 真正空白过不了校验（模型、尺寸、`allow_extensions` 都必填），示例已经是那份合法模板
- **已有文件不覆盖。** 换路径用 `--config`；覆盖是不可逆的，init 不该默认做
- **catalog 四条命令不留别名。** 同一信息进 `config info` 的四段，两种拼法是多余表面
- **`--keep` 改成 `--keep-latest`，不留旧名。** 用户要的就是这个旗标名
- **remove 成功不打字，prune 成功打个数。** dry-run 也是一句个数，不列 id；要看留下哪些用 `run list`
- **为什么不是 prune 仍打整张报告：** 用户只要一句话报个数

---

## config check 与 config info 的文本

**意图**：`config info` 输出太难看了，优化格式。随后 `config check` 也要优化：不要括号挤在同一行。不要做成和 info 同一套字段表，info 也要再改。再按这份字段改 check：`Path` / `Platforms` / `Provider` / `Credential` / `Budget` / `Model` / `Tools`，最后一行 `ok`。Credential 写 accessible 或 readable。`config info` 四张表的表名改成复数。`config info` 的 HOST / KIND 也可以删除。

**步骤**：
- info 先改成 `[log]` 分段、缩进对齐、tool 卡片、列表按逗号折、说明按词折
- 再改：info 文本收成四张表，列标题全大写、多词 `_` 连接；过长的 tool 描述不印；帮助短说明改成 List platforms, providers, models, and tools
- 框表改由 `comfy-table` 画（`ASCII_FULL_CONDENSED`，关 tty）；表名单独一行，再用复数：PLATFORMS / PROVIDERS / MODELS / TOOLS
- tool 表再收：文本只留 NAME / PURPOSE / ROUNDS，NEEDS 与 ABOUT 只在 JSON
- platforms 文本表再收：只留 BASE_URL / TOKEN_FROM；HOST / KIND 只在 JSON
- check 先收成结论句，再改成 `Label: value`，再按「文件 → 这家 → 库存 → 结论」重排并对齐：Path、Provider、Credential、Budget、Platforms、Models、Tools，最后 `ok`
- JSON 两套都未改结构；契约用例、帮助、README、design 跟着改

**决策**：
- **check 是结论，info 是目录。** 两套视觉语言；check 不复用 info 的记录块
- **info 文本就是四张带边框的表：platforms / providers / models / tools。** 表名用复数；格子交给 `comfy-table`，不手画边框；列标题全大写，多词用 `_`
- **tool 文本表只留名字、用途、轮次。** NEEDS / ABOUT 把表拉得比另外三张宽一截；完整契约仍在 JSON
- **platforms 文本表只留地址和令牌来源。** HOST / KIND 已不是配置字段，印出来是推导结果；JSON 仍带
- **为什么不是 `[log]` 假 TOML：** 用户只要那四张 list
- **为什么不是说明折行或缩短 NEEDS：** 用户说可以删列，列比折行干净
- **check 按阅读顺序：文件、这家、库存、结论。** Path → Provider / Credential / Budget → Platforms / Models / Tools → `ok`。草稿里 Platforms 插在 Path 和 Provider 中间，把「用哪家」拆开了
- **计数标签用复数。** `Model: 2` 会读成模型名叫 2；`Models` 和 `Platforms` / `Tools` 一样是个数
- **check 标签对齐到 `Credential:`。** 值在同一列，不是每行一个空格
- **check 的 Models / Platforms / Tools 是个数。** 选中的模型名不占一行；Provider 与 Budget 才是这次会用的那家
- **Credential 用 readable。** 这条命令做的就是把密钥读出来；accessible 更宽，也不是校验结果
- **check 的 budget 写成 `10.00 CNY`。** 原先 `CNY budget 10` 把币种和金额倒了，也没对齐小数

---

## run prune 的成功输出

**意图**：`1 run pruned` / `0 runs pruned` 这个输出也优化一下。随后说还是得要一句话，不要改成 `pruned  N` 那种字段行。

**步骤**：
- 先改成字段行 `pruned  N`，用户不要
- 始终一句同构：`pruned {n}, retaining {m}.`；不用 `and`

**决策**：
- **始终一句、同一种结构、全部小写、用词正式。** `pruned …, retaining …`；逗号带分词，不用 `and`
- **为什么不是字段行：** 用户要的就是一句话报个数

---

## 帮助文案、默认值显示、命令顺序

**意图**：帮助信息也优化一下：各个命令以及子命令的说明；Default 的配置/显示方式跟随 clap；config 放第一个，命令/子命令按语义重新排序。

**步骤**：
- 顶层顺序改为 `config` → `review` → `run`；config 子命令 `init` → `check` → `info`；run 仍是 `list` → `show` → `remove` → `prune`
- `--config` / `--runs-dir` 改成 clap `default_value`，帮助里是 `[default: ~/.reviewbot/config.toml]`，不再手写 `Default:`；加载时 `expand_user` 展开 `~`
- 短说明收成一行；默认值只留给 clap

**决策**：
- **顺序按生命周期，不是按字母。** 先配置，再评审，再管 run；config 里先写再校验再查看
- **默认值只走 clap。** 写在帮助散文里是第二套显示，和 `[default: …]` 重复
- **`~` 仍是帮助里的默认字符串。** 展开成家目录是加载时的事，帮助要可移植

---

## config init 已存在时的失败输出

**意图**：`config init` 文件已在时，`error:` 前面多一个空行。随后说 `config already exists at /tmp/config.yaml` 不像人话。

**步骤**：
- stderr 上的 `Failure`（含 clap）不再加前导空行；摘要末尾的 `problem()` 仍空一行再写 `error:`
- 契约用例改成 stderr 以 `error: ` 开头；design 里跑不起来的示例去掉那行空行
- `AlreadyExists` 改成 `will not overwrite {path}; pass --config to choose another path`，和 init 帮助同一句话

**决策**：
- **空行只为同一条流上已经打出的字段。** 摘要后面没有它，`error:` 会被读成又一个字段
- **为什么不是 stderr 也空一行：** 这条流上什么都还没打，那行空行就是多出来的
- **报错说「不覆盖」，不说「已经存在」。** 后者是程序员对着文件系统的话；init 要告诉人的是它不肯写、以及怎么换路径

---

## 示例配置跟 config 模块走

**意图**：`config init` 如果来自 `examples/reviewbot.toml`，就挪到源码目录，叫 `config/example.yaml`。

**步骤**：
- 模板改到 `src/config/example.toml`；`EXAMPLE_CONFIG` 用同目录 `include_str!`；删掉 `examples/reviewbot.toml`
- README 的 review 示例不再写 `--config`，默认走 `config init` 写下的那份；帮助、design、契约用例跟着改路径

**决策**：
- **扩展名仍是 `.toml`。** 解析器是 TOML，叫 `.yaml` 是另一套格式
- **为什么不是继续放在 `examples/`：** 它是随二进制走的模板，不是给仓库抄的旁路文档

---

## examples 目录名，以及示例配置按字段注释

**意图**：`examples` 要不要改成 `ci-examples`，或别的名字，或保留。`src/config/example.toml` 重新注释，说明每个字段的作用和取值。每个参数都需要描述，tool 全部注释掉，只提供示例。注释要对齐；后来改成只在自己的段内对齐。注释描述不要讲 no xxx 这种。段首整段说明再优化，说明作用即可。随后：说明这个文件是生成的等等，每个标题的描述稍微多一点点。

**步骤**：
- 目录名不动；里面现在只剩两份 CI 模板
- `example.toml` 按字段重写注释：用途、取值、可省略项；补上 `skip_generated` 与 `follow_symlinks` 两个默认布尔，让每个字段都能在文件里对上
- 每个配置字段在首次出现处写清用途和取值，包括原先没注释的 `base_url` / 三项价格 / `max_output_tokens`，以及 `[[tool]]` 的 `params` 子字段（`type` / `description` / `pattern` / `enum`）
- `[[tool]]` 两段（`cppcheck` / `typecheck`）整段注释；用例断言示例里没有未注释的 `[[tool]]`；README / design 改成「取消注释即启用」
- 行尾注释先整文件对齐，再改成按段对齐；过长的说明在该段的列折行；`[[tool]]` 字段清单两列对齐
- 注释改成正面说法，去掉 no compile / no checkout / no range request / no dot / no shell 这类
- 段首整段说明收成一句作用；取值和规则留在字段行尾
- 文件头改成「`config init` 生成、写到哪、可编辑」；各段标题在作用后再补一句去向或怎么用；一句一行，不在 or 中间切开

**决策**：
- **保留 `examples`。** 这两份是给人抄进别的仓库的样例，不是本仓库的流水线；`ci-examples` 更窄，也容易读成「这里就是 CI」
- **可省略的字段用注释写出取值，默认布尔写进文件。** 只注释已填的键，读者对不上 `alias` / `skip_generated`
- **每个字段只在第一次出现时写描述。** 第二段 `[[platform]]` / `[[model]]` 不重复；同名字段不是新参数
- **`[[tool]]` 全部注释，只当示例。** 列在文件里就是启用；init 写出的配置不该假定本机有 cppcheck / gcc
- **行尾注释按段对齐。** 每张表用该段最长赋值定列，不跟别的表抢；字段清单另排两列
- **注释写它是什么。** 不写 no compile / no range request 这种；取值用允许的那几个词，说明用正面句子
- **段首写作用，再多一句怎么用。** 文件头写明由 `config init` 生成；一句一行，不在 or 中间切开；规则和取值仍在字段行尾
- **示例里不写 `kind`。** `gitlab.com` / `github.com` 由 host 定实现；这行会进 `config init` 写出的文件，不该出现

---

## 配置里没有 kind

**意图**：`# kind = "gitlab"` 这个配置就不应该存在。随后问代码里还要不要这条路径。

**步骤**：
- `PlatformEntry` 去掉 `kind` 字段；实现只由 `gitlab.com` / `github.com` 定；写了是未知字段
- 报错改成点名 host；用例、design 跟着改；`config info` 文本不再印 HOST / KIND

**决策**：
- **配置不设 `kind`。** 它只为自建实例服务；已知 host 不需要，未知 host 也不该靠多写一行去猜
- **配置也不设 `host`。** 网页 host 从已知 API 推出：`gitlab.com` → GitLab，`api.github.com` → `github.com`
- **为什么不是从路径形状反推：** `/api/v4` 猜错会拿着错误端点去打真实平台
