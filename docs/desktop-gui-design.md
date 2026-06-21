# Desktop GUI 设计文档（Tauri + JSON 子进程协议）

本文档描述如何为 JuCode-CLI 增加桌面 GUI，并让桌面端与 CLI 引擎通信。
方案对标 Codex：GUI 作为外壳，把 `jucode` 当子进程（sidecar）启动，通过 stdin/stdout
上的行分隔 JSON 双向通信。本文是设计参考，不是实现承诺。

## 1. 目标与非目标

目标：

- 复用现有引擎 `jucode-agent-core`，CLI / TUI / GUI 三端共享同一套引擎与同一套事件。
- 引擎侧改动最小：只新增一个常驻协议模式，输出侧尽量复用已有的 JSON 序列化。
- 协议稳定后可承载多个前端（桌面端、未来的 IDE 扩展等）。

非目标：

- 不在本阶段引入数据库、异步框架或复杂事件总线（沿用现有 `mpsc` + 轮询模型）。
- 不改变引擎的会话语义（会话树、压缩、目标等保持不变）。

## 2. 现状：解耦已经就绪

引擎与 UI 已经通过事件流解耦，这是 GUI 化的基础。

| 层 | 位置 | 说明 |
|---|---|---|
| 引擎 | `crates/agent-core/src/core.rs` `AgentCore` | headless，无终端依赖 |
| 事件定义 | `crates/agent-core/src/event.rs` `AgentEvent` | 引擎对外唯一输出类型 |
| 前端契约 | `crates/tui/src/lib.rs:286` `TuiRuntime` trait | UI 调引擎的全部入口 |
| JSON 序列化 | `src/main.rs:210` `event_json()` | 已能把 `AgentEvent` 转 JSON |
| TUI | `crates/tui/src/lib.rs` | “把 `AgentEvent` 画到终端”的一个客户端 |

引擎的输入面只有 5 个动作：

- `submit_user_message(message) -> Vec<AgentEvent>`（`core.rs:201`）
- `steer() -> Vec<AgentEvent>`（`core.rs:214`，运行中用队列里的下一条消息接管当前回合）
- `interrupt() -> Vec<AgentEvent>`（`core.rs:233`，**已存在但当前 TUI 未接线**）
- `handle_command(input) -> (bool, Vec<AgentEvent>)`（`core.rs:253`，`bool` 表示是否退出）
- `poll_events() -> Vec<AgentEvent>`（`core.rs:528`，驱动后台工作线程的事件出队）

引擎的输出面只有一种：`AgentEvent`（27 个变体，已在 `event_json()` 里全部覆盖）。

TUI 的输入语义（`crates/tui/src/lib.rs`）可作为 GUI 交互的参照：

- Enter：文本以 `/` 开头 → `handle_command`；否则 → `submit_user_message`（`lib.rs:625`）
- Esc：运行中且有 pending 消息 → `steer`；否则清空输入（`lib.rs:582`）
- Ctrl-C / Ctrl-Q：退出
- 主循环每 ~30ms 调用一次 `poll_events()` 与 `model_status_event()`（`lib.rs:386`）

## 3. Codex 的做法（参考）

Codex 的 GUI / IDE 扩展不直接链接引擎，而是把 `codex` 二进制当子进程启动，
在 stdin/stdout 上跑一条行分隔 JSON 协议：前端把“提交”写进 stdin（Submission Queue），
从 stdout 读“事件”（Event Queue）。一条协议同时服务 TUI 之外的所有前端。

JuCode-CLI 的 `--headless` 模式（`src/main.rs:68`）已经实现了这条协议的**输出侧**，
只是它是一次性的（读一条 prompt → 跑完 → 打印 `final_result` → 退出）。
GUI 需要的是把它变成**常驻、双向**的版本。

## 4. 架构总览

```
┌──────────────────────────┐     stdin (NDJSON 命令)      ┌────────────────────────┐
│  Tauri 应用                │  ─────────────────────────▶  │  jucode serve           │
│  ├─ WebView 前端 (TS)      │   {"op":"user_message",...}  │  └─ AgentCore            │
│  │   渲染 AgentEvent       │                              │      (现有引擎，不改语义) │
│  └─ Rust 壳                │  ◀─────────────────────────  │                          │
│     spawn sidecar + 转发   │     stdout (NDJSON 事件)     │  worker thread (mpsc)    │
└──────────────────────────┘   {"type":"assistant_delta"} └────────────────────────┘
```

- 传输：子进程 stdio，**NDJSON**（一行一个 JSON 对象，`\n` 分隔；UTF-8）。
- stderr：保留给引擎的日志/诊断，不参与协议。
- 二进制分发：沿用现有 `npm/cli-*` 的按平台二进制机制（`npm/cli/bin/jucode.cjs`），
  Tauri 用 sidecar 同款思路打包对应平台的 `jucode`。

## 5. 协议规格

### 5.1 启动

GUI 启动 `jucode serve`。引擎进入常驻模式，**立即输出一批启动事件**
（等价于 `startup_events()`：`startup` + `model_status` + `command_list`），
然后开始监听 stdin。

