# JuCode CLI 差距清单(可编辑)

> 配套文档:`docs/coding-agent-audit.md`(审计基线 `main@0758fef` / v0.1.11)。
> 用法:勾选 `[x]` 表示"确定要做";`[ ]` 表示待你拍板或可选。Status 是**现状**,Level 是专家建议的优先级。
> 已预勾选的均为专家一致认为无争议的 must-have。

## 清单

| 做? | ID | 功能 | Status | Level | Notes |
|---|---|---|---|---|---|
| [x] | F2 | Chat Completions 协议支持 | missing | must | `crates/agent-core/src/llm.rs` 现仅有 Responses + Anthropic Messages;Chat Completions 是 Ollama/vLLM/网关的事实标准。技术路线见 D2 |
| [x] | F8 | serve/headless 协议参考文档 + version 字段 | partial | must | 协议本体在 `src/main.rs`(`run_serve`/`handle_serve_line`)已实现,但无文档、无版本协商,embedder 无法做兼容性判断 |
| [x] | F9 | TUI `@` 文件提及补全 | missing | must | `crates/tui/src/input.rs` 无实现;Codex/OpenCode 均有,是最常用的上下文注入交互 |
| [x] | F13 | README/文档事实性修正 | partial | must | README.md:49 声称 `openai` 内置;`config.rs:916` 的 `default_providers()` 只有 `jucode`+`deepseek`。改文档或补模板,二选一但必须一致 |
| [x] | F17 | PR CI(fmt/test/clippy) | missing | must | `.github/workflows/` 只有 release 流水线;`cargo clippy -- -D warnings` 现有 2 个 error,先修再上门禁。与 G24 同一项 |
| [x] | G02 | 工作区文件边界 / 路径策略 | missing | must | 文件工具(`tools.rs` 的 read/write/str_replace 等)无 cwd 边界检查,可读写任意绝对路径。Sol 定为安全 must |
| [x] | G04 | headless 更安全的默认(取消隐式 full-auto) | missing | must | `src/main.rs:153`:headless 强制 full-auto 且拒绝 `--approval-mode`。方向见 D6 |
| [x] | G09 | config/auth 原子写 + 坏 JSON 显式报错 | missing | must | `config.rs:451` 坏 JSON 时 `unwrap_or_else(|_| json!({}))` 静默重置(可能丢密钥);`save()` 非原子(无 tmp+rename)。见 D8 |
| [x] | G23 | 将 `browser_open` 移出 core | done(错位) | must | `tools.rs:268` 桌面限定工具却在 core 工具表;与"排除 browser-use"冲突。移到 Desktop extension(`extensions.rs` 机制已具备)后从 core 删除 |
| [x] | G24 | CI(同 F17) | missing | must | 与 F17 合并执行 |
| [ ] | F1 | OS 级 sandbox | missing | should | 见 D1,与 G01 同一项。Landlock(Linux)/Seatbelt(macOS),Codex 对标项 |
| [ ] | F3 | 更多 provider 内置模板 | partial | optional | 现仅 2 个模板;F2 落地后大多数 provider 可经 Chat Completions 直连,模板需求下降 |
| [ ] | F4 | 持久化权限规则 | missing | should | 现 `/approve` 的 always 仅会话内生效;命令模式级持久规则见 D4 |
| [ ] | F5 | MCP prompts | missing | optional | `mcp/client.rs` 现仅 tools/list + tools/call |
| [ ] | F6 | MCP resources | missing | optional | 现仅在 tools/call 结果中展平 embedded resource |
| [ ] | F7 | MCP HTTP OAuth | missing | should | streamable HTTP 传输已有(`mcp/transport.rs`),缺授权流程;远程 MCP 服务日益要求 OAuth |
| [ ] | F10 | `!` 用户 shell 直通 | missing | should | TUI 惯例交互,实现小 |
| [ ] | F11 | 自定义 prompt 命令 | missing | optional | 用户自定义 slash 命令(`commands.rs` 现为静态表) |
| [ ] | F12 | git 感知状态栏 | missing | optional | 分支/脏状态展示,纯 TUI 增强 |
| [ ] | F14 | ACP adapter | missing | later | 见 D3;仅当需要 Zed/JetBrains 编辑器渠道时做 |
| [ ] | F15 | CLI 内置 LSP | missing | later | 见 D5;专家建议不做内置,走 hook/MCP |
| [ ] | F16 | multi-root 工作区 | missing | later | 会话与路径策略(G02)都需先支持单 root 边界 |
| [ ] | F18 | TUI 图像粘贴 UX | partial | optional | 图像附件后端已有(`tools.rs` 的 `image_attachment_part`),缺终端粘贴交互 |
| [ ] | G01 | OS 级 sandbox(同 F1) | missing | should | 与 F1 合并决策,见 D1 |
| [ ] | G05 | 进程组管理 / PTY | partial | should | `exec_command`/`write_stdin` 已有;缺进程组级清理,孤儿进程风险 |
| [ ] | G06 | 会话级 kill(清理所有子进程) | missing | should | 与 G05 同域,中断/退出时统一收割 |
| [ ] | G07 | 崩溃安全的会话 journal | partial | should | `session.rs` JSONL 追加写,无 fsync/截断恢复;进程被杀可能留半行。见 D8 |
| [ ] | G20 | subagent 写隔离 | missing | should | `subagents.rs` 子代理共享 cwd 无约束,见 D7 |
| [ ] | G25 | in-repo eval harness | missing | should | README 的 token 对比数据不可复现;最小任务集 + 脚本即可起步 |
| [ ] | G26 | LLM 请求重试/退避审计 | partial | optional | `retry_attempts` 配置已有,缺对流中断(SSE 半途断开)的恢复策略(Sol 可靠性项) |
| [ ] | G27 | 工具输出截断/大小上限统一策略 | partial | optional | 各工具截断逻辑分散在 `tools.rs`,建议统一预算(Sol 可靠性项) |

