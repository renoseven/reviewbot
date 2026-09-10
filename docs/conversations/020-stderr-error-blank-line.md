# stderr 上的 error 不再空一行

## config init 已存在时多一个空行

**意图**：`config init` 文件已在时，`error:` 前面多一个空行。随后说 `config already exists at /tmp/config.yaml` 不像人话。

**步骤**：
- stderr 上的 `Failure`（含 clap）不再加前导空行；摘要末尾的 `problem()` 仍空一行再写 `error:`
- 契约用例改成 stderr 以 `error: ` 开头；design 里跑不起来的示例去掉那行空行

**决策**：
- **空行只为同一条流上已经打出的字段。** 摘要后面没有它，`error:` 会被读成又一个字段
- **为什么不是 stderr 也空一行：** 这条流上什么都还没打，那行空行就是多出来的
- **「已经存在」那句人话跟 `config init` 走。** 这条错误类型是那次命令加上的，文案改动不进这次提交
