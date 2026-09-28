# 常驻 Agent 与后台服务：迁移方案

Status: 决策已确认，待实现
Date: 2026-09-28

## 1. 目标

把"长期存在的 Agent + 常驻后台服务"这套能力加进 JuCode，使 JuCode 从"打开才工作的编码助手"变成"关掉界面也能继续推进工作的个人 Agent 环境"。

要达到的使用方式：

- 每个项目有一个长期存在的 Agent，带自己的职责说明、记忆和负责的目录。
- Agent 在 Desktop 关闭后继续工作：定时任务、夜间推进、问题到期后按默认处理、Agent 之间互发消息。
- Agent 有疑问时写一个持久的问题，然后继续做能做的部分，不停下来等人。人回来后集中答复。
- 做完的事以汇报的形式出现在首页，不需要逐个打开会话查看。
- 人不在电脑前时，用手机浏览器打开远程控制页面：看汇报、答复问题、批准待确认动作、给 Agent 发消息。IM 只做通知和简单指令。
- 同一套服务可以跑在本机，也可以放进 Docker。

这些能力已经在 AgentOS（TypeScript + Bun）里实现并有契约测试。本方案用 Rust 在 JuCode 里重新实现，AgentOS 只作为设计参考，停止开发，两边不做对接。

## 2. 约束

沿用本仓库 `AGENTS.md` 的规则：

- 性能与轻量优先。不引入 tokio，用阻塞 I/O 加线程；不引入重依赖和框架层。
- 一条明确、可测的路径，不做多套回退。
- 子 Agent 已内建（`subagents.rs`），不另起子系统。
- 会话以追加写入的 JSONL 为真相源（`docs/agent-session-design.md`），在文件存储被证明不够之前不引入数据库。

客户端协议允许破坏性修改：`serve` 协议、Desktop 的适配器与 `ChatState` 按本方案一起改，不保留旧格式的兼容层。

Desktop 的约束：`ChatState` 只认 jucode 事件格式，其他后端通过适配器翻译进来（`JuCode-Desktop/src/lib/backends/README.md`）。本方案保留这一点。

`JuCode-Desktop/docs/im-bridge.md` 的 v1 边界写的是"不安装常驻 daemon，出现明确需求后重新决策"。本方案就是这次重新决策：常驻服务成为远程控制、IM 入口和无人值守运行的前提。

## 3. 现状对照

| 能力 | JuCode 现状 | AgentOS 中的实现 | 在 JuCode 中的做法 |
| --- | --- | --- | --- |
| 会话与对话记录 | 会话树 JSONL、`/rewind`、`/fork`、压缩、会话锁 | Session + journal（SQLite） | 直接用 JuCode 的会话，不迁移 journal |
| 编码循环 | 完整：编辑工具、快照、钩子、目标模式、MCP、技能 | 自研 runtime，较弱 | 直接用 JuCode 的 `AgentCore` |
| 长期存在的 Agent | 无 | brief 目录（role、capabilities、policy、state、memory） | 新增，见 4.3 |
| 统一唤醒 | 无，只有交互式输入 | `wake()`：用户消息、Agent 消息、定时器、问题答复、子会话结束、IM | 新增，见 4.4 |
| 非阻塞提问 | 审批在当前回合内阻塞等待 | 持久问题，Run 不停，答复或到期后唤醒 | 新增，见 4.5 |
| 汇报 | 无 | `report` 工具，首页展示 | 新增 |
| 定时器 | 无 | 持久定时器，约每分钟检查 | 新增 |
| 权限 | `manual`、`auto-edit`、`auto`、`full-access`，`auto` 用安全模型判定 | `strict`、`auto`、`full` 三档，拦截规则 | 沿用 JuCode 的模式，与沙箱配合，见 4.6 |
| 沙箱 | 无（`cli-gap-checklist.md` 列为 non-goal） | bwrap、sandbox-exec | 参照 Codex 实现，见 4.6 |
| 工作区外目录 | `extra_read_roots` 只读 | 目录授予，ro/rw，挂进沙箱 | 扩展为 Agent 级的 ro/rw 目录，即沙箱的可写根 |
| 从目录创建 Agent | 无 | agent-father 调研，提交提案，一键批准；扫描文件夹批量接入；更新提案带 diff | 新增，见 4.7 |
| brief 修改记录 | 无 | 每次写入记录作者与前后全文 | 新增，文件存储 |
| 远程控制 | 无 | WebUI（仅本机） | 新增手机优先的网页，见 4.9 |
| IM | 仅有设计文档 | 飞书长连接 | 新增，放在后台服务里，只做通知与简单指令 |
| 用量与预算 | Desktop 有用量热力图 | 用量账本、每日预算 | 后台服务记账，Desktop 与网页展示 |

