# JuCode CLI

JuCode CLI is a lightweight coding agent for fast, practical repository work. It is designed to give the model strong tools while avoiding a heavy agent framework, rigid workflow, or long harness prompt.

## What JuCode Focuses On

JuCode keeps the coding loop compact and direct:

- **Ready-to-use coding loop**  
  A compact TUI is available by default for interactive repository work.

- **Context-efficient tools**  
  Tooling is designed to preserve useful context while keeping the active conversation small. This includes projected tool output, saved full oversized results, file outlines, long-running shell sessions, checkpoints, and compacted conversation history.

- **Lightweight subagents**  
  Subtask delegation is handled through one generic `spawn_subagent` tool instead of many fixed roles.

- **Optional skills**  
  Skills work as optional instructions. They can be used as discoverable commands for one-off tasks, or pinned with `/pin <skill>` when a skill should remain in the current session context.

- **Extension support without MCP context bloat**  
  Extensions can expose tools directly. Lazy extensions expose only `extension_list_tools` and `extension_call`, keeping extension access available without loading unnecessary context.

- **Headless JSONL mode**  
  JuCode can run without the TUI for scripts and external harnesses:

  ```bash
  jucode --headless "task"
  ```

## Design Direction

JuCode borrows ideas from modern coding agents and harnesses, including:

- compact tool result projection
- branchable sessions
- lightweight subtask delegation
- machine-readable event streams

The implementation keeps these ideas small and direct so the model stays in control of the work.
