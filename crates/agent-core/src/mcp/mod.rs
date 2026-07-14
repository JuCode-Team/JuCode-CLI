//! MCP server manager: owns one client per configured server, connects in the
//! background, exposes server tools as `mcp__<server>__<tool>` entries in the
//! model tool table, and routes calls back to the owning server.

mod client;
mod transport;

use crate::{
    config::{McpServerConfig, McpTransportKind},
    event::{McpServerView, McpToolView},
    mcp::{
        client::McpClient,
        transport::{HttpTransport, McpTransport, StdioTransport},
    },
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

pub const MCP_TOOL_PREFIX: &str = "mcp__";

#[derive(Clone, Default)]
pub struct McpManager {
    inner: Arc<Mutex<ManagerState>>,
}

#[derive(Default)]
struct ManagerState {
    servers: BTreeMap<String, ServerEntry>,
    /// Human-readable connect/failure notices drained into Info events.
    messages: Vec<String>,
    /// Set on any state change so the core can re-emit the servers view.
    dirty: bool,
}

struct ServerEntry {
    config: McpServerConfig,
    state: ServerState,
    /// Bumped on every (re)connect/disconnect; a finishing connect thread
    /// whose generation no longer matches is discarded.
    generation: u64,
}

enum ServerState {
    Disabled,
    Connecting,
    Connected(Arc<McpClient>),
    Failed(String),
}

impl ServerState {
    fn label(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Connecting => "connecting",
            Self::Connected(_) => "connected",
            Self::Failed(_) => "failed",
        }
    }
}

impl McpManager {
    /// Register all configured servers and start connecting the enabled ones
    /// on background threads. Non-blocking.
    pub fn start(&self, configs: &[McpServerConfig], cwd: &Path) {
        for config in configs {
            self.upsert(config.clone(), cwd);
        }
    }

    /// Add or replace one server and (re)connect it if enabled.
    pub fn upsert(&self, config: McpServerConfig, cwd: &Path) {
        let name = config.name.clone();
        let enabled = config.enabled;
        {
            let mut state = self.lock();
            let entry = state.servers.entry(name.clone()).or_insert(ServerEntry {
                config: config.clone(),
                state: ServerState::Disabled,
                generation: 0,
            });
            entry.config = config;
            entry.generation += 1;
            entry.state = if enabled {
                ServerState::Connecting
            } else {
                ServerState::Disabled
            };
            state.dirty = true;
        }
        if enabled {
            self.spawn_connect(&name, cwd);
        }
    }

    pub fn remove(&self, name: &str) -> bool {
        let mut state = self.lock();
        let removed = state.servers.remove(name).is_some();
        state.dirty |= removed;
        removed
    }

    pub fn contains(&self, name: &str) -> bool {
        self.lock().servers.contains_key(name)
    }

    /// Reconnect one server (used by `/mcp reload` and config mutations).
    pub fn reload(&self, name: &str, cwd: &Path) -> Result<(), String> {
        let config = {
            let state = self.lock();
            let entry = state
                .servers
                .get(name)
                .ok_or_else(|| format!("unknown MCP server: {name}"))?;
            if !entry.config.enabled {
                return Err(format!("MCP server {name} is disabled"));
            }
            entry.config.clone()
        };
        self.upsert(config, cwd);
        Ok(())
    }

    pub fn set_enabled(&self, name: &str, enabled: bool, cwd: &Path) -> Result<(), String> {
        let config = {
            let state = self.lock();
            let entry = state
                .servers
                .get(name)
                .ok_or_else(|| format!("unknown MCP server: {name}"))?;
            let mut config = entry.config.clone();
            config.enabled = enabled;
            config
        };
        self.upsert(config, cwd);
        Ok(())
    }

    fn spawn_connect(&self, name: &str, cwd: &Path) {
        let Some((config, generation)) = self
            .lock()
            .servers
            .get(name)
            .map(|entry| (entry.config.clone(), entry.generation))
        else {
            return;
        };
        let manager = self.clone();
        let cwd = cwd.to_path_buf();
        thread::spawn(move || {
            let result = connect_server(&config, &cwd);
            manager.finish_connect(&config.name, generation, result);
        });
    }

