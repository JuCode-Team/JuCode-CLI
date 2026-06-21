use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const DEFAULT_BASH_TIMEOUT_SECS: u64 = 60;
const MAX_IMAGE_READ_BYTES: u64 = 1024 * 1024;
const LARGE_TEXT_READ_SOFT_BYTES: u64 = 256 * 1024;
const LARGE_RIPGREP_OUTPUT_SOFT_LINES: usize = 200;
const LARGE_RIPGREP_OUTPUT_SOFT_BYTES: usize = 128 * 1024;
const COMMAND_OUTPUT_MAX_LINES: usize = 3_000;
const COMMAND_OUTPUT_MAX_BYTES: usize = 128 * 1024;
const COMMAND_UPDATE_INTERVAL: Duration = Duration::from_millis(500);
const HASHLINE_ALPHABET: &[u8; 16] = b"ZPMQVRWSNKTXJBYH";
const READ_MODEL_CONTENT_OMIT_THRESHOLD: usize = 1024;
const READ_MODEL_HASHLINES_LIMIT: usize = 8 * 1024;
const DIFF_MODEL_OUTPUT_INLINE_LIMIT: usize = 8 * 1024;
const MODEL_OUTPUT_INLINE_LIMIT: usize = 16 * 1024;
const MODEL_OUTPUT_FIELD_LIMIT: usize = 4 * 1024;

pub struct ToolExecutionResult {
    pub output: String,
    pub model_output: String,
    pub is_error: bool,
}

pub enum ToolExecutionEvent {
    Update(String),
}

pub fn definitions() -> Vec<Value> {
    with_function_tool_defaults(vec![
        json!({
            "type": "function",
            "name": "read",
            "description": "Read a text file, image, or binary file metadata. Text supports 1-indexed offset and line limit. Prefer offset/limit for large files; broad reads return a soft warning instead of being blocked. Safe to call in parallel with other read-only tools.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative or absolute file path." },
                    "offset": { "type": "number", "description": "1-indexed line to start reading from. Defaults to 1." },
                    "limit": { "type": "number", "description": "Optional maximum lines to read. Defaults to no line limit." }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "str_replace",
            "description": "Apply one or more exact targeted text replacements to a UTF-8 file. The file must be read first, and each oldText must match exactly once in the current file. Combine multiple edits for the same file in one call.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative or absolute file path." },
                    "edits": {
                        "type": "array",
                        "description": "Targeted replacements matched against the original file.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": { "type": "string", "description": "Exact unique text to replace." },
                                "newText": { "type": "string", "description": "Replacement text." }
                            },
                            "required": ["oldText", "newText"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "hashline_edit",
            "description": "Patch one UTF-8 file using LINE#HASH anchors from the most recent read output. Supports replace, append, and prepend line edits. Prefer this after read() when exact oldText is awkward.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative or absolute file path." },
                    "edits": {
                        "type": "array",
                        "description": "Hashline edits over this file. Anchors are copied from read().hashlines.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "op": { "type": "string", "enum": ["replace", "append", "prepend"], "description": "replace a line/range, append after pos, or prepend before pos." },
                                "pos": { "type": "string", "description": "LINE#HASH anchor. Required for replace; optional for append/prepend." },
                                "end": { "type": "string", "description": "Inclusive LINE#HASH range end for replace." },
                                "lines": {
                                    "description": "Literal replacement/insertion lines. No LINE#HASH prefixes and no diff +/- prefixes.",
                                    "oneOf": [
                                        { "type": "array", "items": { "type": "string" } },
                                        { "type": "string" }
                                    ]
                                }
                            },
                            "required": ["op", "lines"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "write",
            "description": "Write full UTF-8 file content. Creates new files without a prior read; existing files must be read first before overwriting. Prefer for greenfield files or full-file rewrites.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative or absolute file path." },
                    "content": { "type": "string", "description": "Full file content to write." }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "apply_patch",
            "description": "Apply a unified git diff patch to the current workspace. Use this for multi-file edits when exact replacement tools are awkward. If a patch fails, inspect the error and retry with a corrected minimal patch.",
            "parameters": {
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "Unified diff text accepted by git apply." }
                },
                "required": ["patch"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "bash",
            "description": "Run a shell command in the workspace. Returns exit code, stdout, stderr, timeout state, truncation state, or a session_id for long-running commands. Prefer commands that narrow output with paths, filters, or limits. Group dependent shell checks into one command when that reduces round trips; issue independent bash/read/ripgrep calls in the same assistant response when possible.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run." },
                    "workdir": { "type": "string", "description": "Working directory for the command. Relative paths are resolved from the current workspace. Defaults to current workspace." },
                    "timeout": { "type": "number", "description": "Timeout in seconds. Defaults to 60." },
                    "yield_time_ms": { "type": "number", "description": "Return early after this many milliseconds if the command is still running. Use for dev servers, watchers, and long tasks." }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "exec_command",
            "description": "Codex-compatible shell execution alias. Runs a shell command and returns output or a session_id for ongoing interaction. Use it like bash; prefer this name when following Codex-style command plans. Prefer specific paths, globs, head/tail, or tool-native limits for large outputs; broad output returns a soft warning. Independent exec_command calls may be emitted together in one assistant response.",
            "parameters": {
                "type": "object",
                "properties": {
                    "cmd": { "type": "string", "description": "Shell command to execute." },
                    "workdir": { "type": "string", "description": "Working directory for the command. Relative paths are resolved from the current workspace. Defaults to current workspace." },
                    "timeout": { "type": "number", "description": "Timeout in seconds. Defaults to 60." },
                    "yield_time_ms": { "type": "number", "description": "Return early after this many milliseconds if the command is still running." },
                    "max_output_tokens": { "type": "number", "description": "Optional compatibility hint. JuCode may still project very large outputs through its global output budget." },
                    "tty": { "type": "boolean", "description": "Compatibility hint accepted for Codex-style calls; JuCode command execution does not require it." },
                    "login": { "type": "boolean", "description": "Compatibility hint accepted for Codex-style calls; JuCode uses its configured shell invocation." }
                },
                "required": ["cmd"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "write_stdin",
            "description": "Send input to a running bash/exec_command session or poll it. Use the session_id returned by bash or exec_command.",
            "parameters": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "number", "description": "Running bash session id." },
                    "text": { "type": "string", "description": "Text to write to stdin. Omit or pass empty text to only poll." },
                    "chars": { "type": "string", "description": "Codex-compatible alias for text." },
                    "yield_time_ms": { "type": "number", "description": "Milliseconds to wait for more output. Defaults to 1000." }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "ls",
            "description": "List directory contents sorted alphabetically. Directories have a trailing slash. Safe to call in parallel with other read-only exploration tools.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list. Defaults to current workspace." },
                    "limit": { "type": "number", "description": "Optional maximum entries to return. Defaults to no entry limit." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "ripgrep",
            "description": "Search file contents with ripgrep (rg). Respects .gitignore by default and returns matching lines with paths and line numbers. Prefer a narrow path/glob or limit for broad patterns; large result sets return a soft warning. Use multiple ripgrep calls in one response for independent search hypotheses.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern." },
                    "path": { "type": "string", "description": "File or directory to search. Defaults to current workspace." },
                    "glob": { "type": "string", "description": "Optional glob filter, e.g. *.rs or **/*.ts." },
                    "ignoreCase": { "type": "boolean", "description": "Case-insensitive search. Defaults to false." },
                    "literal": { "type": "boolean", "description": "Treat pattern as a literal string. Defaults to false." },
                    "contextLines": { "type": "number", "description": "Lines before and after each match. Defaults to 0." },
                    "limit": { "type": "number", "description": "Optional maximum output lines. Defaults to no output line limit." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "outline",
            "description": "Return a lightweight symbol outline for a source file without reading the full body. Safe to call in parallel with other read-only tools.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Source file path." },
                    "limit": { "type": "number", "description": "Maximum symbols to return. Defaults to 200." }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "checkpoint",
            "description": "Create, list, or restore lightweight file checkpoints under .jucode/checkpoints. This is for local rollback, not git.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["create", "list", "restore"], "description": "Checkpoint action." },
                    "name": { "type": "string", "description": "Checkpoint name for create." },
                    "id": { "type": "string", "description": "Checkpoint id for restore." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Files to snapshot for create." }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        }),
    ])
}

fn with_function_tool_defaults(mut definitions: Vec<Value>) -> Vec<Value> {
    for definition in &mut definitions {
        if definition.get("type").and_then(Value::as_str) == Some("function") {
            if let Some(map) = definition.as_object_mut() {
                map.entry("strict").or_insert(json!(false));
            }
        }
    }
    definitions
}

#[cfg(test)]
fn run_tool(name: &str, arguments: &str, cwd: &Path) -> String {
    run_tool_with_events(name, arguments, cwd, |_| Ok(())).output
}

pub fn run_tool_with_events(
    name: &str,
    arguments: &str,
    cwd: &Path,
    mut emit: impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> ToolExecutionResult {
    let parsed = serde_json::from_str::<Value>(arguments);
    let args = match parsed {
        Ok(args) => args,
        Err(error) => {
            return tool_result(
                name,
                json!({ "error": format!("invalid JSON arguments: {error}") }),
                cwd,
            )
        }
    };

    let result = match name {
        "read" => read_file(&args, cwd),
        "str_replace" | "edit" => str_replace_file(&args, cwd),
        "hashline_edit" => hashline_edit_file(&args, cwd),
        "write" => write_file(&args, cwd),
        "bash" | "execute" | "exec_command" | "shell_command" => bash(&args, cwd, &mut emit),
        "write_stdin" => write_stdin(&args),
        "apply_patch" => apply_patch(&args, cwd, &mut emit),
        "ls" => list_dir(&args, cwd),
        "ripgrep" => ripgrep(&args, cwd),
        "outline" => outline_file(&args, cwd),
        "checkpoint" => checkpoint_tool(&args, cwd),
        _ => json!({ "error": format!("unknown tool: {name}") }),
    };
    tool_result(name, result, cwd)
}

fn tool_result(name: &str, value: Value, cwd: &Path) -> ToolExecutionResult {
    let output = value.to_string();
    let model_output = project_model_output(name, &output, cwd);
    ToolExecutionResult {
        is_error: value.get("error").is_some()
            || value
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
            || value
                .get("timed_out")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        output,
        model_output,
    }
}

fn add_soft_hint(value: &mut Value, warning: &str, suggestion: &str) {
    if let Value::Object(map) = value {
        map.entry("warning".to_string())
            .or_insert_with(|| json!(warning));
        map.entry("suggestion".to_string())
            .or_insert_with(|| json!(suggestion));
    }
}

fn read_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };

    let path = resolve_path(cwd, path);
    let offset = optional_usize(args, "offset").unwrap_or(1).max(1);
    let limit = optional_usize(args, "limit").map(|limit| limit.max(1));

    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };
    if let Some(mime) = image_mime(&path) {
        if metadata.len() > MAX_IMAGE_READ_BYTES {
            return json!({
                "path": path.display().to_string(),
                "kind": "image",
                "mime": mime,
                "bytes": metadata.len(),
                "truncated": true,
                "error": "image is too large to inline; inspect it with an external viewer or a narrower tool"
            });
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return json!({ "path": path.display().to_string(), "error": error.to_string() })
            }
        };
        mark_read(&path);
        return json!({
            "path": path.display().to_string(),
            "kind": "image",
            "mime": mime,
            "bytes": bytes.len(),
            "base64": BASE64_STANDARD.encode(bytes),
        });
    }

    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };
    let Some((text, encoding)) = decode_text_bytes(&bytes) else {
        return json!({
            "path": path.display().to_string(),
            "kind": "binary",
            "bytes": bytes.len(),
            "error": "file is not supported text encoding"
        });
    };

    let mut content = String::new();
    let mut hashlines = String::new();
    let mut lines_read = 0usize;
    let mut truncated = false;
    let mut line_number = 0usize;

    for line in text.split_inclusive('\n') {
        line_number += 1;
        if line_number < offset {
            continue;
        }
        if limit.is_some_and(|limit| lines_read >= limit) {
            truncated = true;
            break;
        }

        content.push_str(line);
        let display_line = line.strip_suffix('\n').unwrap_or(line);
        hashlines.push_str(&format_hashline(line_number, display_line));
        if line.ends_with('\n') {
            hashlines.push('\n');
        }
        lines_read += 1;
    }
    mark_read(&path);

    let mut value = json!({
        "path": path.display().to_string(),
        "kind": "text",
        "encoding": encoding,
        "offset": offset,
        "lines_read": lines_read,
        "truncated": truncated,
        "content": content,
        "hashlines": hashlines,
    });
    if limit.is_none() && metadata.len() > LARGE_TEXT_READ_SOFT_BYTES {
        add_soft_hint(
            &mut value,
            "large file read without a line limit",
            "Use read with offset/limit, outline, or ripgrep to narrow the next read unless the full file is required.",
        );
    }
    value
}

