# TUI Architecture

This document explains how JuCode-CLI's TUI is structured today and how data flows from the agent runtime to terminal output.

## Overview

JuCode's TUI is intentionally lightweight. It does not use a heavyweight widget framework or a large state-management layer. Instead, it follows a direct pipeline:

1. `AgentCore` produces `AgentEvent`s.
2. `TuiApp` owns the interactive TUI state and applies those events.
3. `UiBuilder` converts TUI state into a `UiDocument`.
4. `ProjectedDocument` and `RenderedFrame` turn that document into terminal lines plus cursor position.
5. `TerminalRenderer` writes the frame efficiently to the terminal, using full redraws only when needed and buffer diffs otherwise.

This keeps the architecture close to the product model: runtime events in, state update, document build, terminal render.

## Entry Point and Runtime Boundary

The binary entry point is `src/main.rs`.

- `Runtime(AgentCore)` is a thin adapter.
- It implements the `jucode_tui::TuiRuntime` trait.
- `main()` creates `AgentCore`, starts the update check, and runs `TuiApp::new(Runtime(core)).run()`.

`TuiRuntime` is the main boundary between the TUI crate and the agent runtime:

- `startup_events()`
- `model_status_event()`
- `submit_user_message()`
- `steer()`
- `handle_command()`
- `poll_events()`

This separation is useful because the TUI does not need to know agent internals. It only consumes events and sends user intent back through a small trait.

## Core State: `TuiApp`

`crates/tui/src/lib.rs` defines `TuiApp<R>`, which is the main controller for the TUI.

Its responsibilities are:

- hold interactive state
- receive keyboard and paste input
- poll runtime events
- maintain chat/history state
- build the UI document
- drive rendering timing

Important state fields include:

- `input: InputBuffer`: line editor state, selection, cursor, large-paste placeholders
- `chat: Vec<ChatLine>`: persisted visible transcript in TUI form
- `live_assistant: Option<String>`: streamed assistant text not yet committed to transcript
- `reasoning_index: Option<usize>` and `thinking_tokens`: reasoning display state
- `activity: ActivityState`: current phase such as connecting, thinking, output, tool, compacting
- `commands` and `completion_index`: slash-command completion
- `picker_view: Option<PickerState>`: temporary selection UIs for tree/resume/model flows
- `pending_messages`: queued user messages waiting for steering behavior
- `rendered_history_cache`: cached rendered transcript lines keyed by width and revision
- model/status counters such as provider, model, context usage, input/output tokens

Conceptually, `TuiApp` is both the application state store and the interaction controller.

## Event Model

The TUI is event-driven around `AgentEvent`.

`apply_events()` is the central reducer-like function. It maps runtime events into TUI state changes.

Examples:

- `Startup` -> push startup box into chat history
- `UserMessage` -> append a user line
- `ThinkingStart` / `ReasoningDelta` -> update reasoning state
- `AssistantStart` / `AssistantDelta` -> stream live assistant output
- `ToolStart` / `ToolUpdate` / `ToolOutput` -> update tool blocks in transcript
- `ModelStatus` -> refresh provider/model/status metadata
- `TreeView`, `ResumeView`, `ModelView` -> open a picker UI
- `Goal`, `Info`, `Error` -> append system or error lines
- `Status("ready")` -> commit live assistant text and finish current activity

This is one of the cleanest parts of the design: most UI behavior is expressed as a direct reaction to runtime events.

## Input System

The input editor lives in `crates/tui/src/input.rs`.

### `InputBuffer`

`InputBuffer` is a small editor model with:

- per-cell storage
- cursor position
- optional selection anchor
- movement by character, word, line, and document
- backspace/delete behavior
- multi-line editing

It also supports a special `LargePaste(String)` cell. Large pasted content is stored as one logical cell and displayed as a placeholder like `[Pasted: N chars]`. That avoids turning very large pastes into huge per-character editor state.

### Rendered Cursor and Selection

The input renderer embeds a logical cursor marker into text and uses reverse-video ANSI sequences for:

- block caret display
- selected ranges

This is important because JuCode usually hides the hardware cursor and renders its own cursor appearance in text.

### Paste Burst Handling

`TuiApp` also uses `PasteBurst` to distinguish typed ASCII from a fast burst that is probably a paste. This reduces noisy per-character behavior during terminal paste and helps preserve responsive rendering.

## Interaction Modes

The TUI has two main interaction modes.

### 1. Normal editor mode

Handled by `handle_key_at()`.

Key behaviors include:

- plain text editing
- multi-line input with `Shift+Enter` or `Ctrl+Enter`
- slash-command completion via `Tab`, `Up`, `Down`
- word navigation with `Ctrl`/`Alt` + arrows
- `Esc` clears input, or sends `steer()` when messages are pending during active work
- `BackTab` cycles reasoning effort for the current model

### 2. Picker mode

Handled by `handle_picker_key()` and `handle_picker_prompt_key()`.

Picker mode is used for:

- branch/tree checkout
- session resume
- model selection
- fork/delete prompts inside the tree picker

This mode is intentionally modal and simple. Instead of building a general widget system, the app swaps into a dedicated picker state object.

## UI Data Model

The TUI uses a small intermediate representation instead of rendering directly from raw state.

### Transcript-level items

`ChatLine` represents semantic transcript entries:

- `Startup`
- `User`
- `Assistant`
- `Reasoning`
- `Tool`
- `System`
- `Error`

### Render-level items

`UiLine` is a lower-level visual line with:

- `kind: UiKind`
- `text: String`

`UiKind` carries styling intent such as user, assistant, tool, error, selected, diff-add, diff-remove, and so on.

### Document-level structure