    fn finish_connect(
        &self,
        name: &str,
        generation: u64,
        result: Result<(Arc<McpClient>, usize), String>,
    ) {
        let mut state = self.lock();
        let Some(entry) = state.servers.get_mut(name) else {
            return;
        };
        if entry.generation != generation {
            return; // superseded by a newer reload/toggle/remove
        }
        let message = match result {
            Ok((client, tool_count)) => {
                crate::log_info!(
                    "mcp",
                    "server connected",
                    server = name,
                    tools = tool_count,
                    protocol_version = client.protocol_version(),
                    server_info = client.server_info()
                );
                entry.state = ServerState::Connected(client);
                format!("MCP server {name} connected ({tool_count} tools)")
            }
            Err(error) => {
                crate::log_error!(
                    "mcp",
                    "server connection failed",
                    server = name,
                    error = error.clone()
                );
                entry.state = ServerState::Failed(error.clone());
                format!("MCP server {name} failed to connect: {error}")
            }
        };
        state.messages.push(message);
        state.dirty = true;
    }

    /// Model tool definitions for every connected server, refreshed first if a
    /// server signaled a tool-list change. Duplicate full names lose.
    pub fn definitions(&self) -> Vec<Value> {
        let mut definitions = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (server, client) in self.connected_clients() {
            client.maybe_refresh_tools();
            for tool in client.tools_snapshot() {
                let name = mcp_tool_name(&server, &tool.name);
                if !seen.insert(name.clone()) {
                    crate::log_warn!(
                        "mcp",
                        "duplicate tool name skipped",
                        server = server.clone(),
                        tool = tool.name
                    );
                    continue;
                }
                definitions.push(json!({
                    "type": "function",
                    "name": name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }));
            }
        }
        definitions
    }

    /// Route an `mcp__<server>__<tool>` call. `None` for non-MCP names.
    pub fn run_tool(&self, name: &str, arguments: &str) -> Option<(String, bool)> {
        if !name.starts_with(MCP_TOOL_PREFIX) {
            return None;
        }
        let Some((server, tool)) = self.resolve_tool_name(name) else {
            return Some(error_output(format!("unknown MCP tool: {name}")));
        };
        let client = match self.client_for(&server) {
            Ok(client) => client,
            Err(error) => return Some(error_output(error)),
        };
        let args = match parse_arguments(arguments) {
            Ok(args) => args,
            Err(error) => return Some(error_output(error)),
        };
        client.maybe_refresh_tools();
        Some(client.call_tool(&tool, args))
    }

    /// The cached `annotations.readOnlyHint` for a full MCP tool name.
    pub fn tool_read_only_hint(&self, name: &str) -> Option<bool> {
        let (server, tool) = self.resolve_tool_name(name)?;
        self.client_for(&server).ok()?.read_only_hint(&tool)
    }

    /// Split a full name back into (server, tool) against known server names.
    /// Prefers the longest matching server so names containing `__` resolve.
    fn resolve_tool_name(&self, name: &str) -> Option<(String, String)> {
        let rest = name.strip_prefix(MCP_TOOL_PREFIX)?;
        let state = self.lock();
        state
            .servers
            .keys()
            .filter_map(|server| {
                rest.strip_prefix(server.as_str())
                    .and_then(|tail| tail.strip_prefix("__"))
                    .filter(|tool| !tool.is_empty())
                    .map(|tool| (server.clone(), tool.to_string()))
            })
            .max_by_key(|(server, _)| server.len())
    }

    fn client_for(&self, server: &str) -> Result<Arc<McpClient>, String> {
        let state = self.lock();
        let entry = state
            .servers
            .get(server)
            .ok_or_else(|| format!("unknown MCP server: {server}"))?;
        match &entry.state {
            ServerState::Connected(client) => Ok(Arc::clone(client)),
            other => Err(format!("MCP server {server} is {}", other.label())),
        }
    }

    fn connected_clients(&self) -> Vec<(String, Arc<McpClient>)> {
        self.lock()
            .servers
            .iter()
            .filter_map(|(name, entry)| match &entry.state {
                ServerState::Connected(client) => Some((name.clone(), Arc::clone(client))),
                _ => None,
            })
            .collect()
    }

    pub fn views(&self) -> Vec<McpServerView> {
        self.lock()
            .servers
            .values()
            .map(|entry| McpServerView {
                name: entry.config.name.clone(),
                transport: entry.config.transport.as_str().to_string(),
                state: entry.state.label().to_string(),
                error: match &entry.state {
                    ServerState::Failed(error) => Some(error.clone()),
                    _ => None,
                },
                tools: match &entry.state {
                    ServerState::Connected(client) => client
                        .tools_snapshot()
                        .into_iter()
                        .map(|tool| McpToolView {
                            name: tool.name,
                            description: tool.description,
                        })
                        .collect(),
                    _ => Vec::new(),
                },
            })
            .collect()
    }