fn str_replace_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };
    let Some(edits) = args.get("edits").and_then(Value::as_array) else {
        return json!({ "error": "missing edits" });
    };
    if edits.is_empty() {
        return json!({ "error": "edits must not be empty" });
    }

    let path = resolve_path(cwd, path);
    if !has_read(&path) {
        return json!({
            "path": path.display().to_string(),
            "error": "edit requires reading this file first so oldText matches bytes on disk"
        });
    }
    let original = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };

    let mut replacements = Vec::new();
    for edit in edits {
        let Some(old_text) = edit.get("oldText").and_then(Value::as_str) else {
            return json!({ "path": path.display().to_string(), "error": "each edit requires oldText" });
        };
        let Some(new_text) = edit.get("newText").and_then(Value::as_str) else {
            return json!({ "path": path.display().to_string(), "error": "each edit requires newText" });
        };
        if old_text.is_empty() {
            return json!({ "path": path.display().to_string(), "error": "oldText must not be empty" });
        }

        let matches = original.match_indices(old_text).collect::<Vec<_>>();
        if matches.len() != 1 {
            return json!({
                "path": path.display().to_string(),
                "error": format!("oldText must match exactly once; found {}", matches.len()),
                "oldText": old_text,
            });
        }
        let start = matches[0].0;
        replacements.push((start, start + old_text.len(), new_text.to_string()));
    }

    replacements.sort_by_key(|(start, _, _)| *start);
    for pair in replacements.windows(2) {
        if pair[0].1 > pair[1].0 {
            return json!({ "path": path.display().to_string(), "error": "edits must not overlap" });
        }
    }

    let mut output = String::with_capacity(original.len());
    let mut cursor = 0usize;
    for (start, end, new_text) in &replacements {
        output.push_str(&original[cursor..*start]);
        output.push_str(new_text);
        cursor = *end;
    }
    output.push_str(&original[cursor..]);

    let _ = create_checkpoint(cwd, "auto-edit", std::slice::from_ref(&path));
    match fs::write(&path, &output) {
        Ok(()) => {
            mark_read(&path);
            let diff = unified_diff_for_file(cwd, &path, &original, &output);
            json!({
                "path": path.display().to_string(),
                "edits": replacements.len(),
                "written_bytes": output.len(),
                "diff": diff.unwrap_or_default(),
            })
        }
        Err(error) => json!({ "path": path.display().to_string(), "error": error.to_string() }),
    }
}

#[derive(Clone)]
struct HashlineAnchor {
    line: usize,
    hash: String,
}

struct HashlineEdit {
    op: String,
    pos: Option<HashlineAnchor>,
    end: Option<HashlineAnchor>,
    lines: Vec<String>,
}

struct HashlineSpan {
    start: usize,
    end: usize,
    replacement: String,
}

fn hashline_edit_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };
    let Some(raw_edits) = args.get("edits").and_then(Value::as_array) else {
        return json!({ "error": "missing edits" });
    };
    if raw_edits.is_empty() {
        return json!({ "error": "edits must not be empty" });
    }

    let path = resolve_path(cwd, path);
    if !has_read(&path) {
        return json!({
            "path": path.display().to_string(),
            "error": "hashline_edit requires reading this file first and copying LINE#HASH anchors from read().hashlines"
        });
    }

    let original = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };

    let edits = match parse_hashline_edits(raw_edits) {
        Ok(edits) => edits,
        Err(error) => return json!({ "path": path.display().to_string(), "error": error }),
    };

    let line_index = LineIndex::new(&original);
    if let Err(error) = validate_hashline_anchors(&edits, &line_index) {
        return json!({ "path": path.display().to_string(), "error": error });
    }

    let spans = match resolve_hashline_spans(&edits, &original, &line_index) {
        Ok(spans) => spans,
        Err(error) => return json!({ "path": path.display().to_string(), "error": error }),
    };

    let mut output = original.clone();
    for span in spans.iter().rev() {
        output.replace_range(span.start..span.end, &span.replacement);
    }

    let _ = create_checkpoint(cwd, "auto-hashline-edit", std::slice::from_ref(&path));
    match fs::write(&path, &output) {
        Ok(()) => {
            mark_read(&path);
            let diff = unified_diff_for_file(cwd, &path, &original, &output);
            let changed = changed_line_range(&original, &output);
            let anchors =
                changed.and_then(|(first, last)| post_edit_anchor_block(&output, first, last));
            json!({
                "path": path.display().to_string(),
                "edits": edits.len(),
                "written_bytes": output.len(),
                "diff": diff.unwrap_or_default(),
                "anchors": anchors.unwrap_or_default(),
            })
        }
        Err(error) => json!({ "path": path.display().to_string(), "error": error.to_string() }),
    }
}

fn write_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };
    let Some(content) = args.get("content").and_then(Value::as_str) else {
        return json!({ "error": "missing content" });
    };

    let path = resolve_path(cwd, path);
    let exists = path.exists();
    if exists && !has_read(&path) {
        return json!({
            "path": path.display().to_string(),
            "error": "write requires reading an existing file first before overwriting it; new files can be written without a prior read"
        });
    }
    let original = if exists {
        fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };
    if let Some(parent) = path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return json!({ "path": path.display().to_string(), "error": error.to_string() });
        }
    }

    let _ = create_checkpoint(cwd, "auto-write", std::slice::from_ref(&path));
    match fs::write(&path, content) {
        Ok(()) => {
            mark_read(&path);
            let diff = unified_diff_for_file(cwd, &path, &original, content);
            json!({
                "path": path.display().to_string(),
                "written_bytes": content.len(),
                "diff": diff.unwrap_or_default(),
            })
        }
        Err(error) => json!({ "path": path.display().to_string(), "error": error.to_string() }),
    }
}

fn parse_hashline_edits(raw_edits: &[Value]) -> Result<Vec<HashlineEdit>, String> {
    let mut edits = Vec::new();
    for edit in raw_edits {
        let op = edit
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| "each hashline edit requires op".to_string())?;
        if !matches!(op, "replace" | "append" | "prepend") {
            return Err(format!(
                "[E_BAD_OP] Unknown edit op \"{op}\". Expected replace, append, or prepend."
            ));
        }

        let pos = edit
            .get("pos")
            .and_then(Value::as_str)
            .map(parse_hashline_anchor)
            .transpose()?;
        let end = edit
            .get("end")
            .and_then(Value::as_str)
            .map(parse_hashline_anchor)
            .transpose()?;
        let lines = parse_hashline_lines(edit.get("lines"))?;

        if op == "replace" && pos.is_none() {
            return Err("[E_BAD_OP] Replace requires a pos anchor.".to_string());
        }
        if op != "replace" && end.is_some() {
            return Err(format!("[E_BAD_OP] {op} does not support an end anchor."));
        }
        if op != "replace" && lines.is_empty() {
            return Err(format!("[E_BAD_OP] {op} requires at least one line."));
        }

        edits.push(HashlineEdit {
            op: op.to_string(),
            pos,
            end,
            lines,
        });
    }
    Ok(edits)
}

fn parse_hashline_lines(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Err("each hashline edit requires lines".to_string());
    };
    let lines = if let Some(text) = value.as_str() {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        normalized
            .strip_suffix('\n')
            .unwrap_or(&normalized)
            .split('\n')
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else if let Some(values) = value.as_array() {
        values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "lines array must contain only strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        return Err("lines must be a string or an array of strings".to_string());
    };
    for line in &lines {
        if is_hashline_display_prefix(line) || is_diff_payload_prefix(line) {
            return Err(format!(
                "[E_INVALID_PATCH] lines must contain literal file content, not LINE#HASH or diff prefixes. Offending line: {line:?}"
            ));
        }
    }
    Ok(lines)
}

fn parse_hashline_anchor(ref_text: &str) -> Result<HashlineAnchor, String> {
    let core = ref_text
        .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '>' || ch == '+' || ch == '-')
        .trim_end();
    let Some(hash_pos) = core.find('#') else {
        return Err(format!(
            "[E_BAD_REF] Invalid line reference {ref_text:?}. Expected LINE#HASH."
        ));
    };
    let line = core[..hash_pos].trim().parse::<usize>().map_err(|_| {
        format!("[E_BAD_REF] Invalid line reference {ref_text:?}. Expected numeric LINE#HASH.")
    })?;
    if line == 0 {
        return Err(format!(
            "[E_BAD_REF] Line number must be >= 1 in {ref_text:?}."
        ));
    }
    let hash_part = core[hash_pos + 1..]
        .split_once(':')
        .map(|(hash, _)| hash)
        .unwrap_or(&core[hash_pos + 1..])
        .trim();
    if hash_part.len() != 2
        || !hash_part
            .as_bytes()
            .iter()
            .all(|byte| HASHLINE_ALPHABET.contains(byte))
    {
        return Err(format!(
            "[E_BAD_REF] Invalid line reference {ref_text:?}: hash must be exactly 2 characters from {}.",
            String::from_utf8_lossy(HASHLINE_ALPHABET)
        ));
    }
    Ok(HashlineAnchor {
        line,
        hash: hash_part.to_string(),
    })
}

struct LineIndex {
    lines: Vec<String>,
    starts: Vec<usize>,
    has_terminal_newline: bool,
}

impl LineIndex {
    fn new(content: &str) -> Self {
        let lines = content.split('\n').map(str::to_string).collect::<Vec<_>>();
        let mut starts = Vec::with_capacity(lines.len());
        let mut offset = 0usize;
        for (index, line) in lines.iter().enumerate() {
            starts.push(offset);
            offset += line.len();
            if index < lines.len() - 1 {
                offset += 1;
            }
        }
        Self {
            lines,
            starts,
            has_terminal_newline: content.ends_with('\n'),
        }
    }
}

fn validate_hashline_anchors(edits: &[HashlineEdit], line_index: &LineIndex) -> Result<(), String> {
    let mut mismatches = Vec::new();
    for edit in edits {
        for anchor in [&edit.pos, &edit.end].into_iter().flatten() {
            if anchor.line == 0 || anchor.line > line_index.lines.len() {
                return Err(format!(
                    "[E_RANGE_OOB] Line {} does not exist (file has {} lines).",
                    anchor.line,
                    line_index.lines.len()
                ));
            }
            let actual = compute_line_hash(anchor.line, &line_index.lines[anchor.line - 1]);
            if actual != anchor.hash {
                mismatches.push((anchor.line, anchor.hash.clone(), actual));
            }
        }
    }
    if mismatches.is_empty() {
        return Ok(());
    }
    Err(format_hashline_mismatch(&mismatches, &line_index.lines))
}

fn resolve_hashline_spans(
    edits: &[HashlineEdit],
    content: &str,
    line_index: &LineIndex,
) -> Result<Vec<HashlineSpan>, String> {
    let mut spans = Vec::new();
    for edit in edits {
        let span = match edit.op.as_str() {
            "replace" => resolve_hashline_replace(edit, content, line_index)?,
            "append" => resolve_hashline_append(edit, content, line_index)?,
            "prepend" => resolve_hashline_prepend(edit, content, line_index)?,
            _ => return Err(format!("[E_BAD_OP] Unknown edit op {}.", edit.op)),
        };
        spans.push(span);
    }
    spans.sort_by_key(|span| (span.start, span.end));
    for pair in spans.windows(2) {
        if pair[0].end > pair[1].start {
            return Err("[E_EDIT_CONFLICT] hashline edits must not overlap.".to_string());
        }
        if pair[0].start == pair[1].start && pair[0].end == pair[1].end {
            return Err(
                "[E_EDIT_CONFLICT] hashline edits target the same insertion boundary.".to_string(),
            );
        }
    }
    Ok(spans)
}

