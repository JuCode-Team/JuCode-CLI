# JuCode CLI 编码代理审计报告

## Owner decisions (2026-08-28)

以下决定覆盖本文后续历史审计中的建议口径，后文现状描述仅作为基线证据。

- **Must do:** config 控制编辑工具且默认仅启用 `hashline_edit`；在轻量 vendor package 中手写 provider 协议（含 Chat Completions，无 `genai`/`tokio`）；补齐 MCP prompts/resources/OAuth 与完整 Skills；打造优秀且写隔离的 subagents；完善 TUI（`@`、`!`、自定义 slash commands、git 状态栏、图像粘贴）、headless、reliability engineering、崩溃安全 session journal，以及可复现的 Codex 对比 eval。
- **协议与工程:** 新增 CLI ACP server；embed protocol 仍在评估，不得声称它会替换 `serve`，双轨运行较可能；同时补齐 `serve` 文档/version、README/CI、config/auth 原子写，并将 `browser_open` 移出 core。
- **权限:** workspace 路径边界只是 BASIC permission，不是 sandbox；headless mutation 默认要求显式 `--full-auto`；approval 只保留 `ReadOnly`/`AutoEdit`/`FullAuto` 三档。
- **明确非目标:** 任何 sandbox（包括 OS sandbox、Landlock、Seatbelt、`bwrap`、`sandbox_command`）、持久化命令 pattern 权限规则，以及 LSP/DAP。multi-root 推迟。

> 审计基线:`main@0758fef`(Release v0.1.11)。对照对象:Codex CLI、PI、Oh My PI(omp)、OpenCode。
> 结论先行:**JuCode CLI 是一个高质量、可用的编码代理 harness,但不是 Codex-complete。差距在安全(sandbox)、协议覆盖、可靠性工程,不在 agent 核心。**

## 范围声明

本报告**明确不涉及**以下方向,它们不在当前产品范围内:

- 内置长期记忆(memory)
- computer-use(桌面自动化)
- browser-use(浏览器自动化)

⚠️ 与上述排除项冲突的现状:`crates/agent-core/src/tools.rs` 中存在 `browser_open` 工具(`browser_open_definition()`,约 268 行起),仅在 `JUCODE_DESKTOP` 环境变量存在时可用,CLI 环境下返回错误 `"browser_open is only available when running inside JuCode Desktop"`。既然 CLI 产品线排除 browser-use,**建议将 `browser_open` 从 core 工具表移出,改为 Desktop 侧通过 extension 机制注入**(`crates/agent-core/src/extensions.rs` 已有 `ExtensionRegistry`/`ExtensionTool` 基础设施)。详见 gap checklist G23。

## 一、已验证的当前能力(源码核对)

### 1. Agent 循环(agent-core/src/core.rs)

- 流式 LLM 调用 + 工具调用循环,事件驱动(`crates/agent-core/src/event.rs` 的 `AgentEvent`)。
- 上下文压缩(compaction,配置项 `compaction_threshold_percent`)、token 用量统计(`tokens.rs`)、goal/进度跟踪(`session.rs` 的 `ThreadGoal`)。
- 生命周期 hooks:`crates/agent-core/src/hooks.rs` 支持 `session_start`、`user_prompt_submit`、`pre_tool_use`、`post_tool_use`、`stop` 五个挂点,从 `~/.jucode/hooks.json` 和 `<cwd>/.jucode/hooks.json` 加载。
- README 引用的自测数据:5/5 任务完成,输入+输出 token 比 Codex 基线少 32.1%(注意:这是自采样快照,非公开 benchmark)。

### 2. 工具集(crates/agent-core/src/tools.rs)

内置工具(`definitions()` 中定义):`read`、`str_replace`、`hashline_edit`、`write`、`apply_patch`、`bash`、`exec_command`、`write_stdin`、`ls`、`ripgrep`、`outline`、`checkpoint`,外加 `web_fetch`(`crates/agent-core/src/web_fetch.rs`)和桌面限定的 `browser_open`。支持图像附件(`image_attachment_part`)。`checkpoint` 提供 `.jucode/checkpoints` 本地快照的创建/列出/恢复。