### 5.2 GUI → 引擎（命令 / Submission Queue）

每行一个对象，必含 `op` 字段：

| op | 字段 | 映射到引擎 |
|---|---|---|
| `user_message` | `content: string`，可选 `images: string[]`（本地图片路径） | `submit_user_message_with_images(content, images)` |
| `command` | `input: string`（如 `/model gpt-5.4`） | `handle_command(input)` |
| `steer` | — | `steer()` |
| `interrupt` | — | `interrupt()` |
| `shutdown` | — | 干净退出进程 |

可选：每条命令带 `id: string`，引擎在对应产生的事件上回填同一 `id`（便于前端关联）。
首版可不做，靠事件顺序即可。

示例：

```json
{"op":"user_message","content":"refactor foo.rs"}
{"op":"command","input":"/model"}
{"op":"interrupt"}
{"op":"shutdown"}
```

`handle_command` 返回的 `quit=true`（`/quit`、`/exit`）在 serve 模式下等同收到 `shutdown`。

### 5.3 引擎 → GUI（事件 / Event Queue）

直接复用 `event_json()` 的现有输出，**不新增字段、不改 schema**。前端按 `type` 分发：

```json
{"type":"user_message","content":"refactor foo.rs"}
{"type":"assistant_start"}
{"type":"assistant_delta","delta":"I'll "}
{"type":"tool_start","call_id":"1","name":"read"}
{"type":"tool_output","call_id":"1","name":"read","output":"...","is_error":false}
{"type":"context_usage","tokens":12345,"tokenizer":"o200k"}
{"type":"status","message":"ready"}
```

全部 27 种事件已在 `src/main.rs:210` 覆盖，含 `tree_view` / `model_view` / `resume_view` /
`goal` / `transcript` 等结构化视图，可直接驱动前端的选择器/侧栏。

### 5.4 时序（一次普通对话）

```
GUI                         jucode serve
 │  spawn                          │
 │ ◀──── startup / model_status / command_list
 │ ── {"op":"user_message"} ─────▶ │  submit_user_message()
 │ ◀──── user_message / assistant_start / status:"streaming"
 │ ◀──── assistant_delta * N       │  (worker thread 经 poll_events 出队)
 │ ◀──── tool_start / tool_output  │
 │ ◀──── usage / context_usage     │
 │ ◀──── status:"ready"            │  回合结束
```

中断：

```
 │ ── {"op":"interrupt"} ────────▶ │  interrupt()
 │ ◀──── info:"request interrupted" / status:"interrupted"
```

## 6. 引擎侧改造点

改动集中在 `src/main.rs`，引擎核心 `core.rs` 基本不动。

1. **新增 `serve` 子命令**（`src/main.rs:56` 的参数分支里加一支）。
   与 `--headless` 并列，进入常驻循环。

2. **常驻循环**（约 150–250 行 Rust）：
   - 启动时输出 `core.startup_events()`。
   - 一个线程阻塞读 stdin，逐行解析命令，通过 channel 投递给主循环
     （或直接在主循环里用非阻塞读）。
   - 主循环每 ~30ms：
     - 处理待办命令 → 调用对应 `AgentCore` 方法 → 把返回的 `Vec<AgentEvent>` 写 stdout。
     - 调用 `poll_events()` → 写 stdout。
     - 可选：调用 `model_status_event()`，**仅在状态变化时**输出（去重，避免刷屏）。
   - stdout 每行 `flush`，保证前端低延迟。

3. **复用序列化**：把 `event_json()` 从 `src/main.rs` 提到可共享的位置
   （移入 `agent-core` 的一个 `serde` 模块，或独立小模块），让 serve 与 headless 共用，
   避免两份逻辑漂移。

4. **接线 `interrupt`**：core 已有 `interrupt()`，serve 模式把它作为 `op:"interrupt"` 暴露
   （GUI 的“停止”按钮）。顺带可考虑给 `TuiRuntime` 补上 `interrupt`，让 TUI 也能用 Ctrl-C 软中断
   而非退出——属可选项，不阻塞 GUI。

5. **错误约定**：解析失败的 stdin 行回一条 `{"type":"error","message":...}`，不崩溃、不退出。

## 7. Tauri 侧设计

- **壳（Rust）**：用 Tauri sidecar 打包并 spawn `jucode serve`；建立 stdin/stdout 管道；
  把 stdout 的每行 JSON 透传给前端（Tauri `emit`），把前端的命令写入 stdin。
  壳本身不理解协议语义，只做转发与进程生命周期管理（崩溃重启、退出清理）。
- **前端（TS/Web）**：维护一个由 `AgentEvent` 流驱动的状态机。建议的渲染映射：

  | 事件 | UI |
  |---|---|
  | `assistant_start` / `assistant_delta` | 流式消息气泡 |
  | `reasoning_delta` / `thinking_start` | 可折叠的思考区 |
  | `tool_start` / `tool_update` / `tool_output` | 工具执行卡片（按 `call_id` 聚合） |
  | `tree_view` / `resume_view` / `model_view` | 侧栏 / 选择器 |
  | `goal` / `context_usage` / `usage` | 状态栏（目标、上下文占用、token） |
  | `status` / `info` / `error` | 顶/底状态提示、toast |
  | `compaction_*` | 压缩进度条 |