AgentOS 中不迁移的部分：自研 runtime 与工具集、WebUI（由 Desktop 与新网页取代）、ACP peer（Desktop 已支持多后端）、schema 迁移机制（本方案不用数据库）。

## 4. 设计

### 4.1 后台服务 `jucode daemon`

- 新增 crate `crates/daemon`，入口是子命令 `jucode daemon`。它在一个进程里托管多个 `AgentCore`，每个活跃会话一个实例，各自的工作线程与现在相同。
- `AgentCore::new()` 目前从进程的当前目录取 `cwd`（`core.rs` 唯一一处 `env::current_dir`）。新增一个显式传入 `cwd` 与会话 id 的构造函数，daemon 只用这个。会话锁（`SessionLock`）保证同一会话不会被 daemon 和独立运行的 TUI 同时写入。
- 生命周期：`jucode daemon install` 写入 launchd（macOS）或 systemd user unit（Linux）；Docker 镜像直接以 `jucode daemon` 为入口。
- 状态目录：`~/.jucode/` 下新增 `agents/<id>/`（brief 与记忆）与 `daemon/`（问题、汇报、定时器、消息、待确认动作、修改记录、配对设备等追加日志）。daemon 是这些文件唯一的写入方，原子性由进程内加锁保证，启动时从日志重建内存索引。

### 4.2 协议与传输

- 只有一种传输：HTTP 上的 WebSocket。daemon 默认监听 `127.0.0.1`，同一个端口提供 WebSocket 接口和远程控制网页的静态文件。Desktop 与手机网页用同一个 WebSocket 客户端。不另开 Unix socket，因为浏览器连不上。
- 实现用阻塞式的 `tiny_http` 与 `tungstenite`，每个连接一个线程，不引入 tokio。
- 帧格式沿用 `serve` 的 JSON，一次性修订为 v2：
  - 每个 op 与事件带 `session` 字段用于多路复用；需要应答的 op 带 `id`，应答事件回填同一个 `id`。
  - 新增 Agent 级的 op 与事件：Agent 列表、问题、汇报、待确认动作、提案、定时器。
  - 连接建立时 daemon 发送 `hello`，带协议版本号；版本不符直接断开并提示升级。
  - `jucode serve`（stdio）同步改成 v2 帧格式，只是固定一个会话。
  - 顺带修正现有漂移：Desktop 的 `set_approval_mode` 类型仍是 `read-only`、`plan`、`auto-edit`、`full-auto`，与 CLI 的 `manual`、`auto-edit`、`auto`、`full-access` 不一致。
- 鉴权：
  - 本机：daemon 首次启动生成 token，存在 `~/.jucode/daemon/token`（权限 0600），Desktop 读取后在连接时带上。
  - 手机：见 4.9 的配对流程。每台设备一个独立 token，可在 Desktop 上吊销。
  - 除 `127.0.0.1` 外，监听其他地址必须显式配置，且所有连接都要 token。

### 4.3 Agent

