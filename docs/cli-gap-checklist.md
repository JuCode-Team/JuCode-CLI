# JuCode CLI 差距清单（已锁定）

> 配套文档：`docs/coding-agent-audit.md`（审计基线 `main@0758fef` / v0.1.11）。
> Owner decisions 已于 2026-08-28 锁定。`[x]` 表示接受并纳入计划；`[ ]` 仅表示明确否决或推迟，不再表示“待拍板”。

## 清单

| 做? | ID | 功能 | Status | Level | Notes |
|---|---|---|---|---|---|
| [x] | F2 | Chat Completions 协议支持 | missing | must | 作为 vendor package 的手写协议之一；复用阻塞 I/O，不引入 `genai`/`tokio` |
| [x] | F3 | 完善 providers | partial | must | 在独立 vendor package 中手写 Responses、Anthropic Messages、Chat Completions 等协议与 provider 适配 |
| [x] | F5 | MCP prompts | missing | must | 补齐标准 MCP prompts 能力 |
| [x] | F6 | MCP resources | missing | must | 补齐 resources 的发现、读取、订阅及结果表达 |
| [x] | F7 | MCP HTTP OAuth | missing | must | 为 streamable HTTP 补齐 OAuth 流程 |
| [x] | F8 | `serve` 协议参考文档 + version 字段 | partial | must | 文档化现有 NDJSON 协议并加入版本；不会在评估完成前声称 ACP 将替换 `serve` |
| [x] | F9 | TUI `@` 文件提及补全 | missing | must | 完成文件选择、补全和上下文注入体验 |
| [x] | F10 | TUI `!` shell 直通 | missing | must | 提供惯例 shell 交互 |
| [x] | F11 | 自定义 slash 命令 | missing | must | 支持用户配置的 prompt/slash commands |
| [x] | F12 | git 感知状态栏 | missing | must | 展示分支和工作树状态 |
| [x] | F13 | README/文档事实性修正 | partial | must | 使 provider、能力和使用说明与实现一致 |
| [x] | F14 | CLI ACP server | missing | must | 作为附加协议入口实现；embed 方案仍在评估，较可能与 `serve` 双轨运行 |
| [ ] | F15 | LSP/DAP | missing | non-goal | 明确不做，不内置 LSP/DAP 子系统 |
| [ ] | F16 | multi-root 工作区 | missing | later | 暂缓；先完成单 workspace 的 BASIC 路径边界 |
| [x] | F17 | PR CI（fmt/test/clippy） | missing | must | 修复现有问题后增加 PR 门禁；与 G24 同一项 |
| [x] | F18 | TUI 图像粘贴 UX | partial | must | 基于已有图像附件后端补齐终端粘贴交互 |
| [x] | F19 | 编辑工具可配置 | missing | must | 默认仅启用 `hashline_edit`；`str_replace`、`write`、`apply_patch` 默认关闭，用户可显式启用 |
| [x] | F20 | 完整 Skills | partial | must | 补齐发现、安装、加载、调用、更新及错误处理的完整体验 |
| [ ] | F1 | 任意 sandbox | missing | non-goal | 不做 OS sandbox、Landlock、Seatbelt、`bwrap` 或 `sandbox_command` |
| [ ] | F4 | 持久化命令模式权限规则 | missing | non-goal | 保持 `ReadOnly`/`AutoEdit`/`FullAuto` 三种 approval mode，不增加跨会话命令 pattern |
| [ ] | G01 | 任意 sandbox（同 F1） | missing | non-goal | 与 F1 合并决策；不实现任何 sandbox 路线 |
| [x] | G02 | workspace 文件路径边界 | missing | must | 作为 BASIC permission 实现；它是应用层边界检查，不是 sandbox，也不得包装成 sandbox |
| [x] | G04 | headless 完善及安全默认 | missing | must | 默认不隐式 full-auto；执行 mutation 必须显式传 `--full-auto`，同时完善 headless 能力 |
| [x] | G05 | 进程组管理 / PTY | partial | must | 补齐可靠清理、退出和孤儿进程处理 |
| [x] | G06 | 会话级 kill | missing | must | 中断或退出时统一收割子进程 |
| [x] | G07 | 崩溃安全的 session journal | partial | must | 加固 JSONL 写入、同步、尾部半行检测及恢复 |
| [x] | G09 | config/auth 原子写 + 坏 JSON 显式报错 | missing | must | 使用临时文件 + rename，禁止损坏 JSON 静默重置 |
| [x] | G20 | 优秀 subagents + 写隔离 | partial | must | 提升委派、结果汇总和可观测性，并确保 subagent 写入相互隔离 |
| [x] | G23 | 将 `browser_open` 移出 core | done（错位） | must | 改由 Desktop extension 注入并从 core 工具表删除 |
| [x] | G24 | CI（同 F17） | missing | must | 与 F17 合并执行 |
| [x] | G25 | 可复现的 Codex 对比 eval | missing | must | 提交固定任务集、运行脚本、指标和基线，替代不可复现的快照结论 |
| [x] | G26 | LLM 请求重试/退避与流中断恢复 | partial | must | 纳入整体 reliability engineering |
| [x] | G27 | 工具输出截断/大小上限统一策略 | partial | must | 纳入整体 reliability engineering |

