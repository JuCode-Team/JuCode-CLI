# JuCode CLI

JuCode CLI is a lightweight coding agent focused on fast, practical repository work. It aims to give the model strong tools without forcing a heavy agent framework, rigid workflow, or long harness prompt.

## Focus

- Ready-to-use coding loop with a compact TUI by default.
- Context-efficient tools: projected tool output, saved full oversized results, file outlines, long-running shell sessions, checkpoints, and compacted conversation history.
- Lightweight subagents through one generic `spawn_subagent` tool instead of many fixed roles.
- Skills as optional instructions: discoverable commands for one-off use, plus `/pin <skill>` when a skill should stay in the current session context.
- Extension support without MCP context bloat: normal extensions expose tools directly; lazy extensions expose only `extension_list_tools` and `extension_call`.
- Headless JSONL mode for scripts and external harnesses: `jucode --headless "task"`.

## Inspiration

JuCode borrows ideas from modern coding agents and harnesses: compact tool result projection, branchable sessions, lightweight subtask delegation, and machine-readable event streams. The implementation keeps those ideas small and direct so the model stays in control of the work.