fn resolve_hashline_replace(
    edit: &HashlineEdit,
    content: &str,
    line_index: &LineIndex,
) -> Result<HashlineSpan, String> {
    let pos = edit.pos.as_ref().expect("replace pos validated");
    let end = edit.end.as_ref().unwrap_or(pos);
    if pos.line > end.line {
        return Err(format!(
            "[E_BAD_OP] Range start line {} must be <= end line {}.",
            pos.line, end.line
        ));
    }
    let replacement = edit.lines.join("\n");
    let start = line_index.starts[pos.line - 1];
    let end_offset = if edit.lines.is_empty() {
        if pos.line == 1 && end.line == line_index.lines.len() {
            content.len()
        } else if end.line < line_index.lines.len() {
            line_index.starts[end.line]
        } else {
            line_index.starts[pos.line - 1].saturating_sub(1)
        }
    } else {
        line_index.starts[end.line - 1] + line_index.lines[end.line - 1].len()
    };
    Ok(HashlineSpan {
        start,
        end: end_offset,
        replacement,
    })
}

fn resolve_hashline_append(
    edit: &HashlineEdit,
    content: &str,
    line_index: &LineIndex,
) -> Result<HashlineSpan, String> {
    let inserted = edit.lines.join("\n");
    if content.is_empty() {
        return Ok(HashlineSpan {
            start: 0,
            end: 0,
            replacement: inserted,
        });
    }
    if let Some(pos) = &edit.pos {
        let sentinel_append = line_index.has_terminal_newline && pos.line == line_index.lines.len();
        let offset = if sentinel_append {
            content.len()
        } else {
            line_index.starts[pos.line - 1] + line_index.lines[pos.line - 1].len()
        };
        Ok(HashlineSpan {
            start: offset,
            end: offset,
            replacement: if sentinel_append {
                format!("{inserted}\n")
            } else {
                format!("\n{inserted}")
            },
        })
    } else {
        Ok(HashlineSpan {
            start: content.len(),
            end: content.len(),
            replacement: if line_index.has_terminal_newline {
                format!("{inserted}\n")
            } else {
                format!("\n{inserted}")
            },
        })
    }
}

fn resolve_hashline_prepend(
    edit: &HashlineEdit,
    content: &str,
    line_index: &LineIndex,
) -> Result<HashlineSpan, String> {
    let inserted = edit.lines.join("\n");
    let start = edit
        .pos
        .as_ref()
        .map(|pos| line_index.starts[pos.line - 1])
        .unwrap_or(0);
    Ok(HashlineSpan {
        start,
        end: start,
        replacement: if content.is_empty() {
            inserted
        } else {
            format!("{inserted}\n")
        },
    })
}

fn format_hashline_mismatch(mismatches: &[(usize, String, String)], lines: &[String]) -> String {
    let mut retry_lines = HashSet::new();
    for (line, _, _) in mismatches {
        let start = line.saturating_sub(2).max(1);
        let end = (*line + 2).min(lines.len());
        for retry in start..=end {
            retry_lines.insert(retry);
        }
    }
    let mut sorted = retry_lines.into_iter().collect::<Vec<_>>();
    sorted.sort_unstable();
    let mut out = vec![format!(
        "[E_STALE_ANCHOR] {} stale anchor{}. Retry with the >>> LINE#HASH lines below.",
        mismatches.len(),
        if mismatches.len() == 1 { "" } else { "s" }
    )];
    for line in sorted {
        let content = &lines[line - 1];
        out.push(format!(
            ">>> {}#{}:{}",
            line,
            compute_line_hash(line, content),
            content
        ));
    }
    out.join("\n")
}

fn is_hashline_display_prefix(line: &str) -> bool {
    let trimmed = line
        .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '>' || ch == '+')
        .trim_start();
    let Some((line_part, rest)) = trimmed.split_once('#') else {
        return false;
    };
    if !line_part.trim().chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    let Some((hash, _)) = rest.split_once(':') else {
        return false;
    };
    hash.trim().len() == 2
        && hash
            .trim()
            .as_bytes()
            .iter()
            .all(|byte| HASHLINE_ALPHABET.contains(byte))
}

fn is_diff_payload_prefix(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('-') else {
        return false;
    };
    let trimmed = rest.trim_start();
    let digit_count = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digit_count > 0 && trimmed[digit_count..].starts_with("    ")
}

fn format_hashline(line_number: usize, line: &str) -> String {
    format!(
        "{line_number}#{}:{line}",
        compute_line_hash(line_number, line)
    )
}

fn compute_line_hash(line_number: usize, line: &str) -> String {
    let normalized = line.trim_end_matches('\r').trim_end();
    let seed = if normalized.chars().any(|ch| ch.is_alphanumeric()) {
        0
    } else {
        line_number as u32
    };
    let value = xxh32(normalized.as_bytes(), seed) & 0xff;
    let high = ((value >> 4) & 0x0f) as usize;
    let low = (value & 0x0f) as usize;
    let mut hash = String::with_capacity(2);
    hash.push(HASHLINE_ALPHABET[high] as char);
    hash.push(HASHLINE_ALPHABET[low] as char);
    hash
}

fn xxh32(input: &[u8], seed: u32) -> u32 {
    const PRIME32_1: u32 = 0x9E3779B1;
    const PRIME32_2: u32 = 0x85EBCA77;
    const PRIME32_3: u32 = 0xC2B2AE3D;
    const PRIME32_4: u32 = 0x27D4EB2F;
    const PRIME32_5: u32 = 0x165667B1;

    let mut index = 0usize;
    let mut hash;
    if input.len() >= 16 {
        let mut v1 = seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2);
        let mut v2 = seed.wrapping_add(PRIME32_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME32_1);
        while index <= input.len() - 16 {
            v1 = xxh32_round(v1, read_u32_le(input, index));
            index += 4;
            v2 = xxh32_round(v2, read_u32_le(input, index));
            index += 4;
            v3 = xxh32_round(v3, read_u32_le(input, index));
            index += 4;
            v4 = xxh32_round(v4, read_u32_le(input, index));
            index += 4;
        }
        hash = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
    } else {
        hash = seed.wrapping_add(PRIME32_5);
    }

    hash = hash.wrapping_add(input.len() as u32);
    while index + 4 <= input.len() {
        hash = hash
            .wrapping_add(read_u32_le(input, index).wrapping_mul(PRIME32_3))
            .rotate_left(17)
            .wrapping_mul(PRIME32_4);
        index += 4;
    }
    while index < input.len() {
        hash = hash
            .wrapping_add(u32::from(input[index]).wrapping_mul(PRIME32_5))
            .rotate_left(11)
            .wrapping_mul(PRIME32_1);
        index += 1;
    }

    hash ^= hash >> 15;
    hash = hash.wrapping_mul(PRIME32_2);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(PRIME32_3);
    hash ^= hash >> 16;
    hash
}

fn xxh32_round(acc: u32, lane: u32) -> u32 {
    const PRIME32_1: u32 = 0x9E3779B1;
    const PRIME32_2: u32 = 0x85EBCA77;
    acc.wrapping_add(lane.wrapping_mul(PRIME32_2))
        .rotate_left(13)
        .wrapping_mul(PRIME32_1)
}

fn read_u32_le(input: &[u8], index: usize) -> u32 {
    u32::from_le_bytes([
        input[index],
        input[index + 1],
        input[index + 2],
        input[index + 3],
    ])
}

fn changed_line_range(original: &str, updated: &str) -> Option<(usize, usize)> {
    if original == updated {
        return None;
    }
    let original_bytes = original.as_bytes();
    let updated_bytes = updated.as_bytes();
    let min_len = original_bytes.len().min(updated_bytes.len());
    let mut first_diff = 0usize;
    while first_diff < min_len && original_bytes[first_diff] == updated_bytes[first_diff] {
        first_diff += 1;
    }

    let mut original_tail = original_bytes.len();
    let mut updated_tail = updated_bytes.len();
    while original_tail > first_diff
        && updated_tail > first_diff
        && original_bytes[original_tail - 1] == updated_bytes[updated_tail - 1]
    {
        original_tail -= 1;
        updated_tail -= 1;
    }

    let first = byte_index_to_line(updated, first_diff);
    let last = if updated_tail <= first_diff {
        first
    } else {
        byte_index_to_line(updated, updated_tail.saturating_sub(1))
    };
    Some((first, last.max(first)))
}

