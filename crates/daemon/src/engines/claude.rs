//! Claude Code in stream-json print mode:
//! `claude --print --input-format stream-json --output-format stream-json
//!  --include-partial-messages --verbose --replay-user-messages
//!  --permission-prompt-tool stdio`.
//!
//! Frames (verified against claude 2.1.208):
//! - `system/init` at every turn start; `system/status` (requesting,
//!   compacting, permissionMode changes); `system/compact_boundary`.
//! - `stream_event` wraps raw Anthropic stream events; `assistant` carries each
//!   completed block; `user` carries tool results and, with isReplay, the
//!   echo of a message written to stdin; `result` ends a turn.
//! - Permission prompts are `control_request` can_use_tool frames, answered
//!   with `control_response`. Client requests (set_permission_mode,
//!   list_models, set_model, interrupt) are `control_request`s we send.
//! - Mid-turn user messages queue in the CLI and run as the next turn.
//! - Full access (`bypassPermissions`) is only honored when the CLI starts
//!   with `--dangerously-skip-permissions`, so switching into or out of it
//!   restarts the engine on the same conversation.
//!
//! Translated into the jucode event dialect (`docs/serve-protocol.md`) plus
//! the Desktop's extras (`assistant_uuid`, `rate_limit`, `resume_failed`,
//! `model_label`, approval `questions`).

use super::{home, resolve, Adapter, Line, Options, Output};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::Command,
};

pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const DEFAULT_EFFORT: &str = "medium";
const SUMMARY_CAP: usize = 4000;

/// Settings files written for gateway keys (see `use_gateway`).
static GATEWAY_FILES: std::sync::Mutex<Vec<(String, std::path::PathBuf)>> = std::sync::Mutex::new(Vec::new());

/// This session talks to the JuCode gateway through the daemon's local
/// gateway (`base`, see crate::gateway): a settings file (owner-only) that
/// overrides the endpoint and credential for this process alone. The
/// credential is the local `key`, never the JuCode token; the file is named
/// by the session (`id`), not the key, since its path is in the engine's argv
/// for anyone on the machine to read. An empty ANTHROPIC_API_KEY masks one
/// the user's own settings set.
pub fn use_gateway(command: &mut Command, id: &str, base: &str, key: &str) -> Result<(), String> {
    let settings = json!({ "env": {
        "ANTHROPIC_BASE_URL": base,
        "ANTHROPIC_AUTH_TOKEN": key,
        "ANTHROPIC_API_KEY": "",
    } });
    let dir = home().join(".jucode").join("daemon");
    // Older daemons wrote the JuCode token itself here.
    let _ = std::fs::remove_file(dir.join("claude-gateway.json"));
    let mut name: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    if name.is_empty() {
        // No id yet: any name unrelated to the key.
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).map_err(|error| error.to_string())?;
        name = bytes.iter().map(|b| format!("{b:02x}")).collect();
    }
    let path = dir.join(format!("claude-gateway-{name}.json"));
    crate::store::write_private(&path, settings.to_string().as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    GATEWAY_FILES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((key.to_string(), path.clone()));
    command.arg("--settings").arg(path);
    Ok(())
}

/// The engine holding `key` is gone: its settings file goes too.
pub fn forget_gateway(key: &str) {
    let mut files = GATEWAY_FILES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    files.retain(|(k, path)| {
        if k == key {
            let _ = std::fs::remove_file(path);
        }
        k != key
    });
}

pub fn command(id: &str, options: &Options) -> Command {
    let program = match &options.bin {
        Some(bin) => PathBuf::from(bin),
        None => resolve(
            "claude",
            "CLAUDE_BIN",
            &[home().join(".claude").join("local").join("claude")],
        ),
    };
    let mut command = Command::new(program);
    command.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--verbose",
        "--replay-user-messages",
    ]);
    let mode = to_claude_mode(options.approval_mode.as_deref().unwrap_or_default());
    if mode == "bypassPermissions" {
        // Conflicts with --permission-prompt-tool; nothing prompts anyway.
        command.arg("--dangerously-skip-permissions");
    } else {
        command.args([
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
            mode,
        ]);
    }
    match &options.resume {
        Some(resume) => {
            command.args(["--resume", resume]);
        }
        None => {
            command.args(["--session-id", id]);
        }
    }
    if let Some(at) = &options.resume_at {
        command.args(["--resume-session-at", at]);
    }
    if let Some(model) = &options.model {
        command.args(["--model", model]);
    }
    command
}

/// Client approval mode (jucode or Desktop names) → claude permission mode.
pub fn to_claude_mode(mode: &str) -> &'static str {
    match mode {
        "plan" => "plan",
        "auto" => "auto",
        "auto-edit" | "acceptEdits" => "acceptEdits",
        "full-auto" | "full-access" | "bypassPermissions" => "bypassPermissions",
        _ => "default",
    }
}

/// Claude permission mode → the Desktop's engine mode name.
fn from_claude_mode(mode: &str) -> &'static str {
    match mode {
        "plan" => "plan",
        "auto" => "auto",
        "acceptEdits" => "auto-edit",
        "bypassPermissions" | "dontAsk" => "full-auto",
        _ => "read-only",
    }
}