- 一个 Agent 是 `~/.jucode/agents/<id>/` 下的一组文件：`role.md`、`capabilities.md`、`policy.md`、`state.md`、`memory/`，外加 `agent.json`（启用状态、默认模型、负责的目录与仓库、权限模式覆盖）。
- Agent 的会话就是普通 JuCode 会话，会话元数据里多记 `agent_id` 与可选的父会话。`/rewind`、`/fork`、压缩全部照常可用。
- 每次运行前，`prompt.rs` 在现有的 AGENTS.md 注入之后，加入 brief 四个文件、memory 索引、可用目录清单和当前时间。
- 出厂带一个 agent-father：负责创建其他 Agent，维护"谁负责什么"的总览。

### 4.4 唤醒与路由

所有输入走同一个入口：用户消息（Desktop、网页、CLI、IM）、其他 Agent 的消息、定时器、问题答复或到期、子会话结束。流程：

1. 先写消息日志；带 `dedupe_key` 的消息只处理一次。
2. 选定目标会话：显式指定的会话 → 回复所指消息所在的会话 → 用户与 IM 消息接最近的会话 → 其他 Agent 的消息与未绑定会话的定时器开新会话。
3. 会话空闲就启动一次运行；正在运行就进入现有的消息队列（`pending_messages`）。
4. daemon 启动时恢复：未处理的消息重新投递，过期问题按到期处理。

全局并发上限默认 4，可配置。

### 4.5 问题、汇报与无人值守的审批

- 新工具 `question`：写一条持久问题（标题、正文、所做假设、默认处理、截止时间、重要程度），立即返回，模型继续工作。答复或到期后，以一条消息唤醒提问的会话。
- 新工具 `report`：写一条给人看的汇报，不唤醒任何会话。
- 审批：有客户端（Desktop 或网页）连着并在看这个会话时，行为与现在一致。无人在看时，需要审批的调用不再阻塞，改为写一条待确认动作（记录工具、参数与摘要 digest），工具返回"已提交等待确认"，本次运行继续或结束。人在 Desktop 或手机上批准后，由 daemon 按原参数执行，结果以消息送回会话。
- 同一动作的 digest 相同，批准或拒绝的结论在该会话内复用，避免重复提问。
- 新问题与待确认动作通过 IM 推送一条通知，附网页链接。

### 4.6 沙箱、权限与目录

沙箱参照 Codex 的做法：沙箱划定技术边界，审批模式决定越界时是否询问。

- 沙箱档位，按 Agent 设置：
  - `read-only`：可读，不可写，命令在只读沙箱里运行。
  - `workspace-write`（默认）：工作区、临时目录与 Agent 的 rw 目录可写，其余只读。
  - `full-access`：不进沙箱。
- 平台实现：
  - macOS：Seatbelt（`sandbox-exec`），按档位生成策略文件。
  - Linux 与 WSL2：`bubblewrap`，使用 `PATH` 上找到的 `bwrap`。缺少 `bwrap` 或无法创建用户命名空间时，daemon 启动即报错并给出安装说明，不静默降级。
  - Windows 原生：暂不支持沙箱，只能选 `full-access`，daemon 在 Windows 上建议用 WSL2 或 Docker 运行。
- 作用范围：`bash` 工具、钩子命令以及它们派生的所有子进程（git、包管理器、测试）都在沙箱内。文件读写工具在进程内按同一套路径规则校验。MCP 服务进程不进沙箱，每次调用按工具的只读标注和审批模式处理。
- 可写根内的保护路径，与 Codex 相同，递归只读：`.git`（目录或文件，包括 `gitdir:` 指向的真实目录）、`.jucode`、`.agents`。`git commit` 等写 `.git` 的命令因此需要越界，由审批处理。
- 网络：与 Codex 不同，沙箱内默认允许联网。这是个人开发工具，安装依赖、拉取文档是日常操作；Agent 可以把网络关掉。
- 越界：命令需要写沙箱外的路径、写保护路径或在沙箱外运行时，`bash` 工具带上 `escalate: true` 与理由重新发起，进入审批流程：
  - `manual`：总是问人。
  - `auto-edit`：沙箱内的编辑直接执行，越界问人。
  - `auto`（daemon 默认）：安全模型判定，放行的直接执行，其余问人。
  - `full-access`：直接执行。
  - 无人在看时，"问人"改为 4.5 的待确认动作。