fn byte_index_to_line(text: &str, byte_index: usize) -> usize {
    let end = byte_index.min(text.len());
    text[..end].bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn post_edit_anchor_block(content: &str, first: usize, last: usize) -> Option<String> {
    let lines = content.split('\n').map(str::to_string).collect::<Vec<_>>();
    let visible_line_count = if content.ends_with('\n') {
        lines.len().saturating_sub(1)
    } else {
        lines.len()
    };
    if visible_line_count == 0 {
        return None;
    }
    let start = first.saturating_sub(2).max(1);
    let end = (last + 2).min(visible_line_count);
    if end < start || end - start + 1 > 12 {
        return None;
    }
    let mut out = Vec::new();
    out.push(format!("--- Anchors {start}-{end} ---"));
    for line_number in start..=end {
        out.push(format_hashline(line_number, &lines[line_number - 1]));
    }
    Some(out.join("\n"))
}

fn bash(
    args: &Value,
    cwd: &Path,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Value {
    let Some(command) = args
        .get("command")
        .or_else(|| args.get("cmd"))
        .and_then(Value::as_str)
    else {
        return json!({ "error": "missing command; use `command` for bash or `cmd` for exec_command" });
    };
    let workdir = match command_workdir(args, cwd) {
        Ok(workdir) => workdir,
        Err(error) => return json!({ "command": command, "error": error }),
    };
    let timeout = Duration::from_secs(
        optional_u64(args, "timeout")
            .unwrap_or(DEFAULT_BASH_TIMEOUT_SECS)
            .max(1),
    );
    let yield_time = optional_u64(args, "yield_time_ms").map(Duration::from_millis);

    let (program, shell_args) = shell_command(command);
    let result = if let Some(yield_time) = yield_time {
        run_command_session(
            program,
            &shell_args,
            command,
            &workdir,
            timeout,
            yield_time,
            emit,
        )
    } else {
        run_command_events(program, &shell_args, None, &workdir, timeout, emit)
            .map(|result| command_result_json(command, Ok(result)))
    };
    match result {
        Ok(mut value) => {
            add_command_soft_hint(&mut value);
            value
        }
        Err(error) => json!({ "command": command, "error": error }),
    }
}

fn command_workdir(args: &Value, cwd: &Path) -> Result<PathBuf, String> {
    let Some(workdir) = args.get("workdir").and_then(Value::as_str) else {
        return Ok(cwd.to_path_buf());
    };
    let path = resolve_path(cwd, workdir);
    if !path.exists() {
        return Err(format!("workdir does not exist: {}", path.display()));
    }
    if !path.is_dir() {
        return Err(format!("workdir is not a directory: {}", path.display()));
    }
    Ok(path)
}

fn write_stdin(args: &Value) -> Value {
    let Some(session_id) = args.get("session_id").and_then(Value::as_u64) else {
        return json!({ "error": "missing session_id" });
    };
    let text = args
        .get("text")
        .or_else(|| args.get("chars"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let yield_time = Duration::from_millis(optional_u64(args, "yield_time_ms").unwrap_or(1000));
    match poll_or_write_session(session_id, text, yield_time) {
        Ok(value) => value,
        Err(error) => json!({ "session_id": session_id, "error": error }),
    }
}

fn run_command_session(
    program: &str,
    args: &[&str],
    display_command: &str,
    cwd: &Path,
    timeout: Duration,
    yield_time: Duration,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Result<Value, String> {
    let stdout_path = temp_output_path("stdout");
    let stderr_path = temp_output_path("stderr");
    let stdout_file = File::create(&stdout_path).map_err(|error| error.to_string())?;
    let stderr_file = File::create(&stderr_path).map_err(|error| error.to_string())?;

    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|error| format!("failed to start {program}: {error}"))?;

    let stdin = child.stdin.take();
    let started = SystemTime::now();
    emit(ToolExecutionEvent::Update(format!(
        "started: {}",
        command_display(program, args)
    )))?;

    let wait_until = yield_time.min(timeout);
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            let result = collect_command_result(status.code(), false, &stdout_path, &stderr_path);
            return Ok(command_result_json(display_command, Ok(result)));
        }
        if started.elapsed().unwrap_or_default() >= wait_until {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let (stdout, stdout_truncated) = read_output_file(&stdout_path);
    let (stderr, stderr_truncated) = read_output_file(&stderr_path);
    shell_sessions()
        .lock()
        .map_err(|_| "shell session lock is poisoned".to_string())?
        .insert(
            session_id,
            ShellSession {
                child,
                stdin,
                stdout_path,
                stderr_path,
                command: display_command.to_string(),
                started,
                timeout,
            },
        );

    Ok(json!({
        "command": display_command,
        "session_id": session_id,
        "running": true,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": stdout_truncated || stderr_truncated,
        "note": "command is still running; use write_stdin with this session_id to poll or send input"
    }))
}

fn poll_or_write_session(
    session_id: u64,
    text: &str,
    yield_time: Duration,
) -> Result<Value, String> {
    let started_poll = SystemTime::now();
    loop {
        let mut finished = None;
        {
            let mut sessions = shell_sessions()
                .lock()
                .map_err(|_| "shell session lock is poisoned".to_string())?;
            let Some(session) = sessions.get_mut(&session_id) else {
                return Err("shell session not found".to_string());
            };
            if session.started.elapsed().unwrap_or_default() >= session.timeout {
                kill_child(&mut session.child);
                finished = Some((true, None));
            } else if let Some(status) = session
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
            {
                finished = Some((false, status.code()));
            }
            if let Some((timed_out, code)) = finished {
                let session = sessions.remove(&session_id).expect("session exists");
                let result = collect_command_result(
                    code,
                    timed_out,
                    &session.stdout_path,
                    &session.stderr_path,
                );
                return Ok(command_result_json(&session.command, Ok(result)));
            }
            if !text.is_empty() {
                if let Some(stdin) = session.stdin.as_mut() {
                    if let Err(error) = stdin.write_all(text.as_bytes()).and_then(|_| stdin.flush())
                    {
                        if let Ok(Some(status)) = session.child.try_wait() {
                            let session = sessions.remove(&session_id).expect("session exists");
                            let result = collect_command_result(
                                status.code(),
                                false,
                                &session.stdout_path,
                                &session.stderr_path,
                            );
                            return Ok(command_result_json(&session.command, Ok(result)));
                        }
                        return Err(error.to_string());
                    }
                }
            }
        }

        if started_poll.elapsed().unwrap_or_default() >= yield_time {
            let sessions = shell_sessions()
                .lock()
                .map_err(|_| "shell session lock is poisoned".to_string())?;
            let Some(session) = sessions.get(&session_id) else {
                return Err("shell session not found".to_string());
            };
            let (stdout, stdout_truncated) = read_output_file(&session.stdout_path);
            let (stderr, stderr_truncated) = read_output_file(&session.stderr_path);
            return Ok(json!({
                "command": session.command,
                "session_id": session_id,
                "running": true,
                "stdout": stdout,
                "stderr": stderr,
                "truncated": stdout_truncated || stderr_truncated,
            }));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn collect_command_result(
    exit_code: Option<i32>,
    timed_out: bool,
    stdout_path: &Path,
    stderr_path: &Path,
) -> CommandResult {
    let (stdout, stdout_truncated) = read_output_file(stdout_path);
    let (stderr, stderr_truncated) = read_output_file(stderr_path);
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);
    CommandResult {
        exit_code,
        stdout,
        stderr,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    }
}

fn kill_child(child: &mut Child) {
    #[cfg(windows)]
    {
        let id = child.id().to_string();
        let _ = Command::new("taskkill")
            .args(["/pid", &id, "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        return;
    }
    #[cfg(not(windows))]
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct ShellSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    command: String,
    started: SystemTime,
    timeout: Duration,
}

static SHELL_SESSIONS: OnceLock<Mutex<HashMap<u64, ShellSession>>> = OnceLock::new();
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

fn shell_sessions() -> &'static Mutex<HashMap<u64, ShellSession>> {
    SHELL_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn project_model_output(name: &str, output: &str, cwd: &Path) -> String {
    if let Some(projected) = project_image_model_output(name, output) {
        return projected;
    }
    if let Some(projected) = project_read_model_output(name, output, cwd) {
        return projected;
    }
    if let Some(projected) = project_diff_model_output(name, output, cwd) {
        return projected;
    }
    if output.len() <= MODEL_OUTPUT_INLINE_LIMIT {
        return output.to_string();
    }

    let full_output_path = write_full_tool_output(name, output, cwd);
    let mut projected =
        serde_json::from_str::<Value>(output).unwrap_or_else(|_| json!({ "output": output }));
    let mut truncated = false;
    truncate_large_strings(&mut projected, &mut truncated);

    match &mut projected {
        Value::Object(map) => {
            map.insert("model_output_truncated".to_string(), json!(true));
            map.insert(
                "full_output_path".to_string(),
                json!(full_output_path.display().to_string()),
            );
            map.insert(
                "note".to_string(),
                json!("Large tool output was projected for the model. Inspect full_output_path if exact full output is needed."),
            );
        }
        _ => {
            projected = json!({
                "tool": name,
                "model_output_truncated": true,
                "full_output_path": full_output_path.display().to_string(),
                "output": truncate_text(output, MODEL_OUTPUT_FIELD_LIMIT),
            });
        }
    }

    let serialized = projected.to_string();
    if serialized.len() <= MODEL_OUTPUT_INLINE_LIMIT {
        return serialized;
    }

    json!({
        "tool": name,
        "model_output_truncated": true,
        "full_output_path": full_output_path.display().to_string(),
        "output": truncate_text(&serialized, MODEL_OUTPUT_INLINE_LIMIT / 2),
    })
    .to_string()
}

fn project_read_model_output(name: &str, output: &str, cwd: &Path) -> Option<String> {
    if env::var("JUCODE_PROJECT_READ_MODEL_OUTPUT")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "0" | "false" | "FALSE" | "off" | "OFF"))
    {
        return None;
    }
    project_read_model_output_inner(name, output, cwd)
}

fn project_read_model_output_inner(name: &str, output: &str, cwd: &Path) -> Option<String> {
    if name != "read" {
        return None;
    }
    let mut value = serde_json::from_str::<Value>(output).ok()?;
    let content_len = value
        .get("content")
        .and_then(Value::as_str)
        .map(|text| text.len())
        .unwrap_or(0);
    let hashlines_len = value
        .get("hashlines")
        .and_then(Value::as_str)
        .map(|text| text.len())
        .unwrap_or(0);
    if content_len == 0 || hashlines_len == 0 {
        return None;
    }
    let omit_content = content_len >= READ_MODEL_CONTENT_OMIT_THRESHOLD;
    let truncate_hashlines = hashlines_len > READ_MODEL_HASHLINES_LIMIT;
    if !omit_content && !truncate_hashlines {
        return None;
    }
    let full_output_path = truncate_hashlines.then(|| write_full_tool_output(name, output, cwd));
    if let Value::Object(map) = &mut value {
        if omit_content {
            map.remove("content");
        }
        if truncate_hashlines {
            if let Some(Value::String(hashlines)) = map.get_mut("hashlines") {
                *hashlines = truncate_text(hashlines, MODEL_OUTPUT_FIELD_LIMIT);
            }
        }
        map.insert("model_output_truncated".to_string(), json!(true));
        if let Some(path) = full_output_path {
            map.insert(
                "full_output_path".to_string(),
                json!(path.display().to_string()),
            );
        }
        map.insert(
            "note".to_string(),
            json!("Large read output was projected for the model. Re-read with offset/limit when exact nearby lines or anchors are needed."),
        );
        return Some(value.to_string());
    }
    None
}

/// Replaces the inline base64 payload of an image read with a short note. The
/// actual pixels are delivered to the model as a separate image message via
/// [`image_content_item`], so keeping the base64 in the function output would
/// only waste tokens.
fn project_image_model_output(name: &str, output: &str) -> Option<String> {
    if name != "read" {
        return None;
    }
    let mut value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("kind").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let map = value.as_object_mut()?;
    map.remove("base64")?;
    map.insert(
        "note".to_string(),
        json!("Image content is attached as a separate message; view it directly."),
    );
    Some(value.to_string())
}

/// Builds a user image message from a `read` tool output that contains an inline
/// base64 image, so the model can actually see the pixels. Returns `None` for
/// non-image outputs.
pub fn image_content_item(output: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("kind").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let mime = value.get("mime").and_then(Value::as_str)?;
    let base64 = value.get("base64").and_then(Value::as_str)?;
    Some(json!({
        "role": "user",
        "content": [{
            "type": "input_image",
            "image_url": format!("data:{mime};base64,{base64}"),
        }],
    }))
}

/// Reads a local image file and returns an `input_image` content part for a user
/// message, or an error describing why it cannot be attached.
pub fn image_attachment_part(path: &Path) -> Result<Value, String> {
    let Some(mime) = image_mime(path) else {
        return Err(format!("{}: not a supported image", path.display()));
    };
    let metadata = fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if metadata.len() > MAX_IMAGE_READ_BYTES {
        return Err(format!("{}: image is too large to attach", path.display()));
    }
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(json!({
        "type": "input_image",
        "image_url": format!("data:{mime};base64,{}", BASE64_STANDARD.encode(bytes)),
    }))
}

/// Cheap validation (no read/encode) of an image attachment path. Returns an
/// error message if the path is not an attachable image, else `None`.
pub fn image_attachment_error(path: &Path) -> Option<String> {
    if image_mime(path).is_none() {
        return Some(format!("{}: not a supported image", path.display()));
    }
    match fs::metadata(path) {
        Err(error) => Some(format!("{}: {error}", path.display())),
        Ok(metadata) if metadata.len() > MAX_IMAGE_READ_BYTES => {
            Some(format!("{}: image is too large to attach", path.display()))
        }
        Ok(_) => None,
    }
}

fn project_diff_model_output(name: &str, output: &str, cwd: &Path) -> Option<String> {
    if env::var("JUCODE_PROJECT_DIFF_MODEL_OUTPUT")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "0" | "false" | "FALSE" | "off" | "OFF"))
    {
        return None;
    }
    let mut value = serde_json::from_str::<Value>(output).ok()?;
    let diff = value.get("diff").and_then(Value::as_str)?.to_string();
    if diff.len() <= DIFF_MODEL_OUTPUT_INLINE_LIMIT && output.len() <= MODEL_OUTPUT_INLINE_LIMIT {
        return None;
    }
    let full_output_path = write_full_tool_output(name, output, cwd);
    let summary = summarize_unified_diff(&diff);
    if let Value::Object(map) = &mut value {
        map.insert(
            "diff".to_string(),
            json!(truncate_text(&diff, MODEL_OUTPUT_FIELD_LIMIT)),
        );
        map.insert("diff_summary".to_string(), summary);
        map.insert("model_output_truncated".to_string(), json!(true));
        map.insert(
            "full_output_path".to_string(),
            json!(full_output_path.display().to_string()),
        );
        map.insert(
            "note".to_string(),
            json!("Large diff was summarized for the model. Inspect full_output_path only if exact omitted hunks are necessary; otherwise use path-specific diff/read commands."),
        );
        return Some(value.to_string());
    }
    None
}

fn summarize_unified_diff(diff: &str) -> Value {
    let mut files = Vec::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    let mut hunks = 0usize;
    for line in diff.lines() {
        if let Some(file) = diff_file_from_header(line) {
            if !files.iter().any(|existing| existing == &file) {
                files.push(file);
            }
        } else if line.starts_with("@@") {
            hunks += 1;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
        }
    }
    let total_files = files.len();
    if files.len() > 20 {
        files.truncate(20);
    }
    json!({
        "files_changed": total_files,
        "files_sample": files,
        "hunks": hunks,
        "additions": additions,
        "deletions": deletions,
        "diff_bytes": diff.len(),
        "diff_lines": diff.lines().count(),
    })
}

fn diff_file_from_header(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        let mut parts = rest.split_whitespace();
        let _old = parts.next()?;
        let new = parts.next()?;
        return Some(strip_diff_prefix(new).to_string());
    }
    if let Some(path) = line.strip_prefix("+++ ") {
        let path = path.trim();
        if path != "/dev/null" {
            return Some(strip_diff_prefix(path).to_string());
        }
    }
    None
}

fn strip_diff_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

fn truncate_large_strings(value: &mut Value, truncated: &mut bool) {
    match value {
        Value::String(text) if text.len() > MODEL_OUTPUT_FIELD_LIMIT => {
            *text = truncate_text(text, MODEL_OUTPUT_FIELD_LIMIT);
            *truncated = true;
        }
        Value::Array(items) => {
            for item in items {
                truncate_large_strings(item, truncated);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                truncate_large_strings(value, truncated);
            }
        }
        _ => {}
    }
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let half = limit / 2;
    let head_end = safe_boundary(text, half);
    let tail_start = text.len() - safe_boundary_rev(text, half);
    format!(
        "{}\n\n[...model projection omitted {} bytes...]\n\n{}",
        &text[..head_end],
        text.len()
            .saturating_sub(head_end + (text.len() - tail_start)),
        &text[tail_start..]
    )
}

fn safe_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn safe_boundary_rev(text: &str, mut width: usize) -> usize {
    width = width.min(text.len());
    while width > 0 && !text.is_char_boundary(text.len() - width) {
        width -= 1;
    }
    width
}

fn write_full_tool_output(name: &str, output: &str, cwd: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(output.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let safe_name = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let dir = cwd.join(".jucode").join("truncated-results");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{safe_name}-{}.json", &hash[..16]));
    let _ = fs::write(&path, output);
    path
}

fn apply_patch(
    args: &Value,
    cwd: &Path,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Value {
    let Some(patch) = args.get("patch").and_then(Value::as_str) else {
        return json!({ "error": "missing patch" });
    };
    if patch.trim().is_empty() {
        return json!({ "error": "patch must not be empty" });
    }

    let check = run_command_events(
        "git",
        &["apply", "--check", "--whitespace=nowarn", "-"],
        Some(patch),
        cwd,
        Duration::from_secs(30),
        emit,
    );
    if let Ok(check) = &check {
        if check.exit_code != Some(0) {
            return command_result_json("git apply --check", Ok(check.clone()));
        }
    }

    match check {
        Ok(_) => {
            let result = run_command_events(
                "git",
                &["apply", "--whitespace=nowarn", "-"],
                Some(patch),
                cwd,
                Duration::from_secs(30),
                emit,
            );
            let mut value = command_result_json("git apply", result);
            value["applied"] = json!(value.get("exit_code").and_then(Value::as_i64) == Some(0));
            if value["applied"].as_bool().unwrap_or(false) {
                value["diff"] = json!(git_diff(cwd, None).unwrap_or_default());
            }
            value
        }
        Err(error) => json!({ "command": "git apply --check", "error": error }),
    }
}

fn list_dir(args: &Value, cwd: &Path) -> Value {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(|path| resolve_path(cwd, path))
        .unwrap_or_else(|| cwd.to_path_buf());
    let limit = optional_usize(args, "limit").map(|limit| limit.max(1));

    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };

    let mut names = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let mut name = entry.file_name().to_string_lossy().to_string();
        if entry
            .file_type()
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false)
        {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    let truncated = limit.is_some_and(|limit| names.len() > limit);
    if let Some(limit) = limit {
        names.truncate(limit);
    }

    json!({
        "path": path.display().to_string(),
        "entries": names,
        "truncated": truncated,
    })
}

fn ripgrep(args: &Value, cwd: &Path) -> Value {
    let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
        return json!({ "error": "missing pattern" });
    };
    let search_path = args
        .get("path")
        .and_then(Value::as_str)
        .map(|path| resolve_path(cwd, path))
        .unwrap_or_else(|| cwd.to_path_buf());
    let limit = optional_usize(args, "limit").map(|limit| limit.max(1));
    let context_lines = optional_usize(args, "contextLines").unwrap_or(0);

    let mut command_args = vec![
        "--line-number".to_string(),
        "--no-heading".to_string(),
        "--color".to_string(),
        "never".to_string(),
    ];
    if args
        .get("ignoreCase")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        command_args.push("--ignore-case".to_string());
    }
    if args
        .get("literal")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        command_args.push("--fixed-strings".to_string());
    }
    if context_lines > 0 {
        command_args.push("--context".to_string());
        command_args.push(context_lines.to_string());
    }
    if let Some(glob) = args.get("glob").and_then(Value::as_str) {
        command_args.push("--glob".to_string());
        command_args.push(glob.to_string());
    }
    command_args.push(pattern.to_string());
    command_args.push(search_path.display().to_string());

    let arg_refs = command_args.iter().map(String::as_str).collect::<Vec<_>>();
    let result = run_command("rg", &arg_refs, cwd, Duration::from_secs(30));
    let mut value = command_result_json("rg", result);
    if let (Some(stdout), Some(limit)) = (value.get("stdout").and_then(Value::as_str), limit) {
        let lines = stdout.lines().take(limit).collect::<Vec<_>>();
        let truncated = stdout.lines().count() > limit;
        value["stdout"] = json!(lines.join("\n"));
        value["truncated"] = json!(value["truncated"].as_bool().unwrap_or(false) || truncated);
    }
    value["path"] = json!(search_path.display().to_string());
    add_ripgrep_soft_hint(&mut value, limit.is_none());
    value
}

fn add_ripgrep_soft_hint(value: &mut Value, no_limit: bool) {
    let stdout = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        add_soft_hint(
            value,
            "ripgrep output was truncated",
            "Use a narrower path, glob, more specific pattern, or a limit before reading more.",
        );
    } else if no_limit
        && (stdout.lines().count() > LARGE_RIPGREP_OUTPUT_SOFT_LINES
            || stdout.len() > LARGE_RIPGREP_OUTPUT_SOFT_BYTES)
    {
        add_soft_hint(
            value,
            "large ripgrep output without a line limit",
            "Use a narrower path, glob, more specific pattern, or limit to reduce context before inspecting matches.",
        );
    }
}

