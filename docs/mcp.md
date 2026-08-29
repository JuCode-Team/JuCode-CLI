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
      "oauth": {
        "client_id": "jucode-cli",
        "token_url": "https://example.com/oauth/token",
        "scope": "mcp"
      },
      "enabled": true,
      "timeout_seconds": 60
    }
  ]
}
```

- `name` (required): must match `^[A-Za-z0-9_-]+$`; duplicates are skipped.
- `transport`: `"stdio"` (default) or `"http"`.
- stdio entries require `command`; `args` and `env` are optional.
- http entries require `url`; `headers` is an optional map (including a static
  `Authorization` bearer header).
- `oauth` is optional HTTP refresh metadata. `client_id` and `token_url` must be
  provided together; `scope` is optional. Tokens are kept out of config.json.
- `enabled` defaults to `true`; `timeout_seconds` is the per-request timeout
  (default 60, clamped to 1–3600).

Invalid entries are skipped with a warning in the log; valid entries round-trip on save.

### HTTP bearer and OAuth tokens

For a static token, set `headers.Authorization` to `Bearer <token>`. For a refreshable token, set
the `oauth` metadata and store the initial token bundle in `~/.jucode/auth.json`:

```json
{
  "mcp_servers": {
    "search": {
      "access_token": "<access-token>",
      "refresh_token": "<refresh-token>",
      "access_expires_at": 1787936400
    }
  }
}
```

`access_expires_at` is Unix seconds; use `0` when expiry is unknown. JuCode sends the access
token as a bearer token, refreshes it with the standard `refresh_token` form grant before
expiry or after HTTP 401, handles refresh-token rotation, and atomically persists the new
bundle in auth.json. This is a non-interactive v1 flow: obtain the initial token by the
provider's documented process. JuCode does not invent a provider-specific browser dance.
To keep a non-refreshing bearer in auth.json instead of config.json, use `"oauth": {}`, omit
the refresh token, and set `access_expires_at` to `0`.

## Transports

- **stdio** — the server is spawned as a child process; JSON-RPC messages are
  newline-delimited on stdin/stdout. stderr is drained to debug-level logging. The child is
  killed and reaped (with a short timeout) when the connection is dropped.
- **streamable HTTP** — every JSON-RPC message is POSTed to the server URL with
  `Accept: application/json, text/event-stream`. Both response modes are handled: a single
  JSON body, or an SSE stream read until the matching response event arrives. The
  `Mcp-Session-Id` response header from `initialize` is captured and echoed on subsequent
  requests; the negotiated version is sent as `MCP-Protocol-Version`.

Server-initiated `ping` requests are answered with an empty result. `roots/list` returns the
current working directory as the single `file://` workspace root. Sampling, elicitation, and
other server requests are rejected with a JSON-RPC method-not-found error.
`notifications/tools/list_changed`, `notifications/prompts/list_changed`, and
`notifications/resources/list_changed` refresh their corresponding caches.

## Lifecycle & tool naming

Enabled servers connect on background threads at session start; the agent is usable
immediately and each server's tools join the model tool table once it connects (an info
event reports success or failure — a failed server never blocks the session).

Each server tool is exposed to the model as `mcp__<server>__<tool>`, with the server's
description and JSON input schema passed through. Duplicate full names lose (first wins,
logged).

Servers with the resources capability also add two model-facing tools:

- `mcp__<server>__list_resources` — return the cached, paginated `resources/list` result.
- `mcp__<server>__read_resource` — call `resources/read` for a URI returned by the list.

Only text resources are attached. Prompt and resource output is capped at 256 KiB. File
resource URIs must stay within the reported workspace root (including canonical symlink
resolution); unlisted URIs and path escapes are rejected.

## MCP prompts

Each `prompts/list` entry becomes a dynamic slash command:

```text
/mcp__<server>__<prompt> {"argument":"value"}
```

Arguments are an optional JSON object whose values must be strings. Required arguments from
the prompt metadata are checked before `prompts/get`. The returned text messages are expanded
into the next agent turn, like skill slash commands. Prompt commands are re-emitted to clients
after connection and after `notifications/prompts/list_changed`.

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
- Server-initiated `sampling/*` and elicitation.
- Resource templates/subscriptions and binary resource attachment.
- Automated OAuth discovery, dynamic client registration, and interactive provider login.
