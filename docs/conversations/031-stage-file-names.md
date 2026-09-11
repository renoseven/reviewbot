## stages 文件名去掉编号

**意图**：`stages/` 里的 checkpoint 不要写成 `<n>-<stage>.json`，编号不必进文件名。

**步骤**：
- `layout::stage_file` 改成 `stages/<stage>.json`；`Stage::number()` 仍给进度行用
- 上次把 `design.md` 里无关的引言 / 术语 / 设施改动一并提交了；先 checkout 回上一版，只重写目录树、旧编号文件那一段，以及体积估算里的文件名

**决策**：
- **文件名只留阶段名。** 顺序已经在 `Stage` 上，再写一遍编号只会在改顺序时留下对不上的旧文件
- 为什么不是保留编号方便 `ls` 排序：六个名字本身能认，排序给 `run show` 和进度行