fn outline_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };
    let path = resolve_path(cwd, path);
    let limit = optional_usize(args, "limit").unwrap_or(200).max(1);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };
    let mut symbols = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if let Some(symbol) = symbol_from_line(line) {
            symbols.push(json!({ "line": index + 1, "symbol": symbol }));
            if symbols.len() >= limit {
                break;
            }
        }
    }
    json!({
        "path": path.display().to_string(),
        "symbols": symbols,
        "truncated": content.lines().count() > limit && symbols.len() == limit,
    })
}

fn checkpoint_tool(args: &Value, cwd: &Path) -> Value {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match action {
        "create" => {
            let paths = args
                .get("paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|path| resolve_path(cwd, path))
                .collect::<Vec<_>>();
            if paths.is_empty() {
                return json!({ "error": "checkpoint create requires paths" });
            }
            let name = args.get("name").and_then(Value::as_str).unwrap_or("manual");
            match create_checkpoint(cwd, name, &paths) {
                Ok(meta) => meta,
                Err(error) => json!({ "error": error.to_string() }),
            }
        }
        "list" => match list_checkpoints(cwd) {
            Ok(items) => json!({ "checkpoints": items }),
            Err(error) => json!({ "error": error.to_string() }),
        },
        "restore" => {
            let Some(id) = args.get("id").and_then(Value::as_str) else {
                return json!({ "error": "checkpoint restore requires id" });
            };
            match restore_checkpoint(cwd, id) {
                Ok(value) => value,
                Err(error) => json!({ "error": error.to_string() }),
            }
        }
        _ => json!({ "error": "checkpoint action must be create, list, or restore" }),
    }
}

fn image_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

fn decode_text_bytes(bytes: &[u8]) -> Option<(String, &'static str)> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        return decode_utf16(&bytes[2..], true).map(|text| (text, "utf-16le"));
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return decode_utf16(&bytes[2..], false).map(|text| (text, "utf-16be"));
    }
    String::from_utf8(bytes.to_vec())
        .ok()
        .map(|text| (text, "utf-8"))
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&units).ok()
}

fn symbol_from_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let prefixes = [
        "fn ",
        "pub fn ",
        "struct ",
        "pub struct ",
        "enum ",
        "pub enum ",
        "trait ",
        "pub trait ",
        "impl ",
        "func ",
        "type ",
        "class ",
        "export function ",
        "function ",
        "export class ",
    ];
    prefixes
        .iter()
        .find(|prefix| trimmed.starts_with(**prefix))
        .map(|_| trimmed.trim_end().to_string())
}

fn mark_read(path: &Path) {
    if let Ok(mut paths) = read_tracker().lock() {
        paths.insert(normalize_path_key(path));
    }
}

fn has_read(path: &Path) -> bool {
    read_tracker()
        .lock()
        .map(|paths| paths.contains(&normalize_path_key(path)))
        .unwrap_or(false)
}

fn normalize_path_key(path: &Path) -> String {
    let value = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string();
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

static READ_TRACKER: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn read_tracker() -> &'static Mutex<HashSet<String>> {
    READ_TRACKER.get_or_init(|| Mutex::new(HashSet::new()))
}

fn create_checkpoint(cwd: &Path, name: &str, paths: &[PathBuf]) -> io::Result<Value> {
    let id = format!("cp-{}", now_nanos());
    let mut files = Vec::new();
    let mut bytes = 0usize;
    for path in paths {
        let abs = resolve_existing_or_future(cwd, path);
        if !is_inside(cwd, &abs) {
            continue;
        }
        let rel = diff_label(cwd, &abs);
        let content = match fs::read_to_string(&abs) {
            Ok(content) => {
                bytes += content.len();
                Value::String(content)
            }
            Err(_) if abs.exists() => Value::Null,
            Err(_) => Value::Null,
        };
        files.push(json!({ "path": rel, "content": content }));
    }
    let checkpoint = json!({
        "id": id,
        "name": name,
        "created_at": now_secs(),
        "files": files,
        "bytes": bytes,
    });
    let dir = checkpoint_dir(cwd);
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join(format!("{id}.json")),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&checkpoint).map_err(io::Error::other)?
        ),
    )?;
    Ok(json!({
        "id": id,
        "name": name,
        "files": checkpoint["files"].as_array().map(Vec::len).unwrap_or(0),
        "bytes": bytes,
    }))
}

fn list_checkpoints(cwd: &Path) -> io::Result<Vec<Value>> {
    let dir = checkpoint_dir(cwd);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        items.push(json!({
            "id": value.get("id").cloned().unwrap_or_default(),
            "name": value.get("name").cloned().unwrap_or_default(),
            "created_at": value.get("created_at").cloned().unwrap_or_default(),
            "files": value.get("files").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "bytes": value.get("bytes").cloned().unwrap_or_default(),
        }));
    }
    items.sort_by_key(|item| item.get("created_at").and_then(Value::as_u64).unwrap_or(0));
    items.reverse();
    Ok(items)
}

fn restore_checkpoint(cwd: &Path, id: &str) -> io::Result<Value> {
    let path = checkpoint_dir(cwd).join(format!("{id}.json"));
    let value =
        serde_json::from_str::<Value>(&fs::read_to_string(path)?).map_err(io::Error::other)?;
    let mut restored = Vec::new();
    let mut removed = Vec::new();
    for file in value
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(rel) = file.get("path").and_then(Value::as_str) else {
            continue;
        };
        let abs = resolve_path(cwd, rel);
        if !is_inside(cwd, &abs) {
            continue;
        }
        if let Some(content) = file.get("content").and_then(Value::as_str) {
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&abs, content)?;
            mark_read(&abs);
            restored.push(rel.to_string());
        } else if abs.exists() {
            fs::remove_file(&abs)?;
            removed.push(rel.to_string());
        }
    }
    Ok(json!({ "id": id, "restored": restored, "removed": removed }))
}