- 前缀规则：可以为命令前缀配置 allow、ask、forbid（例如 `git commit` 允许越界，`git push` 必须问人），`forbid` 优先。
- 目录：`agent.json` 里列出 Agent 可用的工作区外目录，每个目录 ro 或 rw，rw 目录就是沙箱的额外可写根。符号链接按真实路径判定。凭据文件、`~/.ssh`、daemon 的状态目录在所有档位下都不可读。

### 4.7 从目录创建与更新 Agent

- `agent` 工具的 `propose` 动作：提交完整提案（brief 各文件、memory 文件、目录、仓库）。id 不存在是新建，已存在是更新。提案进入待确认列表，批准后由 daemon 一次写入，不经模型。
- "新建 Agent"对话框（Desktop 与网页都有）：填一个或多个目录（源码、部署脚本、日志），或者扫描一个父目录批量勾选。daemon 为 agent-father 开一个调研会话，只在这个会话里给它这些目录的只读权限；提案全部处理完、调研会话停止后收回。
- 更新提案显示每个文件的逐行 diff。

### 4.8 Desktop 改动

- 新后端 `daemon`：前端直接用 WebSocket 连本机 daemon，不再 spawn 子进程；适配器翻译 v2 帧，`ChatState` 与现有会话视图不变。
- 新视图：
  - 首页：待处理的问题与待确认动作、汇报、进行中与最近完成的会话。
  - 侧栏：Agent 列表，每个带一行职责与未读、待处理计数。
  - Agent 页：会话、档案（brief 与修改记录）、设置（模型、目录、沙箱档位、审批模式）。
  - 提案卡片、新建 Agent 对话框、已配对设备列表。
- 不经 daemon 的用法保持不变：单个会话仍可以直接 spawn `jucode serve`、codex、claude。

### 4.9 远程控制网页

- 代码放在 JuCode-Desktop 仓库，作为同一个 SvelteKit 项目的第二个构建目标 `web`。复用 `ChatState`、消息列表、工具卡片、审批卡片、Markdown 渲染、i18n 与主题；`protocol.ts` 里对 Tauri `invoke` 的调用改为经过一层传输接口，`web` 目标只实现 WebSocket 这一种。
- 页面按手机优先设计，只包含远程场景需要的部分：
  - 首页：问题、待确认动作、汇报。
  - Agent 列表与 Agent 页。
  - 会话：消息流、输入框、审批、中断。
  - 新建 Agent。
- 编辑器、终端、Git 面板、浏览器面板只在 Desktop 里有。
- daemon 从安装目录下的 `web/` 提供构建产物；发布包与 Docker 镜像都带上这个目录。
- 配对：Desktop 上点"添加设备"，daemon 生成一次性配对码（5 分钟有效），Desktop 显示含地址与配对码的二维码。手机扫码后用配对码换取长期 token，存在浏览器里。
- 手机访问的网络路径：
  - 推荐 Tailscale，用 `tailscale serve` 把本机端口以 HTTPS 暴露到自己的 tailnet，daemon 本身仍只监听 `127.0.0.1`。
  - 也可以用任意反向代理提供 HTTPS。
  - daemon 不内置 TLS，也不直接暴露到公网。

## 5. 分阶段计划

每个阶段单独可用，完成后先在真实项目上使用，再进入下一阶段。