- **输入**：输入框 Enter → `op:user_message`；以 `/` 开头 → `op:command`；停止按钮 → `op:interrupt`。

样式遵循全局 UI 规范（品牌色、克制的图标、不堆砌假数据）。

## 8. 分阶段计划

1. **M1 — 协议打通（引擎侧，可独立验证）**：实现 `jucode serve`，用命令行手动喂 NDJSON
   或写一个最小脚本验证一问一答、`status:ready` 收尾、`interrupt` 生效。此阶段不依赖任何 GUI。
2. **M2 — Tauri MVP**：壳 + 聊天流 + 工具卡片 + 模型切换（`/model` 与 `model_view`）。
3. **M3 — 完整视图**：会话树/恢复（`tree_view`/`resume_view`）、目标、上下文/压缩、技能。
4. **M4 — 打磨**：sidecar 崩溃重启、多窗口/多会话、快捷键、主题。

## 9. 取舍与风险

- **为什么选子进程 + JSON 而非内嵌 agent-core**：换来三端共享同一引擎、前端语言自由、协议可复用于
  未来 IDE 扩展；代价是进程间序列化开销（对话场景下可忽略）。
- **轮询模型**：沿用现有 30ms `poll_events` 节奏即可，无需引入 async。延迟与 TUI 一致。
- **背压**：长工具输出已在 TUI 侧有预览截断逻辑；协议层不截断，截断交给前端展示，避免丢信息。
- **协议演进**：首版不做 `id` 关联与版本协商；预留 `op`/`type` 的向后兼容（新增字段不破坏旧前端）。

## 10. 未决问题

- 命令是否需要 `id` 关联（用于精确把事件归属到某次提交）？MVP 先不做。
- `model_status` 的输出策略：每轮发还是仅变化时发？建议仅变化时发。
- Tauri sidecar 与现有 `npm` 分发如何共用同一套平台二进制产物？需在 release 流程里对齐
  （`scripts/npm/*`、`.github/workflows/release*.yml`）。

## 11. 实现状态（M1）

### 已实现（`src/main.rs` + `crates/agent-core/src/core.rs`）

- `jucode serve` 常驻双向 NDJSON 协议：stdin 读线程 + 30ms 主循环。
- 5 个 op：`user_message` / `command` / `steer` / `interrupt` / `shutdown`。
- 输出复用 `event_json()`，schema 与 `--headless` 完全一致。
- `model_status` 去重：仅在状态变化（ready/streaming/queued 切换）时发。
- 非法 JSON、未知 op 回 `error` 事件，不崩溃、不退出。
- **`/login` 已异步化**：`login_events` 立即返回并把 OAuth 等待放到线程，
  结果在 `poll_events` 里经 `apply_login_result` 落地。不再阻塞主循环；TUI 同样受益。
- **图片附件**：`user_message` op 接受 `images:[路径]`。新增 `EntryKind::UserImage`
  存路径（JSONL 不膨胀），投影时读盘转 `input_image` part，与 read 工具同款格式。
  跨轮持久（每次投影重读）、随会话存盘、`steer`/排队不丢附件；非法路径回 `info` 跳过。

### 已实测通过

- 普通对话回合（`connecting`→`assistant_delta`→`usage`→`status:ready`）。
- `interrupt`（运行中打断，主循环随后正常）。
- `steer`（运行中第二条消息入队并接管）。
- 本地命令：`/new` `/goal` `/context` `/tree` `/doctor` `/stats` `/compact` `/goal clear`。
- `/tree`→`/checkout <id>` 闭环（Python 双向 harness 驱动）。
- 图片附件端到端（模型读出图中文字、跨轮记得、非法路径跳过、写入 JSONL）。
- 工作区测试全绿（187 passed）。

### 仍未覆盖 / 未实测

- 真机 OAuth 往返（弹浏览器、回调落地）——仅验证了“不阻塞”，未跑完整登录。
- `/resume`（列表 + 恢复）、`/fork`、`/delete`——代码路径与 checkout 同族，低风险但未单测。
- `compaction_*`（需超长上下文触发）、`subagent_lifecycle`（需模型 spawn_agent）、
  `retrying`（需网络抖动）、`tool_update`（需特定工具）——事件已序列化，但未在真实流程触发验证。
- 图片/附件：粘贴的图片需前端先落盘再传路径（拖拽的文件天然有路径）；协议只接受本地文件路径，不接受内联 base64。
- 前端选择器状态机（tree/model/resume 导航、fork/delete 命名、effort 循环）在 tui crate，
  GUI 需自建；引擎侧只认最终的 `/checkout` `/model` 等文本命令。
- `command_list` 未列出 `/config /checkout /fork /delete /extensions /stats`，前端需自行知晓。
- stdout 管道被前端关闭时 serve 以 `BrokenPipe` 错误退出（行为正确，但未做静默处理）。