## 需要你拍板的技术方向

以下 8 个决策相互关联(D1↔D4↔D6,D2↔F2/F3,D7↔G20,D8↔G07/G09),每项给出选项、专家推荐与代价。

### D1 沙箱路线(对应 F1/G01)

- **A:OS 原生 sandbox**——Linux Landlock + macOS Seatbelt(Codex 同款路线)。最强隔离;代价是平台分裂(Windows 无对应物)、实现量最大、需处理 sandbox 内 PATH/网络策略。
- **B:仅权限规则**——纯应用层(路径边界 + 命令模式规则),跨平台一致、零依赖;但对恶意/失控命令无硬防护,`bash -c "curl | sh"` 类逃逸拦不住。
- **C:`sandbox_command` 前缀**——配置一个包裹命令(如 `bwrap ...`/`sandbox-exec -p ...`),JuCode 只负责拼接。实现量最小(改 `tools.rs` 的 shell 执行路径即可),把选择权交给用户;代价是默认不安全、体验依赖用户环境。
- **推荐:C 立即做 + B 永远做(它同时是 G02 的载体)+ A 从 Linux(Landlock)先做。三者不互斥,是分层关系。**

### D2 Provider 扩展路线(对应 F2/F3)

- **A:手写 Chat Completions 客户端**——在 `llm.rs` 现有 `ureq` 阻塞 SSE 框架上加第三种协议分支。与"无 tokio、手写协议"宪法完全一致;代价是自己维护协议细节(工具调用格式、SSE 事件差异)。
- **B:引入 genai crate**——25+ provider 一步到位(已核实);但 genai 极可能拖入 tokio 与整棵异步依赖树,**直接违反 AGENTS.md 宪法**,且丢失对请求体/重试的精细控制。
- **C:gateway-only**——只走 jucode 网关,由网关做协议适配。CLI 零改动;代价是绑死自家服务,开源可信度与自带 key 用户全部流失。
- **推荐:A。** Chat Completions 是三种协议里最简单的,`llm.rs` 已有的 Anthropic 分支证明多协议结构成立。

