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
const MCP_LIST_RESOURCES_TOOL: &str = "list_resources";
const MCP_READ_RESOURCE_TOOL: &str = "read_resource";

#[derive(Debug, Clone)]
pub struct McpPromptCommand {
    pub command: String,
    pub args: String,
    pub description: String,
}

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
            client.refresh_changed();
            for tool in client.tools_snapshot() {
                if client.supports_resources()
                    && matches!(
                        tool.name.as_str(),
                        MCP_LIST_RESOURCES_TOOL | MCP_READ_RESOURCE_TOOL
                    )
                {
                    crate::log_warn!(
                        "mcp",
                        "server tool name conflicts with reserved resource tool",
                        server = server.clone(),
                        tool = tool.name
                    );
                    continue;
                }
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
            if client.supports_resources() {
                let resource_definitions = [
                    json!({
                        "type": "function",
                        "name": mcp_tool_name(&server, MCP_LIST_RESOURCES_TOOL),
                        "description": format!("List text resources exposed by the {server} MCP server"),
                        "parameters": { "type": "object", "properties": {}, "additionalProperties": false },
                    }),
                    json!({
                        "type": "function",
                        "name": mcp_tool_name(&server, MCP_READ_RESOURCE_TOOL),
                        "description": format!("Read one listed text resource from the {server} MCP server"),
                        "parameters": {
                            "type": "object",
                            "properties": { "uri": { "type": "string" } },
                            "required": ["uri"],
                            "additionalProperties": false
                        },
                    }),
                ];
                for definition in resource_definitions {
                    if seen.insert(definition["name"].as_str().unwrap_or_default().to_string()) {
                        definitions.push(definition);
                    }
                }
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
        client.refresh_changed();
        if tool == MCP_LIST_RESOURCES_TOOL && client.supports_resources() {
            return Some((client.list_resources_output(), false));
        }
        if tool == MCP_READ_RESOURCE_TOOL && client.supports_resources() {
            let Some(uri) = args.get("uri").and_then(Value::as_str) else {
                return Some(error_output(
                    "read_resource requires string uri".to_string(),
                ));
            };
            return Some(match client.read_resource(uri) {
                Ok(output) => (output, false),
                Err(error) => error_output(error),
            });
        }
        Some(client.call_tool(&tool, args))
    }

    pub fn prompt_commands(&self) -> Vec<McpPromptCommand> {
        let mut commands = Vec::new();
        for (server, client) in self.connected_clients() {
            client.refresh_changed();
            for prompt in client.prompts_snapshot() {
                let required = prompt
                    .arguments
                    .iter()
                    .filter(|argument| argument.required)
                    .map(|argument| argument.name.as_str())
                    .collect::<Vec<_>>();
                let args = if prompt.arguments.is_empty() {
                    String::new()
                } else if required.is_empty() {
                    "[JSON arguments]".to_string()
                } else {
                    format!("<JSON: {}>", required.join(", "))
                };
                commands.push(McpPromptCommand {
                    command: format!("/{}", mcp_tool_name(&server, &prompt.name)),
                    args,
                    description: prompt.description,
                });
            }
        }
        commands.sort_by(|left, right| left.command.cmp(&right.command));
        commands
    }

    pub fn run_prompt(&self, command: &str, arguments: &str) -> Option<Result<String, String>> {
        let name = command.strip_prefix('/')?;
        if !name.starts_with(MCP_TOOL_PREFIX) {
            return None;
        }
        let (server, prompt) = self.resolve_tool_name(name)?;
        let client = match self.client_for(&server) {
            Ok(client) => client,
            Err(error) => return Some(Err(error)),
        };
        client.refresh_changed();
        if !client
            .prompts_snapshot()
            .iter()
            .any(|info| info.name == prompt)
        {
            return None;
        }
        let arguments = match parse_arguments(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return Some(Err(error)),
        };
        Some(client.get_prompt(&prompt, arguments))
    }

    /// Consume list-change notifications outside model calls. The return value
    /// tells the core to re-emit dynamic prompt slash commands.
    pub fn refresh_changed(&self) -> bool {
        let mut prompts_changed = false;
        for (_, client) in self.connected_clients() {
            prompts_changed |= client.refresh_changed();
        }
        if prompts_changed {
            self.lock().dirty = true;
        }
        prompts_changed
    }

    /// The cached `annotations.readOnlyHint` for a full MCP tool name.
    pub fn tool_read_only_hint(&self, name: &str) -> Option<bool> {
        let (server, tool) = self.resolve_tool_name(name)?;
        let client = self.client_for(&server).ok()?;
        if client.supports_resources()
            && matches!(
                tool.as_str(),
                MCP_LIST_RESOURCES_TOOL | MCP_READ_RESOURCE_TOOL
            )
        {
            return Some(true);
        }
        client.read_only_hint(&tool)
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
            match &config.oauth {
                Some(metadata) => {
                    let tokens = crate::config::load_mcp_oauth_tokens(&config.name)
                        .map_err(|error| format!("failed to read MCP OAuth tokens: {error}"))?
                        .ok_or_else(|| {
                            format!(
                                "MCP server {} uses OAuth but auth.json has no mcp_servers.{} token",
                                config.name, config.name
                            )
                        })?;
                    Some((
                        metadata.clone(),
                        tokens,
                        crate::config::mcp_auth_path()
                            .map_err(|error| format!("failed to locate auth.json: {error}"))?,
                    ))
                }
                None => None,
            },
            cwd,
        )),
    };
    let client = McpClient::new(&config.name, transport, timeout, cwd);
    client.initialize()?;
    let tool_count = if client.supports_tools() {
        client.refresh_tools()?
    } else {
        0
    };
    if client.supports_prompts() {
        client.refresh_prompts()?;
    }
    if client.supports_resources() {
        client.refresh_resources()?;
    }
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

    pub(crate) fn manager_with_prompts_and_resources(
        server: &str,
        prompts: Value,
        resources: Value,
        prompt_result: Value,
        resource_result: Value,
    ) -> McpManager {
        let manager = McpManager::default();
        let state = client::test_support::FakeState::new(vec![json!({ "tools": [] })], json!({}));
        state.initialize_result.lock().unwrap()["capabilities"] =
            json!({ "tools": {}, "prompts": {}, "resources": {} });
        *state.prompt_pages.lock().unwrap() = vec![json!({ "prompts": prompts })];
        *state.resource_pages.lock().unwrap() = vec![json!({ "resources": resources })];
        *state.prompt_result.lock().unwrap() = prompt_result;
        *state.resource_result.lock().unwrap() = resource_result;
        let client = McpClient::new(
            server,
            Box::new(client::test_support::FakeTransport(state)),
            Duration::from_secs(5),
            Path::new("/workspace"),
        );
        client.initialize().unwrap();
        client.refresh_tools().unwrap();
        client.refresh_prompts().unwrap();
        client.refresh_resources().unwrap();
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
            oauth: None,
            enabled: true,
            timeout_seconds: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{
        manager_with_prompts_and_resources, manager_with_tools, test_config,
    };
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
    fn prompts_are_exposed_as_dynamic_slash_commands() {
        let manager = manager_with_prompts_and_resources(
            "docs",
            json!([{
                "name": "summarize",
                "description": "Summarize docs",
                "arguments": [{ "name": "style", "required": true }]
            }]),
            json!([]),
            json!({
                "messages": [{ "role": "user", "content": { "type": "text", "text": "Summarize now" } }]
            }),
            json!({ "contents": [] }),
        );
        let commands = manager.prompt_commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "/mcp__docs__summarize");
        assert!(commands[0].args.contains("style"));
        let rendered = manager
            .run_prompt("/mcp__docs__summarize", r#"{"style":"brief"}"#)
            .unwrap()
            .unwrap();
        assert!(rendered.contains("Summarize now"));
    }

    #[test]
    fn resources_add_model_tools_and_route_bounded_reads() {
        let manager = manager_with_prompts_and_resources(
            "docs",
            json!([]),
            json!([{
                "uri": "file:///workspace/guide.md",
                "name": "guide",
                "mimeType": "text/markdown"
            }]),
            json!({ "messages": [] }),
            json!({
                "contents": [{
                    "uri": "file:///workspace/guide.md",
                    "mimeType": "text/markdown",
                    "text": "Guide body"
                }]
            }),
        );
        let names = manager
            .definitions()
            .into_iter()
            .filter_map(|definition| definition["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert!(names.contains(&"mcp__docs__list_resources".to_string()));
        assert!(names.contains(&"mcp__docs__read_resource".to_string()));
        let (listed, list_error) = manager.run_tool("mcp__docs__list_resources", "{}").unwrap();
        assert!(!list_error);
        assert!(listed.contains("guide.md"));
        let (read, read_error) = manager
            .run_tool(
                "mcp__docs__read_resource",
                r#"{"uri":"file:///workspace/guide.md"}"#,
            )
            .unwrap();
        assert!(!read_error);
        assert!(read.contains("Guide body"));
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

    /// Full protocol walk against a real subprocess speaking canned newline
    /// JSON-RPC, including tools, prompts, and resources.
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
    *'"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{},"prompts":{},"resources":{}},"serverInfo":{"name":"canned","version":"1.0"}}}' ;;
    *'"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"greet","description":"Say hi","inputSchema":{"type":"object"}}]}}' ;;
    *'"prompts/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"prompts":[{"name":"review","description":"Review code"}]}}' ;;
    *'"resources/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"resources":[{"uri":"doc://guide","name":"guide","mimeType":"text/plain"}]}}' ;;
    *'"tools/call"'*) printf '%s\n' '{"jsonrpc":"2.0","id":5,"result":{"content":[{"type":"text","text":"hi there"}]}}' ;;
    *'"prompts/get"'*) printf '%s\n' '{"jsonrpc":"2.0","id":6,"result":{"messages":[{"role":"user","content":{"type":"text","text":"Review this"}}]}}' ;;
    *'"resources/read"'*) printf '%s\n' '{"jsonrpc":"2.0","id":7,"result":{"contents":[{"uri":"doc://guide","text":"Guide text"}]}}' ;;
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
        assert_eq!(manager.prompt_commands()[0].command, "/mcp__canned__review");
        assert!(manager
            .run_prompt("/mcp__canned__review", "{}")
            .unwrap()
            .unwrap()
            .contains("Review this"));
        let (resource, is_error) = manager
            .run_tool("mcp__canned__read_resource", r#"{"uri":"doc://guide"}"#)
            .unwrap();
        assert!(!is_error);
        assert!(resource.contains("Guide text"));
        let _ = fs::remove_dir_all(dir);
    }
}
