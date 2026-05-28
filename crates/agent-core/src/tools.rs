use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env, fs,
    fs::File,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const DEFAULT_READ_LIMIT: usize = 400;
const DEFAULT_LIST_LIMIT: usize = 500;
const DEFAULT_RIPGREP_LIMIT: usize = 100;
const DEFAULT_BASH_TIMEOUT_SECS: u64 = 60;
const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_MODEL_OUTPUT_BYTES: usize = 24 * 1024;
const COMMAND_UPDATE_INTERVAL: Duration = Duration::from_millis(500);

pub struct ToolExecutionResult {
    pub output: String,
    pub model_output: String,
    pub is_error: bool,
}

pub enum ToolExecutionEvent {
    Update(String),
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": "read",
            "description": "Read a UTF-8 text file. Supports 1-indexed offset and line limit; output is truncated when large.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative or absolute file path." },
                    "offset": { "type": "number", "description": "1-indexed line to start reading from. Defaults to 1." },
                    "limit": { "type": "number", "description": "Maximum lines to read. Defaults to 400." }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "edit",
            "description": "Apply one or more exact targeted text replacements to a UTF-8 file. Each oldText must match exactly once in the original file.",
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
            "name": "write",
            "description": "Write UTF-8 text to a file. Creates parent directories and overwrites the full file.",
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
            "name": "bash",
            "description": "Run a shell command in the current workspace. Returns exit code, stdout, stderr, timeout state, and truncation state.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run." },
                    "timeout": { "type": "number", "description": "Timeout in seconds. Defaults to 60." },
                    "yield_time_ms": { "type": "number", "description": "Return early after this many milliseconds if the command is still running. Use for dev servers, watchers, and long tasks." }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "write_stdin",
            "description": "Send input to a running bash session or poll it. Use the session_id returned by bash.",
            "parameters": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "number", "description": "Running bash session id." },
                    "text": { "type": "string", "description": "Text to write to stdin. Omit or pass empty text to only poll." },
                    "yield_time_ms": { "type": "number", "description": "Milliseconds to wait for more output. Defaults to 1000." }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "apply_patch",
            "description": "Apply a unified diff patch to files in the current workspace. Use this for multi-file edits, file creation, deletion, and precise code changes.",
            "parameters": {
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "Unified diff patch content accepted by git apply." }
                },
                "required": ["patch"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "diff",
            "description": "Show workspace changes in git diff format. Optionally restrict to one path.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Optional file or directory path to diff." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "ls",
            "description": "List directory contents sorted alphabetically. Directories have a trailing slash.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list. Defaults to current workspace." },
                    "limit": { "type": "number", "description": "Maximum entries to return. Defaults to 500." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "ripgrep",
            "description": "Search file contents with ripgrep (rg). Respects .gitignore by default and returns matching lines with paths and line numbers.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern." },
                    "path": { "type": "string", "description": "File or directory to search. Defaults to current workspace." },
                    "glob": { "type": "string", "description": "Optional glob filter, e.g. *.rs or **/*.ts." },
                    "ignoreCase": { "type": "boolean", "description": "Case-insensitive search. Defaults to false." },
                    "literal": { "type": "boolean", "description": "Treat pattern as a literal string. Defaults to false." },
                    "contextLines": { "type": "number", "description": "Lines before and after each match. Defaults to 0." },
                    "limit": { "type": "number", "description": "Maximum output lines. Defaults to 100." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        }),
    ]
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
        "edit" => edit_file(&args, cwd),
        "write" => write_file(&args, cwd),
        "bash" | "execute" => bash(&args, cwd, &mut emit),
        "write_stdin" => write_stdin(&args),
        "apply_patch" => apply_patch(&args, cwd, &mut emit),
        "diff" => diff_workspace(&args, cwd),
        "ls" => list_dir(&args, cwd),
        "ripgrep" => ripgrep(&args, cwd),
        _ => json!({ "error": format!("unknown tool: {name}") }),
    };
    tool_result(name, result, cwd)
}

