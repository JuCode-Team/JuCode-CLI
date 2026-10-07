# `jucode daemon` Protocol

`jucode daemon` hosts many JuCode sessions in one long-running process.
Sessions keep running when every client disconnects. Clients (Desktop, the
remote web page) connect over a WebSocket and speak the `jucode serve`
protocol version 2 (`docs/serve-protocol.md`), with a `session` field that
says which hosted session an op or event belongs to.

Background and roadmap: `docs/agent-daemon-plan.md`.

## Running

```sh
jucode daemon                      # ws://127.0.0.1:7788
jucode daemon --listen 127.0.0.1:9000
jucode daemon --web path/to/JuCode-Desktop/build   # serve the remote page
jucode daemon --relay wss://relay.example/relay/v1  # another relay
jucode daemon --no-relay           # never connect to a relay
```

Without `--web`, the daemon serves a `web/` directory next to its binary
when one exists (release packages ship it there).

To start it at login and restart it if it exits:

```sh
jucode daemon install              # launchd on macOS, systemd user unit on Linux
jucode daemon uninstall
```

`install` writes the PATH of the shell that ran it into the service, so the
agent finds the same commands as in that shell; rerun it after changing PATH.
The service logs to `~/.jucode/daemon/daemon.log`.

State lives in `~/.jucode/daemon/`:

| File | Contents |
| --- | --- |
| `token` | Client token, created on first start, mode 0600. |
| `sessions.jsonl` | Append log of sessions opened and closed. |
| `actions.jsonl` | Append log of deferred actions and their decisions. |
| `messages.jsonl` | Append log of messages to agents and their delivery. |
| `timers.jsonl` | Append log of agent timers set, fired and cancelled. |
| `questions.jsonl` | Append log of questions asked and answered. |
| `reports.jsonl` | Append log of reports posted and read. |
| `devices.jsonl` | Paired devices (a hash of each token, never the token) and revocations. |
| `settings.json` | Daemon settings: `relay` (whether the relay connection is on). |
| `workspaces.json` | Workspaces and their projects, with a save counter `rev`. |
| `relay-identity.json` | Relay keys (Ed25519 identity, X25519 Noise static key), mode 0600. |

## Plain HTTP

The same port answers plain HTTP requests (anything that is not a
WebSocket upgrade):

- `GET /` redirects to `/remote`.
- `GET <path>` serves the remote page's files from the `--web` directory.
  A path without a file extension gets `index.html` (the page is a
  single-page app). Paths cannot leave the directory.
- `POST /api/pair` with `{"code": "...", "name": "..."}` trades a pairing
  code for a device: `200 {"device", "name", "token"}`, or `403` when the
  code is wrong, expired or already used.

The files hold nothing private; every daemon op still needs a token over
the WebSocket.

## Remote devices

A phone pairs once and then connects with its own token:

1. A local client sends `pair_start` and shows the returned 8-character
   `code` (valid 5 minutes, single use), for example as a QR code of
   `<address>/remote?pair=<code>`.
2. The phone's page posts the code to `/api/pair` and keeps the token.
3. The phone connects to the WebSocket with that token.

Device tokens reach every op except `pair_start`, `pair_link`,
`device_list`, `device_revoke`, `relay_status` and `relay_set`, which only
local clients (holding the daemon token) may send. `device_revoke` drops the device's open connections at once.

To reach the daemon from a phone, keep it on `127.0.0.1` and expose the
port over HTTPS with `tailscale serve` or a reverse proxy; the daemon has no
TLS of its own. Or use the relay (below).

## Relay

With the relay on, the daemon keeps one outbound WebSocket to the JuCode
relay (`--relay`, default `wss://app.jucode.net/relay/v1`) and phones reach
it from anywhere through end-to-end encrypted streams
(`docs/relay-protocol.md`). It is off until a local client sends
`relay_set` with `enabled: true`; the setting survives restarts.
`--no-relay` keeps it off whatever the setting says.

1. A local client sends `pair_link` and shows the returned `link`
   (`https://app.jucode.net/remote#pair=<host>.<key>.<code>`, the origin
   taken from the relay URL) as a QR code. The code is a `pair_start` code.
2. The phone's page connects through the relay with the code in its first
   Noise message and is paired as a device keyed by its Noise static key.
3. Later connections need no code. Relay devices show up in `device_list`;
   `device_revoke` closes their streams.

A relay stream then behaves exactly like a device's local WebSocket.

## Connecting

Connect to `ws://<listen>/?token=<token>` (or send
`Authorization: Bearer <token>`). A missing or wrong token fails the
handshake with HTTP 401. Local clients read the daemon token from
`~/.jucode/daemon/token`; paired devices use their own token.

Each WebSocket text message is one JSON frame. The daemon first sends:

```json
{"type":"hello","protocol":2,"version":"0.3.0"}
{"type":"sessions","sessions":[{"session":"...","cwd":"...","created_at":0,"updated_at":0,"title":"...","archived":false,"open":true,"watchers":0}]}
{"type":"workspaces","rev":1,"workspaces":[...]}
```

followed by the current `agents`, `schedules`, `questions` and `actions`
lists.

A client that does not speak `protocol` 2 must disconnect.

## Replies

A frame may carry an `id`. Replies to daemon ops go only to the client that
sent the op and echo its `id`. Errors from any op are replied as
`{"type":"error","message":"...","id":...}`. Session events are sent to
every connected client.

## Daemon ops

