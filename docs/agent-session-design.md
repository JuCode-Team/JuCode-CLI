# Agent Session Design Notes

This document records reusable design ideas for JuCode-CLI's future agent, session, and context system. It is a design reference, not a commitment to copy another project's implementation.

## Core Philosophy

Treat conversation history as durable data, not as a mutable chat array. The persisted transcript should be the source of truth; the runtime messages sent to the model should be a deterministic projection of that transcript.

Prefer simple, explicit mechanisms:

- Append new entries instead of rewriting old history.
- Move a current-position pointer for rollback and branching.
- Rebuild runtime context from stored entries when switching, forking, compacting, or resuming.
- Store summaries as explicit entries, not hidden side state.
- Keep provider-facing message conversion as the final step.

This fits JuCode-CLI's goals: lightweight, fast, testable, and easy to reason about.

## Suggested Layering

Keep responsibilities narrow:

- `App`: TUI state, editor state, selected model, status, and current visible messages.
- `SessionStore`: durable append-only entries and current leaf pointer.
- `ContextBuilder`: projects a branch of session entries into model-ready messages.
- `AgentRuntime`: owns the live agent loop, tools, skills, and model calls.
- `Ui`: renders state only; it should not own session semantics.

Avoid a broad framework layer. Add modules only when the behavior exists and needs a clear home.

## Session Entry Model

Use a tree-shaped transcript:

```rust
struct SessionEntry {
    id: EntryId,
    parent_id: Option<EntryId>,
    timestamp: SystemTime,
    kind: EntryKind,
}
```

The current conversation position is `leaf_id: Option<EntryId>`.

Appending creates a child of `leaf_id` and advances `leaf_id`. Moving `leaf_id` to an older entry is rollback. Appending after rollback creates a fork. No existing entry needs to be deleted or rewritten.

Useful entry kinds:

- `Message`: user, assistant, tool result, or system-visible message.
- `Compaction`: summary plus first kept entry id.
- `BranchSummary`: summary of an abandoned branch.
- `ModelChange`: selected model on this branch.
- `SkillInvocation`: skill metadata and injected context.
- `Label`: optional bookmark or checkpoint.

## Context Projection

`ContextBuilder` should take all entries plus `leaf_id` and return the exact messages for the model.

Projection algorithm:

1. If `leaf_id` is `None`, return an empty context.
2. Walk from leaf to root through `parent_id`.
3. Reverse into root-to-leaf order.
4. Resolve branch-local state such as model and active settings.
5. Convert only context-bearing entries into runtime messages.
6. If a compaction entry exists on the path, emit its summary first, then include kept messages from `first_kept_entry_id` onward.

This makes context rebuild deterministic and keeps TUI display separate from model input.

## Fork and Rollback

Rollback should be a pointer change:

```text
leaf_id = selected_entry_id
```

For editing a previous user prompt, set `leaf_id` to that prompt's parent and put the original prompt text back into the editor. Submitting creates a new branch from before that prompt.

Forking into a new session should copy only the selected root-to-leaf path into a new transcript, preserving parent links inside that path and recording the source session path as metadata.

## Compaction and Branch Summaries

Compaction should summarize old context while keeping recent work intact. Store a `Compaction` entry containing:

- summary text
- `first_kept_entry_id`
- token estimate before compaction
- optional metadata such as read or modified files

Branch summaries are similar but are created when leaving one branch for another. Find the common ancestor, summarize entries from the old leaf back to that ancestor, then attach the summary at the new position.

Both mechanisms should be opt-in until the basic session tree is stable.

## Transcript to Model Messages

Keep internal entries richer than provider messages. Convert at the boundary:

- shell output becomes a user-style context message unless explicitly excluded
- skill context becomes user-style injected context
- compaction summaries become summary context messages
- normal user, assistant, and tool result messages pass through directly

This avoids provider details leaking into storage.

## Implementation Rules for JuCode-CLI

Keep this design lightweight. Do not introduce a database, async framework, or complex event bus until file-backed sessions and projection tests prove they are needed. Start with in-memory structs and focused unit tests, then add JSONL persistence once the tree behavior is correct.

