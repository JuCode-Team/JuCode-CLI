# `jucode daemon` Protocol

`jucode daemon` hosts many JuCode sessions in one long-running process.
Sessions keep running when every client disconnects. Clients (Desktop, the
remote web page) connect over a WebSocket and speak the `jucode serve`
protocol version 2 (`docs/serve-protocol.md`), with a `session` field that
says which hosted session an op or event belongs to.

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
| `sessions.jsonl` | Append log of sessions opened and closed, and their metadata (title, archive state, project...). A session recorded before sessions carried a project belongs to the one whose `path` is its `cwd`, if any, until it is moved. |
| `actions.jsonl` | Append log of deferred actions and their decisions. |
| `schedules.json` | Scheduled tasks (see "Scheduled tasks"). |
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

followed by the current `schedules`, `actions` and `requirements` lists.

A client that does not speak `protocol` 2 must disconnect.

## Replies

A frame may carry an `id`. Replies to daemon ops go only to the client that
sent the op and echo its `id`. Errors from any op are replied as
`{"type":"error","message":"...","id":...}`. Session events are sent to
every connected client.

## Daemon ops

| Op | Fields | Reply |
| --- | --- | --- |
| `session_list` | — | `sessions`: each with `session`, `cwd`, `chat`, `schedule` (the scheduled task it is a run of, or null), `open`, `watchers`, `title` (set with `session_meta`, else the engine's label), `archived`, `updated_at`, `engine`, `agent` (an ACP session's built-in agent, else null), `project` (the id of the project it belongs to, else null) |
| `gateway_catalog` | — | `gateway_catalog`: `models` (the JuCode models the user chose to show, `jucode_models` in config.json) and `groups` (the gateway's groups with their models and multipliers); empty when not signed in or offline. For clients that cannot read this machine's login (the remote page) |
| `tool_providers` | `tool`: `claude` or `codex` | `tool_providers` with `tool`, `config: {selected, providers}`. Provider rows expose `has_key`, never the key. Local desktop only |
| `tool_provider_save` | `tool`, `provider: {id, name, base_url, format, model, key}` | Updated `tool_providers`. An empty key preserves the saved key on edit. Claude requires `anthropic`; Codex requires `responses`. Local desktop only |
| `tool_provider_select` | `tool`, `provider_id` (empty restores the tool’s own configuration) | Updated `tool_providers`. Applies to new sessions (and to one leaving the JuCode gateway for the first time); a session keeps the provider it started on. Local desktop only |
| `tool_provider_remove` | `tool`, `provider_id` | Updated `tool_providers`; removing the selected provider clears the selection. Sessions on it no longer start until moved (see "Custom provider traffic and usage"). Local desktop only |
| `acp_presets` | — | `acp_presets` with `presets: [{id, name, status, path, version, min_version, approvals, gateway, install, tui}]`: the built-in ACP agents and whether each can start here (`status`: `ready`, `missing` or `unsupported`, older than `min_version`). Each program is asked its `--version` (10 s at most). See "ACP agents" |
| `restart_when_idle` | — | none. Desktop only: this daemon exits once no session is running (after a `daemon_restarting` broadcast), so the desktop can start a newer one without cutting off a task |
| `session_meta` | `session`, and any of `title`, `archived`, `hidden`, `group`, `project` | none; `project` moves the session into that project (an unknown id is an error) or, empty, out of any; every client receives the new `sessions` list. An empty title goes back to the engine's label; a hidden session leaves `session_list` and `session_history` (its conversation stays on disk). A session created here is titled after its first user message (first line, 40 characters) unless a client titled it first. After its 1st and 3rd turns, then every 5th, the title model (`title_model` in config.json, else the main `model`) renames it from the project name, the current title, the first and latest requests and the start of the latest reply. A title a client set with `session_meta` is never replaced |
| `session_history` | `cwd` | `session_history`: every session saved in `cwd`, newest first, whoever ran it (daemon, TUI, `jucode serve`), with `title`, `updated_at`, `entries`, `archived`, `schedule`, `open` |
| `session_create` | `cwd`, optional `engine` (`jucode`, default, `claude`, `codex` or `acp`), `options` and `project` | `session_created` with `session`; the session's startup events follow. See "Other engines". `project`: the project it belongs to, whatever its `cwd` (an unknown id is an error); `""` or null: none; left out: the project whose `path` is `cwd`, if any. A scheduled run is placed the same way; a session started with `start_session` belongs to its parent's project |
| `session_open` | `session`, optional `cwd`, `engine`, `options` | `session_opened`; with `cwd`, also opens a session saved there that the daemon never hosted. Reopens a closed session (or one from before a restart), resuming its transcript and its undecided deferred actions |
| `session_close` | `session` | none; every client receives `session_closed` once the engine has stopped |
| `watch` / `unwatch` | `session` | `watching` with `watching: true/false`; `watch` also sends this client a snapshot of the session: its state events (`startup`, `model_status`, `command_list`, `approval_mode`, `approval_mode_pending`, `mcp_servers`, `engine_caps`, `session_options`, `tool_provider`), a `transcript` of the conversation so far and `attended` |
| `actions_list` | — | `actions`: undecided deferred actions across all sessions, and in `closed` the ones closed in the last 7 days (each with `closed_by`, `closed_reason`, `closed_at`) |
| `pair_start` | — | `pairing` with `code` and `expires_at` (local clients only) |
| `device_list` | — | `devices`: paired, unrevoked devices (local clients only) |
| `device_revoke` | `device` | `device_revoked` (local clients only) |
| `relay_status` | — | `relay_status` with `enabled`, `connected`, `host` (the host id), `url` (null with `--no-relay`) (local clients only) |
| `relay_set` | `enabled` | `relay_status`; turns the relay connection on or off and remembers it (local clients only) |
| `mcp_set` / `mcp_remove` / `mcp_toggle` | as the session ops (`server`; `name`; `name`, `enabled`), with no `session` | `mcp_saved`; saves the change to `config.json` and sends the op to every open JuCode session, which answers with `mcp_servers`. Local clients only, with or without `session` |
| `pair_link` | — | `pair_link` with `link`, `code` and `expires_at`; an error while the relay is off (local clients only) |
| `ping` | — | `pong`. Clients behind the relay send it every minute so an idle stream is not closed |
| `workspaces` | — | `workspaces` with `rev` and `workspaces: [{id, name, is_default?, color?, icon?, projects: [{id, name, path, dirs?, chats?, worktree?, color?, icon?}]}]`. A project's `color` and `icon` are kept as the client sent them (`{kind, id | value | markup}`, as the clients' tab icons). `dirs`: the project's extra directories (absolute paths, not including `path`); remote clients may read them, and an engine started for a session of the project may work in the project's `path` and `dirs` that exist, besides its own `cwd`: Claude Code gets one `--add-dir` each, Codex has them as writable roots of its workspace-write sandbox (and `--add-dir` in its TUI), and a JuCode session's file tools and sandbox treat them as writable workspace (ACP agents get nothing) |
| `workspaces_set` | `rev`, `workspaces` | `workspaces`; replaces the list when `rev` is the current one, else an error (another client changed it). A project id may appear in one workspace only (sessions name their project by id). The sessions of a project a save removes (here or with `project_remove`) belong to none and are archived; every client receives the new `sessions` list. A project's `dirs`, when present, must be a list of absolute paths. Desktop imports its list with `rev: 0` into an empty daemon |
| `project_add` | `path`, optional `workspace`, `project_name`, `workspace_name` | `workspaces`; adds an existing directory. With no workspaces yet, one named `workspace_name` is created |
| `project_create` | `parent`, `name`, optional `git_init`, `workspace`, `workspace_name` | `workspaces`; makes the folder `parent/name` (optionally `git init`) and adds it |
| `project_remove` | `workspace`, `project` | `workspaces`; the files stay |
| `fs_list` | `path` (`~` is the home directory), optional `dirs_only` | `fs_list` with `path`, `git`, `entries: [{name, dir, size}]`, `truncated`. Git-ignored entries and `.git` are left out |
| `fs_read` | `path` | `fs_read` with `size`, `binary`, `text` (first 1 MiB), `truncated` |
| `fs_image` | `path` (a png/jpeg/gif/webp in a known directory or an upload, up to 16 MiB) | `fs_image` with `data`, a data URL: how the remote page shows a message's images |
| `git_status` | `path` | `git_status` with `repo`, `branch`, `files: [{path, status, from}]` (porcelain codes) |
| `git_diff` | `path`, optional `file` | `git_diff` with `diff` (unified, untracked files included, first 1 MiB), `truncated` |
| `session_usage` | `session` (its id) | `session_usage` with `totals`, `sessions` and `turns`: persisted and live usage, with settled gateway charges |
| `schedule_list` | — | `schedules`. See "Scheduled tasks" |
| `schedule_usage` | `schedule` (its id) | `schedule_usage` with `totals` and `sessions`: token counts and costs of the task's run sessions, including follow-ups |
| `schedule_save` | `schedule`: without `id` creates one (`name`, `prompt`, `cwd`, `repeat`, `time`, and `days` / `date` as `repeat` needs; optional `engine`, `mode`, `options`, `enabled`); with `id` changes the fields present. A paired device may only send `id` and `enabled` | `schedule_saved` with `schedule`; `next_run_at` is computed again. An enabled task needs a `cwd` that is a directory |
| `schedule_delete` | `schedule` (its id) | `schedule_deleted` with `schedule`; its runs' sessions stay |
| `schedule_run` | `schedule` (its id) | `schedule_started` with `schedule` and `session`; runs it now, enabled or not, without changing `next_run_at`. An error while its last run is still going |
| `item_close` | `kind` (`action`), `item` (its id), optional `reason` | `item_closed` with `item`; closes an open deferred action without deciding it, then broadcasts `actions` |
| `item_reopen` | `kind` (`action`), `item` | `item_reopened` with `item`; puts a closed action back among the open ones and takes its session out of the archive |
| `skills_catalog` | optional `backend` (`jucode`, default, or `claude`) | `skills_catalog` with `skills: [{id, name, description, tags, source, sourceName, isDefault, installed, license, redistributable, homepage}]`, `warnings` and `installDir` (local clients only). See "Skills" |
| `skill_install` | `source` (`jucode`, `anthropic` or a configured repository's `owner/repo`), `skill` (its `id` in the catalog), optional `backend` | `skill_installed` with `path` (local clients only) |

`decide_action` (a session op) also works for a session that is closed or
was hosted before a restart: the daemon reopens it first.

A hosted session with nothing going on has its engine closed: 30 minutes
after its last activity (an op from a client, a turn starting or ending,
background work ending), or sooner, least recently active first, while more
than 16 engines are open (never one active in the last 2 minutes). Nothing is
going on when no turn runs, no message waits, no background task, subagent or
approval is open and the conversation is not in its TUI. Clients get `session_closed` with `reason: "idle"`; the session
is not removed, and `session_open` resumes it when the user next sends to it.
A `session_open` without `options` opens it as it last ran (model, effort,
approval mode). A `user_message`, `command`, `continue` or `approve_plan` sent
to such a session opens it first, and the sender watches it again, so clients
that do not reopen it themselves keep working. Every client then gets
`session_reopened` with `session`, so one that put it to sleep can watch it
again. `session_closed` for any other close has no `reason`.

With `chat: true` instead of `cwd`, `session_create` makes the session
a chat: it runs in `~/.jucode/chats` with the chat prompt (conversation and
web research) and without project instructions or project skills. Any session
whose directory is `~/.jucode/chats` or lies inside it is a chat session, so
reopening one keeps it a chat.

Changes to workspaces are broadcast to every client as a `workspaces` frame.

`fs_*` and `git_*` read only inside known directories: projects and session
directories. `fs_list` with `dirs_only` may browse
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

Other GitHub skill repositories are listed only when the daemon's environment
sets `JUCODE_SKILL_SOURCES` (`Name=https://github.com/owner/repo`, separated
by `;`); none are listed by default. Their entries have `source`
`owner/repo` and `sourceName` the configured name; a repository is read at
its default branch's current commit and installs from that commit.
`redistributable` is true only for an open-source license (the skill's
frontmatter `license`, else the repository's). A repository that cannot be
read is one of `warnings` and is retried after five minutes.

`backend` picks the directory: `claude` installs into `~/.claude/skills`,
anything else into `~/.jucode/skills`. `installed` means
`<installDir>/<id>/SKILL.md` exists and, for a skill from a GitHub
repository, that it came from this entry's repository; installing a skill
whose name another repository's installed skill already has is an error. `skill_install` looks the skill up again
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
| `effort` | ACP: the thought-level value to switch to once the conversation opens (after `model`) |
| `jucode_gateway` | Claude Code / Codex / ACP agents with `gateway`: `true` runs this session through the JuCode gateway on the user's JuCode login; `false` on the session's custom provider (Claude Code / Codex, see `tool_provider`), or the provider in the user's own config. The endpoint and key go to this process only (Claude: `--settings` file; Codex: `-c` overrides and an env var; ACP: see "ACP agents"), never to the user's config files. Omitted on `session_open`: as the session last ran |
| `tool_provider` | Claude Code / Codex off the gateway: the custom provider (`tool_providers`) to run on, by id; `""` the tool's own configuration. Omitted: a new session takes the tool's selected provider, a reopened one the provider it last ran on. See "Custom provider traffic and usage" |
| `agent` | ACP: a built-in agent by id (`acp_presets`); the daemon finds and starts it. Excludes `command`, `args`, `bin` and `env` |
| `command`, `args` | ACP: the agent's command line (an agent the desktop registered) |
| `bin` | Claude Code / Codex: the engine binary to run instead of the one found on `PATH` |
| `env` | Extra environment variables for the engine process (plain names; no `DYLD_*`/`LD_*`) |

`engine: "acp"` runs an Agent Client Protocol agent: a built-in one named
by `options.agent`, or one the desktop registered, from `options.command`
(`jucode acp`, `gemini --experimental-acp`, ...). Any client may start a
built-in agent by id; only local clients may name `command`, `args`, `bin`
or `env` for any engine, since that names a program to run (`"only the
desktop can choose what an engine runs"`). Paired devices watch and drive
every session like any other. See "ACP agents".

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
- A message sent mid-turn waits in the daemon and starts the next turn when
  this one ends; `unqueue` takes it back. `steer` (`index`, negative from the
  last; every waiting message without one) writes it to Claude Code now,
  which takes it into the running turn at its next tool boundary. Until
  Claude Code takes it in it stays first in `pending_messages`, counted in
  `fixed`: those can no longer be taken back or steered.
- `decide_action`, `mcp_set`/`mcp_remove` and the jucode-only
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
| `context_usage` | What the context holds, by category (Claude Code's `/context`). JuCode sessions answer it too, counting with their own tokenizer; Codex sessions do not | `context_breakdown` with `total`, `max`, `categories: [{name, tokens, kind}]` (`kind`: `used`, `deferred` — tools loaded only when called —, `buffer` — kept free for auto-compaction — or `free`), `memory_files: [{path, tokens}]`; or `error` |
| `agent_runs` | The agent trace: every Workflow and Task subagent of the conversation, live ones and those Claude Code saved before this process | `agent_runs` (below) |
| `subagent_transcript` `agent_id` | One subagent's own conversation, read from the file Claude Code keeps for it | `subagent_transcript` with `agent_id` and `items` (`user`/`assistant`/`reasoning` with `content`; `tool` with `call_id`, `name`, `output`, `is_error`, `running`; the newest 600), or `error` |
| `permission_rules` | The rules in effect, as `/permissions` lists them | `permission_rules` with `rules: [{behavior, source, rule, editability}]`, `directories` |
| `approve` with `always_scope` | `project` saves the always-allow rule to the project's `.claude/settings.local.json`, `user` to `~/.claude/settings.json`; the session otherwise | — |

Codex sessions take the same `steer`, `/fast`, `/thinking`, `rename`,
`agent_runs`, `subagent_transcript`, `mcp_list` and `mcp_reconnect`, plus
`mcp_login` `name` (answered by `mcp_login` with the server's sign-in `url`)
and `/login` (a device code to sign Codex in to ChatGPT, as an `info`). A
message sent mid-turn waits in the daemon (`pending_messages`) and starts the
next turn; `steer` sends it into the running one (`turn/steer`): the
message at `index` (negative from the last), or all that wait without one;
`unqueue` takes one back. Approval
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

### ACP agents

Only the stable ACP schema is used (protocol v1); what it cannot express is
not offered. The built-in agents:

| `agent` | Runs | Approvals | Gateway |
| --- | --- | --- | --- |
| `opencode` | `opencode acp` (2.0.0 or later) | asks for edits and shell commands in `read-only` (and `plan`, `auto`, `manual`), for shell commands in `auto-edit`, through `OPENCODE_CONFIG_CONTENT`; `full-access` leaves the user's own OpenCode rules | no: it runs on the providers the user set up in OpenCode |
| `omp` | `omp acp --approval-mode always-ask` (`write` in `auto-edit`, `yolo` in `full-access`) | yes, as permission prompts and form elicitations | yes |
| `pi` | `npx -y pi-acp@0.0.34`, which runs `pi --mode rpc` (Pi 0.81.0 or later) | none: Pi never asks (`engine_caps.approvals` is `false`) | yes |

A program is found on `PATH`, the usual install directories, then
`~/.opencode/bin` (OpenCode) or `~/.bun/bin` (oh-my-pi), and runs with its
own directory first on `PATH`. A missing or too old one fails the start
(`"<name> is not installed: <install>"`, `"<name> <version> is too old
(needs <min>)"`); an unknown id is `"unknown ACP agent <id>"`.

On the gateway, oh-my-pi and Pi run on JuCode's own agent directory,
`~/.jucode/daemon/acp/<agent>` (`PI_CODING_AGENT_DIR`), written at each
start: the JuCode models (Claude models over Anthropic Messages, others over
Responses) with the session's local gateway key read from
`JUCODE_GATEWAY_TOKEN`. Their sessions stay in the user's own session
store: Pi's per-directory folder (`PI_CODING_AGENT_SESSION_DIR`), and for
oh-my-pi a `sessions` link to `~/.omp/agent/sessions` (it looks sessions up
only under its agent directory). User-level skills,
extensions and instructions in `~/.pi/agent` / `~/.omp/agent` are not
loaded there; project-level ones are.

The session id is the daemon's; the agent's own conversation id is kept
with the session. Reopening it uses `session/load` when the agent can (its
replay becomes one `transcript`), else `session/resume` (no replay), else
starts a new conversation and says so in an `info`. A session of a built-in
agent starts and reopens from any client that may open sessions; one the
desktop registered only from the desktop (`"this agent's session can only be
reopened on the desktop"`). Built-in agents also have their own TUI (`tui`
in `acp_presets`; see "A conversation in its TUI"), run scheduled tasks
(see "Scheduled tasks") and take requirements (`requirement_reply` with
`agent`); registered ones do none of these.

What the agent offers arrives as these state events (part of a watcher's
snapshot), sent at open and whenever they change:

```json
{"type":"engine_caps",
 "agent":{"id":"omp","name":"Oh My Pi","version":"18.1.21"},
 "approvals":true,
 "gateway":true,
 "caps":{"interrupt":true,"approvalModes":true,"modelPicker":true,"contextUsage":false,
         "slashCommands":true,"transcriptReplay":true}}
```

`agent.id` is the built-in agent's id (null for a registered one); `name`
and `version` come from the agent. `approvals`: whether the built-in agent
asks before running tools (null for a registered one). `gateway`: it can run
on the JuCode gateway. `caps`: `approvalModes` when its approval mode can be
switched, `modelPicker` when it has a model option, `contextUsage` once it
reported usage, `slashCommands` when it lists commands, `transcriptReplay`
when it can reopen a conversation with its history.

```json
{"type":"session_options","options":[
  {"id":"mode","name":"Session Mode","description":null,"category":"mode","type":"select","value":"build",
   "choices":[{"value":"build","name":"build","description":null},{"value":"plan","name":"plan","description":null}]},
  {"id":"x","name":"…","description":null,"category":null,"type":"boolean","value":true}]}
```

The agent's config options besides model and effort (those are in
`model_status`), or its legacy modes as one `mode` select when it has no
config options. `{"op":"set_session_option","session":"<id>","id":"mode","value":"plan"}`
(`value` a string or a boolean) changes one; answered by `session_options`
or an `error`.

The rest uses the usual events:

- `model_status` with `provider: "acp"`, `model` and `model_label` (the
  agent's model option value and its name, less a `provider/` prefix the
  value has too), `reasoning_effort` and `reasoning_efforts` (its
  thought-level option; the levels leave out a value that leaves the level
  to the agent, `default` or `auto`, which is `default_effort`, "" when
  none), `context_window` (the size it last reported). `/model` answers with `model_view` (a current
  value the agent does not list comes first with `listed: false`);
  `/model <value> [effort]` sets the options. Agents without a model option
  have neither.
- `context_usage` with `tokens` (and `cost` when the agent reports USD).
- `command_list` with the agent's commands; `/<name> args` sends that text
  as a prompt. Other commands are an `error`.
- `approval_mode` (the launched mode as `read-only`, `auto-edit` or
  `full-auto`) for built-in agents that ask. `set_approval_mode` to
  another mode restarts the agent after the running turn
  (`approval_mode_pending` meanwhile) on the same conversation; other ACP
  agents refuse it.
- `approval_request` for `session/request_permission` (summary: title and
  diffs), and named `mcp_elicitation` with `questions` for a form
  elicitation (a lone field without a title is asked by the message);
  answer with `approve` (`answers` for a form; `decision: "deny"` declines).
- Messages sent before the session opens or during a turn wait
  (`pending_messages`, `status: "queued: N"`) and are echoed as
  `user_message` once sent. Images go as base64 blocks when the agent takes
  them (files over 20 MB are not sent); otherwise an `info` says they were
  not sent.
- `steer` (`index`, negative from the last) cancels the turn the same way
  but keeps the waiting messages, the chosen one first: it runs as soon as
  the turn settles. `unqueue` takes a waiting message back.
- `interrupt` cancels open questions, drops waiting messages and sends
  `session/cancel`; the turn ends with `status: "interrupted"`.
- Agent notices are `info` (or `error`), a turn that ends with no reply
  an `info`. Replies outside a turn (startup banners) are dropped.

## Scheduled tasks

A scheduled task sends its prompt to a new session at set local times: in a
directory, on an engine and in a permission mode. The tasks are saved in
`~/.jucode/daemon/schedules.json`.

```json
{
  "id": "sch-1a2b3c4d",
  "name": "aicare 工单处理",
  "prompt": "处理新工单 …",
  "cwd": "/Users/me/aicare",
  "engine": "claude",
  "mode": "auto",
  "options": { "gateway": true },
  "enabled": true,
  "repeat": "weekly",
  "time": "11:00",
  "days": [1, 3, 5],
  "date": null,
  "created_at": 1790812800000,
  "next_run_at": 1790996400000,
  "last_run": { "at": 1790910000000, "session": "s…", "status": "done", "error": null },
  "skipped": 0
}
```

| Field | Meaning |
| --- | --- |
| `cwd` | Where each run starts (null until set: an enabled task needs one). The project's extra directories come along as for any session. |
| `engine` | `jucode`, `claude`, `codex` or `acp`. An `acp` task runs a built-in agent: `options.agent` names it (`"a scheduled ACP task needs a built-in agent"`, `"unknown ACP agent <id>"`), and its options name no `command`, `args`, `bin` or `env` (`"scheduled ACP tasks run built-in agents only"`). Its runs start in the task's mode (Pi never asks). |
| `mode` | The permission mode of its runs: `ask`, `edits`, `auto` or `all`, translated for the engine (`ask` is `manual` on jucode, `read-only` on Claude Code and Codex). |
| `options` | Start options as `session_create` takes them (`gateway`, `model`, …), saved by the desktop. |
| `repeat` | `once`, `hourly`, `daily`, `weekdays` (Monday to Friday) or `weekly`. A `once` task switches itself off when it fires. |
| `time` | Local `HH:MM`, 24-hour. `hourly` uses only the minute. |
| `days` | `weekly` only, not empty: 0 = Sunday … 6 = Saturday. |
| `date` | `once` only: local `YYYY-MM-DD`. |
| `next_run_at` | ms; null for a disabled task and for a `once` task whose time has passed. |
| `last_run` | The latest run: `status` `running`, `waiting` (an approval waits), `done` or `failed` (with `error`). |
| `skipped` | Times skipped in a row since the last run started. |

Times follow this machine's time zone, daylight saving included: a local
time skipped by a clock change runs an hour later, one that occurs twice
runs at the first. The scheduler checks every second. A due task's
`next_run_at` moves to the next time after now, so runs missed while the
daemon or the computer was off fire once when it starts again, not once per
missed time.

A run is a new session (its `schedule` is the task's id) named
`<name> · MM-DD HH:mm`, switched to the task's mode and sent its prompt.
Earlier runs of the task are archived then, unless they are running or have
a deferred action open. A time that comes while the last run is `running`
or `waiting` is skipped. A run going for more than 2 hours, working or
waiting, is interrupted and counts as failed; a deferred action it left
stays open. A run working when the daemon stopped counts as failed.

The user hears of a run when it fails (it could not start, its turn ended on
an error, or it timed out), when it waits for an approval, and when a time
is skipped (the first in a row): every client gets
`{"type":"schedule_event","schedule":"sch-…","name":"…","kind":"failed","detail":"…","session":"s…"}`
(`kind`: `failed`, `waiting` or `skipped`), and the paired phones a
notification that opens the session. Every client receives the full
`schedules` list after any change.

`schedule_delete` and `schedule_run` also take the task's id as `id` when
`schedule` is absent; that `id` is then the request id too, echoed in the
reply.

`schedule_usage` joins the task's run sessions with persisted turns in
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

A terminal runs on a pty of this computer and is one of two kinds:

- a session's own TUI (see "A conversation in its TUI"). It belongs to its
  session: clients (local ones and paired devices, over the LAN or the
  relay) are its viewers.
- the user's login shell, opened with `term_open` by any client, local or
  paired. It belongs to that client: the client is its only viewer, no
  other client can attach to it, and it is killed (SIGHUP, then SIGKILL)
  when the client disconnects, detaches, is detached or sends `term_close`.

No client names a program to run. `term` is the terminal's id; `data` is
base64 (standard alphabet) of raw bytes.

| Op | Reply | |
| --- | --- | --- |
| `{"op":"term_open","cwd":"/path","cols":80,"rows":24}` | `term_opened` with `term` | Opens a shell; the client is its viewer (nothing to replay). `cwd` must lie in a known directory, as for `fs_*` (`~` is the home directory). A client may have 4 shells open; a fifth `term_open` is an error. |
| `{"op":"term_attach","term":"t-…","cols":80,"rows":24}` | `term_attached` with `term`, `cols`, `rows` | Makes the client a viewer (again, when it already is). The reply comes first, then the latest output (at most 256 KiB) as `term_output` with `"replay":true`. The pty then takes the client's size; when that is its size already, it is changed and set back so the TUI repaints. `cols` and `rows` default to 80 × 24 and are kept within 2–1000. An unknown or ended terminal, or a shell: `"unknown terminal t-…"`. |
| `{"op":"term_detach","term":"t-…"}` | none | Stops this client's output; a TUI goes on, a shell is killed. |
| `{"op":"term_input","term":"t-…","data":"<base64>"}` | none | Written to the program in order. |
| `{"op":"term_resize","term":"t-…","cols":120,"rows":40}` | none | The last resize wins. |
| `{"op":"term_ack","term":"t-…","bytes":524288}` | none | `bytes`: the decoded bytes this client has written to its emulator for the terminal since it opened the terminal or last attached, replay included. Paired devices only (ignored from local clients); a value not above the last one is ignored. |
| `{"op":"term_close","term":"t-…"}` | none | Kills the TUI or shell (SIGHUP, then SIGKILL); every viewer gets `term_exit`, and a TUI's session engine resumes. |

Ops other than `term_attach` from a client that is not a viewer are errors
(`"not attached to terminal t-…"`). Viewers get:

- `{"type":"term_output","term":"t-…","data":"<base64>"}`: the program's
  output, sent after 10 ms without more output, at most 64 KiB before
  encoding per frame. A frame may end inside a UTF-8 character or an
  escape sequence. Replay frames carry `"replay":true`: a client resets its
  emulator before the first one and writes them like live output.
- `{"type":"term_exit","term":"t-…","code":3}`: the program has exited and
  been reaped; `code` is null when it was killed by a signal.
- `{"type":"term_detached","term":"t-…","reason":"slow"}`: the daemon
  detached the client (see below); it may attach to a TUI again, and a
  shell is killed.

A client that disconnects is detached; a TUI keeps running until it exits,
a viewer sends `term_close`, its session closes or the daemon stops. Its
shells are killed. The shell is `$SHELL -l` (else `/bin/zsh` on macOS,
`/bin/bash`, then `/bin/sh`), `%COMSPEC%` or PowerShell on Windows, in
`cwd`, with the daemon's environment plus `TERM=xterm-256color`,
`COLORTERM=truecolor` and `LANG=en_US.UTF-8` when `LANG` is unset.

Output to a paired device is flow-controlled end to end, so a fast program
cannot overflow the relay's queue for a slow phone: at most 512 KiB may be
sent to a device and not acknowledged. While a device's window is full the
daemon stops reading the pty (the program waits on its writes, as on a slow
terminal) and every viewer waits; a device whose window stays full for 15 s
gets `term_detached` with `reason: "slow"`, and output goes on for the
others. A device acknowledges once it has written 64 KiB more than it last
acknowledged, else 100 ms after its last unacknowledged write. Local clients
send no acknowledgements.

### A conversation in its TUI

`{"op":"session_tui","session":"<id>","cols":120,"rows":40}` moves a
claude, codex, jucode or built-in ACP agent conversation from the chat view
into the engine's own terminal interface, on a pty of the daemon. Any
client may send it; the daemon decides what runs. The reply is
`term_opened` with `term`, and the requesting client is the terminal's
first viewer (nothing to replay yet). It is refused while a turn runs unless
the op has `"force":true` (the user agreed to stop it: a jucode turn is
interrupted, another engine is killed and every client gets
`{"type":"status","message":"interrupted"}`), for an ACP agent the desktop
registered (`"a registered ACP agent has no TUI here"`), and for an ACP
conversation that has not opened yet.

The engine process stops first, background tasks with it (clients get an
empty `background_tasks`), and the TUI resumes the same conversation
(`claude --resume <id>`, `codex resume <id>`, `jucode --resume <id>`); a
conversation the engine has not saved yet starts there instead (`claude
--session-id <id>`, plain `codex`, whose new thread the daemon finds in the
directory when the TUI exits) with
the session's approval mode and model; under the gateway it gets its own
key. A built-in ACP agent's TUI resumes the agent's conversation (its ACP
session id is the TUI's too), in the session's approval mode, found and
started as its ACP agent is (same `PATH`; on the gateway the same agent
directory and session store):

| `agent` | TUI |
| --- | --- |
| `opencode` | `opencode --standalone --session <id>`, with `OPENCODE_CONFIG_CONTENT` as for ACP (`--standalone`: its own server, which sees that environment) |
| `omp` | `omp --approval-mode <always-ask\|write\|yolo> --resume <id>` |
| `pi` | `pi --session <id>` (Pi itself, not pi-acp) |

Every client of the session gets
`{"type":"surface","session":"<id>","surface":"tui","term":"t-…","client":1}`
(`client`: the one that opened it), also in a watcher's snapshot: that is
how other clients learn the terminal to attach to.
While the TUI runs, ops on the session other than `snapshot` are errors.

When the TUI exits (the user quits it, or `term_close`) the engine starts
again on the conversation, now including the turns typed in the TUI, and
the session publishes `{"type":"surface","session":"<id>","surface":"gui"}`
and a fresh `transcript` (an ACP agent's from its `session/load` replay).
Variables that would mark the TUI as a nested
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
| `{"op":"requirement_update","requirement":"R-1","text":"…","project":"p…","state":"done"}` | `requirement_saved` | Any of the fields. `project: null` leaves it unassigned. `state`: `idea`, `open`, `done`, `parked`. |
| `{"op":"requirement_delete","requirement":"R-1"}` | `requirement_deleted` | Its sessions stay. |
| `{"op":"requirement_link","requirement":"R-1","session":"s…"}` | `requirement_linked` | The session leaves any other requirement (and its gate); the requirement becomes `open`. |
| `{"op":"requirement_unlink","requirement":"R-1","session":"s…"}` | `requirement_unlinked` | A gate on that session ends. |
| `{"op":"requirement_image","requirement":"R-1","index":0}` | `requirement_image` with `data` (a data URL) | |
| `{"op":"requirement_begin","requirement":"R-1","session":"s…","plan":false,"mode":"edits","text":"…","lang":"zh"}` | `requirement_begun` with `requirement`, `session` | Starts work behind the gate (below): links the session, switches it to read-only and sends the requirement with the ask to only explain its understanding. `text`: the user's added words (optional). `lang`: `zh` (default) or `en`. |
| `{"op":"requirement_confirm","requirement":"R-1","text":"…","lang":"zh"}` | `requirement_confirmed` with `requirement`, `stage` (`plan` or `go`) | The user confirms the gate's current step; an error without a gate or while its session runs a turn. `text` (optional) goes with the next message. |
| `{"op":"requirement_reply","requirement":"R-1","text":"…","new_session":false,"cwd":"/path","engine":"claude","plan":false,"mode":"auto","lang":"zh"}` | `requirement_replied` with `session` | Sends `text` to its latest session (reopened if closed). With `new_session`, or no session yet, starts one in `cwd` (default: the latest session's, else the project's main directory; neither is an error), belonging to `project` (default: the requirement's), on `engine` (default: the latest session's, else jucode; `acp` needs `agent`, a built-in agent's id, which defaults to the latest session's when `engine` is left out: `"an ACP session needs options.agent"`) and runs `requirement_begin` on it with `plan` (default `false`), `mode` (default `auto`) and `text`. |

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

`requirements` is also sent on connect and broadcast on every change:

```json
{"type":"requirements","requirements":[{
  "id":"R-1","text":"…","title":"…","title_auto":false,"images":["/abs/1.png"],
  "project":"p…","state":"open","sessions":["s…"],
  "progress":{"goal":"…","decided":[],"done":[],"doing":[],"blocked":[],"next":[],"files":[],"note":"…"},
  "progress_at":0,"source":"desktop","source_session":null,
  "gate":{"session":"s…","stage":"understand","plan":false,"mode":"auto"},
  "status":"confirm","session_states":{"s…":"idle"},"last_reply":"…",
  "created_at":0,"updated_at":0}]}
```

Words longer than 40 characters get a title from the title model
(`title_auto`). `source` is `desktop`, `phone` or `session` (`agent` on
ones long-lived agents noted). `status` is `state`, except while `open`:
`approval` (a session waits for an approval), `confirm` (the gate's session
is idle: the user confirms its understanding or plan), `failed` (the latest
session's turn failed), `running`, else `review` (the user's turn: the work
is done or the agent asks something), or `open` with no session.

`session_states`: `running`, `waiting`, `failed`, `idle`. `last_reply` (the
end of the latest session's reply) comes with `review`, `confirm` and
`failed`.

Requirements saved before projects had ids carry `projects: ["/path"]`;
once the workspaces have projects, the daemon replaces it with the `project`
whose main directory is the first path (or `null`). A requirement a
long-lived agent proposed (`state: "proposed"`, `proposal`) becomes an
`idea` without the proposal.

## Notifications

A paired device's browser registers its Web Push subscription with
`{"op":"push_subscribe","subscription":{"endpoint":"…","keys":{"p256dh":"…","auth":"…"}}}`,
the JuCode Android app its 个推 (Getui) client id with
`{"op":"push_subscribe","subscription":{"provider":"getui","client_id":"…"}}`
(only paired devices; dropped when the device is revoked) and removes it with
`{"op":"push_unsubscribe","endpoint":"…"}` or `{"op":"push_unsubscribe","client_id":"…"}`.
A client id registered again by another device moves to that device. The daemon notifies when an
open requirement turns to `review`, `confirm` or `approval`, and for
scheduled runs as described in "Scheduled tasks", through the relay (relay-protocol.md, Web Push). `{"op":"push_test"}` (paired devices
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
access (a built-in ACP agent into another mode) only by restarting; switched while a turn runs, they send
`{"type":"approval_mode_pending","mode":"…"}` and apply it when the turn
ends (`mode: null` then; a client can `interrupt` to apply it sooner). A
switch to full access answers the turn's open and later approvals at once.

`set_gateway` (`gateway`: bool, optional `model`) moves a Claude Code, Codex
or gateway-capable ACP agent session between this machine's own login and the JuCode gateway: the engine
restarts once the running turn ends and resumes the conversation, and the
session's `gateway` flag in `session_list` follows.

`set_session_option` (`id`, `value`) changes an ACP agent's setting from
`session_options` (see "ACP agents").

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


## Custom provider traffic and usage

Claude Code and Codex own separate provider lists and defaults, stored in the
daemon’s owner-only settings file. Their API keys stay in the daemon. Only a
per-process local key reaches the tool; the tool’s original configuration stays
intact. A JuCode gateway session (`jucode_gateway: true`) continues to use the
JuCode account instead of the custom default.

JuCode CLI keeps its own existing provider configuration. Hosted API-key
sessions using `anthropic`, `responses` or `chat` also route through the local
gateway. OAuth sessions retain their existing transport. No cross-protocol
conversion is performed.

A Claude Code or Codex session keeps the provider it started on: changing the
default affects new sessions only, and a reopened or restarted session (a mode
or gateway switch) stays on its own, because one upstream rejects another's
conversation state (Claude's thinking signatures). Sessions that ran on the
tool's own configuration stay there. If a session's provider was removed, it
does not start: `session_open` answers an `error` naming the provider, and the
client can reopen it with `jucode_gateway: true` or with `tool_provider: ""`
(the tool's own configuration).

Every Claude Code, Codex and ACP session sends, right after its engine starts
(and again after a restart), the state event

```json
{"type":"tool_provider","session":"…","provider":{"id":"…","name":"…","model":"…"}}
```

naming the custom provider it runs on, or `"provider":null` (the JuCode
gateway or the tool's own configuration). It is part of a watcher's snapshot.
It never carries the key or the URL. A Codex session on a custom provider
reports `jucode_custom` as its `model_status.provider` (`jucode_gateway` is
the JuCode gateway) and has no ChatGPT plan usage. A Claude Code session on
a custom provider sends its background and subagent requests to the
provider's model too.

A JuCode session's request goes on to exactly the URL, with the same
credential header, that its own client would have used without the local
gateway.

For custom traffic the local gateway reads JSON or SSE usage while forwarding
the original bytes. It records input, cache-read, cache-write, output and
reasoning tokens under `channel_kind: local`, namespaced as `tool:provider`.
Requests in one turn are aggregated, including those still in flight when the
tool reports ready. The tool’s duplicate usage is excluded. Custom rates remain
unknown and do not count as JuCode gateway charges. As with existing local usage,
records sync to the signed-in JuCode account without keys or project paths.