fn tool_result(name: &str, value: Value, cwd: &Path) -> ToolExecutionResult {
    let output = value.to_string();
    let model_output = model_projection(name, &output, cwd);
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

fn read_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };

    let path = resolve_path(cwd, path);
    let offset = optional_usize(args, "offset").unwrap_or(1).max(1);
    let limit = optional_usize(args, "limit")
        .unwrap_or(DEFAULT_READ_LIMIT)
        .max(1);

    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };

    let mut content = String::new();
    let mut lines_read = 0usize;
    let mut truncated = false;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut line_number = 0usize;

    loop {
        line.clear();
        let bytes = match reader.read_line(&mut line) {
            Ok(bytes) => bytes,
            Err(error) => {
                return json!({ "path": path.display().to_string(), "error": error.to_string() })
            }
        };
        if bytes == 0 {
            break;
        }

        line_number += 1;
        if line_number < offset {
            continue;
        }
        if lines_read >= limit || content.len() + line.len() > MAX_TOOL_OUTPUT_BYTES {
            truncated = true;
            break;
        }

        content.push_str(&line);
        lines_read += 1;
    }

    json!({
        "path": path.display().to_string(),
        "offset": offset,
        "lines_read": lines_read,
        "truncated": truncated,
        "content": content,
    })
}

