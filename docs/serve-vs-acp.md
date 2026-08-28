# `serve` 与 ACP 双轨决策

## 结论

保留 `jucode serve`，并新增 `jucode acp`，不以 ACP 替换现有 `serve`。Fable 与 Sol 的独立评估得出了相同结论。

两者面向不同集成场景：

- `serve` 继续作为 JuCode Desktop 的完整内部协议。
- ACP 作为标准化 IDE 接口，服务 Zed、JetBrains 等客户端。

## 不能删除 `serve` 的原因

ACP 当前无法无损承载 Desktop 已依赖的完整语义，包括：

- `steer` 与 pending queue；
- `mcp_list`、`mcp_set`、`mcp_remove`、`mcp_toggle`（Desktop MCP 设置）；
- hunk 子集审批；
- 分支树、`checkout` 与 `fork`；
- `rewind` 与 `checkpoint_view`；
- goals；
- `subagent_lifecycle`；
- compaction 进度；
- 每回合 token 用量；
- `trust_prompt`；
- `fill_input`。

更关键的是，Desktop 的 `ChatState` 事件归约器本身就是按 `serve` dialect 构建的。删除 `serve` 不只是协议适配，而会迫使 Desktop 大规模重写状态模型和交互逻辑。

## 实施边界

`jucode acp` 应复用同一套 agent core，但保持独立协议适配层。新增能力可按需要映射到两条协议；不应为了追求单一协议而削弱 Desktop 功能，或把 JuCode 私有扩展强塞进 ACP。