### 3. 会话(crates/agent-core/src/session.rs)

- 每会话 JSONL journal(`{session_id}.jsonl`),条目树结构支持分支(fork)、`/rewind`、`/checkout`。
- resume summary、goal 状态、token/耗时记账均持久化。
- TUI 侧有 `/resume`、`/fork`、`/tree`、`/delete` 等会话命令(`crates/agent-core/src/commands.rs`)。

### 4. 权限/审批(crates/agent-core/src/config.rs:63)

三档 `ApprovalMode`:`ReadOnly`(默认,一切 mutating 工具需审批)/ `AutoEdit`(文件编辑放行,shell 仍审批)/ `FullAuto`。支持 hunk 级别的编辑审批(`hunks.rs`)、subagent 审批门控、`/trust`(`trust.rs`)。**无 OS 级 sandbox,无持久化的命令模式规则**。`--headless` 强制 full-auto(`src/main.rs:153` 显式拒绝 `--approval-mode`:"headless always runs full-auto")。

### 5. Provider(crates/agent-core/src/config.rs、llm.rs)

- 两种线协议:OpenAI **Responses API**(`/responses`)和 **Anthropic Messages API**(经 `protocol` 配置切换),均为流式 SSE,基于阻塞 `ureq`(无 tokio,符合项目宪法)。
- 内置 provider 模板(`default_providers()`,config.rs:916)只有两个:`jucode`(Responses)和 `deepseek`(Anthropic 端点)。
- **README bug**:README.md 第 49 行声称 "`openai` and `deepseek` are built in",但源码模板中**没有 `openai` 条目**,只有 `jucode` + `deepseek`(`openai` 只是 fallback 默认字符串,config.rs:299)。需要修正文档或补齐模板。
- **不支持 Chat Completions 协议**,而这是行业事实标准(Ollama/vLLM/多数网关的最大公约数)。

### 6. MCP / Skills(crates/agent-core/src/mcp/、skills.rs)

- MCP 客户端:stdio + streamable HTTP 传输(`mcp/transport.rs`),手写 JSON-RPC(符合宪法)。实现了 `initialize`、`tools/list`(带 nextCursor 分页 + 防死循环上限)、`tools/call`(`mcp/client.rs`)。
- **缺失**:MCP prompts、resources(仅在 tools/call 结果里展平 embedded resource)、HTTP OAuth 授权。
- Skills:marketplace 拉取/安装(含 sha256 校验,`skills.rs`),`/skills` 命令。

### 7. Subagents(crates/agent-core/src/subagents.rs)

内置 `SubagentManager`(注册表 + 状态机 + 生命周期事件 + usage 汇总),符合"不另建子系统"的宪法。**无写隔离**:子代理与主代理共享 cwd,无 worktree/只读约束。

### 8. TUI(crates/tui/)

markdown 渲染(`markdown.rs`)、工具输出预览(`tool_preview.rs`)、picker(`picker.rs`)、丰富的 slash 命令(`/model`、`/approvals`、`/context`、`/goal`、`/doctor`、`/mcp`、`/stats` 等约 25 个)。**缺失**:`@` 文件提及补全(`crates/tui/src/input.rs` 无相关实现)、`!` 直通 shell、git 状态栏。

### 9. Headless / serve(src/main.rs)

- `--headless "prompt"`:单任务 JSONL 事件输出,支持 stdin 管道。
- `jucode serve`:长驻 NDJSON stdin/stdout 协议,支持 `approve`(含 hunk 粒度)、`mcp_list`、`mcp_set`、shutdown 等 op。
- **缺失**:协议参考文档与版本字段(embedder 无法做兼容性协商)。

### 10. 可靠性 / 工程

