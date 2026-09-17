# Repository Guidelines

## Project Structure & Module Organization

This is a lightweight Rust CLI/TUI workspace. The binary entry point is `src/main.rs`; agent state, sessions, tools, and LLM streaming live in `crates/agent-core/`; terminal rendering and input handling live in `crates/tui/`. Keep tests next to the module they cover with `#[cfg(test)]`. Build artifacts such as `target/` and `target-msvc/` are generated outputs.

`crates/llm-provider-kit/` is a git submodule (github.com/JuCode-Team/llm-provider-kit) holding the provider layer: wire protocols (Responses/Codex/Azure, Anthropic Messages, Chat Completions), the vendored provider catalog (`omp`), provider templates, request builders, the blocking HTTP transport (`transport`), and the catalog-driven login flows (`auth`, `oauth`). It is a workspace member, so `cargo fmt/clippy/test --workspace` covers it. Changes to the provider layer belong there, not in `agent-core`; commit them in the submodule repo first, then bump the pointer here. JuCode-specific pieces stay in this repo: the gateway OAuth flow and auth.json layout (`crates/agent-core/src/oauth.rs`, `config.rs`), the gateway provider template and client name (`crates/agent-core/src/providers.rs`), and the agent loop, tool definitions, approvals, subagents, and safety classifier (`crates/agent-core/src/llm.rs`).

## Build, Test, and Development Commands

- `cargo run`: run the local JuCode TUI.
- `cargo test`: run unit tests.
- `cargo check`: verify the project quickly without producing a final binary.
- `cargo fmt`: format Rust code with rustfmt.
- `cargo clippy -- -D warnings`: run lint checks and treat warnings as failures.
- `cargo build --release`: produce an optimized release binary.

## Coding Style & Naming Conventions

Use Rust 2021 idioms and rustfmt defaults. Prefer small functions with direct control flow. Use `snake_case` for functions, variables, and modules; `PascalCase` for structs, enums, and variants; and `SCREAMING_SNAKE_CASE` for constants. Keep comments sparse and focused on non-obvious decisions.

## Project Design Rules

Performance and lightweight behavior are the first priorities. Do not introduce heavy dependencies, framework layers, or broad abstractions without concrete need. Do not add multiple fallback paths just to make a feature appear to work without evidence; prefer one explicit, testable path and clear error handling.

This project implements a standard MCP client (stdio and streamable HTTP transports, no tokio — blocking I/O plus threads) alongside skills; MCP support must stay dependency-light (hand-rolled JSON-RPC over serde_json, HTTP via the existing blocking `ureq`). See `docs/mcp.md`. Sub-agent functionality is built in, so do not design a separate subsystem for it.

## TUI Guidelines

Keep the TUI minimal and fast. Chat history may use native scrolling; avoid complex custom scroll systems unless required. Use a restrained palette with only a few semantic colors. Theme selection is allowed, but themes must remain simple and readable.

## Testing Guidelines

Add focused unit tests for state transitions, commands, text wrapping, cursor behavior, and candidate filtering. Name tests after behavior, for example `clear_command_resets_history`. Run `cargo test` before submitting changes; run `cargo clippy -- -D warnings` for architecture changes.

## Commit & Pull Request Guidelines

Git history currently has only `Initial commit`, so keep commit messages short and imperative, for example `Add theme setting` or `Simplify chat scrolling`. Pull requests should include a concise description, test results, and screenshots or terminal captures for visible TUI changes. Call out dependency additions.