`UiDocument` splits the screen into two conceptual parts:

- `history`: transcript/history area
- `controls`: live area below history

The live area contains things like:

- live assistant streaming output
- thinking indicator
- picker UI
- pending-message notice
- input box text
- progress line
- bottom status line

This is a good fit for chat-style TUIs: immutable-ish transcript above, active controls below.

## UI Composition: `UiBuilder`

`crates/tui/src/ui_builder.rs` builds a `UiDocument` from current TUI state.

`TuiApp::build_document()` prepares the inputs, then chains builder calls like:

- `rendered_history_lines(...)`
- `thinking_indicator(...)`
- `live_assistant(...)`
- `picker(...)`
- `pending_messages(...)`
- `input(...)`
- `progress(...)`
- `bottom_status(...)`
- `reset_screen(...)`

This is effectively a manual view-composition pipeline.

### What `UiBuilder` renders

- startup welcome box with branded ASCII layout
- markdown-rendered assistant and reasoning text
- tool output blocks and compact previews
- command completion candidates
- picker rows with selection highlighting
- progress line with colored spinner
- bottom status line with model and token/context info

The builder is intentionally string-first. It produces styled text lines, not nested widgets.

## Supporting Presentation Modules

### `markdown.rs`

Renders a limited markdown subset for assistant/reasoning output:

- emphasis
- inline formatting
- code blocks
- tables

This allows the assistant transcript to look structured without depending on a full markdown UI engine.

### `tool_preview.rs`

Builds compact previews for tool output.

It includes special handling for:

- bash output projection
- diff extraction
- edit diff parsing
- intra-line diff highlighting

This is a strong product-oriented choice: tool output is not dumped raw by default, but transformed into something readable in a tight terminal layout.

### `picker.rs`

Implements `PickerState` and tree/model/resume navigation behavior.

Notably, the tree picker maintains both:

- all tree rows
- visible rows derived from expansion state

So the UI can stay simple while still supporting hierarchical navigation.

## Rendering Pipeline

The rendering pipeline has a few layers.

### 1. Build a `UiDocument`

`TuiApp::build_document()` creates the current document.

### 2. Project into terminal lines

`ProjectedDocument::from_document()`:

- wraps history and control lines to terminal width
- pads content with left margin
- extracts the logical cursor marker
- computes final cursor row/column
- combines transcript and active control lines into frame-ready output

### 3. Convert to a `RenderedFrame`

A `RenderedFrame` is just:

- `lines: Vec<String>`
- `cursor: Option<CursorTarget>`

### 4. Render with `TerminalRenderer`

`crates/tui/src/terminal_renderer.rs` decides whether to:

- redraw transcript projection
- do a full render
- do a buffer-diff render

It uses a `ratatui::Buffer` internally, but not the usual ratatui widget tree. Ratatui is used here more as a terminal cell buffer and style structure.

## Rendering Strategy and Performance Choices

This TUI is optimized around a few practical ideas.

### Cached transcript rendering

`rendered_history_cache` stores already-rendered history lines by:

- transcript revision
- width

So input changes or progress animation do not require rebuilding the whole transcript every frame.

### Transcript vs controls split

The renderer tracks whether transcript lines changed separately from the active controls area. That matters because the transcript is usually much more stable than the bottom live region.

### Buffer diff rendering

When possible, `TerminalRenderer` only writes changed cells instead of repainting everything.

This reduces terminal I/O and helps the interface feel smoother during:

- streamed output
- spinner animation
- input editing

### Explicit frame scheduling

`FrameScheduler` decides when the next render should happen. It avoids a constant redraw loop and requests frames only when needed.

That keeps the UI responsive without wasting CPU.

## Terminal Control Model

The app manages terminal mode itself.

`TerminalGuard`:

- enables raw mode on entry
- hides the cursor
- enables bracketed paste
- disables scroll-on-output behavior
- restores terminal state on drop

The code also uses ANSI sequences directly for:

- color
- selection/cursor display
- synchronized updates
- screen clearing

This keeps the stack small and gives tight control over behavior.

## Activity and Progress Model

`ActivityState` tracks the current phase of the agent turn:

- `Idle`
- `Connecting`
- `Compacting`
- `Thinking`
- `Reconnecting`
- `Output`
- `Tool`

It stores timing and token estimates, and `progress()` maps that into a `ProgressState` used by the bottom progress line.

This is a nice separation: the app tracks semantic activity, and the UI layer only asks for a renderable progress view.

## Design Characteristics

The current TUI design has several clear traits.

### Strengths

- small number of concepts
- direct event-to-state flow
- good terminal performance awareness
- minimal dependency on widget abstractions
- strong support for streaming and tool-heavy interaction
- clear runtime boundary through `TuiRuntime`

### Tradeoffs

- `TuiApp` is large and owns many responsibilities
- most UI composition is string-based, so some behavior is harder to validate structurally
- normal mode, picker mode, rendering policy, and event reduction all live close together
- there is no explicit reducer/view-model split yet

## Recommended Improvement

If I were to make one architectural improvement, I would split `TuiApp` into a dedicated state reducer and a controller shell.

Concretely:

- keep terminal polling, frame scheduling, and runtime I/O in `TuiApp`
- move `apply_events()`, status transitions, transcript mutation, and activity updates into a new `TuiState`

That would improve the code in three ways:

1. `TuiApp` would become easier to read because it would focus on orchestration.
2. State transitions could be unit-tested without terminal/rendering setup.
3. Future features like scrollback controls, richer transcript operations, or alternative frontends would be easier to add without growing one very large type.

This would be a good next step because it preserves the current lightweight design instead of replacing it with a framework.
