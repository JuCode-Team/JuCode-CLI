use crate::config::ExtensionConfig;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

const EXTENSION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct ExtensionTool {
    pub extension: ExtensionConfig,
    pub definition: Value,
}

#[derive(Debug, Clone)]
pub struct ExtensionRegistry {
    tools: Vec<ExtensionTool>,
    lazy: Vec<ExtensionConfig>,
    errors: Vec<(String, String)>,
    profile_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSummary {
    pub extension: String,
    pub tool: String,
    pub description: String,
}

impl ExtensionRegistry {
    pub fn load(extensions: &[ExtensionConfig], cwd: &Path, profile_dir: &Path) -> Self {
        let mut tools = Vec::new();
        let mut lazy = Vec::new();
        let mut errors = Vec::new();
        for extension in extensions {
            if extension.lazy {
                lazy.push(extension.clone());
                continue;
            }
            match initialize_extension(extension, cwd, profile_dir) {
                Ok(mut definitions) => {
                    tools.extend(definitions.drain(..).map(|definition| ExtensionTool {
                        extension: extension.clone(),
                        definition,
                    }))
                }
                Err(error) => errors.push((extension.name.clone(), error)),
            }
        }
        Self {
            tools,
            lazy,
            errors,
            profile_dir: profile_dir.to_path_buf(),
        }
    }

    pub fn definitions(&self) -> Vec<Value> {
        let mut definitions = self
            .tools
            .iter()
            .map(|tool| tool.definition.clone())
            .collect::<Vec<_>>();
        if !self.lazy.is_empty() {
            definitions.extend(lazy_extension_definitions());
        }
        definitions
    }

    pub fn summaries(&self) -> Vec<ExtensionSummary> {
        self.tools
            .iter()
            .map(|tool| ExtensionSummary {
                extension: tool.extension.name.clone(),
                tool: tool
                    .definition
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("(unnamed)")
                    .to_string(),
                description: tool
                    .definition
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .chain(self.lazy.iter().map(|extension| ExtensionSummary {
                extension: extension.name.clone(),
                tool: "(lazy)".to_string(),
                description: "Use extension_list_tools before calling lazy tools.".to_string(),
            }))
            .collect()
    }

    pub fn errors(&self) -> &[(String, String)] {
        &self.errors
    }

    pub fn run_tool(&self, name: &str, arguments: &str, cwd: &Path) -> Option<(String, bool)> {
        if name == "extension_list_tools" {
            return Some(self.list_lazy_tools(cwd));
        }
        if name == "extension_call" {
            return Some(self.call_lazy_tool(arguments, cwd));
        }

        let tool = self.tools.iter().find(|tool| {
            tool.definition
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|value| value == name)
        })?;
        let args = serde_json::from_str::<Value>(arguments)
            .unwrap_or_else(|error| json!({ "error": format!("invalid JSON arguments: {error}") }));
        Some(
            match call_extension_tool(&tool.extension, name, args, cwd) {
                Ok(output) => (output.to_string(), output.get("error").is_some()),
                Err(error) => (json!({ "error": error }).to_string(), true),
            },
        )
    }

    fn list_lazy_tools(&self, cwd: &Path) -> (String, bool) {
        let mut tools = Vec::new();
        let mut errors = Vec::new();
        for extension in &self.lazy {
            match initialize_extension(extension, cwd, &self.profile_dir) {
                Ok(definitions) => {
                    tools.extend(definitions.into_iter().map(|definition| {
                        json!({
                            "extension": extension.name,
                            "name": definition.get("name").cloned().unwrap_or_default(),
                            "description": definition.get("description").cloned().unwrap_or_default(),
                            "parameters": definition.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                        })
                    }));
                }
                Err(error) => errors.push(json!({ "extension": extension.name, "error": error })),
            }
        }
        (
            json!({ "tools": tools, "errors": errors }).to_string(),
            !errors.is_empty(),
        )
    }

    fn call_lazy_tool(&self, arguments: &str, cwd: &Path) -> (String, bool) {
        let args = serde_json::from_str::<Value>(arguments)
            .unwrap_or_else(|error| json!({ "error": format!("invalid JSON arguments: {error}") }));
        let Some(extension_name) = args.get("extension").and_then(Value::as_str) else {
            return (
                json!({ "error": "extension_call requires extension" }).to_string(),
                true,
            );
        };
        let Some(tool_name) = args.get("name").and_then(Value::as_str) else {
            return (
                json!({ "error": "extension_call requires name" }).to_string(),
                true,
            );
        };
        let Some(extension) = self.lazy.iter().find(|item| item.name == extension_name) else {
            return (
                json!({ "error": format!("lazy extension not found: {extension_name}") })
                    .to_string(),
                true,
            );
        };
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
        match call_extension_tool(extension, tool_name, arguments, cwd) {
            Ok(output) => (output.to_string(), output.get("error").is_some()),
            Err(error) => (json!({ "error": error }).to_string(), true),
        }
    }
}

fn lazy_extension_definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": "extension_list_tools",
            "description": "List tools exposed by lazy JuCode extensions. Use this only when built-in tools are insufficient.",
            "parameters": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "extension_call",
            "description": "Call one lazy JuCode extension tool after inspecting extension_list_tools.",
            "parameters": {
                "type": "object",
                "properties": {
                    "extension": { "type": "string", "description": "Lazy extension name." },
                    "name": { "type": "string", "description": "Tool name from extension_list_tools." },
                    "arguments": { "type": "object", "description": "Tool arguments." }
                },
                "required": ["extension", "name"],
                "additionalProperties": false
            }
        }),
    ]
}

