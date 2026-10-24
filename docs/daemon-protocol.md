# `jucode daemon` Protocol

`jucode daemon` hosts many JuCode sessions in one long-running process.
Sessions keep running when every client disconnects. Desktop connects over
a local WebSocket, phones and browsers through the JuCode relay; all speak the `jucode serve`
protocol version 2 (`docs/serve-protocol.md`), with a `session` field that
says which hosted session an op or event belongs to.

## Running

```sh
jucode daemon                      # ws://127.0.0.1:7788
jucode daemon --listen 127.0.0.1:9000
jucode daemon --relay ws://127.0.0.1:8080/v1/relay  # another relay (development)
jucode daemon --no-relay           # never connect to a relay
```

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
| `token` | Local client token, created on first start, mode 0600. |
| `sessions.jsonl` | Append log of sessions opened and closed, and their metadata (title, archive state, project...). A session recorded before sessions carried a project belongs to the one whose `path` is its `cwd`, if any, until it is moved. |
| `actions.jsonl` | Append log of deferred actions and their decisions. |
| `schedules.json` | Scheduled tasks (see "Scheduled tasks"). |
| `devices.jsonl` | Devices paired over the relay (a hash of each client's Noise static key, its OAuth authorization, client id and user) and revocations. |
| `settings.json` | Daemon settings: `relay` (false: remote access turned off; missing: on), `relay_user` (the JuCode account the computer is registered under). |
| `workspaces.json` | Workspaces and their projects, with a save counter `rev`. |
| `relay-identity.json` | Relay keys (Ed25519 identity, X25519 Noise static key), mode 0600. |

## Plain HTTP

The port serves only the WebSocket (and the local gateway). Any other plain
HTTP request gets `404`.

## Remote access

Remote access belongs to the JuCode account: the daemon reaches the relay
(`--relay`, default `wss://api.jucode.net/v1/relay`) with this computer's
JuCode login, and only clients signed in to the same account reach it, over
end-to-end encrypted streams (`docs/relay-protocol.md`). It is on whenever
the computer is signed in, unless a local client turns it off with
`relay_set` (`enabled: false`; remembered across restarts). Signed out, the
daemon does not connect, and signing out takes it off the relay.
`--no-relay` keeps it off whatever the setting says.

The relay registers the computer under the account whose token it presented
and names that account in `ready.user`, kept as `relay_user`. When it
differs from the one before (the computer signed in to another account),
every paired device is revoked before any stream is served.

A client device is paired once, in one of two ways, and later connects
without either:

1. **Pairing link.** A local client sends `pair_link` and shows the returned
   `link` (`https://app.jucode.net/remote#pair=<host>.<key>.<code>`) as a QR
   code. The client connects through the relay with the code in its first
   Noise message.
2. **Connect request.** A client of the account that lists this computer
   connects with `request: true` in its first Noise message. With no local
   client connected the daemon refuses at once (`no_desktop`). Otherwise
   local clients receive
   `{"type":"connect_request","id":"cr-…","name":"<device name>","client":"jucode-mobile","at":<ms>}`
   and answer with
   `{"op":"connect_decide","request":"cr-…","allow":true}`, replied to with
   `{"type":"connect_decided","request":"cr-…","allow":true}`. Every local
   client then receives
   `{"type":"connect_request_closed","id":"cr-…","outcome":"allowed"}`
   (`outcome`: `allowed`, `denied`, `expired` after 120 s, or `cancelled`
   when the client went away first). `connect_requests` lists the open ones
   for a desktop that connects later. At most 3 wait at once.

Either way the device is recorded keyed by its Noise static key, with the
client's OAuth authorization, client id and user; it shows up in
`device_list`, and `device_revoke` closes its streams. A relay stream then
serves the device like a local client, except for the ops only local
clients may send: `pair_link`, `connect_requests`, `connect_decide`,
`device_list`, `device_revoke`, `relay_status`, `relay_set`,
`restart_when_idle`, `usage_import_legacy` and the others marked so below.

## Connecting

Connect to `ws://<listen>/?token=<token>` (or send
`Authorization: Bearer <token>`). A missing or wrong token fails the
handshake with HTTP 401. Local clients read the daemon token from
`~/.jucode/daemon/token`; it is the only token the local WebSocket accepts.

Each WebSocket text message is one JSON frame. The daemon first sends:

```json
{"type":"hello","protocol":2,"version":"0.3.0"}
{"type":"sessions","sessions":[{"session":"...","cwd":"...","created_at":0,"updated_at":0,"title":"...","archived":false,"open":true,"watchers":0}]}
{"type":"workspaces","rev":1,"workspaces":[...]}
```

followed by the current `schedules` and `actions` lists.

A client that does not speak `protocol` 2 must disconnect.

## Replies

A frame may carry an `id`. Replies to daemon ops go only to the client that
sent the op and echo its `id`. Errors from any op are replied as
`{"type":"error","message":"...","id":...}`. Session events are sent to
every connected client.

## Daemon ops

| Op | Fields | Reply |
| --- | --- | --- |
| `session_list` | — | `sessions`: each with `session`, `cwd`, `chat`, `schedule` (the scheduled task it is a run of, or null), `open`, `watchers`, `title` (set with `session_meta`, else the engine's label), `archived`, `updated_at`, `engine`, `agent` (an ACP session's built-in agent, else null), `project` (the id of the project it belongs to, else null), `busy` (running a turn or with messages waiting), `background` (background work its turn left going — background commands, agents, workflows — runs: not a turn, so not `busy`; only agents and workflows hold a scheduled run), `parent` (the session that started it with `start_session`, else null) |
| `gateway_catalog` | — | `gateway_catalog`: `models` (the JuCode models the user chose to show, `jucode_models` in config.json) and `groups` (the gateway's groups with their models and multipliers); empty when not signed in or offline. For clients that cannot read this machine's login (the remote page) |
| `tool_providers` | `tool`: `claude` or `codex` | `tool_providers` with `tool`, `config: {selected, providers}`. Provider rows expose `has_key`, never the key. Local desktop only |
| `tool_provider_save` | `tool`, `provider: {id, name, base_url, format, model, key}` | Updated `tool_providers`. An empty key preserves the saved key on edit. Claude requires `anthropic`; Codex requires `responses`. Local desktop only |
| `tool_provider_select` | `tool`, `provider_id` (empty restores the tool’s own configuration) | Updated `tool_providers`. Applies to new sessions (and to one leaving the JuCode gateway for the first time); a session keeps the provider it started on. Local desktop only |
| `tool_provider_remove` | `tool`, `provider_id` | Updated `tool_providers`; removing the selected provider clears the selection. Sessions on it no longer start until moved (see "Custom provider traffic and usage"). Local desktop only |
| `acp_presets` | — | `acp_presets` with `presets: [{id, name, status, path, version, min_version, approvals, gateway, install, tui}]`: the built-in ACP agents and whether each can start here (`status`: `ready`, `missing` or `unsupported`, older than `min_version`). Each program is asked its `--version` (10 s at most). See "ACP agents" |
| `engine_probe` | `engine` (`claude`, `codex`, `acp`), `options` (as `session_create`'s), `cwd`? (home) | `engine_probed` with `events`: the last `model_status`, `model_view`, `engine_caps`, `command_list` and `session_options` the engine sent, from a run of it that no list shows, asked for its models and then stopped (25 s at most). What a client shows of an engine before any session of it. ACP agents do not sign in from a probe. Desktop only for options that run programs |
| `restart_when_idle` | — | none. Desktop only: this daemon exits once no session is running (after a `daemon_restarting` broadcast), so the desktop can start a newer one without cutting off a task |
| `session_meta` | `session`, and any of `title`, `archived`, `hidden`, `group`, `project` | none; `project` moves the session into that project (an unknown id is an error) or, empty, out of any; every client receives the new `sessions` list. An empty title goes back to the engine's label; a hidden session leaves `session_list` and `session_history` (its conversation stays on disk). A session created here is titled after its first user message (first line, 40 characters) unless a client titled it first. After its 1st and 3rd turns, then every 5th, the title model (`title_model` in config.json, else the main `model`) renames it from the project name, the current title, the first and latest requests and the start of the latest reply. A title a client set with `session_meta` is never replaced |
| `session_history` | `cwd` | `session_history`: every session saved in `cwd`, newest first, whoever ran it (daemon, TUI, `jucode serve`), with `title`, `updated_at`, `entries`, `archived`, `schedule`, `open` |
| `session_create` | `cwd`, optional `engine` (`jucode`, default, `claude`, `codex` or `acp`), `options` and `project` | `session_created` with `session`; the session's startup events follow. See "Other engines". `project`: the project it belongs to, whatever its `cwd` (an unknown id is an error); `""` or null: none; left out: the project whose `path` is `cwd`, if any. A scheduled run is placed the same way; a session started with `start_session` belongs to its parent's project |
| `session_open` | `session`, optional `cwd`, `engine`, `options` | `session_opened`; with `cwd`, also opens a session saved there that the daemon never hosted. Reopens a closed session (or one from before a restart), resuming its transcript and its undecided deferred actions |
| `session_close` | `session` | none; every client receives `session_closed` once the engine has stopped |
| `watch` / `unwatch` | `session` | `watching` with `watching: true/false`; `watch` also sends this client a snapshot of the session: its state events (`startup`, `model_status`, `command_list`, `approval_mode`, `approval_mode_pending`, `mcp_servers`, `engine_caps`, `session_options`, `tool_provider`), a `transcript` of the conversation so far and `attended` |
| `actions_list` | — | `actions`: undecided deferred actions across all sessions, and in `closed` the ones closed in the last 7 days (each with `closed_by`, `closed_reason`, `closed_at`) |
| `device_list` | — | `devices`: paired, unrevoked devices, each with `id`, `name`, `paired_at`, `client` (the OAuth client id: `jucode-mobile`, `jucode-web`, `jucode-cli`), `authz` and `user` (local clients only) |
| `device_revoke` | `device` | `device_revoked` (local clients only) |
| `relay_status` | — | `relay_status` with `enabled`, `connected`, `signed_in` (this computer is signed in to JuCode with a login the relay has not refused), `host` (the host id), `url` (null with `--no-relay`), `user` (the account the computer is registered under, null until the relay first registered it) (local clients only) |
| `relay_set` | `enabled` | `relay_status`; turns remote access on or off and remembers it (local clients only) |
| `connect_requests` | — | `connect_requests` with `requests: [{id, name, client, at}]`, the open connect requests (local clients only; see "Remote access") |
| `connect_decide` | `request`, `allow` | `connect_decided` with `request` and `allow`; an error when the request is no longer open (local clients only) |
| `mcp_set` / `mcp_remove` / `mcp_toggle` | as the session ops (`server`; `name`; `name`, `enabled`), with no `session` | `mcp_saved`; saves the change to `config.json` and sends the op to every open JuCode session, which answers with `mcp_servers`. Local clients only, with or without `session` |
| `pair_link` | — | `pair_link` with `link` (`https://app.jucode.net/remote#pair=<host>.<static key>.<code>`), `code` (8 characters, single use) and `expires_at` (5 minutes); an error while remote access is off (local clients only) |
| `ping` | — | `pong`. Clients behind the relay send it every minute so an idle stream is not closed |
| `workspaces` | — | `workspaces` with `rev` and `workspaces: [{id, name, is_default?, color?, icon?, projects: [{id, name, path, dirs?, chats?, worktree?, color?, icon?}]}]`. A project's `color` and `icon` are kept as the client sent them (`{kind, id | value | markup}`, as the clients' tab icons). `dirs`: the project's extra directories (absolute paths, not including `path`); remote clients may read them, and an engine started for a session of the project may work in the project's `path` and `dirs` that exist, besides its own `cwd`: Claude Code gets one `--add-dir` each, Codex has them as writable roots of its workspace-write sandbox (and `--add-dir` in its TUI), and a JuCode session's file tools and sandbox treat them as writable workspace (ACP agents get nothing) |
| `workspaces_set` | `rev`, `workspaces` | `workspaces`; replaces the list when `rev` is the current one, else an error (another client changed it). A project id may appear in one workspace only (sessions name their project by id). The sessions of a project a save removes (here or with `project_remove`) belong to none and are archived; every client receives the new `sessions` list. A project's `dirs`, when present, must be a list of absolute paths. Desktop imports its list with `rev: 0` into an empty daemon |
| `project_add` | `path`, optional `workspace`, `project_name`, `workspace_name` | `workspaces`; adds an existing directory. With no workspaces yet, one named `workspace_name` is created |
| `project_create` | `parent`, `name`, optional `git_init`, `workspace`, `workspace_name` | `workspaces`; makes the folder `parent/name` (optionally `git init`) and adds it |
| `project_remove` | `workspace`, `project` | `workspaces`; the files stay |
| `fs_list` | `path` (`~` is the home directory), optional `dirs_only` | `fs_list` with `path`, `git`, `entries: [{name, dir, size}]`, `truncated`. Git-ignored entries and `.git` are left out |
| `fs_read` | `path` | `fs_read` with `size`, `binary`, `text` (first 1 MiB), `truncated` |
| `fs_image` | `path` (a png/jpeg/gif/webp image or an mp4/m4v/webm/mov video in a known directory or an upload, up to 16 MiB) | `fs_image` with `data`, a data URL: how the remote page shows a message's images and the images and videos an agent presented |
| `git_status` | `path` | `git_status` with `repo`, `branch`, `files: [{path, status, from}]` (porcelain codes) |
| `git_diff` | `path`, optional `file` | `git_diff` with `diff` (unified, untracked files included, first 1 MiB), `truncated` |
| `session_usage` | `session` (its id) | `session_usage` with `totals`, `sessions` and `turns`: persisted and live usage, with settled gateway charges |
| `schedule_list` | — | `schedules`. See "Scheduled tasks" |
| `schedule_usage` | `schedule` (its id) | `schedule_usage` with `totals` and `sessions`: token counts and costs of the task's run sessions, including follow-ups |
| `schedule_save` | `schedule`: without `id` creates one (`name`, `prompt`, `cwd`, `repeat`, `time`, and `days` / `date` as `repeat` needs; optional `engine`, `mode`, `options`, `enabled`, `worktree`, `budget`); with `id` changes the fields present. A paired device may only send `id` and `enabled` | `schedule_saved` with `schedule`; `next_run_at` is computed again. An enabled task needs a `cwd` that is a directory |
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
  `fixed`: those can no longer be taken back or steered. `interrupt` with
  `index` moves that held message to the front and stops the turn, so it
  starts the next one. `unqueue` with `discard: true` drops the message
  instead of sending it back (`unqueued`), and `queue_move` (`index` to
  `to`, both in `pending_messages`) reorders the waiting ones; neither
  touches those counted in `fixed`.
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
`unqueue` takes one back (drops it with `discard: true`), and `queue_move`
(`index` to `to`) reorders them. Approval
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

Every ACP agent is given the user's enabled MCP servers (JuCode's, not those
signing in by OAuth) in `session/new`, `session/load` and `session/resume`:
stdio ones always, http ones when its `mcpCapabilities.http` says it takes
them. An agent that answers `session/new` with "authentication required"
(`-32000`) is signed in once its own way (`authenticate` with its first
method that needs no terminal or environment variable; it may open the
browser), then the session is asked again; still not signed in, the error
says how (`"[acp] <name> needs sign-in: <how>"`). Its models come from its
`model` config option, else from the `models` it reports
(`availableModels`, `currentModelId`), switched with `session/set_model`.

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
in `acp_presets`; see "A conversation in its TUI") and run scheduled tasks
(see "Scheduled tasks"); registered ones do neither.

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
  the turn settles. `unqueue` takes a waiting message back (drops it with
  `discard: true`); `queue_move` (`index` to `to`) reorders them.
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
  "skipped": 0,
  "worktree": false,
  "budget": null,
  "failures": 0,
  "history": []
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
| `last_run` | The latest run: `status` `running`, `waiting` (an approval waits), `background` (its turn ended; background work it left going runs), `done` or `failed` (with `error`). |
| `skipped` | Times skipped in a row since the last run started. |
| `worktree` | Each run starts in a new git worktree of `cwd`'s repository (see below). |
| `budget` | What one run may cost, with the sessions it started (`start_session`), or null: gateway points for runs through the JuCode gateway, estimated US dollars for others (whichever passes it first). Clear it with `null`; it must be above 0. |
| `failures` | Runs failed in a row while the task was on; a run that ends well, or switching the task on, sets it back to 0. |
| `history` | The runs before `last_run`, newest first, up to 20, shaped as `last_run`. |

