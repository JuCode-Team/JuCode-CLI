# JuCode CLI

JuCode CLI is a compact coding-agent CLI for repository work. It provides an interactive terminal UI, a headless JSONL mode, context-aware editing tools, lightweight subagents, and real token-usage reporting.

The project is intentionally small: the agent harness is designed to give the model enough autonomy to implement and verify tasks without loading a large framework prompt or exposing high-noise tools by default.

## Highlights

- **Interactive TUI by default** for day-to-day coding tasks.
- **Headless mode** for benchmarks, CI experiments, and scripted agent runs.
- **Context-efficient tool outputs** with projected read/bash/diff-like edit results and saved full outputs when needed.
- **Real token accounting** for input, cached input, output, reasoning, and tokenizer-counted context usage.
- **Parallel read-only/tool inspection** for independent file reads, searches, listings, and shell checks.
- **Scoped editing tools**: exact replacement, hashline edits, full-file writes, and patch application.
- **Large-output controls**: bash output truncation, ripgrep soft warnings, and `read` offset/limit support.
- **Conversation compaction** based on tokenizer-counted context, not rough character estimates.
- **Branchable sessions, resume, checkout, goals, skills, and lightweight subagents**.
- **TUI diff display without exposing `diff` as an agent tool**.

## Installation

### From source

```bash
git clone https://github.com/JuCode-Team/JuCode-CLI.git
cd JuCode-CLI
cargo build --release
./target/release/jucode
```

### With Cargo from Git

```bash
cargo install --git https://github.com/JuCode-Team/JuCode-CLI.git jucode-cli
jucode
```

JuCode is written in Rust and uses the workspace binary name `jucode`.

## Configuration

On first run, JuCode creates its configuration under the user profile directory. By default it targets an OpenAI-compatible API:

- default provider: `openai`
- default model: `gpt-5`
- default API base URL: `https://api.openai.com/v1`
- default API key environment variable: `OPENAI_API_KEY`

Set your key before running:

```bash
export OPENAI_API_KEY="..."
jucode
```

You can switch model and reasoning effort inside the TUI:

```text
/model gpt-5 medium
/model gpt-5 low
```

The config also supports custom OpenAI-compatible base URLs, retry settings, model metadata, project-instruction discovery, and optional extensions.

## Usage

### Interactive mode

Run JuCode in a repository:

```bash
cd path/to/project
jucode
```

Then ask for implementation, debugging, refactoring, or verification work in natural language.

Useful commands:

```text
/help                         show command summary
/login [web-url] [api-url]    login and sync marketplace defaults
/model [model] [effort]       view or change model and reasoning effort
/tree                         show branchable session tree
/resume [session-id]          resume a previous session
/context                      inspect context and token statistics
/goal <objective>             start or update a persistent goal
/skills list                  list marketplace skills
/skills install <id>          install a skill
/skills sync                  sync default skills
/pin <skill>                  keep a skill in current session context
/compact                      compact older conversation context
/quit                         exit
```

### Headless mode

Headless mode emits JSONL events and finishes with a `final_result` event containing status, usage, context, tool-call counts, and elapsed time.

```bash
jucode --headless "Fix the failing test and verify the focused suite"
```

You can also pipe the task through stdin:

```bash
cat task.md | jucode --headless
```

This mode is useful for evaluation harnesses and reproducible agent experiments.

## Agent tools

JuCode exposes a small set of direct tools to the model:

| Tool | Purpose |
| --- | --- |
| `read` | Read text, image metadata/payload, or binary metadata. Supports `offset` and `limit`. |
| `str_replace` | Apply exact targeted replacements after reading a file. |
| `hashline_edit` | Patch lines using stable `LINE#HASH` anchors from `read`. |
| `write` | Create new files or overwrite previously read files. |
| `apply_patch` | Apply a unified patch when targeted edits are awkward. |
| `bash` / `exec_command` | Run shell commands with timeout, sessions, output truncation, and progress updates. |
| `write_stdin` | Poll or send input to a running shell session. |
| `ls` | List directory entries. |
| `ripgrep` | Search with ripgrep and optional limits. |
| `outline` | Get lightweight source-file symbols without reading full bodies. |
| `checkpoint` | Create/list/restore local `.jucode/checkpoints` snapshots. |
| `spawn_agent`, `wait_agent`, `list_agents`, `send_message`, `close_agent` | Coordinate lightweight subagents. |

`diff` is intentionally not exposed as an agent tool. Edit tools still return diff data for the TUI and for compact model-facing summaries, but workspace diff inspection should happen through scoped shell commands when needed.

## Context and token efficiency

JuCode focuses on reducing unnecessary context growth without hiding useful information:

- tool outputs have separate full output and model-projected output paths;
- large command output is truncated before entering model context;
- large reads return soft guidance to use `offset`, `limit`, `outline`, or `ripgrep`;
- large edit diffs are summarized for the model while the TUI can still display useful change previews;
- tokenizer-counted context is used for context statistics and compaction thresholds;
- prompt-cache usage is reported from real API usage, including cached input tokens.

## Evaluation snapshot

The following numbers come from the local `agent-eval` **test set** run on 2026-06-09. The set contains five representative multi-step tasks:

- three SWE-style issue-regression tasks in existing open-source projects;
- one greenfield TypeScript library task;
- one greenfield frontend dashboard task.

The comparison used the same evaluation harness data currently stored in `/Users/han/dev/projects/jucode/agent-eval/results/aggregate_test.json`. Treat these as a reproducible local snapshot, not a universal public benchmark.

| Agent | Passed | Input + output tokens | Output tokens | Reasoning tokens | Raw cache rate | Filtered cache rate |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| JuCode | 5/5 | 735,437 | 16,028 | 1,657 | 66.5% | 80.5% |
| Codex baseline | 5/5 | 1,082,512 | 24,847 | 2,109 | 81.3% | 81.3% |
| OpenCode | 5/5 | 1,095,558 | 24,045 | 699 | 62.2% | 74.7% |
| PI | 5/5 | 372,037 | 20,382 | 0 | 36.4% | 71.9% |
| Reasonix | 4/5 | 1,586,304 | 18,311 | 0 | 68.4% | 68.4% |

In this test-set snapshot, JuCode completed all five tasks and used **347,075 fewer input+output tokens than the Codex baseline**, a **32.1% reduction**. After excluding zero-cache noise requests, JuCode's cache rate was **80.5%**, close to the Codex baseline's **81.3%**.

## Development

Run the full Rust test suite:

```bash
cargo fmt --check
cargo test --workspace
```

Build the CLI:

```bash
cargo build -p jucode-cli
```

Run a quick headless smoke task:

```bash
./target/debug/jucode --headless "List the repository structure and stop."
```

## Project status

JuCode CLI is an active experimental coding-agent harness. The current direction is to keep the framework small, improve task completion reliability, and optimize context quality rather than adding broad agent abstractions.