- **335 个单元测试**(`#[test]` 计数,全绿)。
- **无 PR CI**:`.github/workflows/` 只有 `release.yml` 和 `release-macos.yml`,fmt/test/clippy 无 PR 门禁。
- `cargo clippy -- -D warnings` 当前有 **2 个 error**,与 AGENTS.md 自身要求相悖。
- 无 in-repo eval harness(README 的对比数据不可复现)。
- 配置/认证写入非原子:`config.rs` 的 `save()` 直接写文件;`auth` 的 `load_or_create()`(config.rs:451)遇到损坏 JSON 时 `unwrap_or_else(|_| json!({}))` **静默重置**,可能丢用户密钥。

## 二、成熟度打分(对照 Codex / PI / omp / OpenCode)

| 维度 | 得分 | 说明 |
|---|---|---|
| Agent 循环 | **4–4.5 / 5** | 两位评审分别给 4.5(Fable)与 4(Sol)。压缩、goal、hooks、事件流齐备;与 Codex 差距主要在恢复策略与长任务鲁棒性 |
| 工具 | **4–5 / 5** | 编辑工具矩阵(str_replace/hashline/patch)+ exec/stdin + checkpoint,覆盖面优于 PI(仅 4 个核心工具) |
| 会话 | **4–4.5 / 5** | JSONL 树、分支、rewind、resume summary;缺崩溃安全写入 |
| 权限/沙箱 | **2 / 5** | 只有 3 档审批,无 OS sandbox、无持久规则、无路径边界;headless 隐式 full-auto。这是与 Codex 的最大差距 |
| Provider | **2–3 / 5** | 双协议(Responses+Anthropic)但无 Chat Completions,内置模板仅 2 个,README 与实现不符 |
| MCP/Skills | **3–3.5 / 5** | tools 完整、传输合规;缺 prompts/resources/OAuth |
| Subagents | **3 / 5** | 内置且可门控,但无写隔离 |
| TUI | **4 / 5** | 快、命令全;缺 @ 文件提及、! 直通 shell 等惯例交互 |
| Headless/serve | **3–3.5 / 5** | 协议本体能用;缺文档与版本协商 |
| 可靠性/评测 | **2 / 5** | 335 测试是好基础,但无 PR CI、clippy 不过、无 eval harness |

## 三、行业对照(含证据强度标注)

以下同行事实用于定位,**标注了核实程度**,引用时注意口径:

- **Codex CLI**:`codex app-server` 作为编辑器/IDE 集成的长驻协议入口——**已核实**。其 sandbox(macOS Seatbelt / Linux Landlock)+ approval policy 组合是权限维度的对标上限。
- **PI**:核心仅 4 个工具、core 不内置 MCP(靠扩展)——**已核实**。说明"小工具集"路线成立,但 JuCode 已选择更全的工具矩阵,不必回退。
- **Oh My PI(omp)**:ACP 支持的调用方式为 `omp acp` 子命令(**不是** `--acp` 标志)——引用时注意。
- **OpenCode**:内置 build/plan 两个主 agent 模式 + general/explore 子代理(v1 另有 scout)——大体核实,版本间有出入,引用需带版本号。
- **ACP(Agent Client Protocol)**:由 Zed 发起,JetBrains 跟进——**已核实**。是 D3 决策(embed 协议)的行业背景。
- **rust-genai**:声称 25+ provider——**已核实**,但**极可能依赖 tokio**,与本项目"无 tokio、阻塞 I/O + 线程"的宪法(AGENTS.md)直接冲突。这是 D2 决策否决 genai 方案的主要依据。

## 四、结论

JuCode CLI 的 agent 核心(循环、工具、会话、TUI)已达到"日常可用的高质量 harness"水平,在 token 效率上有差异化优势。**它不是 Codex-complete**,缺口集中且明确:

1. **安全**:无 sandbox、无路径策略、headless 隐式 full-auto(权限维度 2/5,唯一的红色项之一)。
2. **协议覆盖**:无 Chat Completions、MCP 缺 prompts/resources/OAuth、serve 协议无文档无版本。
3. **可靠性工程**:无 PR CI、clippy 不过、无 eval、配置写入不健壮。

这三类缺口都不需要重写 agent 核心,且大多与"轻量、无 tokio"的项目宪法兼容。具体的逐项清单与技术方向决策见 `docs/cli-gap-checklist.md`。
