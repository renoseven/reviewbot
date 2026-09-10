# 配置指纹不再进 run_id

## 配置指纹不再进 run_id

**意图**：考虑是否有必要进行 config 与 run 的 hash 关联，使用当前配置是否更好。

**步骤**：
- `run_id(identity, head_sha)`，去掉指纹参数。同一 MR 同一 commit 永远是同一个目录
- 指纹切成三片：`input`（`[[platform]]`、是否给了 `--worktree`）、`triage`（`[triage]`）、`review`（`[review]`、`[[tool]]`、`[security]`、`[[model]]`、`[[provider]]`、`--model`）。没有 merge 片。`[log].level` 仍排除
- 重入：先比输入身份与 `head_sha`，不符即失败（堵住 `--run-id` 指到另一个 MR 的洞）；否则找最早变化的片，`mark_incomplete` 到那个阶段，并且 `Recorder::discard_from` 删掉 checkpoint 文件，用当前配置继续
- `Error::FingerprintMismatch` 换成 `Error::DifferentInput`，退出码仍是 2。不同输入那次不印 `next:`，因为同一条命令正是指错目录的那条
- 作废警告只写进 run 的 `log`（`tracing::warn`）。`tracing` 一个字节都不能上 stdout / stderr

**决策**：
- **身份只认输入和 commit，配置改了进同一个目录、按阶段作废。** 指纹进 `run_id` 会为每次改配置留下一个孤儿 run
- **为什么不是改配置就开新 run：** 同一 MR 同一 commit 的结论不该因为调了一下 `max_hits_per_search` 就换目录；孤儿 run 还得人去 `prune`
- **为什么不是改配置就整份拒：** 那是从前 `resume` 的做法，改一个 `[triage]` 就把已经花过钱的 `input` 也废了
- **`mark_incomplete` 不够。** `Review::run` 读自己的进行中快照，只清旗不删文件的话，旧设定下看过的分片会被接着用
- **作废警告不上终端。** 要上屏幕得另发一个 `Progress` 事件，是另一次改动

---

## 未决

- 配置作废时要让终端看见，得给 `Progress` 加事件；今天只有 run 目录里的 `log` 有那一句