fn edit_file(args: &Value, cwd: &Path) -> Value {
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

    match fs::write(&path, &output) {
        Ok(()) => {
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

fn write_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };
    let Some(content) = args.get("content").and_then(Value::as_str) else {
        return json!({ "error": "missing content" });
    };

    let path = resolve_path(cwd, path);
    let original = fs::read_to_string(&path).unwrap_or_default();
    if let Some(parent) = path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return json!({ "path": path.display().to_string(), "error": error.to_string() });
        }
    }

    match fs::write(&path, content) {
        Ok(()) => {
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

fn bash(
    args: &Value,
    cwd: &Path,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Value {
    let Some(command) = args.get("command").and_then(Value::as_str) else {
        return json!({ "error": "missing command" });
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
            cwd,
            timeout,
            yield_time,
            emit,
        )
    } else {
        run_command_events(program, &shell_args, None, cwd, timeout, emit)
            .map(|result| command_result_json(command, Ok(result)))
    };
    match result {
        Ok(value) => value,
        Err(error) => json!({ "command": command, "error": error }),
    }
}

fn write_stdin(args: &Value) -> Value {
    let Some(session_id) = args.get("session_id").and_then(Value::as_u64) else {
        return json!({ "error": "missing session_id" });
    };
    let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
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
            if !text.is_empty() {
                if let Some(stdin) = session.stdin.as_mut() {
                    stdin
                        .write_all(text.as_bytes())
                        .and_then(|_| stdin.flush())
                        .map_err(|error| error.to_string())?;
                }
            }
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

fn model_projection(name: &str, output: &str, cwd: &Path) -> String {
    if output.len() <= MAX_MODEL_OUTPUT_BYTES {
        return output.to_string();
    }
    let saved_path = save_truncated_result(name, output, cwd).ok();
    let tail_budget = 2048usize.min(MAX_MODEL_OUTPUT_BYTES / 8);
    let head_budget = MAX_MODEL_OUTPUT_BYTES.saturating_sub(tail_budget);
    let head = safe_prefix(output, head_budget);
    let tail = safe_suffix(output, tail_budget);
    let dropped = output.len().saturating_sub(head.len() + tail.len());
    let note = saved_path
        .map(|path| format!(" Full result saved at: {path}."))
        .unwrap_or_default();
    format!("{head}\n\n[...truncated {dropped} bytes for model context.{note}]\n\n{tail}")
}

fn save_truncated_result(name: &str, output: &str, cwd: &Path) -> io::Result<String> {
    let dir = cwd.join(".jucode").join("truncated-results");
    fs::create_dir_all(&dir)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let filename = format!("{nanos}-{}.txt", sanitize_filename(name));
    let path = dir.join(filename);
    fs::write(&path, output)?;
    Ok(path
        .strip_prefix(cwd)
        .unwrap_or(&path)
        .to_string_lossy()
        .replace('\\', "/"))
}

fn sanitize_filename(name: &str) -> String {
    let mut value = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .take(48)
        .collect::<String>();
    if value.is_empty() {
        value.push_str("tool");
    }
    value
}

fn safe_prefix(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn safe_suffix(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut start = value.len().saturating_sub(max_bytes);
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_string()
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

fn diff_workspace(args: &Value, cwd: &Path) -> Value {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(|path| resolve_path(cwd, path));
    match git_diff(cwd, path.as_deref()) {
        Some(diff) => json!({
            "path": path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| cwd.display().to_string()),
            "diff": diff,
        }),
        None => json!({ "error": "failed to get git diff" }),
    }
}

fn list_dir(args: &Value, cwd: &Path) -> Value {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(|path| resolve_path(cwd, path))
        .unwrap_or_else(|| cwd.to_path_buf());
    let limit = optional_usize(args, "limit")
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .max(1);

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
    let truncated = names.len() > limit;
    names.truncate(limit);

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
    let limit = optional_usize(args, "limit")
        .unwrap_or(DEFAULT_RIPGREP_LIMIT)
        .max(1);
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
    if let Some(stdout) = value.get("stdout").and_then(Value::as_str) {
        let lines = stdout.lines().take(limit).collect::<Vec<_>>();
        let truncated = stdout.lines().count() > limit;
        value["stdout"] = json!(lines.join("\n"));
        value["truncated"] = json!(value["truncated"].as_bool().unwrap_or(false) || truncated);
    }
    value["path"] = json!(search_path.display().to_string());
    value
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
    match result {
        Ok(result) => json!({
            "command": command,
            "exit_code": result.exit_code,
            "stdout": result.stdout,
            "stderr": result.stderr,
            "timed_out": result.timed_out,
            "truncated": result.truncated,
        }),
        Err(error) => json!({ "command": command, "error": error }),
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
    let old_label = format!("a/{label}");
    let new_label = format!("b/{label}");
    let old_arg = old_path.display().to_string();
    let new_arg = new_path.display().to_string();
    let result = run_command(
        "git",
        &[
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--label",
            &old_label,
            "--label",
            &new_label,
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
            return Some(result.stdout);
        }
    }
    Some(simple_unified_diff(&label, original, updated))
}

fn git_diff(cwd: &Path, path: Option<&Path>) -> Option<String> {
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

    result
        .ok()
        .map(|result| result.stdout)
        .filter(|diff| !diff.trim().is_empty())
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

fn simple_unified_diff(label: &str, original: &str, updated: &str) -> String {
    let old_lines = original.lines().collect::<Vec<_>>();
    let new_lines = updated.lines().collect::<Vec<_>>();
    let mut diff = format!(
        "diff --git a/{label} b/{label}\n--- a/{label}\n+++ b/{label}\n@@ -1,{} +1,{} @@\n",
        old_lines.len(),
        new_lines.len()
    );
    for line in old_lines {
        diff.push('-');
        diff.push_str(line);
        diff.push('\n');
    }
    for line in new_lines {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
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
    let truncated = bytes.len() > MAX_TOOL_OUTPUT_BYTES;
    let start = bytes.len().saturating_sub(MAX_TOOL_OUTPUT_BYTES);
    (
        String::from_utf8_lossy(&bytes[start..]).to_string(),
        truncated,
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
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    env::temp_dir().join(format!(
        "jucode-tool-{label}-{}-{nanos}.log",
        std::process::id()
    ))
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
    fn edit_requires_unique_old_text() {
        let dir = test_dir("edit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "same\nsame\n").unwrap();

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
        assert!(value["diff"].as_str().unwrap().contains("-beta"));
        assert!(value["diff"].as_str().unwrap().contains("+gamma"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_returns_git_style_diff() {
        let dir = test_dir("write-diff");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "before\n").unwrap();

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
    fn definitions_expose_expected_tools() {
        let names = definitions()
            .into_iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "read",
                "edit",
                "write",
                "bash",
                "write_stdin",
                "apply_patch",
                "diff",
                "ls",
                "ripgrep"
            ]
        );
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

        std::thread::sleep(Duration::from_millis(500));
        let result = run_tool(
            "write_stdin",
            &json!({ "session_id": session_id, "yield_time_ms": 100 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["exit_code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("done"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn long_tool_result_gets_model_projection_and_saved_copy() {
        let dir = test_dir("tool-projection");
        fs::create_dir_all(&dir).unwrap();
        let result = tool_result(
            "test_tool",
            json!({ "content": "x".repeat(MAX_MODEL_OUTPUT_BYTES + 4096) }),
            &dir,
        );

        assert!(result.output.len() > result.model_output.len());
        assert!(result
            .model_output
            .contains("Full result saved at: .jucode/truncated-results/"));
        assert!(dir.join(".jucode").join("truncated-results").exists());
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