## 已否决

- **Sandbox（F1/G01）**：任何 OS sandbox、Landlock、Seatbelt、`bwrap`、`sandbox_command` 均为明确非目标。workspace 路径边界只是 BASIC permission。
- **持久化命令规则（F4）**：不增加命令 pattern 的持久 allow/deny 规则；只保留三种 approval mode。
- **LSP/DAP（F15）**：不实现内置或独立的 LSP/DAP 子系统。

`F16` multi-root 未进入本轮计划，待单 workspace 路径边界稳定后再评估。

## 已锁定的技术方向

### D1 权限边界

实现单 workspace 的文件路径边界，并明确标记为 BASIC permission。它不提供进程隔离能力，也不构成 sandbox。任何 sandbox 实现或可配置命令包装器均不在范围内。

### D2 Provider 与协议

创建轻量 vendor package，手写 provider 协议和适配，继续使用 blocking I/O + threads。Chat Completions 是该 package 的组成部分；禁止引入 `genai` 或 `tokio`。

### D3 MCP 与 Skills

MCP 必须从 tools-only 补齐 prompts、resources 和 HTTP OAuth。Skills 必须形成完整、可靠的端到端体验，而不是只保留 marketplace 安装入口。

### D4 嵌入协议

为现有 `serve` 编写协议文档并加入 version，同时新增 CLI ACP server。embed protocol 仍在评估；当前不得声明 ACP 将替换 `serve`，双轨运行是较可能的方向。

### D5 权限模式与 headless

保持 `ReadOnly`、`AutoEdit`、`FullAuto` 三档，不做持久化命令 pattern。headless 默认不得隐式 full-auto，写入或执行必须显式传 `--full-auto`。

### D6 编辑工具

编辑工具列表由 config 控制。默认只启用 `hashline_edit`；`str_replace`、`write`、`apply_patch` 默认关闭，但允许用户逐项开启。

### D7 Subagents

将 subagent 质量、委派体验、结果汇总和写隔离作为同一项 must-have。具体隔离机制需满足并发写入互不污染，不以 sandbox 为前提。

### D8 可靠性与持久化

系统化完成进程生命周期、请求重试、流中断恢复、输出预算和错误可观测性。保留 JSONL session journal 并实现崩溃安全恢复；config/auth 使用原子写，损坏数据必须显式报错。

### D9 TUI、评测与工程

完成 `@` 文件、`!` shell、自定义 slash commands、git 状态栏和图像粘贴；提供可复现的 Codex 对比 eval；修复 README/CI；将 `browser_open` 移出 core。
