use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};

/// User-configured shell hooks that run at lifecycle points. Loaded from
/// `~/.jucode/hooks.json` (global) and `<cwd>/.jucode/hooks.json` (project,
/// only when the project is trusted). Cheap to clone — shared via `Arc`.
#[derive(Debug, Clone, Default)]
pub struct Hooks {
    inner: Arc<HookSet>,
}

#[derive(Debug, Default)]
struct HookSet {
    session_start: Vec<HookEntry>,
    user_prompt_submit: Vec<HookEntry>,
    pre_tool_use: Vec<HookEntry>,
    post_tool_use: Vec<HookEntry>,
    stop: Vec<HookEntry>,
}

#[derive(Debug, Clone)]
struct HookEntry {
    command: String,
    /// Tool names this hook applies to. `None` means every tool.
    tools: Option<Vec<String>>,
}

impl HookEntry {
    fn matches(&self, tool: &str) -> bool {
        match &self.tools {
            Some(tools) => tools.iter().any(|name| name == tool),
            None => true,
        }
    }
}

struct Outcome {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Hooks {
    pub fn load(profile_dir: &Path, cwd: &Path, project_trusted: bool) -> Self {
        let mut set = HookSet::default();
        merge_file(&mut set, &profile_dir.join("hooks.json"));
        if project_trusted {
            merge_file(&mut set, &cwd.join(".jucode").join("hooks.json"));
        }
        Self {
            inner: Arc::new(set),
        }
    }

    /// Runs `session_start` hooks and returns any non-empty stdout to surface.
    pub fn session_start(&self, cwd: &Path) -> Vec<String> {
        self.notify(&self.inner.session_start, "session_start", json!({}), cwd)
    }

    /// Runs `stop` hooks (turn finished) and returns any non-empty stdout.
    pub fn stop(&self, cwd: &Path) -> Vec<String> {
        self.notify(&self.inner.stop, "stop", json!({}), cwd)
    }

    /// Runs `user_prompt_submit` hooks. A non-zero exit blocks the prompt and the
    /// reason is returned as `Err`.
    pub fn user_prompt_submit(&self, prompt: &str, cwd: &Path) -> Result<(), String> {
        let payload = json!({
            "event": "user_prompt_submit",
            "prompt": prompt,
            "cwd": cwd.display().to_string(),
        });
        for entry in &self.inner.user_prompt_submit {
            let outcome = run_command(&entry.command, &payload, cwd);
            if outcome.code != 0 {
                return Err(block_reason(&outcome, "user_prompt_submit"));
            }
        }
        Ok(())
    }

    /// Runs `pre_tool_use` hooks for `tool`. Returns `Some(reason)` to block the
    /// tool from executing.
    pub fn pre_tool(&self, tool: &str, arguments: &str, cwd: &Path) -> Option<String> {
        if self.inner.pre_tool_use.is_empty() {
            return None;
        }
        let payload = json!({
            "event": "pre_tool_use",
            "tool": tool,
            "arguments": parse_arguments(arguments),
            "cwd": cwd.display().to_string(),
        });
        for entry in &self.inner.pre_tool_use {
            if !entry.matches(tool) {
                continue;
            }
            let outcome = run_command(&entry.command, &payload, cwd);
            if outcome.code != 0 {
                return Some(block_reason(&outcome, "pre_tool_use"));
            }
        }
        None
    }

    /// Runs `post_tool_use` hooks for `tool`. Best-effort; failures are ignored.
    pub fn post_tool(&self, tool: &str, output: &str, cwd: &Path) {
        if self.inner.post_tool_use.is_empty() {
            return;
        }
        let payload = json!({
            "event": "post_tool_use",
            "tool": tool,
            "output": output,
            "cwd": cwd.display().to_string(),
        });
        for entry in &self.inner.post_tool_use {
            if entry.matches(tool) {
                let _ = run_command(&entry.command, &payload, cwd);
            }
        }
    }

    fn notify(&self, entries: &[HookEntry], event: &str, extra: Value, cwd: &Path) -> Vec<String> {
        let mut messages = Vec::new();
        for entry in entries {
            let mut payload = json!({ "event": event, "cwd": cwd.display().to_string() });
            if let (Some(map), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
                for (key, value) in extra {
                    map.insert(key.clone(), value.clone());
                }
            }
            let outcome = run_command(&entry.command, &payload, cwd);
            let text = outcome.stdout.trim();
            if !text.is_empty() {
                messages.push(format!("hook ({event}): {text}"));
            }
        }
        messages
    }
}

fn block_reason(outcome: &Outcome, event: &str) -> String {
    let stderr = outcome.stderr.trim();
    let stdout = outcome.stdout.trim();
    if !stderr.is_empty() {
        stderr.to_string()
    } else if !stdout.is_empty() {
        stdout.to_string()
    } else {
        format!("{event} hook exited with code {}", outcome.code)
    }
}

fn parse_arguments(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .unwrap_or_else(|_| Value::String(arguments.to_string()))
}

fn run_command(command: &str, payload: &Value, cwd: &Path) -> Outcome {
    #[cfg(windows)]
    let (shell, flag) = ("cmd", "/C");
    #[cfg(not(windows))]
    let (shell, flag) = ("sh", "-c");

    let event = payload
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let spawn = Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(cwd)
        .env("JUCODE_HOOK_EVENT", event)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(error) => {
            return Outcome {
                code: -1,
                stdout: String::new(),
                stderr: error.to_string(),
            }
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.to_string().as_bytes());
    }
    match child.wait_with_output() {
        Ok(output) => Outcome {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        },
        Err(error) => Outcome {
            code: -1,
            stdout: String::new(),
            stderr: error.to_string(),
        },
    }
}

fn merge_file(set: &mut HookSet, path: &Path) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return;
    };
    push_entries(&mut set.session_start, &value, "session_start");
    push_entries(&mut set.user_prompt_submit, &value, "user_prompt_submit");
    push_entries(&mut set.pre_tool_use, &value, "pre_tool_use");
    push_entries(&mut set.post_tool_use, &value, "post_tool_use");
    push_entries(&mut set.stop, &value, "stop");
}

fn push_entries(target: &mut Vec<HookEntry>, value: &Value, key: &str) {
    let Some(entries) = value.get(key).and_then(Value::as_array) else {
        return;
    };
    for entry in entries {
        let Some(command) = entry
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
        else {
            continue;
        };
        let tools = entry.get("tools").and_then(Value::as_array).map(|tools| {
            tools
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
        target.push(HookEntry {
            command: command.to_string(),
            tools,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks_from(value: Value) -> Hooks {
        let mut set = HookSet::default();
        push_entries(&mut set.pre_tool_use, &value, "pre_tool_use");
        Hooks {
            inner: Arc::new(set),
        }
    }

    #[test]
    fn pre_tool_blocks_on_nonzero_exit() {
        let hooks = hooks_from(json!({
            "pre_tool_use": [{ "command": "echo denied >&2; exit 1", "tools": ["bash"] }]
        }));
        let reason = hooks.pre_tool("bash", "{}", Path::new("."));
        assert_eq!(reason.as_deref(), Some("denied"));
        // Tool filter excludes "read".
        assert!(hooks.pre_tool("read", "{}", Path::new(".")).is_none());
    }

    #[test]
    fn pre_tool_allows_on_zero_exit() {
        let hooks = hooks_from(json!({
            "pre_tool_use": [{ "command": "exit 0" }]
        }));
        assert!(hooks.pre_tool("bash", "{}", Path::new(".")).is_none());
    }
}
