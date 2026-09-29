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
{"type":"sessions","sessions":[{"session":"...","cwd":"...","created_at":0,"open":true,"watchers":0}]}
```

followed by the current `agents`, `questions` and `actions` lists.

A client that does not speak `protocol` 2 must disconnect.

## Replies

A frame may carry an `id`. Replies to daemon ops go only to the client that
sent the op and echo its `id`. Errors from any op are replied as
`{"type":"error","message":"...","id":...}`. Session events are sent to
every connected client.

## Daemon ops

| Op | Fields | Reply |
| --- | --- | --- |
| `session_list` | — | `sessions`: each with `session`, `cwd`, `chat`, `agent`, `open`, `watchers` |
| `session_create` | `cwd` | `session_created` with `session`; the session's startup events follow |
| `session_open` | `session` | `session_opened`; reopens a closed session (or one from before a restart), resuming its transcript and its undecided deferred actions |
| `session_close` | `session` | none; every client receives `session_closed` once the engine has stopped |
| `watch` / `unwatch` | `session` | `watching` with `watching: true/false`; `watch` also sends this client a snapshot of the session: its state events (`startup`, `model_status`, `command_list`, `approval_mode`, `mcp_servers`), a `transcript` of the conversation so far and `attended` |
| `actions_list` | — | `actions`: undecided deferred actions across all sessions |
| `pair_start` | — | `pairing` with `code` and `expires_at` (local clients only) |
| `device_list` | — | `devices`: paired, unrevoked devices (local clients only) |
| `device_revoke` | `device` | `device_revoked` (local clients only) |
| `relay_status` | — | `relay_status` with `enabled`, `connected`, `host` (the host id), `url` (null with `--no-relay`) (local clients only) |
| `relay_set` | `enabled` | `relay_status`; turns the relay connection on or off and remembers it (local clients only) |
| `pair_link` | — | `pair_link` with `link`, `code` and `expires_at`; an error while the relay is off (local clients only) |
| `agent_list` | — | `agents` |
| `agent_create` | `agent` (the new agent's id), `name`, `cwd`, `role` | `agent_created`; every client also receives the new `agents` list |
| `message_send` | `agent`, `body`, optional `session`, `reply_to`, `dedupe_key` | `message_accepted` with `message` and `duplicate` |
| `timer_list` | — | `timers`: active timers of all agents |
| `agent_get` | `agent` | `agent`: settings, `brief` (the four files), `memory` file names and the agent's `sessions` |
| `agent_update` | `agent`, optional `name`, `enabled`, `approval_mode` | `agent_updated`; every client also receives the new `agents` list |
| `question_list` | — | `questions`: unanswered questions |
| `question_answer` | `question`, `answer` | `question_answered`; the answer is delivered to the session that asked |
| `report_list` | optional `limit` (50) | `reports`, newest first, with `read` |
| `report_read` | `report` | `report_read` |

`decide_action` (a session op) also works for a session that is closed or
was hosted before a restart: the daemon reopens it first.

`session_create` also accepts `agent` instead of `cwd`: the session runs in
the agent's directory as that agent. With `chat: true` instead, the session is
a chat: it runs in `~/.jucode/chats` with the chat prompt (conversation and
web research) and without project instructions or project skills. Any session
whose directory is `~/.jucode/chats` or lies inside it is a chat session, so
reopening one keeps it a chat.

## Agents

A long-lived agent is a directory `~/.jucode/agents/<id>/`: its brief
(`role.md`, `capabilities.md`, `policy.md`, `state.md`), `memory/<topic>.md`
notes and `agent.json`:

| Field | Default | Meaning |
| --- | --- | --- |
| `name`, `cwd`, `enabled` | | Display name, working directory, whether it takes messages. |
| `approval_mode` | `auto` | `manual`, `auto-edit`, `auto` or `full-access`. |
| `sandbox` | `workspace-write` (`full-access` on Windows) | Where its shell commands run; see below. |
| `network` | `true` | Whether sandboxed commands may connect out. |
| `directories` | `[]` | `[{"path": "/abs/dir", "mode": "ro" \| "rw"}]`: directories outside `cwd` it may read, or read and write. |
| `command_rules` | `git add`/`git commit` allow, `git push` ask | `[{"prefix": "git push", "action": "allow" \| "ask" \| "forbid"}]`. |

`agent_update` changes any of these fields.

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

Messages (from `message_send`, `message_agent` or a fired timer) are
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
`agents` list when an agent starts or stops working, an updated `questions`
list when a question is asked or answered, `report_posted` for a new report,
and an updated `actions` list when an action is deferred or decided.

## Session ops

Every op from `docs/serve-protocol.md` (`user_message`, `command`, `steer`,
`interrupt`, `approve`, `set_approval_mode`, `decide_action`, `mcp_*`) is
accepted with a `session` field and forwarded to that session's engine. Its
events carry the same `session` field.

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