fn initialize_extension(
    extension: &ExtensionConfig,
    cwd: &Path,
    profile_dir: &Path,
) -> Result<Vec<Value>, String> {
    let response = send_extension_request(
        extension,
        json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "cwd": cwd.display().to_string(),
                "profileDir": profile_dir.display().to_string()
            }
        }),
        cwd,
    )?;
    Ok(response
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn call_extension_tool(
    extension: &ExtensionConfig,
    name: &str,
    arguments: Value,
    cwd: &Path,
) -> Result<Value, String> {
    send_extension_request(
        extension,
        json!({
            "id": 1,
            "method": "tool.call",
            "params": {
                "name": name,
                "arguments": arguments
            }
        }),
        cwd,
    )
}

fn send_extension_request(
    extension: &ExtensionConfig,
    request: Value,
    cwd: &Path,
) -> Result<Value, String> {
    let mut child = shell_command(&extension.command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start extension {}: {error}", extension.name))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "extension stdin is unavailable".to_string())?;
        writeln!(stdin, "{request}").map_err(|error| error.to_string())?;
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "extension stdout is unavailable".to_string())?;
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;

    let start = std::time::Instant::now();
    while start.elapsed() < EXTENSION_TIMEOUT {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            if !status.success() && line.trim().is_empty() {
                let stderr = child
                    .stderr
                    .take()
                    .and_then(|stderr| {
                        let mut reader = BufReader::new(stderr);
                        let mut value = String::new();
                        reader.read_line(&mut value).ok()?;
                        Some(value)
                    })
                    .unwrap_or_default();
                return Err(format!("extension exited with {status}: {stderr}"));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    if line.trim().is_empty() {
        return Err("extension returned no response".to_string());
    }
    let response = serde_json::from_str::<Value>(&line).map_err(|error| error.to_string())?;
    if let Some(error) = response.get("error") {
        return Err(error.to_string());
    }
    Ok(response
        .get("result")
        .cloned()
        .unwrap_or_else(|| json!({ "error": "extension response missing result" })))
}

fn shell_command(command: &str) -> Command {
    if cfg!(windows) {
        let mut process = Command::new("cmd");
        process.arg("/C").arg(command);
        process
    } else {
        let mut process = Command::new("sh");
        process.arg("-c").arg(command);
        process
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn registry_loads_no_extensions() {
        let registry = ExtensionRegistry::load(&[], Path::new("."), Path::new("."));
        assert!(registry.definitions().is_empty());
    }

    #[test]
    fn registry_loads_and_calls_stdio_extension() {
        let root = std::env::temp_dir().join(format!(
            "jucode-extension-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let script = root.join(if cfg!(windows) {
            "extension.cmd"
        } else {
            "extension.sh"
        });
        let content = if cfg!(windows) {
            r#"@echo off
set /p line=
echo %line% | findstr /C:"initialize" >nul
if %errorlevel%==0 (
  echo {"id":1,"result":{"tools":[{"type":"function","name":"hello","description":"Say hello","parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}}]}}
) else (
  echo {"id":1,"result":{"content":"hello"}}
)
"#
        } else {
            r#"#!/bin/sh
read line
case "$line" in
  *initialize*) echo '{"id":1,"result":{"tools":[{"type":"function","name":"hello","description":"Say hello","parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}}]}}' ;;
  *) echo '{"id":1,"result":{"content":"hello"}}' ;;
esac
"#
        };
        fs::write(&script, content).unwrap();

        let extension = ExtensionConfig {
            name: "test".to_string(),
            command: extension_command(&script),
            lazy: false,
        };
        let registry = ExtensionRegistry::load(&[extension], &root, &root);

        assert_eq!(registry.definitions().len(), 1);
        assert_eq!(
            registry.definitions()[0]
                .get("name")
                .and_then(Value::as_str),
            Some("hello")
        );

        let (output, is_error) = registry
            .run_tool("hello", r#"{"name":"JuCode"}"#, &root)
            .unwrap();

        assert!(!is_error);
        assert!(output.contains("hello"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lazy_extension_exposes_dispatch_tools() {
        let root = std::env::temp_dir().join(format!(
            "jucode-lazy-extension-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let script = root.join(if cfg!(windows) {
            "extension.cmd"
        } else {
            "extension.sh"
        });
        let content = if cfg!(windows) {
            r#"@echo off
set /p line=
echo %line% | findstr /C:"initialize" >nul
if %errorlevel%==0 (
  echo {"id":1,"result":{"tools":[{"type":"function","name":"hello","description":"Say hello","parameters":{"type":"object","properties":{},"additionalProperties":false}}]}}
) else (
  echo {"id":1,"result":{"content":"lazy hello"}}
)
"#
        } else {
            r#"#!/bin/sh
read line
case "$line" in
  *initialize*) echo '{"id":1,"result":{"tools":[{"type":"function","name":"hello","description":"Say hello","parameters":{"type":"object","properties":{},"additionalProperties":false}}]}}' ;;
  *) echo '{"id":1,"result":{"content":"lazy hello"}}' ;;
esac
"#
        };
        fs::write(&script, content).unwrap();

        let extension = ExtensionConfig {
            name: "lazy-test".to_string(),
            command: extension_command(&script),
            lazy: true,
        };
        let registry = ExtensionRegistry::load(&[extension], &root, &root);

        assert_eq!(
            registry
                .definitions()
                .iter()
                .filter_map(|definition| definition.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            vec!["extension_list_tools", "extension_call"]
        );

        let (listed, listed_error) = registry
            .run_tool("extension_list_tools", "{}", &root)
            .unwrap();
        let (output, is_error) = registry
            .run_tool(
                "extension_call",
                r#"{"extension":"lazy-test","name":"hello","arguments":{}}"#,
                &root,
            )
            .unwrap();

        assert!(!listed_error);
        assert!(listed.contains("hello"));
        assert!(!is_error);
        assert!(output.contains("lazy hello"));

        let _ = fs::remove_dir_all(root);
    }

    fn extension_command(script: &Path) -> String {
        if cfg!(windows) {
            script.display().to_string()
        } else {
            format!("sh {}", script.display())
        }
    }
}