| Op | Fields | Reply |
| --- | --- | --- |
| `session_list` | — | `sessions`: each with `session`, `cwd`, `chat`, `agent`, `open`, `watchers`, `title` (set with `session_meta`, else the engine's label), `archived`, `updated_at` |
| `gateway_catalog` | — | `gateway_catalog`: `models` (the JuCode models the user chose to show, `jucode_models` in config.json) and `groups` (the gateway's groups with their models and multipliers); empty when not signed in or offline. For clients that cannot read this machine's login (the remote page) |
| `restart_when_idle` | — | none. Desktop only: this daemon exits once no session is running (after a `daemon_restarting` broadcast), so the desktop can start a newer one without cutting off a task |
| `session_meta` | `session`, and any of `title`, `archived`, `hidden`, `group` | none; every client receives the new `sessions` list. An empty title goes back to the engine's label; a hidden session leaves `session_list` and `session_history` (its conversation stays on disk). A session created here is titled after its first user message (first line, 40 characters) unless a client titled it first. After its 1st and 3rd turns, then every 5th, the title model (`title_model` in config.json, else the main `model`) renames it from the project name, the current title, the first and latest requests and the start of the latest reply. A title a client set with `session_meta` is never replaced |
| `session_history` | `cwd` | `session_history`: every session saved in `cwd`, newest first, whoever ran it (daemon, TUI, `jucode serve`), with `title`, `updated_at`, `entries`, `archived`, `agent`, `open` |
| `session_create` | `cwd`, optional `engine` (`jucode`, default, `claude`, `codex` or `acp`) and `options` | `session_created` with `session`; the session's startup events follow. See "Other engines" |
| `session_open` | `session`, optional `cwd`, `engine`, `options` | `session_opened`; with `cwd`, also opens a session saved there that the daemon never hosted. Reopens a closed session (or one from before a restart), resuming its transcript and its undecided deferred actions |
| `session_close` | `session` | none; every client receives `session_closed` once the engine has stopped |
| `watch` / `unwatch` | `session` | `watching` with `watching: true/false`; `watch` also sends this client a snapshot of the session: its state events (`startup`, `model_status`, `command_list`, `approval_mode`, `approval_mode_pending`, `mcp_servers`), a `transcript` of the conversation so far and `attended` |
| `actions_list` | — | `actions`: undecided deferred actions across all sessions, and in `closed` the ones closed lately (as in `questions`) |
| `pair_start` | — | `pairing` with `code` and `expires_at` (local clients only) |
| `device_list` | — | `devices`: paired, unrevoked devices (local clients only) |
| `device_revoke` | `device` | `device_revoked` (local clients only) |
| `relay_status` | — | `relay_status` with `enabled`, `connected`, `host` (the host id), `url` (null with `--no-relay`) (local clients only) |
| `relay_set` | `enabled` | `relay_status`; turns the relay connection on or off and remembers it (local clients only) |
| `mcp_set` / `mcp_remove` / `mcp_toggle` | as the session ops (`server`; `name`; `name`, `enabled`), with no `session` | `mcp_saved`; saves the change to `config.json` and sends the op to every open JuCode session, which answers with `mcp_servers`. Local clients only, with or without `session` |
| `pair_link` | — | `pair_link` with `link`, `code` and `expires_at`; an error while the relay is off (local clients only) |
| `ping` | — | `pong`. Clients behind the relay send it every minute so an idle stream is not closed |
| `workspaces` | — | `workspaces` with `rev` and `workspaces: [{id, name, is_default?, color?, icon?, projects: [{id, name, path, dirs?, chats?, worktree?, color?, icon?}]}]`. A project's `color` and `icon` are kept as the client sent them (shaped as an agent's, see "Agents"). `dirs`: the project's extra directories (absolute paths, not including `path`); remote clients may read them, and an engine started in `path` may work in those that exist: Claude Code gets one `--add-dir` each, Codex has them as writable roots of its workspace-write sandbox (and `--add-dir` in its TUI), and a JuCode session's file tools and sandbox treat them as writable workspace (ACP agents get nothing) |
| `workspaces_set` | `rev`, `workspaces` | `workspaces`; replaces the list when `rev` is the current one, else an error (another client changed it). A project's `dirs`, when present, must be a list of absolute paths. Desktop imports its list with `rev: 0` into an empty daemon |
| `project_add` | `path`, optional `workspace`, `project_name`, `workspace_name` | `workspaces`; adds an existing directory. With no workspaces yet, one named `workspace_name` is created |
| `project_create` | `parent`, `name`, optional `git_init`, `workspace`, `workspace_name` | `workspaces`; makes the folder `parent/name` (optionally `git init`) and adds it |
| `project_remove` | `workspace`, `project` | `workspaces`; the files stay |
| `fs_list` | `path` (`~` is the home directory), optional `dirs_only` | `fs_list` with `path`, `git`, `entries: [{name, dir, size}]`, `truncated`. Git-ignored entries and `.git` are left out |
| `fs_read` | `path` | `fs_read` with `size`, `binary`, `text` (first 1 MiB), `truncated` |
| `fs_image` | `path` (a png/jpeg/gif/webp in a known directory or an upload, up to 16 MiB) | `fs_image` with `data`, a data URL: how the remote page shows a message's images |
| `git_status` | `path` | `git_status` with `repo`, `branch`, `files: [{path, status, from}]` (porcelain codes) |
| `git_diff` | `path`, optional `file` | `git_diff` with `diff` (unified, untracked files included, first 1 MiB), `truncated` |
| `agent_list` | — | `agents` |
| `agent_create` | `agent` (the new agent's id), `name`, `cwd`, `role`, optional `icon`, `color`, `avatar_seed`, `project` (see "Agents") | `agent_created`; every client also receives the new `agents` list |
| `message_send` | `agent`, `body`, optional `session`, `reply_to`, `dedupe_key` | `message_accepted` with `message` (the new message's id) and `duplicate` (a message with this `dedupe_key` was already recorded; nothing is sent). Routed as below; delivery is broadcast as `message_delivered` |
| `timer_list` | optional `agent` | `timers: [{timer, agent, session, fire_at, body}]`: active timers (of all agents, or of `agent`), soonest first; `fire_at` in ms |
| `timer_cancel` | `timer` (its id) | `timer_cancelled` with `timer`; persists cancellation so the reminder will not fire after a restart. Errors if it already fired or was cancelled |
| `agent_get` | `agent` | `agent`: `agent` (settings), `brief` (`{"role.md": text, "capabilities.md": …, "policy.md": …, "state.md": …}`), `memory` (file names, `["memory/deploy.md", …]`) and the agent's `sessions` (as in `session_list`) |
| `agent_update` | `agent`, optional `name` (not empty), `role` (rewrites `role.md`), `enabled`, `approval_mode`, `sandbox`, `network`, `directories`, `command_rules`, `icon`, `color`, `avatar_seed`, `project` (`null` clears `icon`, `color` or `project`; see "Agents") | `agent_updated` with `agent`; every client also receives the new `agents` list |
| `agent_delete` | `agent` | `agent_deleted` with `agent`; an error while any of its sessions is running. See "Agents" |
| `agent_memory_read` | `agent`, `file` (`deploy.md`, or `memory/deploy.md` as `agent_get` lists it) | `agent_memory` with `agent`, `file`, `content`; an error for anything but an existing `memory/<name>.md` (letters, digits, `-`, `_`) |
| `schedule_list` | optional `agent` | `schedules`: all scheduled tasks, or `agent`'s. See "Scheduled tasks" |
| `session_usage` | `session` (its id) | `session_usage` with `totals`, `sessions` and `turns`: persisted and live usage, with settled gateway charges |
| `schedule_usage` | `schedule` (its id) | `schedule_usage` with `totals` and `sessions`: token counts and costs of the task's run sessions, including follow-ups; reused sessions count once |
| `schedule_save` | `schedule`: without `id` creates one (`agent`, `name`, `prompt`, `repeat`, `time`, and `days` / `date` as `repeat` needs; optional `enabled`, `new_session`, both default `true`); with `id` changes the fields present among `name`, `prompt`, `enabled`, `repeat`, `time`, `days`, `date`, `new_session` | `schedule_saved` with `schedule`; `next_run_at` is computed again |
| `schedule_delete` | `schedule` (its id) | `schedule_deleted` with `schedule` |
| `schedule_run` | `schedule` (its id) | `schedule_started` with `schedule`; runs it now, enabled or not (an error when its agent is disabled), without changing `next_run_at` |
| `question_list` | — | `questions`: unanswered questions, and in `closed` the ones closed in the last 7 days (each with `closed_by`, `closed_reason`, `closed_at`) |
| `question_answer` | `question`, `answer` | `question_answered`; the answer is delivered to the session that asked |
| `item_close` | `kind` (`question` or `action`), `item` (its id), optional `reason` | `item_closed` with `item`; closes an open question or deferred action without answering it, then broadcasts `questions` / `actions`. Nothing is sent to the agent |
| `item_reopen` | `kind`, `item` | `item_reopened` with `item`; puts a closed item back among the open ones and takes its session out of the archive |
| `report_list` | optional `limit` (50) | `reports`, newest first, with `read` |
| `report_read` | `report` | `report_read` |
| `skills_catalog` | optional `backend` (`jucode`, default, or `claude`) | `skills_catalog` with `skills: [{id, name, description, tags, source, isDefault, installed, license, redistributable, homepage}]`, `warnings` and `installDir` (local clients only). See "Skills" |
| `skill_install` | `source` (`jucode` or `anthropic`), `skill` (its `id` in the catalog), optional `backend` | `skill_installed` with `path` (local clients only) |

`decide_action` (a session op) also works for a session that is closed or
was hosted before a restart: the daemon reopens it first.

`session_create` also accepts `agent` instead of `cwd`: the session runs in
the agent's directory as that agent. With `chat: true` instead, the session is
a chat: it runs in `~/.jucode/chats` with the chat prompt (conversation and
web research) and without project instructions or project skills. Any session
whose directory is `~/.jucode/chats` or lies inside it is a chat session, so
reopening one keeps it a chat.

Changes to workspaces are broadcast to every client as a `workspaces` frame.

`fs_*` and `git_*` read only inside known directories: projects, session
directories and agent directories. `fs_list` with `dirs_only` may browse
folders anywhere under the home directory (for picking a new project).
Credentials (`~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.jucode/auth.json`,
`~/.jucode/daemon`) are never readable. Paths are resolved (symlinks
followed) before the check.

## Skills

`skills_catalog` lists the JuCode marketplace (`/v1/skills/marketplace` on
the configured JuCode API, with the JuCode login's token when there is one)
and the bundled index of github.com/anthropics/skills. When the marketplace
cannot be reached its error is one of `warnings` and only the Anthropic
skills are listed. `source` is `jucode` or `anthropic`; `redistributable` is
false for Anthropic's source-available document skills, which `skill_install`
refuses.

`backend` picks the directory: `claude` installs into `~/.claude/skills`,
anything else into `~/.jucode/skills`. `installed` means
`<installDir>/<id>/SKILL.md` exists. `skill_install` looks the skill up again
in its source, then downloads it: a JuCode skill's inline content or
checksummed package, or an Anthropic skill's whole directory at the index's
pinned commit. The new install replaces an old one only once complete. Both
ops are refused for paired devices: an installed skill is instructions and
scripts that later sessions run.

## Other engines

`session_create` with `engine: "claude"` runs Claude Code
(`claude --print --input-format stream-json ...`) and `engine: "codex"` runs
Codex (`codex app-server`) in `cwd` instead of a jucode engine. The binary
comes from `CLAUDE_BIN` / `CODEX_BIN`, then PATH, then the usual install
directories. The daemon translates its stream into the same session
events a jucode session sends and client ops into Claude Code frames, so
clients need nothing engine-specific. `options`:

| Field | Meaning |
| --- | --- |
| `approval_mode` | `manual`/`read-only` (Claude's `default`), `plan`, `auto`, `auto-edit`, `full-access`/`full-auto` |
| `model` | Model to start with |
| `resume_at` | Claude Code: resume the conversation as it was at this assistant message uuid |
| `effort`, `ultracode`, `fast`, `thinking` | Claude Code: start with this thinking effort, ultracode on, fast mode on, thinking summaries shown (`false` hides them). A restart (full access, gateway switch) keeps the session's own |
| `jucode_gateway` | Claude Code / Codex: `true` runs this session through the JuCode gateway on the user's JuCode login; `false` on the provider in the user's own Claude Code / Codex config. The endpoint and key go to this process only (Claude: `--settings` file; Codex: `-c` overrides and an env var), never to the user's config files. Omitted on `session_open`: as the session last ran |
| `command`, `args` | ACP: the agent's command line |
| `bin` | Claude Code / Codex: the engine binary to run instead of the one found on `PATH` |
| `env` | Extra environment variables for the engine process (plain names; no `DYLD_*`/`LD_*`) |

`engine: "acp"` runs an Agent Client Protocol agent (`jucode acp`,
`gemini --experimental-acp`, ...) from `options.command`. Only local
clients may start or reopen one, or pass `bin` or `env` for any engine,
since that names a program to run; paired devices watch and drive such
sessions like any other. ACP agents keep no
conversation the daemon can resume, so reopening an ACP session starts a
new conversation under the same session id.

The session id is the engine's conversation id (Claude Code's session id,
Codex's thread id), so reopening a closed session resumes the same
conversation (`--resume`, `thread/resume`), and `session_open` with `cwd`
and `engine` opens any conversation the engine saved for that directory
(`~/.claude/projects`, `~/.codex/sessions`). `session_history` lists those
too, with their `engine`. `session_created` for Codex comes once its thread
is open.
Every session in `session_list` carries its `engine`.

Differences from a jucode session:

- A watching client gets a snapshot rebuilt by the daemon: the latest state
  events, the conversation so far (text and tool results; a reopened
  session starts from the text Claude Code saved) and any open permission
  prompts. Prompts wait for whichever client answers them first; an
  unwatched session is not switched to deferred actions.
- Switching into or out of full access restarts Claude Code on the same
  conversation (it only honors that mode as a start flag), after the running
  turn. Codex applies a new mode, and a model picked with `/model`, from the
  next turn.
- `steer`, `decide_action`, `mcp_set`/`mcp_remove` and the jucode-only
  commands (`/resume`, `/rewind`, `/tree`, ...) are refused with an `error`
  event. Other slash commands go to Claude Code as a user message, as Claude
  Code expects.

Claude Code sessions take more:

| Op / command | Does | Answered by |
| --- | --- | --- |
| `/effort ultracode [on\|off]`, `/fast [on\|off]`, `/thinking [on\|off]` | Ultracode (standing multi-agent Workflow orchestration), fast mode, thinking summaries | `model_status` with `ultracode`, `ultracode_available`, `fast`, `fast_state`, `fast_available`, `thinking_summaries`; a refused fast mode also an `info` |
| `/btw <question>` | A side question, answered from the conversation without tools and never added to it | `side_answer` with `question` and `answer` or `error` |
| `stop_task` `task_id` | Stops a background task | `background_tasks`, then `task_done` |
| `task_output` `task_id` | A background shell's or Monitor's output, its last 8 KiB | `task_output` with `task_id`, `output`, `truncated` (or `error`) |
| `mcp_list`, `mcp_toggle` `name` `enabled`, `mcp_reconnect` `name` | Claude Code's own MCP servers, for this session | `mcp_servers` |
| `agent_runs` | The agent trace: every Workflow and Task subagent of the conversation, live ones and those Claude Code saved before this process | `agent_runs` (below) |
| `subagent_transcript` `agent_id` | One subagent's own conversation, read from the file Claude Code keeps for it | `subagent_transcript` with `agent_id` and `items` (`user`/`assistant`/`reasoning` with `content`; `tool` with `call_id`, `name`, `output`, `is_error`, `running`; the newest 600), or `error` |
| `permission_rules` | The rules in effect, as `/permissions` lists them | `permission_rules` with `rules: [{behavior, source, rule, editability}]`, `directories` |
| `approve` with `always_scope` | `project` saves the always-allow rule to the project's `.claude/settings.local.json`, `user` to `~/.claude/settings.json`; the session otherwise | — |

Codex sessions take the same `steer`, `/fast`, `/thinking`, `rename`,
`agent_runs`, `subagent_transcript`, `mcp_list` and `mcp_reconnect`, plus
`mcp_login` `name` (answered by `mcp_login` with the server's sign-in `url`)
and `/login` (a device code to sign Codex in to ChatGPT, as an `info`). A
message sent mid-turn waits in the daemon (`pending_messages`) and starts the
next turn; `steer` sends it into the running one (`turn/steer`). Approval
mode `plan` is Codex's plan collaboration mode and `auto` its `auto_review`
reviewer. Codex's own requests for permissions, for the user's answers
(`ask_question` with `questions`) and MCP elicitations come as approval
cards; a command card with `scopes: ["session", "rule"]` can be allowed
always with `always_scope: "rule"` (a Codex exec rule). Subagents are
threads of their own: their events feed `agent_runs` and
`subagent_lifecycle`, never the conversation.

And sends more events: `background_tasks` (`tasks: [{id, kind,
description}]`, the live set: replace yours), `task_progress` (`task_id`,
`message`), `task_done` (`task_id`, `kind`, `status`, `summary`: a background
task ended; its `<task-notification>` message is not echoed as a
`user_message`), `subagent_lifecycle` with a `label` (Task subagents, by task
id) and `tool_start` with `subagent` (the subagent that made the call),
`retrying` (API retries), `model_fallback` (`from`, `to`, `reason`),
`prompt_suggestion` (`text`, the likely next prompt), `agent_runs`
(`workflows: [{id, tool_use_id, name, description, status, started_at,
duration_ms, tokens, tool_calls, phases: [{index, title}], agents: [{id,
label, phase, model, state, started_at, duration_ms, tokens, tool_calls,
prompt, result, error}]}]`, `agents: [{id, label, type, tool_use_id, status,
model, started_at, duration_ms, tokens, tool_calls}]`; sent whenever a run
changes, and part of a watcher's snapshot), and `approval_request`
named `mcp_elicitation` for an MCP server's question (`url` to open, or
`questions` from its form; answered with `approve` and `answers`). Images
attached to a `user_message` go as image blocks (files over 3.75 MB as a path
to Read). A session's name is shared both ways: a `session_meta` title is
sent to Claude Code (`claude --resume` lists it), and a name Claude Code
has for the session becomes its title.

## Agents

A long-lived agent is a directory `~/.jucode/agents/<id>/`: its brief
(`role.md`, `capabilities.md`, `policy.md`, `state.md`), `memory/<topic>.md`
notes and `agent.json`:

| Field | Default | Meaning |
| --- | --- | --- |
| `name`, `cwd`, `enabled` | | Display name, working directory, whether it takes messages (and runs its scheduled tasks). |
| `approval_mode` | `auto` | `manual`, `auto-edit`, `auto` or `full-access`. |
| `sandbox` | `workspace-write` (`full-access` on Windows) | Where its shell commands run; see below. |
| `network` | `true` | Whether sandboxed commands may connect out. |
| `directories` | `[]` | `[{"path": "/abs/dir", "mode": "ro" \| "rw"}]`: directories outside `cwd` it may read, or read and write. |
| `command_rules` | `git add`/`git commit` allow, `git push` ask | `[{"prefix": "git push", "action": "allow" \| "ask" \| "forbid"}]`. |
| `icon` | none | `{"kind": "builtin", "id": "rocket"}`, `{"kind": "slug", "value": "…"}` (32 UTF-16 units at most) or `{"kind": "svg", "markup": "<svg…>"}` (8192 bytes at most). The daemon checks only the shape and size: clients sanitize an SVG before drawing it. |
| `color` | none | `#rrggbb`. |
| `avatar_seed` | random hex at creation | Seeds the generated avatar clients draw when there is no `icon`. Agents created before it have none; clients then use the id. |
| `project` | `null` | The project it belongs to (an id from `workspaces`; a project has any number of agents). Setting it moves `cwd` to the project's main directory (an unknown project is an error); the agent's prompt names the project and its directories. |

`agent_update` changes any of these fields except `cwd` (which follows
`project`), and rewrites `role.md` from `role`.

`agent_delete` removes `~/.jucode/agents/<id>/` (brief, memory, settings,
scheduled tasks), cancels the agent's active timers, closes its open
questions and its open sessions. It is refused while one of its sessions is
running. Its sessions stay in `session_list` with their `agent`; a message
still waiting for it becomes undeliverable, and new messages to it are
refused. Every client receives the new `agents`, `schedules` and `questions`
lists.

### Sandbox

An agent's shell commands run in an OS sandbox (Seatbelt on macOS,
`bwrap` on Linux; a session does not start when the sandbox is missing):

- `read-only`: nothing is writable.
- `workspace-write`: `cwd`, the `rw` directories, temp and package-cache
  directories are writable; `.git` (and a worktree's real git directory),
  `.jucode`, `.agents` inside them and the `ro` directories stay read-only.
- `full-access`: no sandbox.

`~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.jucode/auth.json` and
`~/.jucode/daemon` are unreadable in every sandboxed mode. File tools check
writes against the same rules and can also read and write the agent's
directories.

A command inside the sandbox needs no approval (except under `manual`). A
command that must leave it (commit to git, write elsewhere) is called with
`escalate: true` and a `justification` and goes through the approval mode:
`auto` asks the safety model, the others ask a person, and an unattended
session defers it. Command rules come first: `forbid` never runs, `ask`
always asks a person, `allow` lets an escalation run without asking;
`forbid` wins over other matches, otherwise the longest prefix. Every turn of an agent session gets the brief, the memory index and
the other agents in its system prompt, and three tools:

| Tool | Does |
| --- | --- |
| `message_agent` | Sends a message to another agent. |
| `timer` | `set` (after `in_seconds` or at unix `at`), `list`, `cancel`. A timer wakes the session that set it unless `new_session` is true, whether or not a client is connected. |
| `brief` | Reads or rewrites the agent's own brief and memory files. |
| `question` | Records a question for the user (`title`, `body`, `assumption`, `default`, `due_in_seconds`, `importance`) and returns at once. The answer, or the deadline passing (the agent then goes with `default`), is delivered to the session that asked. |
| `report` | Records a report (`title`, `body`) for the user to read; wakes nobody. |
| `requirements` | `list`, `get`, `progress`, `propose_create`, `propose_close`: see "Agents and requirements". Noting and closing a requirement are only proposals the user accepts. |

Messages (from `message_send`, `message_agent`, a fired timer or a scheduled
task) are
recorded in `messages.jsonl` before delivery and routed to a session:

1. the `session` the message names;
2. the session that received the message it replies to (`reply_to`);
3. for a message from the user, the agent's most recently active session;
4. otherwise a new session.

A delivered message is a user message in that session: it starts a run, or
queues behind the running one. At most 4 runs are in progress at once; a
message that would start a fifth waits. Messages are retried every second,
including ones left over from before a restart, and a fired timer is
delivered once (its id is the message's dedupe key). Every client receives
`message_delivered` (`id`, `agent`, `from`, `session`) and an updated
`agents` list when an agent starts or stops working, an updated `schedules`
list when a scheduled task changes or runs, an updated `questions`
list when a question is asked or answered, `report_posted` for a new report,
and an updated `actions` list when an action is deferred or decided.

### Scheduled tasks

A scheduled task sends its prompt to an agent at set local times. Each
agent's tasks are saved in `~/.jucode/agents/<id>/schedules.json`.

```json
{
  "id": "sch-1a2b3c4d5e6f7a8b",
  "agent": "ops",
  "name": "aicare 工单处理",
  "prompt": "处理新工单 …",
  "enabled": true,
  "repeat": "weekly",
  "time": "11:00",
  "days": [1, 3, 5],
  "date": null,
  "new_session": true,
  "created_at": 1790812800,
  "last_run_at": null,
  "last_session": null,
  "next_run_at": 1790996400
}
```

| Field | Meaning |
| --- | --- |
| `repeat` | `once`, `hourly`, `daily`, `weekdays` (Monday to Friday) or `weekly`. |
| `time` | Local `HH:MM`, 24-hour. `hourly` uses only the minute. |
| `days` | `weekly` only, not empty: 0 = Sunday … 6 = Saturday. |
| `date` | `once` only: local `YYYY-MM-DD`. |
| `new_session` | `true`: each run starts a new session. `false`: a run continues `last_session`, or starts a new one when that session is gone or no longer the agent's. |
| `created_at`, `last_run_at`, `next_run_at` | Unix **seconds** (other daemon times are milliseconds). `next_run_at` is null for a disabled task and for a `once` task whose time has passed. |
| `last_session` | The session the last run was delivered to, filled in once it is delivered. |

Times follow this machine's time zone, daylight saving included: a local
time skipped by a clock change runs an hour later, one that occurs twice
runs at the first.

The scheduler checks every second. A due task (`next_run_at` ≤ now) is
recorded as a message from `schedule:<id>` with dedupe key
`schedule:<id>:<next_run_at>`, then `last_run_at` becomes now and
`next_run_at` the next time after now. Runs missed while the daemon or the
computer was off therefore fire once when it starts again, not once per
missed time. A task of a disabled agent neither fires nor moves on; it fires
once when the agent is enabled again. `schedule_run` uses the dedupe key
`schedule:<id>:run:<unix seconds>`, so a second run within the same second is
dropped.

The agent receives the prompt after one line naming the task:
`定时任务「<name>」：`, under the usual delivery line
(`[scheduled task <id> · <message id>]`).

Every client receives the full `schedules` list after any change, run or
delivery.

`schedule_delete` and `schedule_run` also take the task's id as `id` when
`schedule` is absent; that `id` is then the request id too, echoed in the
reply.

`schedule_usage` joins the task's delivered sessions with persisted turns in
`usage.jsonl`, including any turn still running. Each unique session is counted
once, including follow-up turns. `totals` and each row in `sessions` carry the
five token counters, `turns`, `gateway_cost` (settled points, including group
multipliers), `estimated_cost_usd` (reference USD cost), `pending_requests`
(gateway charges not yet available), `unpriced_requests` and `running`.
Session rows also carry `session` and `started_at` (milliseconds or null).
Cached input and reasoning tokens are subsets of input and output respectively.
The two currencies are separate; unavailable amounts are null, never zero.
Reference estimates are saved with new turns at the time of usage; old turns
without prices stay unpriced. Gateway amounts are read by turn id from the
signed-in account's `POST /v1/oauth/agent-usage/charges` endpoint. A failed
billing lookup sets `billing_error` while still returning the local token counts.

`session_usage` uses the same ledger for a single session, including its history.
Both usage replies also include `gateway_requests` in each total and a `turns`
array, whose rows carry `turn_id`, `session` and the same totals. Live `usage`
events carry `billing_turn` (the ledger key) and `billing_gateway`, allowing the
client to update the matching reply's fee even if settlement arrives during a
later turn. Ledger amounts already include each request's group multiplier;
clients must not multiply them again or display points as dollars. Ledger reads
run off the connection's input thread, so an unavailable billing server cannot
hold up user input or interrupts. Clients should coalesce refreshes, recheck
pending charges while watching, and retain settled values with an explicit error
when a subsequent lookup fails.

## Dispatch

A dispatch hands the daemon a batch of requests without a project or
session. A reserved agent, `dispatch` (never listed), splits it into tasks,
sends each to a session in one of the user's projects, and reports back.
Each dispatch is one session of that agent; `id` is that session.
`agent_create` refuses the id `dispatch`. A session runs one dispatch task at
a time: a session that is busy, or running another dispatch's task, is not
given one.

| Op | Reply | |
| --- | --- | --- |
| `{"op":"dispatch_send","text":"…","plan":false,"approval_mode":"auto"}` | `dispatch_started` | `approval_mode`: `manual`, `auto-edit`, `auto` or `full-access`; every task runs in it. With `plan`, no task starts before `dispatch_confirm`. |
| `{"op":"dispatch_confirm","dispatch":"…","approve":true,"note":"…"}` | `dispatch_confirmed` | Only while the dispatch is `awaiting`. |
| `{"op":"dispatch_list"}` | `dispatches` | |

`dispatches` is also sent on connect and broadcast on every change:

```json
{"type":"dispatches","dispatches":[{
  "id":"s…","text":"…","plan":true,"mode":"auto","status":"running",
  "tasks":[{"id":1,"title":"…","project":"/path","session":"s…","engine":"jucode",
            "status":"running","reply":"…"}],
  "summary":"","created_at":0,"updated_at":0}]}
```

Dispatch `status`: `planning`, `awaiting` (plan mode, waiting for the user),
`running`, `done`, `cancelled`, `failed` (the dispatcher's turn failed).
Task `status`: `planned`, `sent`, `running`,
`waiting` (an approval in its session waits for the user), `done`, `failed`.
`reply` is the end of the task session's last reply.

## Uploads

A client sends a file to this computer (the remote page has no file system
the engines can read) in chunks of base64 over its connection, waiting for
each reply before the next chunk. Through the relay a file is ordinary
frames, end to end encrypted; the relay stores nothing.

| Op | Reply | |
| --- | --- | --- |
| `{"op":"upload","name":"photo.jpg","data":"<base64>","last":false}` | `upload_part` with `upload` (its id) and `size` | Starts a file. |
| `{"op":"upload","upload":"u-…","offset":262144,"data":"<base64>","last":true}` | `uploaded` with `upload`, `path`, `name`, `size`, `image` | `offset` must be the size received so far. The reply to the `last` chunk carries the file's path. |

A chunk is at most 4 MB decoded, a file at most 100 MB. Files land in
`~/.jucode/uploads/<date>/<id>-<name>` (outside the daemon's state directory,
so the jucode sandbox lets tools read them) and are written as `<path>.part`
until the last chunk; parts left over and files older than 30 days are
removed when the daemon starts. A message names them as the desktop does
its attachments: images in `user_message`'s `images`, other files as paths
in its text. The session's `user_message` event, and the user items of its
`transcript`, carry those `images` too, whichever engine runs it.

## Terminals

A client (local or a paired device) may run the user's login shell on a pty
on this computer. `term` is the terminal's id; `data` is base64 (standard
alphabet) of raw bytes.

| Op | Reply | |
| --- | --- | --- |
| `{"op":"term_open","cwd":"/path","cols":80,"rows":24}` | `term_opened` with `term` | `cwd` must lie in a known directory, as for `fs_*` (`~` is the home directory). `cols` and `rows` default to 80 × 24 and are kept within 2–1000. |
| `{"op":"term_input","term":"t-…","data":"<base64>"}` | none | Written to the shell in order. |
| `{"op":"term_resize","term":"t-…","cols":120,"rows":40}` | none | |
| `{"op":"term_close","term":"t-…"}` | none | Kills the shell; `term_exit` follows. |

Only the client that opened a terminal receives its events:

- `{"type":"term_output","term":"t-…","data":"<base64>"}`: the shell's
  output, sent after 10 ms without more output, at most 64 KiB before
  encoding per frame. A frame may end inside a UTF-8 character or an
  escape sequence.
- `{"type":"term_exit","term":"t-…","code":3}`: the shell has exited and
  been reaped; `code` is null when it was killed by a signal.

A client may have 4 terminals open; a fifth `term_open` is an error. Ops on
another client's terminal or an unknown one are errors. A terminal belongs to
its connection: when the client disconnects its shells are killed (SIGHUP,
then SIGKILL), and there is no reattaching. The shell is `$SHELL -l` (else
`/bin/zsh` on macOS, `/bin/bash`, then `/bin/sh`), `%COMSPEC%` or
PowerShell on Windows, with the daemon's environment plus
`TERM=xterm-256color`, `COLORTERM=truecolor` and `LANG=en_US.UTF-8` when
`LANG` is unset.

### A conversation in its TUI

`{"op":"session_tui","session":"<id>","cols":120,"rows":40}` moves a
claude, codex or jucode conversation from the client's chat view into the
engine's own terminal interface, on a pty of the daemon. The reply is
`term_opened`; the terminal then works like one from `term_open` (only the
requesting client gets its output and may type into it, `term_close` ends
it). It is refused while a turn runs unless the op has `"force":true`
(the user agreed to stop it: a jucode turn is interrupted, a claude or codex
engine is killed and every client gets `{"type":"status","message":"interrupted"}`),
for an ACP engine, and for an agent's session.

The engine process stops first, background tasks with it (clients get an
empty `background_tasks`), and the TUI resumes the same conversation
(`claude --resume <id>`, `codex resume <id>`, `jucode --resume <id>`); a
conversation the engine has not saved yet starts there instead (`claude
--session-id <id>`, plain `codex`, whose new thread the daemon finds in the
directory when the TUI exits) with
the session's approval mode and model; under the gateway it gets its own
key. Every client of the session gets
`{"type":"surface","session":"<id>","surface":"tui","term":"t-…","client":1}`.
While the TUI runs, ops on the session other than `snapshot` are errors.

When the TUI exits (the user quits it, or `term_close`) the engine starts
again on the conversation, now including the turns typed in the TUI, and
the session publishes `{"type":"surface","session":"<id>","surface":"gui"}`
and a fresh `transcript`. Variables that would mark the TUI as a nested
Claude Code session (`CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`, …) are
removed so it saves its transcript.

## Requirements

A requirement is what the user means to get done: their words (`text`),
screenshots, the project it belongs to (`project`, a project id, or `null`),
and the sessions that work on it. A session works on one requirement at a
time. After each turn of a linked session, the title model rewrites the
requirement's `progress` from the previous record and the session's first
and latest requests and the end of its reply, so a new session starts from
where the last one stopped. The requirement's id comes as `requirement`
(`id` is the request's).

| Op | Reply | |
| --- | --- | --- |
| `{"op":"requirement_list"}` | `requirements` | |
| `{"op":"requirement_create","text":"…","project":"p…","images":["/…/.jucode/uploads/…/u-…-shot.png"],"source":"phone","session":"s…"}` | `requirement_created` with `requirement` | All but `text` optional. `project`: a known project's id, or `null` for none; left out, a requirement noted in `session` belongs to the project whose main directory is that session's `cwd`. `images`: up to 4 uploaded images (see "Uploads"; any other path is refused), moved to `~/.jucode/uploads/requirements/<id>/`. |
| `{"op":"requirement_update","requirement":"R-1","text":"…","project":"p…","state":"done"}` | `requirement_saved` | Any of the fields. `project: null` leaves it unassigned. `state`: `idea`, `open`, `done`, `parked`, `proposed`. |
| `{"op":"requirement_delete","requirement":"R-1"}` | `requirement_deleted` | Its sessions stay. |
| `{"op":"requirement_link","requirement":"R-1","session":"s…"}` | `requirement_linked` | The session leaves any other requirement (and its gate); the requirement becomes `open`. |
| `{"op":"requirement_unlink","requirement":"R-1","session":"s…"}` | `requirement_unlinked` | A gate on that session ends. |
| `{"op":"requirement_image","requirement":"R-1","index":0}` | `requirement_image` with `data` (a data URL) | |
| `{"op":"requirement_begin","requirement":"R-1","session":"s…","plan":false,"mode":"edits","text":"…","lang":"zh"}` | `requirement_begun` with `requirement`, `session` | Starts work behind the gate (below): links the session, switches it to read-only and sends the requirement with the ask to only explain its understanding. `text`: the user's added words (optional). `lang`: `zh` (default) or `en`. |
| `{"op":"requirement_confirm","requirement":"R-1","text":"…","lang":"zh"}` | `requirement_confirmed` with `requirement`, `stage` (`plan` or `go`) | The user confirms the gate's current step; an error without a gate or while its session runs a turn. `text` (optional) goes with the next message. |
| `{"op":"requirement_reply","requirement":"R-1","text":"…","new_session":false,"cwd":"/path","engine":"claude","plan":false,"mode":"auto","lang":"zh"}` | `requirement_replied` with `session` | Sends `text` to its latest session (reopened if closed). With `new_session`, or no session yet, starts one in `cwd` (default: the latest session's, else the project's main directory; neither is an error) on `engine` (default: the latest session's, else jucode) and runs `requirement_begin` on it with `plan` (default `false`), `mode` (default `auto`) and `text`. |
| `{"op":"requirement_proposal","requirement":"R-1","accept":true}` | `requirement_proposal_decided` with `requirement`, `accept` | Decides an agent's proposal: a proposed requirement accepted becomes `idea`, turned down is deleted; a close proposal accepted sets `state` to its `outcome` (and ends any gate), turned down is dropped. The agent is not told. |

`dispatch_send` also takes `requirement`: its tasks' sessions are linked to it.

### The start gate

A session starts on a requirement in steps, each confirmed by the user, so
nothing changes before they agreed what is to be done. `gate` on the
requirement, for one session:

```json
"gate": {"session":"s…","stage":"understand","plan":true,"mode":"auto","answered":false}
```

`answered` turns true when a turn of that session ends at the stage; only
then does the requirement show `confirm` (an engine also reports ready when
it starts or switches modes). While the gate stands the session is held
read-only: a client's `set_approval_mode` and the mode its engine starts or
restarts in are replaced by the read-only one.

1. `understand`: the session runs read-only (jucode `manual`, Claude Code
   and Codex `read-only`) and replies with the goal, scope, assumptions and
   open questions, and whether the work is worth doing, then stops.
2. With `plan`, confirming moves to `plan`: still read-only, it writes an
   implementation plan and stops.
3. The last confirmation switches the session to `mode` and sends it to
   work; `gate` is removed.

`mode` is the permission mode to work in, as the client names it (`ask`,
`plan`, `auto`, `edits`, `all`) or an engine does (`manual`, `read-only`,
`auto-edit`, `full-auto`, `full-access`); the daemon translates it for the
session's engine (`plan` is `manual` on jucode). A user who replies in the
session instead of confirming has it revise its understanding or plan; the
gate stays at its step.

### Agents and requirements

An agent's sessions have a `requirements` tool: `list` (the requirements of
the agent's project, or all when it has none; filters `project`, with
`none` for unassigned ones, and `state`), `get`, `progress` (appends items
to `decided`, `done`, `doing`, `blocked`, `next` once each, replaces
`note`, and sets `progress_at`), and two proposals the user decides with
`requirement_proposal`:

- `propose_create` (`text`, `reason`): a new requirement in the agent's
  project with `state: "proposed"`, `source: "agent"` and
  `proposal: {kind: "create", agent, session, reason, at}`.
- `propose_close` (`requirement`, `reason`, `outcome`: `done` or `parked`):
  on an `idea` or `open` requirement, sets
  `proposal: {kind: "close", outcome, agent, session, reason, at}`,
  replacing an earlier one.

`requirements` is also sent on connect and broadcast on every change:

```json
{"type":"requirements","requirements":[{
  "id":"R-1","text":"…","title":"…","title_auto":false,"images":["/abs/1.png"],
  "project":"p…","state":"open","sessions":["s…"],
  "progress":{"goal":"…","decided":[],"done":[],"doing":[],"blocked":[],"next":[],"files":[],"note":"…"},
  "progress_at":0,"source":"desktop","source_session":null,
  "gate":{"session":"s…","stage":"understand","plan":false,"mode":"auto"},
  "proposal":{"kind":"close","outcome":"done","agent":"ops","session":"s…","reason":"…","at":0},
  "status":"confirm","session_states":{"s…":"idle"},"last_reply":"…",
  "created_at":0,"updated_at":0}]}
```

Words longer than 40 characters get a title from the title model
(`title_auto`). `source` is `desktop`, `phone`, `session` or `agent`.
`status` is `state` (`proposed` while an agent's proposal to note it
waits), except:

- `proposal`: an `idea` or `open` requirement with a close proposal.
- While `open`: `approval` (a session waits for an approval), `confirm`
  (the gate's session is idle: the user confirms its understanding or
  plan), `failed` (the latest session's turn failed), `running`, else
  `review` (the user's turn: the work is done or the agent asks
  something), or `open` with no session.

`session_states`: `running`, `waiting`, `failed`, `idle`. `last_reply` (the
end of the latest session's reply) comes with `review`, `confirm` and
`failed`.

Requirements saved before projects had ids carry `projects: ["/path"]`;
once the workspaces have projects, the daemon replaces it with the `project`
whose main directory is the first path (or `null`).

## Notifications

A paired device's browser registers its Web Push subscription with
`{"op":"push_subscribe","subscription":{"endpoint":"…","keys":{"p256dh":"…","auth":"…"}}}`,
the JuCode Android app its 个推 (Getui) client id with
`{"op":"push_subscribe","subscription":{"provider":"getui","client_id":"…"}}`
(only paired devices; dropped when the device is revoked) and removes it with
`{"op":"push_unsubscribe","endpoint":"…"}` or `{"op":"push_unsubscribe","client_id":"…"}`.
A client id registered again by another device moves to that device. The daemon notifies when a
dispatch's plan waits for the user, a task waits for an approval, a
dispatch is done, an open requirement turns to `review`, `confirm` or
`approval`, and an agent proposes to note or close a requirement,
through the relay (relay-protocol.md, Web Push). `{"op":"push_test"}` (paired devices
only) sends a test notification to that device's browsers now and replies
`push_tested` with `results: [{service, status}]` (or `error`): the push
service's host (`getui` for the Android app) and its answer, as the relay passed it on. A notification's `url` is
the remote page, with `?requirement=R-1` for a requirement; one about a session
also carries `session`, which the Android app opens.

## Session ops

Every op from `docs/serve-protocol.md` (`user_message`, `command`, `steer`,
`interrupt`, `approve`, `set_approval_mode`, `decide_action`, `mcp_*`) is
accepted with a `session` field and forwarded to that session's engine. Its
events carry the same `session` field.

`set_approval_mode` applies at once where the engine allows it: a JuCode
session's running turn (and its subagents) gates its next tool call by the
new mode, and calls waiting for a decision the new mode no longer needs run.
Codex takes the mode with each turn, and Claude Code goes in or out of full
access only by restarting; switched while a turn runs, they send
`{"type":"approval_mode_pending","mode":"…"}` and apply it when the turn
ends (`mode: null` then; a client can `interrupt` to apply it sooner). A
switch to full access answers the turn's open and later approvals at once.

`set_gateway` (`gateway`: bool, optional `model`) moves a Claude Code or Codex
session between this machine's own login and the JuCode gateway: the engine
restarts once the running turn ends and resumes the conversation, and the
session's `gateway` flag in `session_list` follows.

Differences from `jucode serve`:

- A hosted engine keeps one session for its whole life. `/new` and
  `/resume <id>` are refused; use `session_create` and `session_open`.
  `/quit` closes the session.
- `set_attended` is refused. A session is attended while at least one
  connected client watches it: the first `watch` sets it attended, and the
  last `unwatch` or disconnect sets it unattended, which turns any pending
  `approval_request` into a deferred action.
- `decide_action` is recorded in `actions.jsonl` before the engine acts on
  it, so an approved action is never offered again after a restart.
