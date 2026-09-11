# CI 落地

把 `reviewbot` 嵌进 GitLab CI 或 GitHub Actions：`config check` → `review` → `run prune`。配置、密钥、cache 与 artifact 怎么拆，以及怎么用 `summary.json` 自己卡流水线。

**这篇还没对着真实流水线调过。** 命令、flag 和 `summary.json` 字段与现行实现一致；镜像里有没有二进制、token 权限够不够、cache 是否命中、artifact 文件名对不对，都还没在 gitlab.com 或 GitHub Actions 上跑通。示例在 [examples/gitlab-ci.yml](../examples/gitlab-ci.yml) 与 [examples/github-actions.yml](../examples/github-actions.yml)，当作抄写起点，不当作已经验过的配方。

跑评审、写配置见 [README](../README.md)；设计见 [design.md](design.md)。

## 形状

三步，顺序不要倒：

1. `config check` — 只读本地配置和密钥，不发请求。失败用退出码 2，流水线在花钱之前停。
2. `review --output-dir …` — 评这次 MR/PR。GitLab 的 cache 只收项目内路径，所以 `--runs-dir` 写在仓库里（例如 `.reviewbot/runs`，并进 `.gitignore`）。GitHub Actions 的 cache 可以在 workspace 外。
3. `run prune` — `review` 从不删 run。收尾按修改时间清掉旧的。

artifact **只收 `--output-dir`**：`report-<run_id>.md` 与 `summary-<run_id>.json`。不要把整个 runs 目录当 artifact，`traces/` 是 internal 视图。

给了 `--publish` 才发帖。同一条 `review` 再跑一遍会继续这个 run：已完成的阶段不重付，没发出去的评论补发。

## 配置与密钥

配置不进被评的仓库。放镜像路径（示例里的 `/etc/reviewbot/reviewbot.toml`）或 CI 的 file variable。`--config` 指进工作树只 warn、不硬失败，但一份能被 MR 改掉的配置就能对自己网开一面。

配置里只写密钥**来源**：环境变量名，或仓库外、权限 600 的凭据文件。流水线用 CI variable / Actions secret 注入同名环境变量。示例认这几个：

| 变量 | 给谁 |
|---|---|
| `DEEPSEEK_API_KEY` | `[[provider]]` 的 `api_key`（按你配的模型改名） |
| `GITLAB_TOKEN` / `REVIEW_BOT_TOKEN` | GitLab `[[platform]].api_token` |
| `GITHUB_TOKEN` | GitHub `[[platform]].api_token`。示例里用独立的 `REVIEW_BOT_TOKEN`，不要复用 Actions 自带的那枚，除非它真有下面的写权限 |

平台 403 时，错误消息里的口径是：GitLab 要 `api` scope，GitHub 要 `pull_requests: write`。拉 diff 还要读权限；发帖另要写 discussion / review。token 不够，run 停掉，不会退化成只出报告。

`--worktree .` 时 HEAD 必须等于平台给的 `head_sha`。GitHub 的 `actions/checkout` 默认是 merge commit，和 PR head 对不上就会启动失败；要评的是 PR 头，检出得站在那颗 commit 上。这一点也还没在真实 runner 上对过。

## 用 `summary.json` 卡流水线

评审结论不进退出码。要卡合并，读 `--output-dir` 里那份 `summary-<run_id>.json`。字段见 [design.md](design.md) §7，脚本常用这些：

| 字段 | 含义 |
|---|---|
| `overall_score` | 模型给的 0–100；跳过打分时是 `null`，另看 `unscored_reason` |
| `by_severity` | 按严重程度计数 |
| `comments` | 按置信度计数 |
| `stopped` | 预算截断等原因；有值时进程退出码是 3 |
| `unreviewed` / `unproduced` / `unavailable` | 没评到的、没产出的、这次 worktree 做不到的 |

例如：没打分，或 overall 低于 70，就失败（阈值是你的策略，不是 reviewbot 的）：

```bash
jq -e '
  .overall_score != null
  and .overall_score >= 70
  and .stopped == null
' artifacts/summary-*.json
```

`jq` 表达式和 glob 都没在真实 job 里跑过。`overall_score` 是模型对「这次找到了什么」的判断，不是发现条数；空清单仍可能有一个高分。

## 检查器

外部命令的 cwd 是 `<run dir>/checks`，不是工作树根。`requires_build` 还要 `[security].allow_build_tools`。reviewbot 保证自己不写 `--worktree`；拦不住 `cppcheck` 一类往盘上写，除非 runner 只读挂载工作树，或运行用户没有写权限。加一条 `[[tool]]` 见 [README](../README.md) 的「增加外部工具」。