fn checkpoint_dir(cwd: &Path) -> PathBuf {
    cwd.join(".jucode").join("checkpoints")
}

fn resolve_existing_or_future(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn is_inside(root: &Path, path: &Path) -> bool {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    path == root || path.starts_with(root)
}

#[derive(Clone)]
struct CommandResult {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    truncated: bool,
}

fn run_command(
    program: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandResult, String> {
    run_command_events(program, args, None, cwd, timeout, &mut |_| Ok(()))
}

fn run_command_events(
    program: &str,
    args: &[&str],
    stdin: Option<&str>,
    cwd: &Path,
    timeout: Duration,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Result<CommandResult, String> {
    let stdout_path = temp_output_path("stdout");
    let stderr_path = temp_output_path("stderr");
    let stdout_file = File::create(&stdout_path).map_err(|error| error.to_string())?;
    let stderr_file = File::create(&stderr_path).map_err(|error| error.to_string())?;

    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|error| format!("failed to start {program}: {error}"))?;

    if let Some(input) = stdin {
        if let Some(mut child_stdin) = child.stdin.take() {
            child_stdin
                .write_all(input.as_bytes())
                .map_err(|error| error.to_string())?;
        }
    }

    let started = SystemTime::now();
    let mut last_update = started;
    let mut timed_out = false;
    emit(ToolExecutionEvent::Update(format!(
        "started: {}",
        command_display(program, args)
    )))?;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => return Err(error.to_string()),
        }

        if started.elapsed().unwrap_or_default() >= timeout {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        if last_update.elapsed().unwrap_or_default() >= COMMAND_UPDATE_INTERVAL {
            let (stdout, stdout_truncated) = read_output_file(&stdout_path);
            let (stderr, stderr_truncated) = read_output_file(&stderr_path);
            emit(ToolExecutionEvent::Update(command_update_text(
                &stdout,
                &stderr,
                stdout_truncated || stderr_truncated,
                started.elapsed().unwrap_or_default(),
            )))?;
            last_update = SystemTime::now();
        }
        thread::sleep(Duration::from_millis(50));
    }

    let status = child.wait().ok();
    let (stdout, stdout_truncated) = read_output_file(&stdout_path);
    let (stderr, stderr_truncated) = read_output_file(&stderr_path);
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);

    Ok(CommandResult {
        exit_code: status.and_then(|status| status.code()),
        stdout,
        stderr,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    })
}

fn command_result_json(command: &str, result: Result<CommandResult, String>) -> Value {
    let mut value = match result {
        Ok(result) => json!({
            "command": command,
            "exit_code": result.exit_code,
            "stdout": result.stdout,
            "stderr": result.stderr,
            "timed_out": result.timed_out,
            "truncated": result.truncated,
        }),
        Err(error) => json!({ "command": command, "error": error }),
    };
    add_command_soft_hint(&mut value);
    value
}

fn add_command_soft_hint(value: &mut Value) {
    let stdout = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let stderr = value
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let output_bytes = stdout.len().saturating_add(stderr.len());
    let output_lines = stdout
        .lines()
        .count()
        .saturating_add(stderr.lines().count());
    if value
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        add_soft_hint(
            value,
            "command timed out",
            "Inspect partial output, narrow the command, or rerun with a longer timeout only if the full command is necessary.",
        );
    } else if value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        add_soft_hint(
            value,
            "command output was truncated",
            "Rerun with a narrower command, redirect detailed output to a file, or inspect the referenced full output if provided.",
        );
    } else if output_bytes > COMMAND_OUTPUT_MAX_BYTES || output_lines > COMMAND_OUTPUT_MAX_LINES {
        add_soft_hint(
            value,
            "large command output",
            "Prefer a narrower command with paths, globs, grep/head/tail, or tool-native limits before requesting more output.",
        );
    }
}

fn unified_diff_for_file(cwd: &Path, path: &Path, original: &str, updated: &str) -> Option<String> {
    if original == updated {
        return None;
    }

    let old_path = temp_output_path("diff-old");
    let new_path = temp_output_path("diff-new");
    if fs::write(&old_path, original).is_err() || fs::write(&new_path, updated).is_err() {
        let _ = fs::remove_file(old_path);
        let _ = fs::remove_file(new_path);
        return None;
    }

    let label = diff_label(cwd, path);
    let old_arg = old_path.display().to_string();
    let new_arg = new_path.display().to_string();
    let result = run_command(
        "git",
        &[
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--",
            &old_arg,
            &new_arg,
        ],
        cwd,
        Duration::from_secs(30),
    );
    let _ = fs::remove_file(old_path);
    let _ = fs::remove_file(new_path);

    if let Ok(result) = result {
        if !result.stdout.trim().is_empty() {
            return Some(relabel_no_index_diff(&result.stdout, &label));
        }
    }
    Some(simple_unified_diff(&label, original, updated))
}

fn git_diff(cwd: &Path, path: Option<&Path>) -> Result<String, String> {
    if path.is_some_and(|path| is_internal_tool_path(cwd, path)) {
        return Ok(String::new());
    }
    let result = if let Some(path) = path {
        let path_arg = diff_path_arg(cwd, path);
        run_command(
            "git",
            &["diff", "--no-ext-diff", "--", &path_arg],
            cwd,
            Duration::from_secs(30),
        )
    } else {
        run_command(
            "git",
            &["diff", "--no-ext-diff"],
            cwd,
            Duration::from_secs(30),
        )
    };

    let result = result?;
    if result.exit_code != Some(0) {
        return Err(command_failure_message("git diff", &result));
    }

    let mut chunks = Vec::new();
    let tracked_diff = filter_internal_diff_sections(&result.stdout);
    if !tracked_diff.trim().is_empty() {
        chunks.push(tracked_diff);
    }
    for path in git_untracked_paths(cwd, path)? {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Some(diff) = unified_diff_for_file(cwd, &path, "", &content) {
                chunks.push(diff);
            }
        }
    }
    Ok(chunks.join("\n"))
}

fn git_untracked_paths(cwd: &Path, path: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    if path.is_some_and(|path| is_internal_tool_path(cwd, path)) {
        return Ok(Vec::new());
    }
    let path_arg = path.map(|path| diff_path_arg(cwd, path));
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "--"];
    if let Some(path_arg) = path_arg.as_deref() {
        args.push(path_arg);
    }
    let result = run_command("git", &args, cwd, Duration::from_secs(30))?;
    if result.exit_code != Some(0) {
        return Err(command_failure_message("git ls-files", &result));
    }
    Ok(result
        .stdout
        .lines()
        .map(|line| cwd.join(line))
        .filter(|path| path.is_file())
        .filter(|path| !is_internal_tool_path(cwd, path))
        .collect())
}

fn filter_internal_diff_sections(diff: &str) -> String {
    let mut output = String::new();
    let mut section = String::new();
    let mut keep_section = true;
    let mut saw_header = false;

    for line in diff.split_inclusive('\n') {
        let bare = line.trim_end_matches('\n');
        if bare.starts_with("diff --git ") {
            if keep_section {
                output.push_str(&section);
            }
            section.clear();
            saw_header = true;
            keep_section = !diff_header_is_internal(bare);
        }
        section.push_str(line);
    }
    if keep_section {
        output.push_str(&section);
    }
    if saw_header {
        output
    } else {
        diff.to_string()
    }
}

fn diff_header_is_internal(line: &str) -> bool {
    line.split_whitespace().skip(2).any(|path| {
        let path = strip_diff_prefix(path);
        path == ".jucode" || path.starts_with(".jucode/")
    })
}

fn is_internal_tool_path(cwd: &Path, path: &Path) -> bool {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let relative = path.strip_prefix(cwd).unwrap_or(path.as_path());
    relative
        .components()
        .next()
        .is_some_and(|component| component.as_os_str() == ".jucode")
}