### D3 嵌入协议(对应 F8/F14)

- **A:保持 NDJSON serve**——现有 `jucode serve` 补文档 + version 字段(即 F8)。实现量最小,协议自主可控。
- **B:另加 `jucode acp` adapter**——在 serve 之外加一个 ACP 翻译层,获得 Zed/JetBrains 渠道(ACP 由 Zed 发起、JetBrains 跟进,已核实);代价是维护两套协议映射。
- **C:用 ACP 替换 serve**——协议归一;但 ACP 尚在演进,且会破坏现有 Desktop(jucode-desktop 走 serve)集成。
- **推荐:A 现在做,B 在确认需要编辑器渠道后做。永远不做 C。**

### D4 权限粒度(对应 F4)

- **A:保持 3 档**——`ReadOnly`/`AutoEdit`/`FullAuto`(config.rs:63)不动。零成本;但用户每个会话重复审批同类命令,体验落后于 Codex/OpenCode。
- **B:命令模式持久规则**——如 `allow: ["cargo test", "git status"]` 持久化到配置,`/approve always` 升级为跨会话。实现集中在 `trust.rs`;代价是规则匹配语义要设计好(前缀?glob?)。
- **C:Codex 式 policy + sandbox 联动**——审批策略与沙箱强度联动(如 sandbox 内自动放行)。最完整;但强依赖 D1-A 落地。
- **推荐:先 B,D1 的 OS sandbox 落地后再演进到 C。**

### D5 LSP/DAP

- **A:不做**——保持现状(`outline` 工具已覆盖部分符号需求)。
- **B:内置 LSP 客户端**——诊断/跳转喂给模型;但 LSP 客户端是重型子系统(进程管理、能力协商、增量同步),与轻量宪法冲突。
- **C:hook 配方 + MCP**——用 `post_tool_use` hook 跑 `cargo check`/tsc 等把诊断注入,复杂需求交给社区 LSP MCP server。几乎零核心代码。
- **推荐:C。DAP 不做(任何选项下)。**

### D6 headless 默认权限(对应 G04)

- **A:默认拒绝,需显式 `--full-auto`**——mutating 工具在 headless 下直接失败,除非用户显式授权。安全默认,CI 脚本加一个 flag 即可迁移。
- **B:policy 文件**——headless 读取项目内策略文件决定放行范围。灵活;但在 D4-B 落地前是重复建设。
- **C:保持隐式 full-auto**——现状(main.rs:153)。零成本,但任何人 `cat task.md | jucode --headless` 就是无审批任意执行,是审计中最尖锐的安全项。
- **推荐:A。** B 可在 D4-B 之后作为增强。

### D7 subagent 写权限(对应 G20)

- **A:默认只读**——子代理默认 `ReadOnly`,需主代理显式升级。改动集中在 `subagents.rs` spawn 路径;探索类子代理(占多数)零感知。
- **B:共享 cwd + 写锁**——允许写但串行化;实现锁语义复杂,且防不了逻辑冲突(两个子代理改同一文件的不同轮次)。
- **C:每 subagent 一个 git worktree**——物理隔离最干净(OpenCode 方向);但引入 git 依赖假设、合并回主树的策略复杂。
- **推荐:先做 A。** C 留作后续可选增强,B 不建议。

### D8 会话存储(对应 G07/G09)

- **A:加固 JSONL**——原子写(tmp+rename)、追加后按需 fsync、加载时容忍尾部半行并截断恢复。改动集中在 `session.rs`/`config.rs`,零新依赖。
- **B:迁移 SQLite**——事务性最好;但引入 rusqlite/C 依赖,违背轻量宪法,且现有 journal 树结构(分支/rewind)迁移成本高。
- **推荐:A。** JSONL 的问题全部可以在应用层修好,SQLite 解决的是这里不存在的并发写问题。