Times follow this machine's time zone, daylight saving included: a local
time skipped by a clock change runs an hour later, one that occurs twice
runs at the first. The scheduler checks every second. A due task's
`next_run_at` moves to the next time after now, so runs missed while the
daemon or the computer was off fire once when it starts again, not once per
missed time.

A run whose turn ends while work it left going will come back to it —
background agents or workflows, or a session it started still working — is
`background` until that work ends: it still keeps the computer awake,
counts against its budget and timeout, and holds the next time off. A
background command (a dev server) never comes back, so it holds no run.
A run stopped for its budget or its time has its turn interrupted and, on
Claude Code, its background tasks stopped. A turn
the engine starts by itself (Claude Code reporting on finished background
work) makes it `running` again, and its end settles it again. A turn is
one from a message, or one the engine starts with output of its own (not a
subagent's: Claude Code's subagent events carry `nested: true`).

Once a run's turn has ended (`background` or `done`), a message the user
sends in its session is theirs: the run is `done` (`followed_up` in
`last_run`), and no timeout, budget or alert applies there.

A run is a new session (its `schedule` is the task's id) named
`<name> · MM-DD HH:mm`, switched to the task's mode and sent its prompt.
Earlier runs of the task are archived then, unless they are running or have
a deferred action open. A time that comes while the last run is `running`
or `waiting` is skipped. A run going for more than 2 hours, working or
waiting, is interrupted and counts as failed; a deferred action it left
stays open. A run working when the daemon stopped counts as failed. After
3 failures in a row the task switches itself off.

A run of a `worktree` task starts in
`<repo parent>/.jucode-worktrees/<repo name>/<task id>-MMDD-HHmm` (a free
name), on a new branch `jucode/<that name>` from the branch the repository
has checked out (its commit when detached), and works in `cwd`'s place
inside it; the session keeps `cwd`'s project and lists `worktree`
(`{repo, path, branch, base, slug}`, plus `gone: true` once removed) in
`session_list`. A `cwd` outside a
repository fails the run. When the run is archived, its worktree and branch
are removed if nothing was left in them: no uncommitted or untracked files
and no commit past `base`; while its engine is still open, when the
engine closes. The
project's main directory is not one of the session's extra directories (nor
of any session working in a worktree of it).

Once a minute the daemon checks the costs of runs with a `budget`, as
`schedule_usage` reads them (gateway charges arrive from the account's
ledger a little late), and interrupts a run that has passed it; it fails.

While a run is `running` the daemon keeps the computer from idle sleep
(macOS `caffeinate -i`, Linux `systemd-inhibit --what=idle:sleep`, Windows
`SetThreadExecutionState`). One hold covers every feature that needs one.
On macOS and Windows closing the lid or choosing Sleep still sleeps; on
Linux the inhibitor also holds off a suspend the user asks for, unless it
is forced (`systemctl suspend -i`).

The user hears of a run when it fails the first time in a row (it could not
start, its turn ended on an error, it timed out or passed its budget), when
the task switches itself off, when it waits for an approval, and when a time
is skipped (the first in a row): every client gets
`{"type":"schedule_event","schedule":"sch-…","name":"…","kind":"failed","detail":"…","session":"s…"}`
(`kind`: `failed`, `stopped`, `waiting` or `skipped`), and the account's
phones and browsers a notification that opens the session (see
"Notifications"). Every client receives the full
`schedules` list after any change.

`schedule_delete` and `schedule_run` also take the task's id as `id` when
`schedule` is absent; that `id` is then the request id too, echoed in the
reply.

`schedule_usage` joins the task's run sessions, and the sessions they started
(rows with `parent`), with persisted turns in
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

A `transcript` item is `user`/`assistant`/`reasoning` with `content`, `tool`
with `name` and `output`, `branch` or `plan`. Items carry `at` (ms) when the
engine knows when they were written: Claude's saved rows, jucode's session
entries and the daemon's own snapshot do; a resumed Codex thread and an ACP
replay do not. A `tool` item carries `is_error: true` when the call failed,
its `call_id` when the replay knows it, and `subagent` when it was a
subagent's call (Claude), which the desktop keeps out of the conversation.
A Claude replay has every tool call the turn showed, not only its text.

A fork is a `session_create` whose `options` carry `fork_from` (the
engine's id of the conversation it forks): the new session goes on from it
while the original stays as it is. Claude Code resumes it at `resume_at`
(the uuid of the reply it forks after, which Claude replays carry as
`uuid`) as a new conversation (`--fork-session`); Codex forks the thread
through the turn before its `fork_turns`-th user turn ends (`thread/fork`
with `lastTurnId`); jucode copies the session's first `fork_turns` user
turns into a new session (read from `fork_cwd`'s sessions when the fork
works in another directory, its own worktree; Claude Code and Codex find
the conversation by id from any directory). An ACP agent's conversation cannot fork. The
session list names the source as `forked_from` (`session`, `turns`, `at`);
a Claude fork closed before its first turn is forked again when reopened.

## Terminals

A terminal runs on a pty of this computer and is one of two kinds:

- a session's own TUI (see "A conversation in its TUI"). It belongs to its
  session: clients (local ones and paired devices over the relay) are its
  viewers.
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

## Notifications

Clients register for notifications with the JuCode backend
(`POST /v1/oauth/push/register`), not with the daemon. The daemon notifies
for scheduled runs as described in "Scheduled tasks" by posting
`{"title","body","host","session"}` to `POST /v1/oauth/notify` with this
computer's JuCode token; the backend sends it to every push registration of
the account except the daemon's own. `host` is this computer's host id,
`session` the session a tap opens. Only while signed in and with remote
access on; failures are logged.

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
