# 发布按「文件有没有有效评论」过滤

## 发布按「文件有没有有效评论」过滤

### 意图

- 每个被审过的文件加一个字段，表示这份文件有没有有效评论
- 出错、没评论、条目全被丢掉都不算有效
- 发布按这个标志过滤
- 标志不是模型给的，是 reviewbot 根据模型结论（过完 merge 检查后）自己标的
- 不用考虑前向兼容
- 把这个步骤的对话单独拆出来

### 步骤

- `MergeOutput` 加 `files: Vec<ReviewedFile>`（`path` + `has_valid_comments`），不设 `#[serde(default)]`
- `reviewed_files` 按 chunk 路径去重保序，任一幸存评论就把该路径标 true
- `Publish::post` 只发 `has_valid_comments` 为 true 的文件上的评论；summary 仍按 run 发，这种跳过不计入 `skipped_as_duplicate`
- merge 测空文档、解析失败、全丢掉、保住一条；publish 测无效文件不发 inline、仍发 summary
- 现有会发评论的 publish 夹具补上 `files`
- 从 `027` 拆到本文件

### 决策

- 标志由 merge 从幸存 `comments` 推导，不进模型输出、不进 `submit_comment`
- 切成多块的同一文件只占一行，任一小块留下评论即为 true
- 旧 checkpoint 缺 `files` 直接反序列化失败：用户明确不要前向兼容
- 为什么不是 publish 现场数评论：评论列表和「这份文件算不算有效」是两件事（夹具里可以有评论但标志为 false）
- 为什么不是留在 `027`：那是编码/测试规范与收 `src/` 形状；发布过滤是另一类问题