| 阶段 | 内容 | 验证 |
| --- | --- | --- |
| 0 | `AgentCore` 显式 `cwd` 构造函数；进程级状态的梳理与拆分；审批的"无人在看"语义与待确认动作记录（先在 `serve` 下实现并测试） | 单元测试：同进程两个 `AgentCore` 分别在两个目录工作；无人值守时审批不阻塞回合 |
| 1 | `jucode daemon` 骨架：WebSocket 服务、协议 v2（`serve` 同步切换）、本机 token、多会话托管、会话在 Desktop 关闭后继续运行；Desktop `daemon` 后端 | 关闭 Desktop 后任务继续，重新打开能看到后续输出；daemon 重启后会话可恢复 |
| 2 | Agent 与唤醒：`agents/` 目录、agent.json、brief 注入、消息投递与路由、Agent 间消息、定时器、启动恢复；Desktop 侧栏 Agent 列表 | 定时器在 Desktop 关闭时触发并完成一次运行；消息只投递一次 |
| 3 | 持久问题、汇报、待确认动作；Desktop 首页 | 夜间场景：Agent 提问后继续工作，早上在首页答复，答复后会话被唤醒 |
| 4 | 远程控制网页：传输接口抽象、`web` 构建目标、手机页面、配对与设备管理 | 手机经 Tailscale 打开网页，答复问题、批准待确认动作、给 Agent 发消息 |
| 5 | 沙箱（Seatbelt、bubblewrap）、保护路径、越界与前缀规则；Agent 目录（ro/rw）、Agent 级沙箱档位与审批模式 | 源码 rw、部署脚本只读、日志只读的组合；写 `.git` 与沙箱外路径触发审批；无人值守时越界变为待确认动作 |
| 6 | agent-father、提案、从目录创建、扫描批量接入、更新提案与 diff、brief 修改记录 | 从三个目录创建一个项目 Agent，全程只点一次"创建" |
| 7 | 飞书 IM（通知与简单指令）、用量账本与每日预算、Docker 镜像、Agent 与会话的归档和删除 | 在 Docker 中运行 daemon，Desktop 与手机远程连接；IM 收到问题通知并跳转网页答复 |

### 阶段 0 结果

- `AgentCore::open(cwd)` 按显式目录打开引擎，`new()` 改为用进程当前目录调用它。工具、钩子、MCP、会话存储原本就显式接收 `cwd`，进程当前目录只在这一处读取。
- 无人值守：`set_attended(false)` 之后，需要审批的调用写成待确认动作（`actions.rs`），模型收到"已提交等待确认"，回合不阻塞；切换时正在等审批的调用也一并转成待确认动作。`decide_action` 批准后在后台线程按原参数执行，拒绝则不执行，结果都以用户消息送回会话。相同 digest 的调用复用已有动作或已有结论。`serve` 增加 `set_attended`、`decide_action` 两个 op 和 `action_deferred`、`action_decided`、`attended` 三个事件。
- 待确认动作目前只保存在引擎内存里，持久化由阶段 1 的 daemon 根据 `action_deferred` 事件写日志完成。
- 验证：`crates/agent-core/tests/hosted_engines.rs` 使用临时 HOME 和本地假模型服务，覆盖同进程两个引擎各自在自己的目录执行命令、无人值守时推迟后批准执行、拒绝后同一调用被直接拒绝、等待审批中切为无人值守后回合继续。

梳理出的进程级状态及阶段 1 的处理结果：

| 状态 | 位置 | 多引擎下的问题 | 处理 |
| --- | --- | --- | --- |
| 已读文件记录 | `tools.rs` | 一个会话读过的文件，另一个会话可以直接编辑 | 已改为每个引擎一份 `ToolState` |
| shell 会话表 | `tools.rs` | 任一引擎可以按编号向其他引擎的 shell 写入 | `ToolState` 记录引擎启动的 shell，`write_stdin` 只能访问自己的 |
| 配置 | `Config` | 每个引擎整文件保存，会覆盖其他会话的修改 | 模型、推理强度、MCP 的修改改为读取磁盘上的当前配置、只改对应字段再保存；登录与切换 provider 属于全局操作，仍整文件保存 |
| MCP 连接 | `McpManager` | 每个引擎各自启动 MCP 服务进程 | 未处理，真实使用中测量占用后再定 |
| 日志、环境变量 | `logging.rs` 等 | 只读或本来就全局，无问题 | 不处理 |