    pub fn drain_messages(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().messages)
    }

    pub fn take_dirty(&self) -> bool {
        std::mem::take(&mut self.lock().dirty)
    }

    /// One `/doctor` line summarizing MCP state.
    pub fn doctor_line(&self) -> String {
        let state = self.lock();
        if state.servers.is_empty() {
            return "mcp: none".to_string();
        }
        let connected = state
            .servers
            .values()
            .filter(|entry| matches!(entry.state, ServerState::Connected(_)))
            .count();
        format!(
            "mcp: {} server(s), {connected} connected",
            state.servers.len()
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ManagerState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn insert_connected_for_tests(
        &self,
        config: McpServerConfig,
        client: Arc<McpClient>,
    ) {
        let mut state = self.lock();
        state.servers.insert(
            config.name.clone(),
            ServerEntry {
                config,
                state: ServerState::Connected(client),
                generation: 0,
            },
        );
        state.dirty = true;
    }
}

pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("{MCP_TOOL_PREFIX}{server}__{tool}")
}

fn error_output(error: String) -> (String, bool) {
    (json!({ "error": error }).to_string(), true)
}

fn parse_arguments(arguments: &str) -> Result<Value, String> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Null) => Ok(json!({})),
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err("MCP tool arguments must be a JSON object".to_string()),
        Err(error) => Err(format!("invalid JSON arguments: {error}")),
    }
}

