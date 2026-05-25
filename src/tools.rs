use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": "read",
            "description": "Read a UTF-8 text file from the current workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative or absolute file path." }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "write",
            "description": "Write UTF-8 text to a file. Creates parent directories when needed.",
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
            "name": "execute",
            "description": "Run a shell command in the current workspace and return stdout, stderr, and exit code.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command to run." }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
    ]
}

pub fn run_tool(name: &str, arguments: &str, cwd: &Path) -> String {
    let parsed = serde_json::from_str::<Value>(arguments);
    let args = match parsed {
        Ok(args) => args,
        Err(error) => {
            return json!({ "error": format!("invalid JSON arguments: {error}") }).to_string()
        }
    };

    let result = match name {
        "read" => read_file(&args, cwd),
        "write" => write_file(&args, cwd),
        "execute" => execute(&args, cwd),
        _ => json!({ "error": format!("unknown tool: {name}") }),
    };
    result.to_string()
}

fn read_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };

    let path = resolve_path(cwd, path);
    match fs::read_to_string(&path) {
        Ok(content) => json!({ "path": path.display().to_string(), "content": content }),
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
    if let Some(parent) = path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return json!({ "path": path.display().to_string(), "error": error.to_string() });
        }
    }

    match fs::write(&path, content) {
        Ok(()) => json!({ "path": path.display().to_string(), "written_bytes": content.len() }),
        Err(error) => json!({ "path": path.display().to_string(), "error": error.to_string() }),
    }
}

fn execute(args: &Value, cwd: &Path) -> Value {
    let Some(command) = args.get("command").and_then(Value::as_str) else {
        return json!({ "error": "missing command" });
    };

    let output = if cfg!(windows) {
        Command::new("powershell")
            .args(["-NoProfile", "-Command", command])
            .current_dir(cwd)
            .output()
    } else {
        Command::new("sh")
            .args(["-lc", command])
            .current_dir(cwd)
            .output()
    };

    match output {
        Ok(output) => json!({
            "command": command,
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }),
        Err(error) => json!({ "command": command, "error": error.to_string() }),
    }
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}