### 阶段 1 结果

- `crates/daemon`：`jucode daemon [--listen]` 用 WebSocket 托管多个会话，协议见 `docs/daemon-protocol.md`。
  - 鉴权：首次启动生成 `~/.jucode/daemon/token`（0600），连接必须带 token。
  - 会话：`session_create`、`session_open`、`session_close`；会话创建时立即落盘，关闭或 daemon 重启后都能按 id 重开，未决定的待确认动作随之恢复。
  - 有人在看：客户端 `watch` 一个会话即为有人在看，最后一个客户端 `unwatch` 或断开后转为无人在看，正在等审批的调用转成待确认动作。`watch` 同时向该客户端发送会话快照（状态事件与对话记录）。
  - 追加日志：`sessions.jsonl`、`actions.jsonl`。`decide_action` 在引擎执行前记录，daemon 中途停止也不会重复执行。
  - 托管的会话拒绝 `/new` 与 `/resume <id>`，由 daemon 的会话操作代替。
  - `jucode daemon install|uninstall` 写入 launchd 或 systemd 用户服务，并带上安装时的 PATH。
- 协议 v2：`serve` 首行发 `hello`（带协议版本），每个事件带 `session`。事件序列化与指令处理移到 `agent-core/src/protocol.rs`，`serve` 与 daemon 共用。
- Desktop：
  - 设置 → 后端 → 后台服务：打开后新的 JuCode 会话由 daemon 托管。`src/lib/daemon.ts` 用一个 WebSocket 连接 daemon，托管会话在 Desktop 里的表现与子进程相同，适配器、`ChatState` 和各视图不变。
  - 关闭 Desktop 只断开连接，会话继续运行；关闭标签页会结束对应的 daemon 会话；恢复、重启、切换 provider 都按 id 重开 daemon 会话。
  - 修正：jucode 会话的指令改为经过适配器编码，审批模式名称（`read-only`、`full-auto`）在发送前映射为引擎的 `manual`、`full-access`。适配器检查 `hello` 的协议版本。
- 验证：
  - `crates/daemon/tests/daemon.rs` 用真实 WebSocket 客户端和假模型覆盖错误 token、无人在看时推迟、客户端离开后会话继续、有人在看时提示且观察者离开后转为推迟、关闭后重开并恢复待确认动作、新会话关闭后重开、快照、拒绝切换会话的命令。
  - Desktop 的 `DaemonClient` 与 SessionStore 托管流程有单元测试，另用真实 `jucode daemon` 跑通创建、快照、关闭、按 id 重开。
  - 尚未在 Tauri 界面中手动走一遍完整流程。

### 阶段 2 结果

- 引擎：新增宿主扩展接口（`host.rs`）。宿主可以给引擎加工具（由宿主执行，不走审批），并在每轮系统提示词末尾附加文字。Agent 的概念只存在于 daemon 中，agent-core 不知道 Agent。
- daemon：
  - Agent 存在 `~/.jucode/agents/<id>/`：brief 四个文件、`memory/` 和 `agent.json`（名称、工作目录、启用状态、审批模式，默认 `auto`）。Agent 的会话在它的工作目录里运行，每轮都带上 brief、记忆索引和其他 Agent 的清单。
  - Agent 会话有三个工具：`message_agent`（给其他 Agent 发消息）、`timer`（设置、列出、取消定时器）、`brief`（改写自己的 brief 与记忆）。
  - 消息先写入 `messages.jsonl` 再投递。路由顺序：消息指定的会话 → 所回复消息所在的会话 → 用户消息接该 Agent 最近活跃的会话 → 新会话。带 `dedupe_key` 的消息只投递一次。每秒重试未投递的消息（包括重启前留下的），同时最多 4 个运行。
  - 定时器写入 `timers.jsonl`，默认回到设置它的会话。触发时以定时器 id 作为消息的去重键，daemon 停机期间到期的定时器在启动后只触发一次。
  - 新增 op：`agent_list`、`agent_create`、`message_send`、`timer_list`；`session_create` 可以指定 `agent`；新增广播：`agents`、`message_delivered`。
