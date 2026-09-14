# `jucode serve` Protocol

`jucode serve` is a persistent bidirectional protocol mode for GUI/IDE
front-ends. The process reads newline-delimited JSON commands on stdin and
emits the engine's `AgentEvent` stream as newline-delimited JSON on stdout —
the same schema `--headless` uses. It runs until stdin closes, an `op:"shutdown"`
arrives, or a `command` op carries `/quit` or `/exit`.

This is the richer of JuCode's two embedding protocols. `jucode acp` is the
standardized subset for ACP clients; see `docs/serve-vs-acp.md` for the split.

## Framing

- One JSON object per line, both directions. No length prefixes, no batching.
- stdout is flushed after every event line; consumers should read incrementally.
- Empty lines are ignored.
- A line that fails to parse produces `{"type":"error","message":"invalid command: ..."}`
  and the process keeps running. An unknown op produces
  `{"type":"error","message":"unknown op: ..."}`. Malformed input never exits
  the process.

## Lifecycle

On spawn, before any command, the engine emits its startup batch:

```jsonl
{"type":"startup","version":"0.2.0","session_id":"...","profile_dir":"...","config_path":"...","cwd":"...","model":"...","context_window":200000}
{"type":"model_status","provider":"...","model":"...","reasoning_effort":"...","context_window":200000,"context_limit":160000,"max_output_tokens":32000,"reasoning_efforts":["low","medium","high"],"state":"ready"}
{"type":"command_list","commands":[{"command":"/help","marker":null,"args":"","description":"..."}]}
{"type":"approval_mode","mode":"read-only"}
{"type":"mcp_servers","servers":[...]}
{"type":"trust_prompt","cwd":"...","repo_root":"..."}        // only when an untrusted project has local resources
{"type":"info","message":"..."}                              // session_start hook output, if any
```

`startup.version` is the CLI version; front-ends should record it for
compatibility checks.

After startup the loop polls every ~30 ms. `model_status` is deduplicated:
it is re-emitted only when its content changes. All other events are emitted
as they occur — command responses first, then background worker events
(streaming deltas, tool progress, MCP state changes, update notices) as they
arrive.

The process exits with code 0 on `shutdown`, stdin EOF, or `/quit` / `/exit`
sent through the `command` op.

## Commands (stdin)

Every command is a JSON object with an `op` field.

### `user_message`

```json
{"op":"user_message","content":"refactor the parser","images":["/abs/path.png"]}
```

Submits a user turn. `images` is an optional array of local image paths;
unattachable paths produce `info` events and are skipped.

If a turn is already running, the message is queued instead: the engine emits
`pending_messages` (the full queue) and `status:"queued: N"`. Queued messages
start automatically when the current turn ends — no client action needed.

### `command`

```json
{"op":"command","input":"/model gpt-5.5 medium"}
```

Runs any slash command exactly as the TUI would (`/model`, `/resume`,
`/compact`, `/approve`, `/mcp`, custom commands, MCP prompt commands, ...).
Structured views come back as events (`model_view`, `resume_view`,
`tree_view`, ...). `/quit` and `/exit` terminate the process.

### `steer`

```json
{"op":"steer"}
```

Stops the in-flight turn and immediately starts the next queued message.
Emits `status:"steering"`, the updated `pending_messages`, then the new turn's
`user_message` event. No-op when idle or when the queue is empty.

### `interrupt`

```json
{"op":"interrupt"}
```

Stops the in-flight turn without touching the queue: aborts the worker,
closes subagents, clears pending approvals. Emits
`info:"request interrupted"` and `status:"interrupted"`. Queued messages still
auto-start as the next turn; send `interrupt` again (or avoid queueing) to
stop those too. No-op when idle.

### `approve`

```json
{"op":"approve","call_id":"call_9","decision":"allow","always":false,"hunks":["f0h1"]}
```

Structured answer to an `approval_request` event; equivalent to the
`/approve` slash command.

- `call_id` (required): the `call_id` from the request.
- `decision` (required): `"allow"` or `"deny"`.
- `always` (optional, default `false`): on allow, add the tool to the
  per-session allowlist so it stops asking.
- `hunks` (optional): for edit-tool requests that carried a `hunks` list,
  apply only these hunk ids. Omit or `null` for the whole call. Cannot be
  combined with `always`.

Validation errors emit `error` and leave the request pending, so the client
can retry with a corrected op.

### `set_approval_mode`

```json
{"op":"set_approval_mode","mode":"read-only"}
```

`mode` is `read-only`, `auto-edit`, or `full-auto`. Emits
`status:"approval mode: ..."` and an `approval_mode` event. The change
applies to new turns; an in-flight turn's gating can only loosen.

### MCP ops

`mcp_list`, `mcp_set`, `mcp_remove`, `mcp_toggle` manage configured MCP
servers and emit the `mcp_servers` view. See `docs/mcp.md` → "Serve protocol
ops" for the full shapes.

### `shutdown`

```json
{"op":"shutdown"}
```

Exits the process with code 0. Closing stdin has the same effect.

## Events (stdout)

