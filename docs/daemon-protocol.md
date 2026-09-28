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
| `token` | Client token, created on first start, mode 0600. |
| `sessions.jsonl` | Append log of sessions opened and closed. |
| `actions.jsonl` | Append log of deferred actions and their decisions. |
| `messages.jsonl` | Append log of messages to agents and their delivery. |
| `timers.jsonl` | Append log of agent timers set, fired and cancelled. |
| `questions.jsonl` | Append log of questions asked and answered. |
| `reports.jsonl` | Append log of reports posted and read. |

## Connecting

Connect to `ws://<listen>/?token=<token>` (or send
`Authorization: Bearer <token>`). A missing or wrong token fails the
handshake with HTTP 401. Local clients read the token from
`~/.jucode/daemon/token`.

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
| `session_list` | — | `sessions` |
| `session_create` | `cwd` | `session_created` with `session`; the session's startup events follow |
| `session_open` | `session` | `session_opened`; reopens a closed session (or one from before a restart), resuming its transcript and its undecided deferred actions |
| `session_close` | `session` | none; every client receives `session_closed` once the engine has stopped |
| `watch` / `unwatch` | `session` | `watching` with `watching: true/false`; `watch` also sends this client a snapshot of the session: its state events (`startup`, `model_status`, `command_list`, `approval_mode`, `mcp_servers`), a `transcript` of the conversation so far and `attended` |
| `actions_list` | — | `actions`: undecided deferred actions across all sessions |
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
the agent's directory as that agent.

## Agents

A long-lived agent is a directory `~/.jucode/agents/<id>/`: its brief
(`role.md`, `capabilities.md`, `policy.md`, `state.md`), `memory/<topic>.md`
notes and `agent.json` (`name`, `cwd`, `enabled`, `approval_mode`, default
`auto`). Every turn of an agent session gets the brief, the memory index and
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