- Desktop：打开后台服务后，侧栏顶部显示 Agent 分区（名称、职责第一行、是否在工作）。点击 Agent 打开它最近的会话，没有会话时新建一个；"+"打开新建 Agent 对话框（名称、标识、工作目录、职责）。daemon 连不上时显示提示并每 5 秒重试。
- 顺带修正：
  - 临时文件名只用了进程号和时间戳，两个线程同时生成 diff 时会互相覆盖，已加计数器。
  - `agent_create` 原先用 `id` 表示 Agent 标识，与请求编号冲突，改为 `agent`。
- 验证：`crates/daemon/tests/daemon.rs` 新增用户消息开新会话、后续消息接同一会话、brief 进入系统提示词、定时器在无客户端连接时触发并完成一次运行、Agent 之间互发消息、去重、停机期间到期的定时器在启动后触发、第 5 个运行等待空位。Desktop 的 Agent 目录与打开 Agent 会话有单元测试，另用真实 daemon 跑通创建 Agent 和以 Agent 身份开会话。
- 未做：brief 修改记录（阶段 6）；Agent 页（会话列表、档案、设置），放到阶段 3 与首页一起做。

### 阶段 3 结果

- daemon：
  - `question` 工具记录标题、正文、期间假设、默认处理、截止时间和重要程度，写入后立即返回。用户答复或到期后，结果作为消息送回提问的会话；到期时消息写明按默认处理。同一问题只能答复一次，答复与到期不会同时生效。
  - `report` 工具记录汇报，不唤醒任何会话，可标记已读。
  - 新增 `questions.jsonl`、`reports.jsonl`；新增 op：`question_list`、`question_answer`、`report_list`、`report_read`、`agent_get`、`agent_update`。问题列表、待确认动作列表变化时广播，新汇报广播 `report_posted`，客户端连接时先收到问题与待确认动作列表。
  - `decide_action` 可以处理已关闭或 daemon 重启前的会话里的动作，daemon 会先重开会话。
  - 协议修正：`decide_action` 的动作编号字段由 `id` 改为 `action`，与请求编号分开。
- Desktop：
  - 工作台（侧栏 Agent 分区顶部，显示待处理数量）：待你处理（问题可直接答复，待确认动作可批准或拒绝，均可打开对应会话）、正在工作的 Agent、汇报（展开即标记已读）。
  - Agent 页（Agent 行上的档案按钮）：启用开关、审批模式、会话列表（打开或新建）、brief 四个文件和记忆文件列表。brief 只读，编辑与修改记录放到阶段 6。
  - 托管会话的聊天里显示待确认动作的记录与处理结果。
- 验证：`crates/daemon/tests/daemon.rs` 新增答复唤醒提问的会话、到期按默认处理、汇报不唤醒会话且可标记已读、会话关闭后仍可处理待确认动作、Agent 页读取与修改设置、停用的 Agent 不再接收消息。Desktop 的工作台状态与聊天提示有单元测试，另用真实 daemon 跑通 Agent 页和工作台的请求。
- 未在 Tauri 界面中手动走查；夜间场景（提问后继续工作、早上答复后唤醒）由 daemon 集成测试覆盖，界面部分待真实使用确认。

### 阶段 4 结果

- daemon：
  - 同一端口区分 WebSocket 与普通 HTTP。普通请求提供远程网页的构建产物（`--web <dir>`，未指定时用二进制旁边的 `web/`），`/` 跳转到 `/remote`，没有扩展名的路径返回 `index.html`，路径不能越出目录。
  - 配对：本机客户端发 `pair_start` 拿到 8 位一次性配对码（5 分钟有效）。手机把配对码 `POST /api/pair`，换取设备 token。`devices.jsonl` 只存 token 的哈希。设备 token 可以使用除设备管理以外的所有操作；`pair_start`、`device_list`、`device_revoke` 只允许本机 token。吊销后，该设备已打开的连接立即断开。