/// Claude tool name → the jucode tool-card name.
fn tool_name(name: &str) -> &str {
    match name {
        "Bash" => "bash",
        "Write" => "write",
        "Edit" | "MultiEdit" | "NotebookEdit" => "str_replace",
        "Read" => "read",
        "Grep" | "Glob" => "ripgrep",
        "WebSearch" => "web_search",
        "WebFetch" => "web_fetch",
        other => other,
    }
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn is_tool_use(block: &Value) -> bool {
    matches!(
        text(&block["type"]),
        "tool_use" | "server_tool_use" | "mcp_tool_use"
    )
}

fn object(value: &Value) -> Value {
    if value.is_object() {
        value.clone()
    } else {
        json!({})
    }
}

fn prefix_lines(text: &str, prefix: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    text.split('\n')
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn pair(old: &str, new: &str) -> String {
    [prefix_lines(old, "-"), prefix_lines(new, "+")]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Unified-diff-ish rendering of an edit tool's input.
fn edit_diff(name: &str, input: &Value) -> String {
    match name {
        "Write" => prefix_lines(text(&input["content"]), "+"),
        "NotebookEdit" => prefix_lines(text(&input["new_source"]), "+"),
        "MultiEdit" => input["edits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|edit| pair(text(&edit["old_string"]), text(&edit["new_string"])))
            .filter(|diff| !diff.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => pair(text(&input["old_string"]), text(&input["new_string"])),
    }
}

fn edit_path(input: &Value) -> &str {
    let path = text(&input["file_path"]);
    if path.is_empty() {
        text(&input["notebook_path"])
    } else {
        path
    }
}

/// One selectable hunk per MultiEdit replacement (two or more).
fn edit_hunks(name: &str, input: &Value) -> Value {
    let edits = input["edits"].as_array().cloned().unwrap_or_default();
    if name != "MultiEdit" || edits.len() < 2 {
        return Value::Null;
    }
    let file = edit_path(input);
    let hunks: Vec<Value> = edits
        .iter()
        .enumerate()
        .map(|(i, edit)| {
            let lines: Vec<String> = pair(text(&edit["old_string"]), text(&edit["new_string"]))
                .split('\n')
                .map(str::to_string)
                .collect();
            json!({ "id": format!("e{i}"), "file": file, "header": format!("edit {}/{}", i + 1, edits.len()), "lines": lines })
        })
        .collect();
    json!(hunks)
}

/// The tool-card JSON Desktop's cards understand, from a tool input.
fn card_json(name: &str, input: &Value) -> String {
    let card = match tool_name(name) {
        "bash" => json!({ "command": text(&input["command"]) }),
        "write" | "str_replace" => {
            let path = edit_path(input);
            let paths: Vec<&str> = if path.is_empty() { vec![] } else { vec![path] };
            json!({ "path": path, "paths": paths, "diff": edit_diff(name, input) })
        }
        "read" => json!({ "path": text(&input["file_path"]) }),
        "ripgrep" => {
            let mut card = json!({ "pattern": text(&input["pattern"]) });
            if !text(&input["path"]).is_empty() {
                card["path"] = input["path"].clone();
            }
            card
        }
        "web_search" => json!({ "query": text(&input["query"]) }),
        "web_fetch" => json!({ "url": text(&input["url"]) }),
        _ => input.clone(),
    };
    card.to_string()
}

fn cap(text: String) -> String {
    if text.chars().count() > SUMMARY_CAP {
        format!("{}…", text.chars().take(SUMMARY_CAP).collect::<String>())
    } else {
        text
    }
}

/// TodoWrite input → a `plan` event.
fn plan_events(input: &Value) -> Vec<Value> {
    let plan: Vec<Value> = input["todos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|todo| {
            let step = [text(&todo["content"]), text(&todo["activeForm"])]
                .into_iter()
                .find(|step| !step.is_empty())?;
            let status = text(&todo["status"]);
            Some(json!({ "step": step, "status": if status.is_empty() { "pending" } else { status } }))
        })
        .collect();
    if plan.is_empty() {
        vec![]
    } else {
        vec![json!({ "type": "plan", "plan": plan })]
    }
}

fn approval_summary(request: &Value) -> String {
    let input = object(&request["input"]);
    let name = text(&request["tool_name"]);
    match name {
        "Bash" => {
            let command = text(&input["command"]);
            let description = text(&input["description"]);
            cap(if !description.is_empty() && description != command {
                format!("{command}\n{description}")
            } else {
                command.to_string()
            })
        }
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            cap([edit_path(&input).to_string(), edit_diff(name, &input)]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "Read" => text(&input["file_path"]).to_string(),
        // The plan in full, so the plan card can copy all of it.
        "ExitPlanMode" => match text(&input["plan"]) {
            "" => request["input"].to_string(),
            plan => plan.to_string(),
        },
        "Task" => {
            let what = [text(&input["description"]), text(&input["prompt"])]
                .into_iter()
                .find(|what| !what.is_empty())
                .unwrap_or_default();
            let summary = [text(&input["subagent_type"]), what]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(": ");
            cap(if summary.is_empty() {
                request["input"].to_string()
            } else {
                summary
            })
        }
        "AskUserQuestion" => {
            let questions: Vec<String> = input["questions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|question| {
                    let options: Vec<String> = question["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|option| match text(&option["description"]) {
                            "" => format!("  • {}", text(&option["label"])),
                            detail => format!("  • {} — {detail}", text(&option["label"])),
                        })
                        .collect();
                    [text(&question["question"]).to_string(), options.join("\n")]
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|question| !question.is_empty())
                .collect();
            cap(if questions.is_empty() {
                request["input"].to_string()
            } else {
                questions.join("\n\n")
            })
        }
        _ => {
            let detail = match text(&request["description"]) {
                "" => request["input"].to_string(),
                detail => detail.to_string(),
            };
            cap(format!("{name}: {detail}"))
        }
    }
}

/// A tool_result's content (string or blocks) as text.
fn result_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .map(|block| text(&block["text"]))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn error_event(message: &str) -> Value {
    let lower = message.to_lowercase();
    let auth = [
        "invalid api key",
        "/login",
        "authentication_error",
        "authentication error",
        "401",
        "unauthorized",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || (lower.contains("oauth") && (lower.contains("expired") || lower.contains("revoked")));
    let hint = if auth {
        " (Claude Code needs to sign in again: run `claude` in a terminal and log in.)"
    } else {
        ""
    };
    json!({ "type": "error", "message": format!("{message}{hint}") })
}

fn context_tokens(usage: &Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .map(|key| usage[*key].as_u64().unwrap_or(0))
    .sum()
}

/// "claude-opus-4-8[1m]" → "Opus 4.8 (1M)", "claude-haiku-4-5-20251001" →
/// "Haiku 4.5".
pub fn compact_model(resolved: &str, raw: &str) -> String {
    let id = if resolved.is_empty() { raw } else { resolved };
    let long = raw.to_lowercase().contains("[1m]") || id.to_lowercase().contains("[1m]");
    let stripped = id.strip_prefix("claude-").unwrap_or(id);
    let stripped = stripped
        .strip_suffix("[1m]")
        .or_else(|| stripped.strip_suffix("[1M]"))
        .unwrap_or(stripped);
    if stripped.is_empty() {
        return raw.to_string();
    }
    let mut parts = stripped.split('-');
    let family = parts.next().unwrap_or_default();
    let family = family
        .chars()
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + &family[first.len_utf8()..])
        .unwrap_or_default();
    let version: Vec<&str> = parts
        .take_while(|part| {
            !part.is_empty() && part.len() != 8 && part.chars().all(|c| c.is_ascii_digit())
        })
        .collect();
    let base = if version.is_empty() {
        family
    } else {
        format!("{family} {}", version.join("."))
    };
    if long {
        format!("{base} (1M)")
    } else {
        base
    }
}

/// `/effort <level>` and its confirmation are bookkeeping, not conversation.
fn is_effort_echo(text: &str) -> bool {
    let text = text.trim();
    text == "/effort"
        || text.starts_with("/effort ")
        || text.to_lowercase().starts_with("set effort level to ")
}

struct ToolMeta {
    claude_name: String,
    name: String,
    input: Value,
}

#[derive(Default)]
struct Block {
    streamed: usize,
    tool: Option<String>,
    partial: String,
}

pub struct Claude {
    /// The claude permission mode the client asked for.
    mode: &'static str,
    requested_model: Option<String>,
    request_seq: u64,
    /// Our outstanding control requests: id → (subtype, tag).
    pending: HashMap<String, (String, String)>,
    started: bool,
    conversation: String,
    model: String,
    effort: String,
    effort_dirty: bool,
    engine_mode: String,
    context_window: u64,
    catalog: Vec<Value>,
    pending_model: String,
    model_windows: HashMap<String, u64>,
    /// Synthetic call id → (control request id, the can_use_tool request).
    approvals: HashMap<String, (String, Value)>,
    approval_seq: u64,
    tools: HashMap<String, ToolMeta>,
    blocks: HashMap<u64, Block>,
    completed_blocks: u64,
    last_context: u64,
    /// The input counts of the message streaming now, from its start: a
    /// `message_delta` may leave them out.
    start_usage: Value,
    active_turn: bool,
    interrupting: bool,
    /// The composer's slash commands: the CLI's `initialize` reply (with
    /// descriptions and argument hints), plus names only `system/init` lists.
    commands: Vec<Value>,
}

impl Claude {
    pub fn new(options: &Options) -> Self {
        Self {
            mode: to_claude_mode(options.approval_mode.as_deref().unwrap_or_default()),
            requested_model: options.model.clone(),
            request_seq: 0,
            pending: HashMap::new(),
            started: false,
            conversation: options.resume.clone().unwrap_or_default(),
            model: String::new(),
            effort: DEFAULT_EFFORT.to_string(),
            effort_dirty: false,
            engine_mode: String::new(),
            context_window: 0,
            catalog: Vec::new(),
            pending_model: String::new(),
            model_windows: HashMap::new(),
            approvals: HashMap::new(),
            approval_seq: 0,
            tools: HashMap::new(),
            blocks: HashMap::new(),
            completed_blocks: 0,
            last_context: 0,
            start_usage: Value::Null,
            active_turn: false,
            interrupting: false,
            commands: Vec::new(),
        }
    }

    fn control_request(&mut self, request: Value, tag: &str) -> String {
        self.request_seq += 1;
        let id = format!("jucode-{}", self.request_seq);
        self.pending.insert(
            id.clone(),
            (text(&request["subtype"]).to_string(), tag.to_string()),
        );
        json!({ "type": "control_request", "request_id": id, "request": request }).to_string()
    }

    fn user_text(text: &str) -> String {
        json!({ "type": "user", "message": { "role": "user", "content": [{ "type": "text", "text": text }] } })
            .to_string()
    }

    fn model_status(&self) -> Value {
        json!({
            "type": "model_status",
            "provider": "anthropic",
            "model": self.model,
            "model_label": if self.model.is_empty() { String::new() } else { compact_model(&self.model, &self.model) },
            "reasoning_effort": self.effort,
            "reasoning_efforts": EFFORT_LEVELS,
            "context_window": self.context_window,
            "context_limit": 0,
            "state": if self.active_turn { "streaming" } else { "ready" },
        })
    }

    /// `/model` and `/resume` are the desktop's own pickers, so they lead
    /// even when the CLI does not list them.
    fn command_list(&self) -> Value {
        let mut commands: Vec<Value> = ["model", "resume"]
            .into_iter()
            .map(|name| {
                let command = format!("/{name}");
                self.commands
                    .iter()
                    .find(|c| c["command"] == command.as_str())
                    .cloned()
                    .unwrap_or_else(|| command_entry(name, "", ""))
            })
            .collect();
        for entry in &self.commands {
            if !commands.iter().any(|c| c["command"] == entry["command"]) {
                commands.push(entry.clone());
            }
        }
        json!({ "type": "command_list", "commands": commands })
    }

    /// The CLI's own commands from its `initialize` reply: built-ins first,
    /// then the user's skills and custom commands.
    fn set_commands(&mut self, reply: &[Value]) {
        let (builtin, others): (Vec<&Value>, Vec<&Value>) = reply
            .iter()
            .filter(|c| !text(&c["name"]).is_empty())
            .partition(|c| c["builtin"] == true);
        self.commands = builtin
            .into_iter()
            .chain(others)
            .map(|c| {
                command_entry(
                    text(&c["name"]),
                    text(&c["argumentHint"]),
                    text(&c["description"]),
                )
            })
            .collect();
    }

    fn model_view(&self) -> Value {
        let visible: Vec<&Value> = self
            .catalog
            .iter()
            .filter(|m| {
                let name = match text(&m["displayName"]) {
                    "" => text(&m["value"]),
                    name => name,
                }
                .to_lowercase();
                !name.contains("default") && !name.contains("recommended")
            })
            .collect();
        let concrete = |m: &Value| -> String {
            match text(&m["resolvedModel"]) {
                "" => text(&m["value"]).to_string(),
                resolved => resolved.to_string(),
            }
        };
        let mut rows: Vec<Value> = visible
            .iter()
            .map(|m| {
                let id = concrete(m);
                json!({
                    "model": m["value"],
                    "label": compact_model(&id, text(&m["value"])),
                    "vendor": id,
                    "active": false,
                    "context_window": self.model_windows.get(&id).copied().unwrap_or(0),
                    "max_output_tokens": 0,
                    "reasoning_efforts": [],
                })
            })
            .collect();
        match visible.iter().position(|m| {
            text(&m["value"]) == self.model || text(&m["resolvedModel"]) == self.model
        }) {
            Some(active) => {
                rows[active]["active"] = json!(true);
                if rows[active]["context_window"] == 0 {
                    rows[active]["context_window"] = json!(self.context_window);
                }
            }
            None if !self.model.is_empty() => rows.insert(
                0,
                json!({
                    "model": self.model,
                    "label": compact_model(&self.model, &self.model),
                    "vendor": self.model,
                    "active": true,
                    "context_window": self.context_window,
                    "max_output_tokens": 0,
                    "reasoning_efforts": [],
                    "listed": false,
                }),
            ),
            None => {}
        }
        json!({ "type": "model_view", "models": rows, "active_effort": "" })
    }

    // --- tools ---

    fn tool_use_events(&mut self, block: &Value, authoritative: bool) -> Vec<Value> {
        let id = text(&block["id"]).to_string();
        if id.is_empty() {
            return vec![];
        }
        let claude_name = text(&block["name"]).to_string();
        let input = object(&block["input"]);
        if claude_name == "TodoWrite" {
            let events = plan_events(&input);
            self.tools.insert(
                id,
                ToolMeta {
                    claude_name,
                    name: "todo".into(),
                    input,
                },
            );
            return events;
        }
        if claude_name == "ExitPlanMode" {
            self.tools.insert(
                id,
                ToolMeta {
                    claude_name: claude_name.clone(),
                    name: claude_name,
                    input,
                },
            );
            return vec![];
        }
        let known = self.tools.contains_key(&id);
        if known && !authoritative {
            return vec![];
        }
        let name = tool_name(&claude_name).to_string();
        let update = json!({ "type": "tool_update", "call_id": id, "output": card_json(&claude_name, &input) });
        self.tools.insert(
            id.clone(),
            ToolMeta {
                claude_name,
                name: name.clone(),
                input,
            },
        );
        if known {
            vec![update]
        } else {
            vec![
                json!({ "type": "tool_start", "call_id": id, "name": name }),
                update,
            ]
        }
    }

    fn tool_block_start(&mut self, block: &Value, index: u64) -> Vec<Value> {
        self.blocks.insert(
            index,
            Block {
                streamed: 0,
                tool: Some(text(&block["id"]).to_string()),
                partial: String::new(),
            },
        );
        self.tool_use_events(block, false)
    }

    fn tool_input_delta(&mut self, index: u64, partial: &str) -> Vec<Value> {
        let Some(track) = self.blocks.get_mut(&index) else {
            return vec![];
        };
        let Some(tool) = track.tool.clone() else {
            return vec![];
        };
        track.partial.push_str(partial);
        let Ok(input @ Value::Object(_)) = serde_json::from_str::<Value>(&track.partial) else {
            return vec![];
        };
        let Some(meta) = self.tools.get_mut(&tool) else {
            return vec![];
        };
        meta.input = input.clone();
        match meta.claude_name.as_str() {
            "TodoWrite" => plan_events(&input),
            "ExitPlanMode" => vec![],
            name => vec![
                json!({ "type": "tool_update", "call_id": tool, "output": card_json(name, &input) }),
            ],
        }
    }

    fn tool_result_events(&mut self, block: &Value, structured: &Value) -> Vec<Value> {
        let id = text(&block["tool_use_id"]).to_string();
        let Some(meta) = self.tools.remove(&id) else {
            return vec![];
        };
        if meta.claude_name == "ExitPlanMode" {
            return vec![];
        }
        let is_error = block["is_error"] == true;
        let body = result_text(&block["content"]);
        let failed = || {
            if body.is_empty() {
                "failed".to_string()
            } else {
                body.clone()
            }
        };
        let output = match meta.name.as_str() {
            "bash" => {
                let mut card = json!({ "command": text(&meta.input["command"]) });
                if is_error {
                    let stderr = text(&structured["stderr"]);
                    card["error"] = json!(if !body.is_empty() {
                        body.clone()
                    } else if !stderr.is_empty() {
                        stderr.to_string()
                    } else {
                        "failed".to_string()
                    });
                } else {
                    card["stdout"] = match structured["stdout"].as_str() {
                        Some(stdout) => json!(stdout),
                        None => json!(body),
                    };
                    if !text(&structured["stderr"]).is_empty() {
                        card["stderr"] = structured["stderr"].clone();
                    }
                }
                card.to_string()
            }
            "write" | "str_replace" => {
                let path = edit_path(&meta.input);
                if is_error {
                    json!({ "path": path, "error": failed() }).to_string()
                } else {
                    let paths: Vec<&str> = if path.is_empty() { vec![] } else { vec![path] };
                    json!({ "path": path, "paths": paths, "diff": edit_diff(&meta.claude_name, &meta.input) }).to_string()
                }
            }
            "read" => {
                let path = text(&meta.input["file_path"]);
                if is_error {
                    json!({ "path": path, "error": failed() }).to_string()
                } else {
                    json!({ "path": path }).to_string()
                }
            }
            "ripgrep" => {
                let mut card = json!({ "pattern": text(&meta.input["pattern"]) });
                if is_error {
                    card["error"] = json!(failed());
                } else {
                    card["stdout"] = json!(body);
                }
                card.to_string()
            }
            _ => body.clone(),
        };
        vec![
            json!({ "type": "tool_output", "call_id": id, "name": meta.name, "output": output, "is_error": is_error }),
        ]
    }

    /// A Task subagent's inner stream: only its tool activity, so the card
    /// that started it fills and completes.
    fn subagent_frame(&mut self, frame: &Value) -> Vec<Value> {
        match text(&frame["type"]) {
            "stream_event" => {
                let event = &frame["event"];
                let index = event["index"].as_u64().unwrap_or(0);
                if event["type"] == "content_block_start" && is_tool_use(&event["content_block"]) {
                    return self.tool_block_start(&event["content_block"], index);
                }
                if event["type"] == "content_block_delta"
                    && event["delta"]["type"] == "input_json_delta"
                {
                    return self.tool_input_delta(index, text(&event["delta"]["partial_json"]));
                }
                vec![]
            }
            "assistant" => frame["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter(|block| is_tool_use(block))
                .flat_map(|block| self.tool_use_events(block, true))
                .collect(),
            "user" => frame["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter(|block| block["type"] == "tool_result")
                .flat_map(|block| self.tool_result_events(block, &frame["tool_use_result"]))
                .collect(),
            _ => vec![],
        }
    }

    // --- frames ---

    fn on_init(&mut self, frame: &Value) -> Vec<Value> {
        self.conversation = text(&frame["session_id"]).to_string();
        let model = text(&frame["model"]).to_string();
        let engine_mode = text(&frame["permissionMode"]).to_string();
        if self.started {
            let mut events = Vec::new();
            if !model.is_empty() && model != self.model {
                let from = std::mem::replace(&mut self.model, model.clone());
                self.context_window = self
                    .model_windows
                    .get(&model)
                    .copied()
                    .unwrap_or(self.context_window);
                events.push(self.model_status());
                let from = if from.is_empty() {
                    String::new()
                } else {
                    format!("{} → ", compact_model(&from, &from))
                };
                events.push(json!({ "type": "info", "message": format!("[claude] model rerouted: {from}{}", compact_model(&model, &model)) }));
            } else if self.effort_dirty {
                self.effort_dirty = false;
                events.push(self.model_status());
            }
            if !engine_mode.is_empty() && engine_mode != self.engine_mode {
                events.push(
                    json!({ "type": "approval_mode", "mode": from_claude_mode(&engine_mode) }),
                );
                self.engine_mode = engine_mode;
            }
            return events;
        }
        self.started = true;
        self.model = model;
        self.engine_mode = engine_mode;
        for name in frame["slash_commands"].as_array().into_iter().flatten() {
            let name = text(name).trim_start_matches('/').trim();
            let command = format!("/{name}");
            if !name.is_empty()
                && !self
                    .commands
                    .iter()
                    .any(|c| c["command"] == command.as_str())
            {
                self.commands.push(command_entry(name, "", ""));
            }
        }
        let mut events = vec![
            json!({
                "type": "startup",
                "model": self.model,
                "cwd": frame["cwd"],
                "session_id": self.conversation,
                "context_window": self.context_window,
            }),
            self.model_status(),
            self.command_list(),
            json!({ "type": "approval_mode", "mode": from_claude_mode(&self.engine_mode) }),
        ];
        if let Some(servers) = frame["mcp_servers"]
            .as_array()
            .filter(|servers| !servers.is_empty())
        {
            let servers: Vec<Value> = servers
                .iter()
                .map(|server| {
                    let state = match text(&server["status"]) {
                        "connected" => "connected",
                        "" => "connecting",
                        other => other,
                    };
                    json!({ "name": server["name"], "transport": "stdio", "state": state, "tools": [] })
                })
                .collect();
            events.push(json!({ "type": "mcp_servers", "servers": servers }));
        }
        events.push(json!({ "type": "status", "message": "ready" }));
        events
    }

    fn on_status(&mut self, frame: &Value) -> Vec<Value> {
        let mut events = Vec::new();
        let engine_mode = text(&frame["permissionMode"]);
        if !engine_mode.is_empty() && engine_mode != self.engine_mode {
            self.engine_mode = engine_mode.to_string();
            events.push(json!({ "type": "approval_mode", "mode": from_claude_mode(engine_mode) }));
        }
        match text(&frame["status"]) {
            "requesting" => {
                self.active_turn = true;
                events.push(json!({ "type": "connecting" }));
            }
            "compacting" => {
                self.active_turn = true;
                events.push(json!({ "type": "compaction_start" }));
            }
            _ => {}
        }
        let result = text(&frame["compact_result"]);
        if !result.is_empty() && result != "success" {
            events.push(json!({ "type": "compaction_failed", "error": result }));
        }
        events
    }

    fn on_stream_event(&mut self, frame: &Value) -> Vec<Value> {
        let event = &frame["event"];
        let index = event["index"].as_u64().unwrap_or(0);
        match text(&event["type"]) {
            "message_start" => {
                self.active_turn = true;
                self.blocks.clear();
                self.completed_blocks = 0;
                self.start_usage = event["message"]["usage"].clone();
                self.last_context = context_tokens(&self.start_usage);
                vec![json!({ "type": "context_usage", "tokens": self.last_context })]
            }
            "content_block_start" => {
                let block = &event["content_block"];
                if block["type"] == "text" {
                    self.blocks.insert(index, Block::default());
                    return vec![json!({ "type": "assistant_start" })];
                }
                if is_tool_use(block) {
                    return self.tool_block_start(block, index);
                }
                self.blocks.insert(index, Block::default());
                vec![]
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                match text(&delta["type"]) {
                    "text_delta" => {
                        let chunk = text(&delta["text"]);
                        if let Some(track) = self.blocks.get_mut(&index) {
                            track.streamed += chunk.len();
                        }
                        if chunk.is_empty() {
                            vec![]
                        } else {
                            vec![json!({ "type": "assistant_delta", "delta": chunk })]
                        }
                    }
                    "thinking_delta" => {
                        let chunk = text(&delta["thinking"]);
                        if let Some(track) = self.blocks.get_mut(&index) {
                            track.streamed += chunk.len();
                        }
                        if chunk.is_empty() {
                            vec![]
                        } else {
                            vec![json!({ "type": "reasoning_delta", "delta": chunk })]
                        }
                    }
                    "input_json_delta" => {
                        self.tool_input_delta(index, text(&delta["partial_json"]))
                    }
                    _ => vec![],
                }
            }
            "message_delta" => {
                let usage = &event["usage"];
                if !usage.is_object() {
                    return vec![];
                }
                let output = usage["output_tokens"].as_u64().unwrap_or(0);
                let input = if context_tokens(usage) > 0 {
                    usage
                } else {
                    &self.start_usage
                };
                let count = |key: &str| input[key].as_u64().unwrap_or(0);
                self.last_context = context_tokens(input) + output;
                vec![
                    json!({
                        "type": "usage",
                        "input_tokens": context_tokens(input),
                        "cached_input_tokens": count("cache_read_input_tokens"),
                        "cache_write_tokens": count("cache_creation_input_tokens"),
                        "output_tokens": output,
                    }),
                    json!({ "type": "context_usage", "tokens": self.last_context }),
                ]
            }
            _ => vec![],
        }
    }

    fn on_assistant(&mut self, frame: &Value) -> Vec<Value> {
        let mut events = Vec::new();
        for block in frame["message"]["content"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            // The k-th completed block is stream index k.
            let streamed = self
                .blocks
                .get(&self.completed_blocks)
                .map_or(0, |b| b.streamed);
            self.completed_blocks += 1;
            match text(&block["type"]) {
                "text" => {
                    let full = text(&block["text"]);
                    let tail = full.get(streamed..).unwrap_or_default();
                    if tail.is_empty() || is_effort_echo(full) {
                        continue;
                    }
                    if streamed == 0 {
                        events.push(json!({ "type": "assistant_start" }));
                    }
                    events.push(json!({ "type": "assistant_delta", "delta": tail }));
                }
                "thinking" => {
                    let tail = text(&block["thinking"]).get(streamed..).unwrap_or_default();
                    if !tail.is_empty() {
                        events.push(json!({ "type": "reasoning_delta", "delta": tail }));
                    }
                }
                _ if is_tool_use(&block) => events.extend(self.tool_use_events(&block, true)),
                _ => {}
            }
        }
        let uuid = text(&frame["uuid"]);
        if !uuid.is_empty() {
            events.push(json!({ "type": "assistant_uuid", "uuid": uuid }));
        }
        events
    }

    fn on_user(&mut self, frame: &Value) -> Vec<Value> {
        let content = &frame["message"]["content"];
        if frame["isReplay"] == true {
            let message = match content.as_str() {
                Some(message) => message.to_string(),
                None => content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|block| block["type"] == "text")
                    .map(|block| text(&block["text"]).to_string())
                    .unwrap_or_default(),
            };
            let trimmed = message.trim();
            if trimmed.starts_with("<command-")
                || trimmed.starts_with("<local-command-")
                || is_effort_echo(&message)
                || message.is_empty()
            {
                return vec![];
            }
            return vec![json!({ "type": "user_message", "content": message })];
        }
        content
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter(|block| block["type"] == "tool_result")
            .flat_map(|block| self.tool_result_events(block, &frame["tool_use_result"]))
            .collect()
    }

    fn on_result(&mut self, frame: &Value) -> Vec<Value> {
        self.active_turn = false;
        let was_interrupting = std::mem::take(&mut self.interrupting);
        let mut events = Vec::new();
        if let Some(usage) = frame["modelUsage"].as_object() {
            for (name, usage) in usage {
                if let Some(window) = usage["contextWindow"].as_u64().filter(|w| *w > 0) {
                    self.model_windows.insert(name.clone(), window);
                }
            }
        }
        if let Some(window) = self.model_windows.get(&self.model).copied() {
            if window != self.context_window {
                self.context_window = window;
                events.push(self.model_status());
            }
        }
        if let Some(cost) = frame["total_cost_usd"].as_f64() {
            events.push(
                json!({ "type": "context_usage", "tokens": self.last_context, "cost": cost }),
            );
        }
        let subtype = text(&frame["subtype"]);
        let failed = frame["is_error"] == true || subtype.to_lowercase().starts_with("error");
        if failed && !was_interrupting {
            let errors = frame["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .map(text)
                .filter(|error| !error.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            let detail = [text(&frame["result"]), errors.as_str(), subtype]
                .into_iter()
                .find(|detail| !detail.is_empty())
                .unwrap_or("error")
                .to_string();
            if detail.contains("No conversation found with session ID") {
                events.push(json!({ "type": "resume_failed" }));
            } else {
                events.push(error_event(&detail));
            }
        }
        events.push(json!({ "type": "status", "message": "ready" }));
        events
    }

    fn on_control_request(&mut self, frame: &Value) -> Output {
        let request = &frame["request"];
        let request_id = text(&frame["request_id"]).to_string();
        if request["subtype"] != "can_use_tool" {
            let subtype = text(&request["subtype"]);
            return Output {
                frames: vec![json!({
                    "type": "control_response",
                    "response": { "subtype": "error", "request_id": request_id, "error": format!("unsupported by client: {subtype}") },
                })
                .to_string()],
                events: vec![json!({ "type": "info", "message": format!("[claude] unsupported request: {subtype}") })],
            };
        }
        let tool = text(&request["tool_name"]).to_string();
        let input = object(&request["input"]);
        self.approval_seq += 1;
        let call = format!("approval-{}", self.approval_seq);
        if tool == "AskUserQuestion" {
            let event = json!({
                "type": "approval_request",
                "call_id": call,
                "name": "ask_question",
                "summary": approval_summary(request),
                "subagent_id": null,
                "hunks": null,
                "questions": input.get("questions").cloned().unwrap_or(json!([])),
            });
            self.approvals.insert(call, (request_id, request.clone()));
            return Output::events(vec![event]);
        }
        let mut events = Vec::new();
        let tool_id = text(&request["tool_use_id"]).to_string();
        if tool != "ExitPlanMode" && !tool_id.is_empty() {
            let name = tool_name(&tool).to_string();
            let known = self.tools.contains_key(&tool_id);
            self.tools.insert(
                tool_id.clone(),
                ToolMeta {
                    claude_name: tool.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
            );
            if !known {
                events.push(json!({ "type": "tool_start", "call_id": tool_id, "name": name }));
            }
            events.push(json!({ "type": "tool_update", "call_id": tool_id, "output": card_json(&tool, &input) }));
        }
        events.push(json!({
            "type": "approval_request",
            "call_id": call,
            "name": tool_name(&tool),
            "summary": approval_summary(request),
            "subagent_id": null,
            "hunks": edit_hunks(&tool, &input),
        }));
        self.approvals.insert(call, (request_id, request.clone()));
        Output::events(events)
    }

    fn on_control_response(&mut self, frame: &Value) -> Vec<Value> {
        let response = &frame["response"];
        let Some((subtype, tag)) = self.pending.remove(text(&response["request_id"])) else {
            return vec![];
        };
        if response["subtype"] == "error" {
            if subtype == "set_model" {
                self.pending_model.clear();
            }
            let message = match text(&response["error"]) {
                "" => format!("{subtype} failed"),
                error => error.to_string(),
            };
            return vec![error_event(&message)];
        }
        match subtype.as_str() {
            "set_permission_mode" => {
                if self.started {
                    return vec![];
                }
                // The first ack means the engine is up; its init only comes
                // with the first turn.
                let acked = text(&response["response"]["mode"]);
                self.engine_mode = if acked.is_empty() {
                    self.mode.to_string()
                } else {
                    acked.to_string()
                };
                vec![
                    json!({ "type": "approval_mode", "mode": from_claude_mode(&self.engine_mode) }),
                    self.command_list(),
                    json!({ "type": "status", "message": "ready" }),
                ]
            }
            "list_models" => {
                self.catalog = response["response"]["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| !text(&m["value"]).is_empty())
                    .cloned()
                    .collect();
                if tag == "view" {
                    return vec![self.model_view()];
                }
                let mut events = Vec::new();
                if self.model.is_empty() {
                    let default = self
                        .catalog
                        .iter()
                        .find(|m| {
                            let name = match text(&m["displayName"]) {
                                "" => text(&m["value"]),
                                name => name,
                            }
                            .to_lowercase();
                            name.contains("default") || name.contains("recommended")
                        })
                        .or(self.catalog.first());
                    if let Some(default) = default {
                        let resolved = match text(&default["resolvedModel"]) {
                            "" => text(&default["value"]),
                            r => r,
                        };
                        if !resolved.is_empty() {
                            self.model = resolved.to_string();
                            events.push(self.model_status());
                        }
                    }
                }
                if tag == "boot" {
                    self.engine_mode = "bypassPermissions".to_string();
                    events.push(json!({ "type": "approval_mode", "mode": "full-auto" }));
                    events.push(self.command_list());
                    events.push(json!({ "type": "status", "message": "ready" }));
                }
                events
            }
            "initialize" => {
                self.set_commands(
                    response["response"]["commands"]
                        .as_array()
                        .map_or(&[], Vec::as_slice),
                );
                vec![self.command_list()]
            }
            "set_model" => {
                let pick = std::mem::take(&mut self.pending_model);
                if pick.is_empty() {
                    return vec![];
                }
                let entry = self
                    .catalog
                    .iter()
                    .find(|m| text(&m["value"]) == pick || text(&m["resolvedModel"]) == pick);
                self.model = entry
                    .map(|m| match text(&m["resolvedModel"]) {
                        "" => text(&m["value"]).to_string(),
                        r => r.to_string(),
                    })
                    .unwrap_or(pick);
                self.context_window = self.model_windows.get(&self.model).copied().unwrap_or(0);
                vec![self.model_status()]
            }
            _ => vec![],
        }
    }

    /// Session-scoped always-allow rules: the CLI's own suggestions, else the
    /// whole tool.
    fn always_permissions(request: &Value) -> Value {
        let suggested: Vec<Value> = request["permission_suggestions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| {
                s["type"] == "addRules" && s["behavior"] == "allow" && s["rules"].is_array()
            })
            .map(|s| {
                let mut s = s.clone();
                s["destination"] = json!("session");
                s
            })
            .collect();
        if suggested.is_empty() {
            json!([{ "type": "addRules", "rules": [{ "toolName": request["tool_name"] }], "behavior": "allow", "destination": "session" }])
        } else {
            json!(suggested)
        }
    }
}

fn command_entry(name: &str, args: &str, description: &str) -> Value {
    json!({ "command": format!("/{name}"), "marker": null, "args": args, "description": description })
}

impl Adapter for Claude {
    fn start(&mut self) -> Vec<String> {
        let mut frames = if self.mode == "bypassPermissions" {
            vec![self.control_request(json!({ "subtype": "list_models" }), "boot")]
        } else {
            let mode = self.mode;
            vec![
                self.control_request(
                    json!({ "subtype": "set_permission_mode", "mode": mode }),
                    "",
                ),
                self.control_request(json!({ "subtype": "list_models" }), ""),
            ]
        };
        // The command list with descriptions, before the first turn.
        frames.push(self.control_request(json!({ "subtype": "initialize" }), ""));
        frames
    }

    fn translate(&mut self, line: Line) -> Output {
        let frame = match line {
            Line::Stderr(line) => {
                let line = super::strip_ansi(&line);
                let line = line.trim();
                if line.is_empty()
                    || matches!(super::log_level(line), Some("INFO" | "DEBUG" | "TRACE"))
                {
                    return Output::default();
                }
                if line.contains("No conversation found with session ID") {
                    return Output::events(vec![json!({ "type": "resume_failed" })]);
                }
                return Output::events(vec![
                    json!({ "type": "info", "message": format!("[claude] {line}") }),
                ]);
            }
            Line::Frame(frame) => frame,
        };
        if !text(&frame["parent_tool_use_id"]).is_empty() {
            return Output::events(self.subagent_frame(&frame));
        }
        let events = match text(&frame["type"]) {
            "system" => match text(&frame["subtype"]) {
                "init" => self.on_init(&frame),
                "status" => self.on_status(&frame),
                "compact_boundary" => vec![json!({ "type": "compaction_end" })],
                "permission_denied" => {
                    let reason = text(&frame["reason"]);
                    let reason = if reason.is_empty() {
                        String::new()
                    } else {
                        format!(": {reason}")
                    };
                    vec![
                        json!({ "type": "info", "message": format!("[claude] {} denied{reason}", tool_name(text(&frame["tool_name"]))) }),
                    ]
                }
                "files_persisted" => {
                    let names: Vec<&str> = frame["files"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|f| match text(&f["filename"]) {
                            "" => text(&f["fileId"]),
                            name => name,
                        })
                        .filter(|name| !name.is_empty())
                        .collect();
                    if names.is_empty() {
                        vec![]
                    } else {
                        vec![
                            json!({ "type": "info", "message": format!("[claude] saved: {}", names.join(", ")) }),
                        ]
                    }
                }
                "hook_response" if frame["exit_code"].as_i64().is_some_and(|code| code != 0) => {
                    let detail = [text(&frame["stderr"]), text(&frame["output"])]
                        .into_iter()
                        .find(|d| !d.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("exit {}", frame["exit_code"]));
                    vec![
                        json!({ "type": "info", "message": format!("[claude] hook {} failed: {detail}", text(&frame["hook_name"])) }),
                    ]
                }
                "mirror_error" => vec![error_event(match text(&frame["error"]) {
                    "" => "workspace mirror error",
                    e => e,
                })],
                _ => vec![],
            },
            "stream_event" => self.on_stream_event(&frame),
            "assistant" => self.on_assistant(&frame),
            "user" => self.on_user(&frame),
            "result" => self.on_result(&frame),
            "control_request" => return self.on_control_request(&frame),
            "control_response" => self.on_control_response(&frame),
            "rate_limit_event" => rate_limit_events(&frame),
            _ => vec![],
        };
        Output::events(events)
    }

    fn encode(&mut self, op: &Value) -> Result<Output, String> {
        let frames = match text(&op["op"]) {
            "user_message" => {
                let mut content = vec![json!({ "type": "text", "text": op["content"] })];
                for image in op["images"].as_array().into_iter().flatten() {
                    content.push(json!({ "type": "text", "text": format!("[Attached image: {} — open it with the Read tool]", text(image)) }));
                }
                vec![
                    json!({ "type": "user", "message": { "role": "user", "content": content } })
                        .to_string(),
                ]
            }
            "approve" => {
                let call = text(&op["call_id"]);
                let Some((request_id, request)) = self.approvals.remove(call) else {
                    return Err(format!("no open approval {call}"));
                };
                let input = object(&request["input"]);
                let tool = text(&request["tool_name"]);
                let mut updated = input.clone();
                if tool == "AskUserQuestion" && !op["answers"].is_null() {
                    updated = json!({ "questions": input["questions"], "answers": op["answers"] });
                } else if tool == "MultiEdit" {
                    if let (Some(keep), Some(edits)) =
                        (op["hunks"].as_array(), input["edits"].as_array())
                    {
                        let keep: Vec<&str> = keep.iter().map(text).collect();
                        let edits: Vec<Value> = edits
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| keep.contains(&format!("e{i}").as_str()))
                            .map(|(_, edit)| edit.clone())
                            .collect();
                        updated["edits"] = json!(edits);
                    }
                }
                let result = if op["decision"] == "deny" {
                    json!({ "behavior": "deny", "message": "The user denied this tool use." })
                } else {
                    let mut allow = json!({ "behavior": "allow", "updatedInput": updated });
                    if op["always"] == true {
                        allow["updatedPermissions"] = Self::always_permissions(&request);
                    }
                    allow
                };
                vec![json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": result } }).to_string()]
            }
            "interrupt" => {
                if !self.active_turn {
                    return Ok(Output::default());
                }
                self.interrupting = true;
                vec![self.control_request(json!({ "subtype": "interrupt" }), "")]
            }
            "set_approval_mode" => {
                let mode = to_claude_mode(text(&op["mode"]));
                self.mode = mode;
                vec![self.control_request(
                    json!({ "subtype": "set_permission_mode", "mode": mode }),
                    "",
                )]
            }
            "command" => {
                let input = text(&op["input"]).trim();
                let (command, arg) = match input.split_once(' ') {
                    Some((command, arg)) => (command, arg.trim()),
                    None => (input, ""),
                };
                match command {
                    "/model" if arg.is_empty() => {
                        vec![self.control_request(json!({ "subtype": "list_models" }), "view")]
                    }
                    "/model" => {
                        let mut frames = Vec::new();
                        let parts: Vec<&str> = arg.split_whitespace().collect();
                        if let Some(effort) = parts.iter().find(|p| EFFORT_LEVELS.contains(p)) {
                            if *effort != self.effort {
                                self.effort = effort.to_string();
                                self.effort_dirty = true;
                                frames.push(Self::user_text(&format!("/effort {effort}")));
                            }
                        }
                        if let Some(name) = parts.iter().find(|p| !EFFORT_LEVELS.contains(p)) {
                            if *name != self.model {
                                self.pending_model = name.to_string();
                                frames.push(self.control_request(
                                    json!({ "subtype": "set_model", "model": name }),
                                    "",
                                ));
                            }
                        }
                        frames
                    }
                    "/resume" | "/rewind" | "/tree" | "/checkout" | "/fork" | "/undo" | "/new" => {
                        return Err(format!(
                            "{command} is not available in a Claude Code session"
                        ))
                    }
                    _ => vec![Self::user_text(input)],
                }
            }
            "shutdown" => vec![],
            other => return Err(format!("Claude Code sessions do not support {other}")),
        };
        Ok(Output {
            events: Vec::new(),
            frames,
        })
    }

    fn busy(&self) -> bool {
        self.active_turn
    }

    fn restart_for(&self, op: &Value) -> Option<Options> {
        if op["op"] != "set_approval_mode" {
            return None;
        }
        let next = to_claude_mode(text(&op["mode"]));
        let bypass = |mode: &str| mode == "bypassPermissions";
        (bypass(next) != bypass(self.mode)).then(|| Options {
            approval_mode: Some(next.to_string()),
            model: (!self.model.is_empty())
                .then(|| self.model.clone())
                .or(self.requested_model.clone()),
            resume: (!self.conversation.is_empty()).then(|| self.conversation.clone()),
            resume_at: None,
            ..Options::default()
        })
    }

    fn conversation(&self) -> Option<String> {
        (!self.conversation.is_empty()).then(|| self.conversation.clone())
    }

    fn approval_mode(&self) -> Option<String> {
        Some(from_claude_mode(self.mode).to_string())
    }
}

/// A subscription's usage from a `rate_limit_info`: per window when the
/// event carries `unifiedWindows` (5-hour, weekly), else the limiting window.
/// `utilization` is the share used, 0-1 (above 1 past a cap). Absent for
/// API-key sessions.
fn plan_usage(info: &Value) -> Option<Value> {
    const WINDOWS: [(&str, u64); 3] =
        [("five_hour", 300), ("seven_day", 10_080), ("seven_day_overage_included", 10_080)];
    let mut windows = Vec::new();
    if let Some(unified) = info["unifiedWindows"].as_object() {
        for (key, minutes) in WINDOWS {
            if let Some(used) = unified.get(key).and_then(|w| w["utilization"].as_f64()) {
                windows.push(super::plan_window(key, used * 100.0, &unified[key]["resetsAt"], Some(minutes)));
            }
        }
    } else if let (Some(used), Some(key)) = (info["utilization"].as_f64(), info["rateLimitType"].as_str()) {
        let minutes = WINDOWS.iter().find(|(k, _)| *k == key).map(|(_, m)| *m);
        windows.push(super::plan_window(key, used * 100.0, &info["resetsAt"], minutes));
    }
    (!windows.is_empty()).then(|| json!({ "type": "plan_usage", "plan": null, "windows": windows }))
}

fn rate_limit_events(frame: &Value) -> Vec<Value> {
    // Claude Code nests it as `rate_limit_info`; older builds as `rate_limit`.
    let info = ["rate_limit_info", "rate_limit"]
        .iter()
        .map(|key| &frame[*key])
        .find(|value| value.is_object())
        .unwrap_or(frame);
    let mut events = limit_banner(frame, info);
    events.extend(plan_usage(info));
    events
}

fn limit_banner(frame: &Value, info: &Value) -> Vec<Value> {
    let status = [text(&info["status"]), text(&frame["status"])]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_lowercase();
    let reset = ["resetsAt", "resets_at", "resetAt"]
        .iter()
        .find_map(|key| info[*key].as_f64())
        .or_else(|| frame["resetsAt"].as_f64())
        .unwrap_or(0.0);
    let resets_at = if reset > 1e12 {
        json!(reset as u64)
    } else if reset > 1e9 {
        json!((reset * 1000.0) as u64)
    } else {
        Value::Null
    };
    let message = [
        text(&info["message"]),
        text(&frame["message"]),
        status.as_str(),
    ]
    .into_iter()
    .find(|m| !m.is_empty())
    .unwrap_or_default()
    .to_string();
    let has = |words: &[&str]| words.iter().any(|word| status.contains(word));
    if has(&["reject", "exceed", "limit_reached", "throttl"]) {
        vec![
            json!({ "type": "rate_limit", "level": "limited", "message": message, "resets_at": resets_at }),
        ]
    } else if has(&["warn", "approaching"]) {
        vec![
            json!({ "type": "rate_limit", "level": "warning", "message": message, "resets_at": resets_at }),
        ]
    } else if has(&["allow", "ok", "normal"]) {
        vec![json!({ "type": "rate_limit", "level": "ok", "message": "", "resets_at": null })]
    } else {
        vec![]
    }
}

// --- saved conversations ---

/// Claude Code's project directory for `cwd`: every character of the real
/// path (Claude Code resolves symlinks, e.g. macOS `/tmp` → `/private/tmp`)
/// that is not ASCII alphanumeric becomes `-`.
fn project_dir(home: &Path, cwd: &Path) -> PathBuf {
    let real = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let munged: String = real
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    home.join(".claude").join("projects").join(munged)
}

fn is_conversation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('-')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Rows Claude adds that are not conversation: subagent streams, meta notes,
/// the post-compaction summary.
fn is_synthetic(row: &Value) -> bool {
    [
        "isSidechain",
        "isMeta",
        "isCompactSummary",
        "isVisibleInTranscriptOnly",
    ]
    .iter()
    .any(|key| row[*key] == true)
}

fn row_text(row: &Value) -> Option<String> {
    let content = &row["message"]["content"];
    if let Some(text) = content.as_str() {
        let text = text.trim();
        return (!text.is_empty() && !text.starts_with('<')).then(|| text.to_string());
    }
    let text = content
        .as_array()?
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .filter(|text| !text.trim().is_empty() && !text.trim_start().starts_with('<'))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn rows(path: &Path, limit: u64) -> impl Iterator<Item = Value> {
    let reader = fs::File::open(path)
        .ok()
        .map(|file| BufReader::new(file.take(limit)));
    reader
        .into_iter()
        .flat_map(|reader| reader.lines().map_while(Result::ok))
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
}

/// The user and assistant text of a saved conversation, as transcript items
/// (the newest 500).
pub fn transcript(cwd: &Path, id: &str) -> Vec<Value> {
    transcript_in(&home(), cwd, id)
}

fn transcript_in(home: &Path, cwd: &Path, id: &str) -> Vec<Value> {
    if !is_conversation_id(id) {
        return vec![];
    }
    let mut items: Vec<Value> = rows(
        &project_dir(home, cwd).join(format!("{id}.jsonl")),
        64 * 1024 * 1024,
    )
    .filter(|row| matches!(text(&row["type"]), "user" | "assistant") && !is_synthetic(row))
    .filter_map(|row| {
        row_text(&row).map(|content| json!({ "role": row["type"], "content": content }))
    })
    .collect();
    let excess = items.len().saturating_sub(500);
    items.drain(..excess);
    items
}

/// Conversations saved for `cwd`, newest first: (id, title, updated ms).
pub fn saved(cwd: &Path) -> Vec<(String, String, u64)> {
    saved_in(&home(), cwd)
}

fn saved_in(home: &Path, cwd: &Path) -> Vec<(String, String, u64)> {
    let Ok(entries) = fs::read_dir(project_dir(home, cwd)) else {
        return vec![];
    };
    let mut found: Vec<(String, u64, PathBuf)> = entries
        .flatten()
        .take(2000)
        .filter_map(|entry| {
            let path = entry.path();
            let id = path.file_stem()?.to_str()?.to_string();
            if path.extension()? != "jsonl" || !is_conversation_id(&id) {
                return None;
            }
            let modified = entry
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as u64;
            Some((id, modified, path))
        })
        .collect();
    found.sort_by_key(|(_, modified, _)| std::cmp::Reverse(*modified));
    found.truncate(50);
    found
        .into_iter()
        .map(|(id, modified, path)| (id, title(&path), modified))
        .collect()
}

/// The stored AI title, else the first user message's first line.
fn title(path: &Path) -> String {
    let mut first = String::new();
    for row in rows(path, 256 * 1024) {
        match text(&row["type"]) {
            "ai-title" if !text(&row["aiTitle"]).trim().is_empty() => {
                return text(&row["aiTitle"]).trim().chars().take(80).collect();
            }
            "user" if first.is_empty() && !is_synthetic(&row) => {
                if let Some(text) = row_text(&row) {
                    first = text
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .take(80)
                        .collect();
                }
            }
            _ => {}
        }
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> Claude {
        Claude::new(&Options::default())
    }

    fn frame(claude: &mut Claude, value: Value) -> Vec<Value> {
        claude.translate(Line::Frame(value)).events
    }

    fn types(events: &[Value]) -> Vec<&str> {
        events.iter().map(|e| text(&e["type"])).collect()
    }

    #[test]
    fn the_gateway_key_stays_out_of_the_command_line() {
        let mut command = Command::new("claude");
        use_gateway(&mut command, "sess-1", "http://127.0.0.1:7788/gw", "jgw-secretkey").unwrap();
        let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(args.iter().all(|a| !a.contains("secretkey")), "{args:?}");
        let path = args.last().unwrap().clone();
        assert!(path.ends_with("claude-gateway-sess-1.json"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("jgw-secretkey"));
        forget_gateway("jgw-secretkey");
        assert!(!std::path::Path::new(&path).exists());
    }

    #[test]
    fn a_rate_limit_event_shows_the_banner_and_the_plan_usage() {
        let mut c = claude();
        // The shape Claude Code 2.1 emits for a subscription session.
        let events = frame(
            &mut c,
            json!({ "type": "rate_limit_event", "rate_limit_info": {
                "status": "allowed_warning", "resetsAt": 1790800000, "rateLimitType": "five_hour", "utilization": 0.83,
                "unifiedWindows": {
                    "five_hour": { "utilization": 0.83, "resetsAt": 1790800000 },
                    "seven_day": { "utilization": 0.412, "resetsAt": 1791300000 }
                }
            } }),
        );
        assert_eq!(types(&events), ["rate_limit", "plan_usage"]);
        assert_eq!(events[0]["level"], "warning");
        assert_eq!(events[0]["resets_at"], 1_790_800_000_000u64);
        let windows = events[1]["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0], json!({ "key": "five_hour", "used": 83.0, "resets_at": 1_790_800_000_000u64, "minutes": 300 }));
        assert_eq!(windows[1]["used"], 41.2);
        // Only the limiting window: that one.
        let events = frame(
            &mut c,
            json!({ "type": "rate_limit_event", "rate_limit_info": { "status": "allowed", "rateLimitType": "seven_day", "utilization": 0.2 } }),
        );
        assert_eq!(types(&events), ["rate_limit", "plan_usage"]);
        assert_eq!(events[1]["windows"][0]["minutes"], 10_080);
        // An API-key session: status only.
        let events = frame(&mut c, json!({ "type": "rate_limit_event", "rate_limit_info": { "status": "allowed" } }));
        assert_eq!(types(&events), ["rate_limit"]);
    }

    #[test]
    fn a_turn_streams_text_and_tools_and_ends_ready() {
        let mut c = claude();
        let boot = c.start();
        assert_eq!(boot.len(), 3);
        assert!(boot[0].contains("set_permission_mode") && boot[0].contains("\"default\""));
        assert!(boot[2].contains("initialize"));
        let ready = frame(
            &mut c,
            json!({ "type": "control_response", "response": { "subtype": "success", "request_id": "jucode-1", "response": { "mode": "default" } } }),
        );
        assert_eq!(types(&ready), ["approval_mode", "command_list", "status"]);
        let listed = frame(
            &mut c,
            json!({ "type": "control_response", "response": { "subtype": "success", "request_id": "jucode-3", "response": { "commands": [
                { "name": "review", "description": "Review a diff (user)", "argumentHint": "" },
                { "name": "compact", "description": "Free up context", "argumentHint": "<instructions>", "builtin": true },
                { "name": "model", "description": "Set the AI model", "argumentHint": "<model>", "builtin": true }
            ] } } }),
        );
        let names: Vec<&str> = listed[0]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["command"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["/model", "/resume", "/compact", "/review"]);
        assert_eq!(listed[0]["commands"][2]["args"], "<instructions>");
        assert_eq!(listed[0]["commands"][2]["description"], "Free up context");

        let init = frame(
            &mut c,
            json!({ "type": "system", "subtype": "init", "session_id": "abc", "model": "claude-opus-4-8", "permissionMode": "default", "cwd": "/p", "slash_commands": ["compact", "context"] }),
        );
        assert_eq!(
            types(&init),
            [
                "startup",
                "model_status",
                "command_list",
                "approval_mode",
                "status"
            ]
        );
        assert_eq!(init[1]["model_label"], "Opus 4.8");
        // Names only the init frame lists join the described ones.
        let names: Vec<&str> = init[2]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["command"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["/model", "/resume", "/compact", "/review", "/context"]
        );
        assert_eq!(c.conversation().as_deref(), Some("abc"));

        assert_eq!(
            types(&frame(
                &mut c,
                json!({ "type": "system", "subtype": "status", "status": "requesting" })
            )),
            ["connecting"]
        );
        assert!(c.busy());
        frame(
            &mut c,
            json!({ "type": "stream_event", "event": { "type": "message_start", "message": { "usage": { "input_tokens": 10 } } } }),
        );
        let usage = frame(
            &mut c,
            json!({ "type": "stream_event", "event": { "type": "message_delta", "usage": { "input_tokens": 10, "cache_read_input_tokens": 3000, "cache_creation_input_tokens": 40, "output_tokens": 5 } } }),
        );
        assert_eq!(
            usage[0],
            json!({ "type": "usage", "input_tokens": 3050, "cached_input_tokens": 3000, "cache_write_tokens": 40, "output_tokens": 5 })
        );
        frame(
            &mut c,
            json!({ "type": "stream_event", "event": { "type": "message_start", "message": { "usage": { "input_tokens": 7, "cache_read_input_tokens": 100 } } } }),
        );
        let usage = frame(
            &mut c,
            json!({ "type": "stream_event", "event": { "type": "message_delta", "usage": { "output_tokens": 2 } } }),
        );
        assert_eq!(usage[0]["input_tokens"], 107);
        assert_eq!(usage[0]["cached_input_tokens"], 100);
        assert_eq!(
            types(&frame(
                &mut c,
                json!({ "type": "stream_event", "event": { "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } } })
            )),
            ["assistant_start"]
        );
        let delta = frame(
            &mut c,
            json!({ "type": "stream_event", "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hel" } } }),
        );
        assert_eq!(delta[0]["delta"], "Hel");
        // The completed block only adds what did not stream.
        let done = frame(
            &mut c,
            json!({ "type": "assistant", "uuid": "u1", "message": { "content": [{ "type": "text", "text": "Hello" }] } }),
        );
        assert_eq!(done[0], json!({ "type": "assistant_delta", "delta": "lo" }));
        assert_eq!(done[1]["type"], "assistant_uuid");

        let start = frame(
            &mut c,
            json!({ "type": "stream_event", "event": { "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "t1", "name": "Bash", "input": {} } } }),
        );
        assert_eq!(types(&start), ["tool_start", "tool_update"]);
        assert_eq!(start[0]["name"], "bash");
        let filled = frame(
            &mut c,
            json!({ "type": "stream_event", "event": { "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"command\":\"ls\"}" } } }),
        );
        assert_eq!(filled[0]["output"], "{\"command\":\"ls\"}");
        let result = frame(
            &mut c,
            json!({ "type": "user", "message": { "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "a\nb" }] }, "tool_use_result": { "stdout": "a\nb" } }),
        );
        assert_eq!(result[0]["type"], "tool_output");
        assert_eq!(
            serde_json::from_str::<Value>(text(&result[0]["output"])).unwrap()["stdout"],
            "a\nb"
        );

        let end = frame(
            &mut c,
            json!({ "type": "result", "subtype": "success", "total_cost_usd": 0.5, "modelUsage": { "claude-opus-4-8": { "contextWindow": 200000 } } }),
        );
        assert_eq!(types(&end), ["model_status", "context_usage", "status"]);
        assert!(!c.busy());
    }

    #[test]
    fn a_permission_prompt_becomes_an_approval_answered_once() {
        let mut c = claude();
        let events = frame(
            &mut c,
            json!({ "type": "control_request", "request_id": "r9", "request": { "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t2", "input": { "command": "rm -rf build" } } }),
        );
        assert_eq!(
            types(&events),
            ["tool_start", "tool_update", "approval_request"]
        );
        let call = text(&events[2]["call_id"]).to_string();
        assert_eq!(events[2]["summary"], "rm -rf build");

        let reply = c
            .encode(
                &json!({ "op": "approve", "call_id": call, "decision": "allow", "always": true }),
            )
            .unwrap();
        let sent: Value = serde_json::from_str(&reply.frames[0]).unwrap();
        assert_eq!(sent["response"]["request_id"], "r9");
        assert_eq!(sent["response"]["response"]["behavior"], "allow");
        assert_eq!(
            sent["response"]["response"]["updatedPermissions"][0]["destination"],
            "session"
        );
        assert!(c
            .encode(&json!({ "op": "approve", "call_id": call, "decision": "allow" }))
            .is_err());
    }

    #[test]
    fn multi_edit_hunks_approve_only_the_chosen_edits() {
        let mut c = claude();
        let input = json!({ "file_path": "/p/a.rs", "edits": [{ "old_string": "a", "new_string": "b" }, { "old_string": "c", "new_string": "d" }] });
        let events = frame(
            &mut c,
            json!({ "type": "control_request", "request_id": "r1", "request": { "subtype": "can_use_tool", "tool_name": "MultiEdit", "tool_use_id": "t", "input": input } }),
        );
        let request = events.last().unwrap();
        assert_eq!(request["hunks"][1]["id"], "e1");
        let reply = c.encode(&json!({ "op": "approve", "call_id": request["call_id"], "decision": "allow", "hunks": ["e1"] })).unwrap();
        let sent: Value = serde_json::from_str(&reply.frames[0]).unwrap();
        assert_eq!(
            sent["response"]["response"]["updatedInput"]["edits"],
            json!([{ "old_string": "c", "new_string": "d" }])
        );
    }

    #[test]
    fn full_access_needs_a_restart_on_the_same_conversation() {
        let mut c = claude();
        frame(
            &mut c,
            json!({ "type": "system", "subtype": "init", "session_id": "abc", "model": "claude-haiku-4-5", "permissionMode": "default" }),
        );
        assert!(c
            .restart_for(&json!({ "op": "set_approval_mode", "mode": "auto-edit" }))
            .is_none());
        let next = c
            .restart_for(&json!({ "op": "set_approval_mode", "mode": "full-access" }))
            .unwrap();
        assert_eq!(next.approval_mode.as_deref(), Some("bypassPermissions"));
        assert_eq!(next.resume.as_deref(), Some("abc"));
        let args: Vec<String> = command("abc", &next)
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"--permission-prompt-tool".to_string()));
        assert!(args.windows(2).any(|w| w == ["--resume", "abc"]));
    }

    #[test]
    fn the_desktop_may_name_the_binary_and_its_environment() {
        let options = Options::from_json(&json!({
            "bin": "/opt/claude/bin/claude",
            "env": { "CLAUDE_CONFIG_DIR": "/tmp/c" },
        }));
        assert!(options.runs_programs());
        let (command, _) = super::super::command(super::super::Kind::Claude, "x", &options).unwrap();
        assert_eq!(command.get_program(), "/opt/claude/bin/claude");
        assert!(command
            .get_envs()
            .any(|(name, value)| name == "CLAUDE_CONFIG_DIR" && value == Some("/tmp/c".as_ref())));
        assert!(!Options::from_json(&json!({ "model": "m" })).runs_programs());
    }

    #[test]
    fn a_new_session_pins_its_id_and_slash_commands_pass_through() {
        let args: Vec<String> = command("1111-2222", &Options::default())
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.windows(2).any(|w| w == ["--session-id", "1111-2222"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--permission-mode", "default"]));
        let mut c = claude();
        let frames = c
            .encode(&json!({ "op": "command", "input": "/compact" }))
            .unwrap()
            .frames;
        assert!(frames[0].contains("/compact"));
        assert!(c
            .encode(&json!({ "op": "command", "input": "/resume x" }))
            .is_err());
        assert!(c.encode(&json!({ "op": "steer" })).is_err());
    }

    #[test]
    fn replayed_user_messages_skip_command_echoes() {
        let mut c = claude();
        assert_eq!(
            frame(
                &mut c,
                json!({ "type": "user", "isReplay": true, "message": { "content": [{ "type": "text", "text": "fix it" }] } })
            )[0]["content"],
            "fix it"
        );
        assert!(frame(&mut c, json!({ "type": "user", "isReplay": true, "message": { "content": "<command-name>/compact</command-name>" } })).is_empty());
        assert!(frame(
            &mut c,
            json!({ "type": "user", "isReplay": true, "message": { "content": "/effort high" } })
        )
        .is_empty());
    }

    #[test]
    fn model_names_are_compact() {
        assert_eq!(
            compact_model("claude-opus-4-8[1m]", "opus[1m]"),
            "Opus 4.8 (1M)"
        );
        assert_eq!(
            compact_model("claude-haiku-4-5-20251001", "haiku"),
            "Haiku 4.5"
        );
        assert_eq!(compact_model("", "sonnet"), "Sonnet");
    }

    #[test]
    fn saved_conversations_are_read_from_the_project_directory() {
        let home = std::env::temp_dir().join(format!("claude-home-{}", std::process::id()));
        let cwd = Path::new("/work/my_app");
        let dir = home.join(".claude/projects/-work-my-app");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("11-22.jsonl"),
            [
                json!({ "type": "user", "message": { "content": "hello there" } }),
                json!({ "type": "user", "isMeta": true, "message": { "content": "caveat" } }),
                json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "hi" }, { "type": "tool_use" }] } }),
                json!({ "type": "user", "message": { "content": "<command-name>/x</command-name>" } }),
            ]
            .iter()
            .map(|row| row.to_string() + "\n")
            .collect::<String>(),
        )
        .unwrap();
        let items = transcript_in(&home, cwd, "11-22");
        let listed = saved_in(&home, cwd);
        assert_eq!(
            items,
            vec![
                json!({ "role": "user", "content": "hello there" }),
                json!({ "role": "assistant", "content": "hi" })
            ]
        );
        assert_eq!(listed[0].0, "11-22");
        assert_eq!(listed[0].1, "hello there");
        assert!(transcript_in(&home, cwd, "../etc").is_empty());

        // A symlinked directory is saved under its real path.
        let real = home.join("real-dir");
        let link = home.join("link-dir");
        fs::create_dir_all(&real).unwrap();
        let _ = fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(unix)]
        assert_eq!(project_dir(&home, &link), project_dir(&home, &real));
    }
}