Every line is `{"type": <name>, ...}`. All types emitted by the engine:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `startup` | `version`, `session_id`, `profile_dir`, `config_path`, `cwd`, `model`, `context_window` | First event; identifies the session. |
| `model_status` | `provider`, `model`, `reasoning_effort`, `context_window`, `context_limit`, `max_output_tokens`, `reasoning_efforts`, `state` | Current model selection; deduplicated, re-emitted on change. |
| `command_list` | `commands: [{command, marker, args, description}]` | Available slash commands incl. custom and MCP prompt commands. |
| `approval_mode` | `mode` | Current approval mode; emitted at startup and on change. |
| `mcp_servers` | `servers: [{name, transport, state, tools, error?}]` | MCP server states; `state` ∈ `connecting`/`connected`/`failed`/`disabled`. |
| `trust_prompt` | `cwd`, `repo_root` | Project has local resources (skills, commands, hooks) and no stored trust decision; answer via `command` `/trust yes\|no\|repo`. |
| `user_message` | `content` | A user turn was accepted and started (echoes the submitted text). |
| `pending_messages` | `messages` | The queued-message list after it changed. |
| `fill_input` | `content` | Front-end should pre-fill its input box (e.g. after `/checkout`). |
| `connecting` | — | A model request is starting. |
| `thinking_start` | — | Reasoning output begins. |
| `reasoning_delta` | `delta` | Reasoning text chunk. |
| `assistant_start` | — | Assistant reply begins. |
| `assistant_delta` | `delta` | Reply text chunk. |
| `retrying` | `attempt` | The request is being retried after a transient failure. |
| `tool_start` | `call_id`, `name` | Tool call begins. |
| `tool_update` | `call_id`, `name`, `output` | Intermediate tool progress (e.g. long-running bash). |
| `tool_output` | `call_id`, `name`, `output`, `is_error` | Tool call finished. |
| `approval_request` | `call_id`, `name`, `summary`, `subagent_id`, `hunks` | A gated tool call waits for an `approve` op. `hunks` is a list of `{id, file, header, lines}` for partial approval, `null` otherwise. `subagent_id` is set when a subagent issued the call. |
| `subagent_lifecycle` | `path`, `status`, `message` | Subagent spawn/progress/finish notices. |
| `usage` | `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_tokens` | Real API usage for the completed turn. |
| `context_usage` | `tokens`, `tokenizer`, `cost` | Tokenizer-counted context size; `cost` is cumulative USD (0 when unpriced). |
| `compaction_start` / `compaction_end` | — | Context compaction began/finished. |
| `compaction_progress` | `output_tokens` | Compaction summary tokens produced so far. |
| `compaction_failed` | `error` | Compaction failed; the session continues uncompacted. |
| `model_view` | `models: [{model, active, context_window, max_output_tokens, reasoning_efforts}]`, `active_effort` | Model picker data (`/model`). |
| `tree_view` | `nodes: [{id, parent_id, label, active}]` | Session branch tree (`/tree`). |
| `resume_view` | `items: [{id, label, active}]` | Session picker data (`/resume`). |
| `checkpoint_view` | `items: [{id, label, detail}]` | Checkpoint picker data. |
| `goal` | `goal: {objective, status, token_budget, tokens_used, time_used_seconds, created_at, updated_at} \| null` | Goal state; `null` clears it. |
| `plan` | `plan: [{step, status}]` | Goal/plan step list. |
| `transcript` | `items: [{role, ...}]` | Rendered conversation (`/transcript`); roles: `user`, `assistant`, `tool` (`name`, `output`), `branch` (`label`). |
| `info` | `message` | Informational line (hook output, notices, update available). |
| `error` | `message` | Error line. Also used for malformed commands and unknown ops. |
| `status` | `message` | Turn status string: `ready`, `streaming`, `queued: N`, `steering`, `interrupted`, `compacting`, `approval mode: ...`, `trusted ...`, etc. `ready` marks the end of a turn. |

## Turn sequence

A normal turn looks like:

```jsonl
{"op":"user_message","content":"fix the test"}
< {"type":"user_message","content":"fix the test"}
< {"type":"connecting"}
< {"type":"thinking_start"}            // reasoning models only
< {"type":"reasoning_delta","delta":"..."}
< {"type":"assistant_start"}
< {"type":"assistant_delta","delta":"..."}
< {"type":"tool_start","call_id":"call_1","name":"bash"}
< {"type":"tool_output","call_id":"call_1","name":"bash","output":"...","is_error":false}
< {"type":"usage","input_tokens":1234,"cached_input_tokens":800,"output_tokens":56,"reasoning_tokens":12}
< {"type":"context_usage","tokens":5678,"tokenizer":"o200k","cost":0.0123}
< {"type":"status","message":"ready"}
```

Approval round-trip:

```jsonl
< {"type":"approval_request","call_id":"call_2","name":"bash","summary":"rm -rf build","subagent_id":null,"hunks":null}
> {"op":"approve","call_id":"call_2","decision":"deny"}
```

Interrupt:

```jsonl
> {"op":"interrupt"}
< {"type":"info","message":"request interrupted"}
< {"type":"status","message":"interrupted"}
```

## Mapping to UI

Suggested rendering (as used by JuCode Desktop):

| Events | UI |
| --- | --- |
| `assistant_start` / `assistant_delta` | streaming message bubble |
| `thinking_start` / `reasoning_delta` | collapsible thinking section |
| `tool_start` / `tool_update` / `tool_output` | tool cards keyed by `call_id` |
| `approval_request` | approval dialog → `approve` op |
| `model_view` / `tree_view` / `resume_view` / `checkpoint_view` | pickers/sidebars |
| `goal` / `context_usage` / `usage` | status bar |
| `compaction_*` | compaction progress |
| `status` / `info` / `error` | status line / toasts |
| `pending_messages` | queued-message indicator |
| `fill_input` | pre-fill the input box |
