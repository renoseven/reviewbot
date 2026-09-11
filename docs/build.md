# 编译与安装

从一台没有 Rust 的 Linux 机器，装好工具链，编出 `reviewbot`，装到 `PATH` 上。跑评审、写配置见仓库根目录的 [README](../README.md)；设计见 [design.md](design.md)。

只支持 Linux（含 WSL）。macOS、Windows 上不编、不装、不跑。目录锁用 `flock`，默认路径按 Unix home 写；那两套系统上的工具链、链接器和 `PATH` 约定本文不覆盖。

`Cargo.toml` 没有 feature 开关。`cargo build --release` 编出来的就是完整二进制：CLI、六个阶段、平台与协议适配器、内建 tool 都在里面。外部检查器（`cppcheck` 一类）是运行时配置，不参与这次编译。

## 需要什么

| 项 | 要求 |
|---|---|
| 操作系统 | Linux（含 WSL）。其余 OS 不支持 |
| 编译器 | `rustc` **1.85 或更新**。本仓库 `edition = "2024"`，更旧的工具链读不了 `Cargo.toml` |
| 链接器 | 系统 C 工具链。Debian / Ubuntu / WSL 装 `build-essential`；Fedora 装 `gcc`；其他发行版装能提供 `cc` 的包（`gcc` 或 `clang`） |
| 源码 | 一份仓库检出。编译本身不访问网络上的 GitLab / GitHub，但第一次拉 crates.io 依赖需要出网 |
| 不需要 | OpenSSL 开发包。TLS 走 `reqwest` 的 `rustls`，不链系统 `libssl` |

评 diff 或对仓库做 `git diff` 才需要 `git`；只编译、安装二进制可以没有它。

## 1. 安装 Rust

用 [rustup](https://rustup.rs/)。它同时装 `rustc`、`cargo`，并把 `~/.cargo/bin` 接到 `PATH`。

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

按提示选默认安装（stable）。装完让当前 shell 读到 `PATH`：

```bash
source "$HOME/.cargo/env"
```

以后新开的终端，rustup 装进去的那行 `PATH` 会自己生效。没有的话，把上面那句写进 `~/.bashrc` 或 `~/.zshrc`。

已经装过 rustup、但 `rustc` 低于 1.85：

```bash
rustup update stable
```

核对：

```bash
rustc --version    # 须 ≥ 1.85.0
cargo --version
```

官方源慢时，可以换国内镜像再跑同一条安装命令，例如 [rsproxy](https://rsproxy.cn/)：

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
```

`cargo` 拉依赖另看它的 crates.io 源，和 rustup 的这两个变量不是一回事。需要的话再给 `~/.cargo/config.toml` 配镜像，见该镜像站点的说明。

## 2. 系统链接器

Debian / Ubuntu / WSL 上还没有 `cc` 时：

```bash
sudo apt update
sudo apt install -y build-essential
```

Fedora：

```bash
sudo dnf install -y gcc
```

其他发行版装本机提供 `cc` 的包。`cc --version` 能印出版本即可。缺链接器时，`cargo build` 会报 `linker 'cc' not found`。

## 3. 取得源码

已在仓库根目录就跳过。否则：

```bash
git clone <本仓库 URL> reviewbot
cd reviewbot
```

后面的命令都在含 `Cargo.toml` 的根目录执行。

## 4. 编译

开发时用调试构建，改完就能跑：

```bash
cargo build
```

产物是 `target/debug/reviewbot`。带调试符号，比发布版慢、体积大。

交付、装到机器上用发布构建。`--locked` 按仓库里的 `Cargo.lock` 解析依赖，避免 crates.io 上后来的修订把图解偏：

```bash
cargo build --release --locked
```

第一次会下载依赖并编整棵图，几分钟到十几分钟，看机器和网络。成功后二进制在：

```
target/release/reviewbot
```

可以直接跑：

```bash
./target/release/reviewbot --version
./target/release/reviewbot --help
```

`--version` 印的是 `Cargo.toml` 里的 `version`（现在是 `0.1.0`）。

## 5. 安装

把发布版拷进 cargo 的 bin 目录，通常是 `~/.cargo/bin/reviewbot`。rustup 已经把这个目录放进 `PATH`，之后任意目录都能调用 `reviewbot`：

```bash
cargo install --path . --locked
```

这条会再做一次发布构建（缓存命中则很快），然后覆盖同名旧文件。指定别的前缀：

```bash
cargo install --path . --locked --root /usr/local
```

`--root /usr/local` 落到 `/usr/local/bin/reviewbot`，写入系统目录需要相应权限。

也可以不走 `cargo install`，自己拷：

```bash
install -m 755 target/release/reviewbot ~/.local/bin/reviewbot
```

目标目录必须已在 `PATH` 里。

卸掉 `cargo install` 装上的那一份：

```bash
cargo uninstall reviewbot
```

## 6. 核对

`which reviewbot` 应指向刚才装的那份，而不是仓库里的 `target/`。然后：

```bash
reviewbot --version
reviewbot --help
```

到这里编译和安装就结束了。要跑一次评审，还得有配置和密钥，见 [README](../README.md) 的 `config init` / `config check`。配置**不**从当前目录读，只认 `--config` 或 `~/.reviewbot/config.toml`。

## 测试（可选）

编译不依赖测试。要确认这棵树能过现有用例：

```bash
cargo test --locked
```

测试离线，不打真实平台、不调真实模型。失败先看本机 `rustc` 是否过旧、`Cargo.lock` 是否被改过。

## 常见问题

**`feature edition2024 is required`**  
`cargo` / `rustc` 低于 1.85。走第 1 节更新 stable，再看 `rustc --version`。`rustup --version` 报的是 rustup 自己，不是编译器。

**`linker 'cc' not found`**  
缺系统 C 工具链，走第 2 节。

**编过了，`reviewbot: command not found`**  
二进制在，但目录不在 `PATH`。`cargo install` 默认写 `~/.cargo/bin`；确认 `echo "$PATH"` 里有它，或重新 `source "$HOME/.cargo/env"`。

**只想更新已经装过的一份**  
在仓库根目录再跑一遍 `cargo install --path . --locked`。它覆盖 `~/.cargo/bin/reviewbot`，不碰 `~/.reviewbot/` 里的配置和 runs。

**CI 里怎么编**  
同一条：先有 1.85+ 的工具链和链接器，再 `cargo build --release --locked` 或 `cargo install --path . --locked --root <前缀>`。流水线里*调用*已经装好的 `reviewbot` 去评 MR/PR，见 `examples/gitlab-ci.yml` 与 `examples/github-actions.yml`，那两份假定镜像或 runner 上二进制已在 `PATH`。
