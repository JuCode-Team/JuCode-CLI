# MCP Support

JuCode implements a standard [Model Context Protocol](https://modelcontextprotocol.io) client
(protocol version `2025-06-18`; an older version offered back by a server is accepted and
recorded). The implementation is dependency-light by design: hand-rolled JSON-RPC 2.0 over
`serde_json`, blocking I/O with `std::process` / `std::thread` / `mpsc` for stdio, and the
existing blocking `ureq` agent for HTTP. No tokio, no SDK crates.

## Configuration

Servers are declared in the `mcp_servers` array of `~/.jucode/config.json`:

```json
{
  "mcp_servers": [
    {
      "name": "files",
      "transport": "stdio",
      "command": "mcp-server-files",
      "args": ["--root", "."],
      "env": { "DEBUG": "1" },
      "enabled": true,
      "timeout_seconds": 60
    },
    {
      "name": "search",
      "transport": "http",
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer <token>" },
      "enabled": true,
      "timeout_seconds": 60
    }
  ]
}
```

- `name` (required): must match `^[A-Za-z0-9_-]+$`; duplicates are skipped.
- `transport`: `"stdio"` (default) or `"http"`.
- stdio entries require `command`; `args` and `env` are optional.
- http entries require `url`; `headers` is an optional map (e.g. a bearer token).
- `enabled` defaults to `true`; `timeout_seconds` is the per-request timeout
  (default 60, clamped to 1–3600).

Invalid entries are skipped with a warning in the log; valid entries round-trip on save.

## Transports

- **stdio** — the server is spawned as a child process; JSON-RPC messages are
  newline-delimited on stdin/stdout. stderr is drained to debug-level logging. The child is
  killed and reaped (with a short timeout) when the connection is dropped.
- **streamable HTTP** — every JSON-RPC message is POSTed to the server URL with
  `Accept: application/json, text/event-stream`. Both response modes are handled: a single
  JSON body, or an SSE stream read until the matching response event arrives. The
  `Mcp-Session-Id` response header from `initialize` is captured and echoed on subsequent
  requests; the negotiated version is sent as `MCP-Protocol-Version`.

Server-initiated `ping` requests are answered with an empty result; all other
server-initiated requests are rejected with a JSON-RPC method-not-found error.
`notifications/tools/list_changed` marks the tool cache stale (refreshed on next use);
other notifications are ignored.

## Lifecycle & tool naming

Enabled servers connect on background threads at session start; the agent is usable
immediately and each server's tools join the model tool table once it connects (an info
event reports success or failure — a failed server never blocks the session).

Each server tool is exposed to the model as `mcp__<server>__<tool>`, with the server's
description and JSON input schema passed through. Duplicate full names lose (first wins,
logged).

## Approval behavior

MCP tools are untrusted by default and gate on the session approval mode:

| Mode        | MCP tool approval                                              |
| ----------- | -------------------------------------------------------------- |
| `read-only` | every MCP tool call asks                                        |
| `auto-edit` | asks, unless the server marks the tool `annotations.readOnlyHint: true` |
| `full-auto` | never asks                                                      |

The `readOnlyHint` annotation is server-provided metadata: it can only loosen gating in
auto-edit, never bypass read-only mode. "Always allow" from the approval prompt adds the
full tool name to the per-session allowlist as usual.

## `/mcp` command

- `/mcp` — list servers (name, transport, state, tool count or error).
- `/mcp tools <server>` — list a server's tools with their full model names.
- `/mcp reload <server>` — reconnect a server.
- `/mcp enable <server>` / `/mcp disable <server>` — toggle and persist to config.

## Serve protocol ops

- `{"op":"mcp_list"}` — emit the current servers view.
- `{"op":"mcp_set","server":{ ...full config entry... }}` — add or update by name, persist,
  reconnect.
- `{"op":"mcp_remove","name":"..."}` — remove and persist.
- `{"op":"mcp_toggle","name":"...","enabled":true|false}` — toggle and persist.

Each op (and any state change, including startup) emits:

```json
{
  "type": "mcp_servers",
  "servers": [
    {
      "name": "files",
      "transport": "stdio",
      "state": "connected",
      "tools": [{ "name": "read_file", "description": "..." }],
      "error": "only present when state is failed"
    }
  ]
}
```

`state` is one of `connecting`, `connected`, `failed`, `disabled`. The startup event may
report `connecting`; a follow-up event arrives when connections settle.

## Unsupported

- The standalone GET listening stream (server-push over HTTP) — responses are only read
  from the POST that triggered them.
- Server-initiated `sampling/*` and `roots/*` requests (rejected with method-not-found).
- Prompts and resources (`prompts/*`, `resources/*`) — tools only.
- Elicitation and any other server→client capability.