fn connect_server(config: &McpServerConfig, cwd: &Path) -> Result<(Arc<McpClient>, usize), String> {
    let timeout = Duration::from_secs(config.timeout_seconds);
    let transport: Box<dyn McpTransport> = match config.transport {
        McpTransportKind::Stdio => Box::new(StdioTransport::spawn(
            &config.name,
            &config.command,
            &config.args,
            &config.env,
            cwd,
        )?),
        McpTransportKind::Http => Box::new(HttpTransport::new(
            &config.name,
            &config.url,
            &config.headers,
        )),
    };
    let client = McpClient::new(&config.name, transport, timeout);
    client.initialize()?;
    let tool_count = client.refresh_tools()?;
    Ok((Arc::new(client), tool_count))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A manager with one connected fake server exposing `tools`.
    pub(crate) fn manager_with_tools(server: &str, tools: Value, call_result: Value) -> McpManager {
        let manager = McpManager::default();
        let client = client::test_support::fake_client(server, tools, call_result);
        manager.insert_connected_for_tests(test_config(server), Arc::new(client));
        manager
    }

    pub(crate) fn test_config(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Stdio,
            command: "true".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            enabled: true,
            timeout_seconds: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{manager_with_tools, test_config};
    use super::*;

    fn echo_tools() -> Value {
        json!([{
            "name": "echo",
            "description": "Echo text",
            "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } },
            "annotations": { "readOnlyHint": true }
        }])
    }

    #[test]
    fn definitions_use_prefixed_names_and_pass_schema_through() {
        let manager = manager_with_tools("files", echo_tools(), json!({}));
        let definitions = manager.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0]["name"], "mcp__files__echo");
        assert_eq!(definitions[0]["description"], "Echo text");
        assert_eq!(definitions[0]["parameters"]["type"], "object");
    }

    #[test]
    fn duplicate_tool_names_keep_the_first_definition() {
        let tools = json!([
            { "name": "echo", "description": "first", "inputSchema": { "type": "object" } },
            { "name": "echo", "description": "second", "inputSchema": { "type": "object" } }
        ]);
        let manager = manager_with_tools("files", tools, json!({}));
        let definitions = manager.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0]["description"], "first");
    }

    #[test]
    fn run_tool_routes_to_the_owning_server() {
        let manager = manager_with_tools(
            "files",
            echo_tools(),
            json!({ "content": [{ "type": "text", "text": "echoed!" }] }),
        );
        let (output, is_error) = manager
            .run_tool("mcp__files__echo", r#"{"text":"hi"}"#)
            .unwrap();
        assert!(!is_error);
        assert_eq!(output, "echoed!");
    }

    #[test]
    fn run_tool_ignores_non_mcp_names_and_errors_on_unknown_servers() {
        let manager = manager_with_tools("files", echo_tools(), json!({}));
        assert!(manager.run_tool("bash", "{}").is_none());
        let (output, is_error) = manager.run_tool("mcp__nope__echo", "{}").unwrap();
        assert!(is_error);
        assert!(output.contains("unknown MCP tool"));
    }

    #[test]
    fn run_tool_rejects_non_object_arguments() {
        let manager = manager_with_tools("files", echo_tools(), json!({}));
        let (output, is_error) = manager.run_tool("mcp__files__echo", "[1,2]").unwrap();
        assert!(is_error);
        assert!(output.contains("JSON object"));
        let (_, is_error) = manager.run_tool("mcp__files__echo", "").unwrap();
        assert!(!is_error, "empty arguments default to an empty object");
    }

    #[test]
    fn tool_names_with_double_underscores_resolve_to_longest_server() {
        let manager = manager_with_tools(
            "a",
            json!([{ "name": "b__tool", "inputSchema": { "type": "object" } }]),
            json!({ "content": [{ "type": "text", "text": "short" }] }),
        );
        let client = client::test_support::fake_client(
            "a__b",
            json!([{ "name": "tool", "inputSchema": { "type": "object" } }]),
            json!({ "content": [{ "type": "text", "text": "long" }] }),
        );
        manager.insert_connected_for_tests(test_config("a__b"), Arc::new(client));
        let (output, _) = manager.run_tool("mcp__a__b__tool", "{}").unwrap();
        assert_eq!(output, "long");
    }

    #[test]
    fn read_only_hint_is_surfaced_per_tool() {
        let manager = manager_with_tools("files", echo_tools(), json!({}));
        assert_eq!(manager.tool_read_only_hint("mcp__files__echo"), Some(true));
        assert_eq!(manager.tool_read_only_hint("mcp__files__nope"), None);
        assert_eq!(manager.tool_read_only_hint("bash"), None);
    }

    #[test]
    fn views_expose_state_and_tools() {
        let manager = manager_with_tools("files", echo_tools(), json!({}));
        let views = manager.views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "files");
        assert_eq!(views[0].state, "connected");
        assert_eq!(views[0].tools.len(), 1);
        assert_eq!(views[0].tools[0].name, "echo");
        assert!(views[0].error.is_none());
    }

    #[test]
    fn dirty_flag_and_messages_are_drained_once() {
        let manager = manager_with_tools("files", echo_tools(), json!({}));
        assert!(manager.take_dirty());
        assert!(!manager.take_dirty());
        assert!(manager.drain_messages().is_empty());
    }

    #[test]
    fn stale_connect_result_is_discarded_after_regeneration() {
        let manager = McpManager::default();
        let mut config = test_config("files");
        config.enabled = false;
        manager.upsert(config.clone(), Path::new("."));
        // A connect result from generation 0 must not override generation 1.
        manager.finish_connect("files", 0, Err("stale".to_string()));
        assert_eq!(manager.views()[0].state, "disabled");
    }

    #[test]
    fn disabled_upsert_keeps_server_listed_but_disconnected() {
        let manager = McpManager::default();
        let mut config = test_config("files");
        config.enabled = false;
        manager.upsert(config, Path::new("."));
        assert!(manager.contains("files"));
        assert_eq!(manager.views()[0].state, "disabled");
        let (output, is_error) = manager.run_tool("mcp__files__echo", "{}").unwrap();
        assert!(is_error);
        assert!(output.contains("disabled"), "{output}");
    }

    /// Full protocol walk (initialize → initialized → tools/list → tools/call)
    /// against a real subprocess speaking canned newline JSON-RPC. Request ids
    /// are monotonic from 1, so the script matches on method names.
    #[cfg(unix)]
    #[test]
    fn connect_server_speaks_full_protocol_over_stdio() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!(
            "jucode-mcp-e2e-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.sh");
        fs::write(
            &script,
            r##"#!/bin/sh
while read -r line; do
  case "$line" in
    *'"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"canned","version":"1.0"}}}' ;;
    *'"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"greet","description":"Say hi","inputSchema":{"type":"object"}}]}}' ;;
    *'"tools/call"'*) printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"hi there"}]}}' ;;
  esac
done
"##,
        )
        .unwrap();

        let mut config = test_config("canned");
        config.command = "/bin/sh".to_string();
        config.args = vec![script.display().to_string()];
        let (client, tool_count) = connect_server(&config, &dir).unwrap();
        assert_eq!(tool_count, 1);
        assert_eq!(client.protocol_version(), "2025-06-18");
        assert_eq!(client.server_info(), "canned 1.0");

        let manager = McpManager::default();
        manager.insert_connected_for_tests(config, client);
        assert_eq!(manager.definitions()[0]["name"], "mcp__canned__greet");
        let (output, is_error) = manager.run_tool("mcp__canned__greet", "{}").unwrap();
        assert!(!is_error);
        assert_eq!(output, "hi there");
        let _ = fs::remove_dir_all(dir);
    }
}