- Desktop：
  - daemon 地址改为可替换的来源（`setDaemonEndpoint`），外链打开改为 `openExternal`（Tauri 内用系统浏览器，普通浏览器开新标签页）。这是网页目标需要的全部传输接口：聊天渲染这条链路原本就不依赖 Tauri。
  - 新增 `/remote` 路由，与桌面端同一次构建产出。页面按手机设计：配对页（支持扫码链接里的配对码）、工作台（与桌面端共用 `DeskContent`）、Agent 列表、会话页（消息流、审批卡片、输入框、停止）。查看会话即为有人在看，离开只取消查看，不结束会话。
  - 设置 → 后端 → 后台服务：填写手机访问地址（推荐 `tailscale serve` 的 HTTPS 地址），"添加设备"显示配对码和二维码（新增依赖 `uqr`，无传递依赖），已配对设备列表可吊销。
- 验证：
  - `crates/daemon/tests/daemon.rs` 新增静态文件、跳转、路径越界、配对码错误与一次性、设备不能管理设备、吊销后连接断开且 token 失效。
  - Desktop 新增配对与连接地址的单元测试。
  - 用无头浏览器（手机尺寸）加本地假模型，对真实 daemon 与构建产物跑通：扫码链接配对 → Agent 列表 → 打开会话 → 发消息 → 收到回复。过程中发现并修正：会话打开完成前就能发送消息，消息会被随后到达的快照清掉且没有提示；现在打开完成前输入框不可发送。
- 未做：发布包和 Docker 镜像带上 `web/` 目录，放到阶段 7。

AgentOS 现有数据不做迁移，只有少量会话。需要保留的 brief 可以直接复制到 `~/.jucode/agents/`。

## 6. 风险

- `AgentCore` 目前按"一个进程一个会话"写成，部分状态可能是进程级的：配置、MCP 连接、信任记录。阶段 0 要逐一确认哪些可以在实例之间共享、哪些必须按实例隔离。
- 多个 `AgentCore` 各自连接 MCP 服务，同一台机器上的进程数会增加，需要在阶段 1 测量资源占用。
- Desktop 的适配器与 `ChatState` 按单会话单进程设计，多路复用后要确认会话重启、崩溃恢复等路径仍然成立。
- 协议 v2 是一次性的破坏性修改，CLI 与 Desktop 必须同时发布；版本不符时靠 `hello` 里的版本号直接报错。
- Desktop 前端有多处直接调用 Tauri：`protocol.ts` 及其他 8 个文件使用 `@tauri-apps/api`，另有若干文件使用 Tauri 插件。阶段 4 的传输接口只覆盖网页需要的调用，其余留在 Desktop 专用代码里。
- `.git` 设为保护路径后，`git commit` 在 `auto` 下每次都要经安全模型判定，可能拖慢频繁提交的工作流。阶段 5 用前缀规则放行常见的 git 写操作，并在真实使用中观察。
- macOS 的 `sandbox-exec` 已被 Apple 标记为弃用但仍可用，Codex 也依赖它；若未来移除，需要改用其他机制。

## 7. 已确认的决策

| # | 问题 | 决定 |
| --- | --- | --- |
| 1 | 持久化方式 | 追加日志文件，daemon 是唯一写入方 |
| 2 | 客户端协议 | 沿用 `serve` 的 JSON 帧，修订为 v2，允许破坏性修改；传输统一为 WebSocket |
| 3 | 沙箱 | 参照 Codex：三档沙箱、Seatbelt 与 bubblewrap、保护路径、越界走审批；网络默认开启 |
| 4 | 无人在看时审批的处理 | 写待确认动作并继续 |
| 5 | 远程界面 | 手机优先的远程控制网页，由 daemon 提供；IM 只做通知与简单指令 |
| 6 | daemon 代码位置 | CLI workspace 新 crate `crates/daemon` |