fn command_failure_message(command: &str, result: &CommandResult) -> String {
    [result.stderr.trim(), result.stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{command} failed"))
}

fn diff_label(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn diff_path_arg(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

fn relabel_no_index_diff(diff: &str, label: &str) -> String {
    let mut output = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            output.push_str(&format!("diff --git a/{label} b/{label}\n"));
        } else if line.starts_with("--- ") {
            output.push_str(&format!("--- a/{label}\n"));
        } else if line.starts_with("+++ ") {
            output.push_str(&format!("+++ b/{label}\n"));
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

fn simple_unified_diff(label: &str, original: &str, updated: &str) -> String {
    let old_lines = original.lines().collect::<Vec<_>>();
    let new_lines = updated.lines().collect::<Vec<_>>();
    let mut diff = format!(
        "diff --git a/{label} b/{label}\n--- a/{label}\n+++ b/{label}\n@@ -1,{} +1,{} @@\n",
        old_lines.len(),
        new_lines.len()
    );
    for op in line_diff_ops(&old_lines, &new_lines) {
        let (prefix, line) = match op {
            DiffOp::Context(line) => (' ', line),
            DiffOp::Remove(line) => ('-', line),
            DiffOp::Add(line) => ('+', line),
        };
        diff.push(prefix);
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

enum DiffOp<'a> {
    Context(&'a str),
    Remove(&'a str),
    Add(&'a str),
}

fn line_diff_ops<'a>(old_lines: &[&'a str], new_lines: &[&'a str]) -> Vec<DiffOp<'a>> {
    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_mid = &old_lines[prefix..old_lines.len() - suffix];
    let new_mid = &new_lines[prefix..new_lines.len() - suffix];
    let mut ops = Vec::new();
    ops.extend(old_lines[..prefix].iter().copied().map(DiffOp::Context));
    ops.extend(line_diff_middle(old_mid, new_mid));
    ops.extend(
        old_lines[old_lines.len() - suffix..]
            .iter()
            .copied()
            .map(DiffOp::Context),
    );
    ops
}

fn line_diff_middle<'a>(old_lines: &[&'a str], new_lines: &[&'a str]) -> Vec<DiffOp<'a>> {
    if old_lines.is_empty() {
        return new_lines.iter().copied().map(DiffOp::Add).collect();
    }
    if new_lines.is_empty() {
        return old_lines.iter().copied().map(DiffOp::Remove).collect();
    }

    let cols = new_lines.len() + 1;
    let mut lcs = vec![0usize; (old_lines.len() + 1) * cols];
    for old_index in (0..old_lines.len()).rev() {
        for new_index in (0..new_lines.len()).rev() {
            let index = old_index * cols + new_index;
            lcs[index] = if old_lines[old_index] == new_lines[new_index] {
                lcs[(old_index + 1) * cols + new_index + 1] + 1
            } else {
                lcs[(old_index + 1) * cols + new_index].max(lcs[old_index * cols + new_index + 1])
            };
        }
    }

    let mut ops = Vec::new();
    let mut old_index = 0usize;
    let mut new_index = 0usize;
    while old_index < old_lines.len() && new_index < new_lines.len() {
        if old_lines[old_index] == new_lines[new_index] {
            ops.push(DiffOp::Context(old_lines[old_index]));
            old_index += 1;
            new_index += 1;
        } else if lcs[(old_index + 1) * cols + new_index] >= lcs[old_index * cols + new_index + 1] {
            ops.push(DiffOp::Remove(old_lines[old_index]));
            old_index += 1;
        } else {
            ops.push(DiffOp::Add(new_lines[new_index]));
            new_index += 1;
        }
    }
    ops.extend(old_lines[old_index..].iter().copied().map(DiffOp::Remove));
    ops.extend(new_lines[new_index..].iter().copied().map(DiffOp::Add));
    ops
}

fn command_display(program: &str, args: &[&str]) -> String {
    let mut parts = vec![program.to_string()];
    parts.extend(args.iter().map(|arg| arg.to_string()));
    parts.join(" ")
}

fn command_update_text(stdout: &str, stderr: &str, truncated: bool, elapsed: Duration) -> String {
    let mut lines = vec![format!("running {:.1}s", elapsed.as_secs_f32())];
    if !stdout.is_empty() {
        lines.push(format!("stdout:\n{}", tail_lines(stdout, 8)));
    }
    if !stderr.is_empty() {
        lines.push(format!("stderr:\n{}", tail_lines(stderr, 8)));
    }
    if truncated {
        lines.push("output truncated".to_string());
    }
    lines.join("\n")
}

fn tail_lines(text: &str, limit: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(limit);
    lines[start..].join("\n")
}

fn read_output_file(path: &Path) -> (String, bool) {
    let Ok(bytes) = fs::read(path) else {
        return (String::new(), false);
    };
    truncate_command_output(&String::from_utf8_lossy(&bytes))
}

fn truncate_command_output(text: &str) -> (String, bool) {
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let line_start = lines.len().saturating_sub(COMMAND_OUTPUT_MAX_LINES);
    let mut output = lines[line_start..].concat();

    let mut omitted_lines = line_start;
    let mut omitted_bytes = text.len().saturating_sub(output.len());
    if output.len() > COMMAND_OUTPUT_MAX_BYTES {
        let keep_width = safe_boundary_rev(&output, COMMAND_OUTPUT_MAX_BYTES);
        let keep_start = output.len() - keep_width;
        omitted_bytes += keep_start;
        omitted_lines += output[..keep_start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        output = output[keep_start..].to_string();
    }

    if omitted_lines == 0 && omitted_bytes == 0 {
        return (output, false);
    }

    (
        format!(
            "[...command output truncated: omitted {omitted_lines} earlier lines and {omitted_bytes} bytes...]\n{output}"
        ),
        true,
    )
}

fn shell_command(command: &str) -> (&'static str, Vec<&str>) {
    if cfg!(windows) {
        ("powershell", vec!["-NoProfile", "-Command", command])
    } else {
        ("sh", vec!["-lc", command])
    }
}

fn temp_output_path(label: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "jucode-tool-{label}-{}-{}.log",
        std::process::id(),
        now_nanos()
    ))
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn optional_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn optional_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_read_projects_note_and_builds_image_message() {
        let output = json!({
            "path": "/tmp/pic.png",
            "kind": "image",
            "mime": "image/png",
            "bytes": 3,
            "base64": "AAEC",
        })
        .to_string();

        let projected = project_model_output("read", &output, Path::new("."));
        let projected_value = serde_json::from_str::<Value>(&projected).unwrap();
        assert!(projected_value.get("base64").is_none());
        assert!(projected_value.get("note").is_some());

        let image = image_content_item(&output).unwrap();
        assert_eq!(image["role"], "user");
        assert_eq!(image["content"][0]["type"], "input_image");
        assert_eq!(
            image["content"][0]["image_url"],
            "data:image/png;base64,AAEC"
        );
    }

    #[test]
    fn non_image_read_has_no_image_message() {
        let output = json!({ "content": "hello", "hashlines": "1#ab hello" }).to_string();
        assert!(image_content_item(&output).is_none());
    }

    #[test]
    fn read_supports_offset_and_limit() {
        let dir = test_dir("read");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "one\ntwo\nthree\n").unwrap();

        let result = run_tool(
            "read",
            &json!({ "path": path, "offset": 2, "limit": 1 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["content"], "two\n");
        assert_eq!(value["lines_read"], 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_defaults_to_no_line_limit() {
        let dir = test_dir("read-no-limit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        let content = (1..=550)
            .map(|line| format!("line-{line}\n"))
            .collect::<String>();
        fs::write(&path, &content).unwrap();

        let result = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["lines_read"], 550);
        assert_eq!(value["truncated"], false);
        assert_eq!(value["content"], content);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_warns_when_large_file_has_no_limit() {
        let dir = test_dir("read-large-warning");
        let path = dir.join("large.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "x".repeat(300 * 1024)).unwrap();

        let result = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["warning"]
            .as_str()
            .unwrap()
            .contains("large file read"));
        assert!(value["suggestion"]
            .as_str()
            .unwrap()
            .contains("offset/limit"));
        assert_eq!(value["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn bash_truncates_large_stdout() {
        let dir = test_dir("bash-truncate");
        fs::create_dir_all(&dir).unwrap();

        let result = run_tool(
            "bash",
            &json!({
                "command": "i=0; while [ $i -lt 3505 ]; do echo line-$i; i=$((i+1)); done"
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        let stdout = value["stdout"].as_str().unwrap();

        assert_eq!(value["truncated"], true);
        assert!(stdout.contains("command output truncated"));
        assert!(stdout.contains("line-3504"));
        assert!(!stdout.contains("line-0\n"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_requires_unique_old_text() {
        let dir = test_dir("edit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "same\nsame\n").unwrap();
        let _ = run_tool("read", &json!({ "path": path }).to_string(), &dir);

        let result = run_tool(
            "edit",
            &json!({
                "path": path,
                "edits": [{ "oldText": "same", "newText": "next" }]
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["error"].as_str().unwrap().contains("exactly once"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "same\nsame\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_applies_targeted_replacement() {
        let dir = test_dir("edit-ok");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "alpha\nbeta\n").unwrap();
        let _ = run_tool("read", &json!({ "path": path }).to_string(), &dir);

        let result = run_tool(
            "edit",
            &json!({
                "path": path,
                "edits": [{ "oldText": "beta", "newText": "gamma" }]
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["edits"], 1);
        assert_eq!(
            fs::read_to_string(&path).unwrap().replace("\r\n", "\n"),
            "alpha\ngamma\n"
        );
        let diff = value["diff"].as_str().unwrap();
        assert!(diff.contains("-beta"));
        assert!(diff.contains("+gamma"));
        assert!(diff.contains(" alpha"));
        assert!(!diff.contains("-alpha"));
        assert!(!diff.contains("+alpha"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_returns_git_style_diff() {
        let dir = test_dir("write-diff");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "before\n").unwrap();
        let _ = run_tool("read", &json!({ "path": path }).to_string(), &dir);

        let result = run_tool(
            "write",
            &json!({ "path": path, "content": "after\n" }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["diff"].as_str().unwrap().contains("--- a/"));
        assert!(value["diff"].as_str().unwrap().contains("+++ b/"));
        assert!(value["diff"].as_str().unwrap().contains("-before"));
        assert!(value["diff"].as_str().unwrap().contains("+after"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_creates_new_file_without_prior_read() {
        let dir = test_dir("write-create");
        let path = dir.join("src").join("main.rs");
        fs::create_dir_all(&dir).unwrap();

        let result = run_tool(
            "write",
            &json!({ "path": path, "content": "fn main() {}\n" }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "fn main() {}\n");
        assert!(value.get("error").is_none());
        assert!(value["diff"].as_str().unwrap().contains("+fn main() {}"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_still_requires_read_before_overwriting_existing() {
        let dir = test_dir("write-read-first");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "before\n").unwrap();

        let result = run_tool(
            "write",
            &json!({ "path": path, "content": "after\n" }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("requires reading"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "before\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn git_diff_returns_empty_success_for_clean_workspace() {
        let dir = test_dir("diff-clean");
        fs::create_dir_all(&dir).unwrap();
        run_command("git", &["init"], &dir, Duration::from_secs(30)).unwrap();

        let diff = git_diff(&dir, None).unwrap();

        assert_eq!(diff, "");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn git_diff_includes_untracked_text_file() {
        let dir = test_dir("diff-untracked");
        fs::create_dir_all(&dir).unwrap();
        run_command("git", &["init"], &dir, Duration::from_secs(30)).unwrap();
        fs::write(dir.join("new.txt"), "hello\n").unwrap();

        let diff = git_diff(&dir, Some(&dir.join("new.txt"))).unwrap();

        assert!(diff.contains("new.txt"));
        assert!(diff.contains("+hello"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn git_diff_excludes_jucode_internal_files() {
        let dir = test_dir("diff-internal");
        fs::create_dir_all(dir.join(".jucode").join("checkpoints")).unwrap();
        run_command("git", &["init"], &dir, Duration::from_secs(30)).unwrap();
        fs::write(
            dir.join(".jucode").join("checkpoints").join("cp.json"),
            "{}\n",
        )
        .unwrap();
        fs::write(dir.join("new.txt"), "hello\n").unwrap();

        let diff = git_diff(&dir, None).unwrap();

        assert!(diff.contains("new.txt"));
        assert!(!diff.contains(".jucode"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hashline_edit_applies_anchor_replacement() {
        let dir = test_dir("hashline-edit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

        let read = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let read = serde_json::from_str::<Value>(&read).unwrap();
        let beta_anchor = read["hashlines"]
            .as_str()
            .unwrap()
            .lines()
            .find(|line| line.ends_with(":beta"))
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();

        let result = run_tool(
            "hashline_edit",
            &json!({
                "path": path,
                "edits": [{ "op": "replace", "pos": beta_anchor, "lines": ["BETA"] }]
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap().replace("\r\n", "\n"),
            "alpha\nBETA\ngamma\n"
        );
        assert!(value["anchors"].as_str().unwrap().contains("2#"));
        assert!(value["diff"].as_str().unwrap().contains("-beta"));
        assert!(value["diff"].as_str().unwrap().contains("+BETA"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hashline_hash_matches_reference_encoding() {
        assert_eq!(compute_line_hash(1, "alpha"), "JN");
        assert_eq!(compute_line_hash(2, "beta"), "NK");
        assert_eq!(compute_line_hash(3, "gamma"), "WB");
        assert_eq!(compute_line_hash(4, ""), "RW");
        assert_eq!(compute_line_hash(5, "  "), "BT");
        assert_eq!(compute_line_hash(6, "{"), "KM");
    }

    #[test]
    fn hashline_edit_rejects_stale_anchor() {
        let dir = test_dir("hashline-stale");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "alpha\nbeta\n").unwrap();

        let read = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let read = serde_json::from_str::<Value>(&read).unwrap();
        let beta_anchor = read["hashlines"]
            .as_str()
            .unwrap()
            .lines()
            .find(|line| line.ends_with(":beta"))
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();
        fs::write(&path, "alpha\nchanged\n").unwrap();

        let result = run_tool(
            "hashline_edit",
            &json!({
                "path": path,
                "edits": [{ "op": "replace", "pos": beta_anchor, "lines": ["BETA"] }]
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("[E_STALE_ANCHOR]"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha\nchanged\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ls_sorts_and_marks_directories() {
        let dir = test_dir("ls");
        fs::create_dir_all(dir.join("b_dir")).unwrap();
        fs::write(dir.join("a.txt"), "").unwrap();

        let result = run_tool("ls", &json!({ "path": dir }).to_string(), &env::temp_dir());
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["entries"][0], "a.txt");
        assert_eq!(value["entries"][1], "b_dir/");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ls_defaults_to_no_entry_limit() {
        let dir = test_dir("ls-no-limit");
        fs::create_dir_all(&dir).unwrap();
        for index in 0..550 {
            fs::write(dir.join(format!("{index:03}.txt")), "").unwrap();
        }

        let result = run_tool("ls", &json!({ "path": dir }).to_string(), &env::temp_dir());
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["entries"].as_array().unwrap().len(), 550);
        assert_eq!(value["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ripgrep_defaults_to_no_output_line_limit() {
        let dir = test_dir("rg-no-limit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "needle\n".repeat(150)).unwrap();

        let result = run_tool(
            "ripgrep",
            &json!({ "pattern": "needle", "path": path }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["stdout"].as_str().unwrap().lines().count(), 150);
        assert_eq!(value["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ripgrep_warns_when_broad_output_has_no_limit() {
        let dir = test_dir("rg-large-warning");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "needle\n".repeat(250)).unwrap();

        let result = run_tool(
            "ripgrep",
            &json!({ "pattern": "needle", "path": path }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["warning"]
            .as_str()
            .unwrap()
            .contains("large ripgrep output"));
        assert!(value["suggestion"].as_str().unwrap().contains("limit"));
        assert_eq!(value["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn definitions_expose_expected_tools() {
        let tools = definitions();
        let names = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "read",
                "str_replace",
                "hashline_edit",
                "write",
                "apply_patch",
                "bash",
                "exec_command",
                "write_stdin",
                "ls",
                "ripgrep",
                "outline",
                "checkpoint"
            ]
        );
        assert!(tools
            .iter()
            .all(|tool| tool.get("strict") == Some(&json!(false))));
    }

    #[test]
    fn exec_command_accepts_codex_style_cmd_and_workdir() {
        let dir = test_dir("exec-command");
        let subdir = dir.join("sub");
        fs::create_dir_all(&subdir).unwrap();
        fs::write(subdir.join("marker.txt"), "ok").unwrap();
        let command = if cfg!(windows) {
            "Get-ChildItem marker.txt | Select-Object -ExpandProperty Name"
        } else {
            "pwd; ls marker.txt"
        };

        let result = run_tool(
            "exec_command",
            &json!({ "cmd": command, "workdir": "sub", "timeout": 5 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["exit_code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("marker.txt"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn exec_command_truncates_large_output() {
        let dir = test_dir("exec-large-truncate");
        fs::create_dir_all(&dir).unwrap();
        let command = if cfg!(windows) {
            "1..3500 | ForEach-Object { 'line' }"
        } else {
            "yes line | head -n 3500"
        };

        let result = run_tool(
            "exec_command",
            &json!({ "cmd": command, "timeout": 5 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["truncated"], true);
        assert!(value["stdout"]
            .as_str()
            .unwrap()
            .contains("command output truncated"));
        assert!(value["warning"]
            .as_str()
            .unwrap()
            .contains("command output was truncated"));
        assert!(value["suggestion"]
            .as_str()
            .unwrap()
            .contains("narrower command"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bash_accepts_workdir() {
        let dir = test_dir("bash-workdir");
        let subdir = dir.join("sub");
        fs::create_dir_all(&subdir).unwrap();
        fs::write(subdir.join("marker.txt"), "ok").unwrap();
        let command = if cfg!(windows) {
            "Get-ChildItem marker.txt | Select-Object -ExpandProperty Name"
        } else {
            "ls marker.txt"
        };

        let result = run_tool(
            "bash",
            &json!({ "command": command, "workdir": "sub", "timeout": 5 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["exit_code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("marker.txt"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn apply_patch_applies_unified_diff() {
        let dir = test_dir("apply-patch");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "alpha\nbeta\n").unwrap();

        let patch = r#"diff --git a/sample.txt b/sample.txt
--- a/sample.txt
+++ b/sample.txt
@@ -1,2 +1,2 @@
 alpha
-beta
+gamma
"#;
        let result = run_tool("apply_patch", &json!({ "patch": patch }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["applied"], true);
        assert_eq!(
            fs::read_to_string(&path).unwrap().replace("\r\n", "\n"),
            "alpha\ngamma\n"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bash_emits_lifecycle_updates() {
        let dir = test_dir("bash-updates");
        fs::create_dir_all(&dir).unwrap();
        let mut updates = Vec::new();

        let result = run_tool_with_events(
            "bash",
            &json!({ "command": "echo hello", "timeout": 5 }).to_string(),
            &dir,
            |event| {
                let ToolExecutionEvent::Update(output) = event;
                updates.push(output);
                Ok(())
            },
        );
        let value = serde_json::from_str::<Value>(&result.output).unwrap();

        assert!(!result.is_error);
        assert_eq!(value["exit_code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("hello"));
        assert!(updates.iter().any(|update| update.contains("started:")));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bash_can_yield_running_session_and_poll_it() {
        let dir = test_dir("bash-session");
        fs::create_dir_all(&dir).unwrap();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 300; Write-Output done"
        } else {
            "sleep 0.3; echo done"
        };

        let result = run_tool(
            "bash",
            &json!({ "command": command, "timeout": 5, "yield_time_ms": 1 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        let session_id = value["session_id"].as_u64().unwrap();
        assert_eq!(value["running"], true);

        let mut value = json!({ "running": true });
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            let result = run_tool(
                "write_stdin",
                &json!({ "session_id": session_id, "yield_time_ms": 100 }).to_string(),
                &dir,
            );
            value = serde_json::from_str::<Value>(&result).unwrap();
            if value["running"] != true {
                break;
            }
        }

        assert_eq!(value["exit_code"], 0, "{value}");
        assert!(value["stdout"].as_str().unwrap().contains("done"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_stdin_accepts_codex_style_chars() {
        let dir = test_dir("write-stdin-chars");
        fs::create_dir_all(&dir).unwrap();
        let command = if cfg!(windows) {
            "$line = [Console]::In.ReadLine(); Write-Output $line"
        } else {
            "head -n 1"
        };

        let result = run_tool(
            "exec_command",
            &json!({ "cmd": command, "timeout": 5, "yield_time_ms": 1 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        let session_id = value["session_id"].as_u64().unwrap();

        let mut value = json!({ "running": true });
        for index in 0..20 {
            let args = if index == 0 {
                json!({ "session_id": session_id, "chars": "hello\n", "yield_time_ms": 250 })
            } else {
                json!({ "session_id": session_id, "yield_time_ms": 250 })
            };
            let result = run_tool("write_stdin", &args.to_string(), &dir);
            value = serde_json::from_str::<Value>(&result).unwrap();
            if value["running"] != true {
                break;
            }
        }

        assert_eq!(value["exit_code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("hello"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bash_truncates_output_instead_of_returning_full_stdout() {
        let dir = test_dir("bash-tail-limit");
        fs::create_dir_all(&dir).unwrap();
        let command = if cfg!(windows) {
            "1..9000 | ForEach-Object { 'line' }"
        } else {
            "yes line | head -n 9000"
        };

        let result = run_tool(
            "bash",
            &json!({ "command": command, "timeout": 5 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["truncated"], true);
        assert!(value["stdout"].as_str().unwrap().lines().count() < 9000);
        assert!(value["stdout"]
            .as_str()
            .unwrap()
            .contains("command output truncated"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn long_tool_result_is_projected_for_model() {
        let dir = test_dir("tool-projection");
        fs::create_dir_all(&dir).unwrap();
        let content = "x".repeat(128 * 1024);
        let result = tool_result("test_tool", json!({ "content": content }), &dir);
        let projected = serde_json::from_str::<Value>(&result.model_output).unwrap();
        let full_output_path = projected["full_output_path"].as_str().unwrap();

        assert_ne!(result.output, result.model_output);
        assert!(result.model_output.contains("model_output_truncated"));
        assert_eq!(fs::read_to_string(full_output_path).unwrap(), result.output);
        assert!(dir.join(".jucode").join("truncated-results").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn large_edit_diff_is_summarized_for_model_output() {
        let dir = test_dir("diff-projection");
        fs::create_dir_all(&dir).unwrap();
        let diff_body = (0..700)
            .map(|index| format!("+added line {index}\n"))
            .collect::<String>();
        let diff = format!(
            "diff --git a/src/app.rs b/src/app.rs\n--- a/src/app.rs\n+++ b/src/app.rs\n@@ -0,0 +1,700 @@\n{diff_body}"
        );

        let result = tool_result(
            "write",
            json!({
                "path": dir.display().to_string(),
                "has_changes": true,
                "diff": diff
            }),
            &dir,
        );
        let projected = serde_json::from_str::<Value>(&result.model_output).unwrap();
        let full_output_path = projected["full_output_path"].as_str().unwrap();

        assert_ne!(result.output, result.model_output);
        assert_eq!(projected["model_output_truncated"], true);
        assert_eq!(projected["diff_summary"]["files_changed"], 1);
        assert_eq!(projected["diff_summary"]["additions"], 700);
        assert!(projected["diff"].as_str().unwrap().len() < 6000);
        assert_eq!(fs::read_to_string(full_output_path).unwrap(), result.output);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_model_output_omits_duplicate_content_but_keeps_hashlines() {
        let dir = test_dir("read-projection");
        fs::create_dir_all(&dir).unwrap();
        let content = format!("{}\n{}", "a".repeat(2048), "b".repeat(2048));
        let hashlines = format!("1#AA:{}\n2#BB:{}", "a".repeat(2048), "b".repeat(2048));
        let output = json!({
            "path": "/tmp/example.py",
            "kind": "text",
            "content": content,
            "hashlines": hashlines,
            "lines_read": 2,
            "truncated": false
        })
        .to_string();

        let projected = serde_json::from_str::<Value>(
            &project_read_model_output_inner("read", &output, &dir).unwrap(),
        )
        .unwrap();

        assert!(projected.get("content").is_none());
        assert!(projected["hashlines"].as_str().unwrap().contains("1#AA:"));
        assert_eq!(projected["model_output_truncated"], true);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_model_output_truncates_large_hashlines_and_keeps_full_output_path() {
        let dir = test_dir("read-hashlines-projection");
        fs::create_dir_all(&dir).unwrap();
        let content = (0..600)
            .map(|index| format!("line {index}: {}\n", "x".repeat(32)))
            .collect::<String>();
        let hashlines = content
            .lines()
            .enumerate()
            .map(|(index, line)| format!("{}#AA:{line}\n", index + 1))
            .collect::<String>();
        let output = json!({
            "path": "/tmp/example.py",
            "kind": "text",
            "content": content,
            "hashlines": hashlines,
            "lines_read": 600,
            "truncated": false
        })
        .to_string();

        let projected = serde_json::from_str::<Value>(
            &project_read_model_output_inner("read", &output, &dir).unwrap(),
        )
        .unwrap();
        let full_output_path = projected["full_output_path"].as_str().unwrap();

        assert!(projected.get("content").is_none());
        assert!(projected["hashlines"]
            .as_str()
            .unwrap()
            .contains("model projection omitted"));
        assert_eq!(fs::read_to_string(full_output_path).unwrap(), output);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_reports_image_payloads() {
        let dir = test_dir("read-image");
        let path = dir.join("tiny.png");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, [137, 80, 78, 71]).unwrap();

        let result = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["kind"], "image");
        assert_eq!(value["mime"], "image/png");
        assert_eq!(value["base64"], "iVBORw==");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_supports_utf16_bom_text() {
        let dir = test_dir("read-utf16");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, [0xff, 0xfe, b'h', 0, b'i', 0, b'\n', 0]).unwrap();

        let result = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["kind"], "text");
        assert_eq!(value["encoding"], "utf-16le");
        assert_eq!(value["content"], "hi\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_requires_read_first() {
        let dir = test_dir("read-before-edit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "alpha\n").unwrap();

        let result = run_tool(
            "edit",
            &json!({ "path": path, "edits": [{ "oldText": "alpha", "newText": "beta" }] })
                .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("requires reading"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn checkpoint_can_restore_file_content() {
        let dir = test_dir("checkpoint");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "before\n").unwrap();

        let created = run_tool(
            "checkpoint",
            &json!({ "action": "create", "name": "manual", "paths": [path] }).to_string(),
            &dir,
        );
        let created = serde_json::from_str::<Value>(&created).unwrap();
        fs::write(dir.join("sample.txt"), "after\n").unwrap();
        let restored = run_tool(
            "checkpoint",
            &json!({ "action": "restore", "id": created["id"] }).to_string(),
            &dir,
        );
        let restored = serde_json::from_str::<Value>(&restored).unwrap();

        assert_eq!(restored["restored"][0], "sample.txt");
        assert_eq!(
            fs::read_to_string(dir.join("sample.txt")).unwrap(),
            "before\n"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn outline_extracts_lightweight_symbols() {
        let dir = test_dir("outline");
        let path = dir.join("lib.rs");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "pub struct App {}\nimpl App {}\npub fn run() {}\n").unwrap();

        let result = run_tool("outline", &json!({ "path": path }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["symbols"][0]["symbol"], "pub struct App {}");
        assert_eq!(value["symbols"][2]["symbol"], "pub fn run() {}");
        let _ = fs::remove_dir_all(dir);
    }

    fn test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("jucode-tools-test-{name}-{nanos}"))
    }
}
