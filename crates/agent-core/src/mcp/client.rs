//! MCP protocol client: initialize handshake, paginated tools/prompts/resources,
//! prompt/resource reads, and tool calls. Transport-agnostic.

use crate::mcp::transport::McpTransport;
use serde_json::{json, Value};
use std::{
    path::{Component, Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

/// Protocol version this client speaks. An older version offered back by the
/// server in the initialize result is accepted and recorded.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Safety cap on tools/list pagination to survive a broken cursor loop.
const MAX_TOOL_LIST_PAGES: usize = 64;
const MAX_PROMPT_LIST_PAGES: usize = 64;
const MAX_RESOURCE_LIST_PAGES: usize = 64;
const MAX_MCP_CONTEXT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// The server's `annotations.readOnlyHint`. Untrusted metadata — it only
    /// ever loosens approval in auto-edit mode, never grants full trust.
    pub read_only_hint: bool,
}

#[derive(Debug, Clone)]
pub struct McpPromptInfo {
    pub name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArgument>,
}

#[derive(Debug, Clone)]
pub struct McpPromptArgument {
    pub name: String,
    pub required: bool,
}

#[derive(Debug, Clone)]
pub struct McpResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Default)]
struct ServerCapabilities {
    tools: bool,
    prompts: bool,
    resources: bool,
}

pub struct McpClient {
    server: String,
    transport: Box<dyn McpTransport>,
    timeout: Duration,
    protocol_version: Mutex<String>,
    server_info: Mutex<String>,
    capabilities: Mutex<ServerCapabilities>,
    tools: Mutex<Vec<McpToolInfo>>,
    prompts: Mutex<Vec<McpPromptInfo>>,
    resources: Mutex<Vec<McpResourceInfo>>,
    root: PathBuf,
}

impl McpClient {
    pub fn new(
        server: &str,
        transport: Box<dyn McpTransport>,
        timeout: Duration,
        root: &Path,
    ) -> Self {
        Self {
            server: server.to_string(),
            transport,
            timeout,
            protocol_version: Mutex::new(MCP_PROTOCOL_VERSION.to_string()),
            server_info: Mutex::new(String::new()),
            capabilities: Mutex::new(ServerCapabilities::default()),
            tools: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
            resources: Mutex::new(Vec::new()),
            root: root.to_path_buf(),
        }
    }

    /// `initialize` request/response followed by the `notifications/initialized`
    /// notification. Records the server's (possibly older) protocol version.
    pub fn initialize(&self) -> Result<(), String> {
        let params = json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "roots": { "listChanged": false } },
            "clientInfo": { "name": "jucode", "version": env!("CARGO_PKG_VERSION") }
        });
        let result = self.transport.request("initialize", params, self.timeout)?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(MCP_PROTOCOL_VERSION)
            .to_string();
        if version != MCP_PROTOCOL_VERSION {
            crate::log_info!(
                "mcp",
                "server negotiated a different protocol version",
                server = self.server.clone(),
                version = version.clone()
            );
        }
        self.transport.set_protocol_version(&version);
        if let Ok(mut slot) = self.protocol_version.lock() {
            *slot = version;
        }
        if let Ok(mut slot) = self.server_info.lock() {
            *slot = format_server_info(&result);
        }
        let capabilities = parse_server_capabilities(&result);
        if !capabilities.tools {
            crate::log_warn!(
                "mcp",
                "server does not advertise the tools capability",
                server = self.server.clone()
            );
        }
        if let Ok(mut slot) = self.capabilities.lock() {
            *slot = capabilities;
        }
        self.transport
            .notify("notifications/initialized", Value::Null)
    }

    /// Full `tools/list` walk following `nextCursor`; replaces the tool cache.
    pub fn refresh_tools(&self) -> Result<usize, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_LIST_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.transport.request("tools/list", params, self.timeout)?;
            if let Some(page) = result.get("tools").and_then(Value::as_array) {
                tools.extend(page.iter().map(parse_tool_info));
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                let count = tools.len();
                if let Ok(mut slot) = self.tools.lock() {
                    *slot = tools;
                }
                return Ok(count);
            }
        }
        Err("tools/list pagination did not terminate".to_string())
    }

    pub fn refresh_prompts(&self) -> Result<usize, String> {
        let mut prompts = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PROMPT_LIST_PAGES {
            let params = cursor
                .as_ref()
                .map(|cursor| json!({ "cursor": cursor }))
                .unwrap_or_else(|| json!({}));
            let result = self
                .transport
                .request("prompts/list", params, self.timeout)?;
            if let Some(page) = result.get("prompts").and_then(Value::as_array) {
                prompts.extend(page.iter().map(parse_prompt_info));
            }
            cursor = next_cursor(&result);
            if cursor.is_none() {
                let count = prompts.len();
                if let Ok(mut slot) = self.prompts.lock() {
                    *slot = prompts;
                }
                return Ok(count);
            }
        }
        Err("prompts/list pagination did not terminate".to_string())
    }

    pub fn refresh_resources(&self) -> Result<usize, String> {
        let mut resources = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_RESOURCE_LIST_PAGES {
            let params = cursor
                .as_ref()
                .map(|cursor| json!({ "cursor": cursor }))
                .unwrap_or_else(|| json!({}));
            let result = self
                .transport
                .request("resources/list", params, self.timeout)?;
            if let Some(page) = result.get("resources").and_then(Value::as_array) {
                resources.extend(page.iter().map(parse_resource_info));
            }
            cursor = next_cursor(&result);
            if cursor.is_none() {
                let count = resources.len();
                if let Ok(mut slot) = self.resources.lock() {
                    *slot = resources;
                }
                return Ok(count);
            }
        }
        Err("resources/list pagination did not terminate".to_string())
    }

    /// Refresh every cache invalidated by server notifications. Returns true
    /// when prompt commands may have changed.
    pub fn refresh_changed(&self) -> bool {
        if self.transport.take_tools_list_changed() {
            log_refresh_error(&self.server, "tools", self.refresh_tools());
        }
        let prompts_changed = self.transport.take_prompts_list_changed();
        if prompts_changed {
            log_refresh_error(&self.server, "prompts", self.refresh_prompts());
        }
        if self.transport.take_resources_list_changed() {
            log_refresh_error(&self.server, "resources", self.refresh_resources());
        }
        prompts_changed
    }

    pub fn supports_prompts(&self) -> bool {
        self.capabilities
            .lock()
            .map(|capabilities| capabilities.prompts)
            .unwrap_or(false)
    }

    pub fn supports_tools(&self) -> bool {
        self.capabilities
            .lock()
            .map(|capabilities| capabilities.tools)
            .unwrap_or(false)
    }

    pub fn supports_resources(&self) -> bool {
        self.capabilities
            .lock()
            .map(|capabilities| capabilities.resources)
            .unwrap_or(false)
    }

    pub fn prompts_snapshot(&self) -> Vec<McpPromptInfo> {
        self.prompts
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }

    pub fn resources_snapshot(&self) -> Vec<McpResourceInfo> {
        self.resources
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }

    pub fn get_prompt(&self, name: &str, arguments: Value) -> Result<String, String> {
        validate_prompt_arguments(
            self.prompts_snapshot()
                .iter()
                .find(|prompt| prompt.name == name),
            &arguments,
        )?;
        let result = self.transport.request(
            "prompts/get",
            json!({ "name": name, "arguments": arguments }),
            self.timeout,
        )?;
        render_prompt_result(&result)
    }

    pub fn list_resources_output(&self) -> String {
        let resources = self
            .resources_snapshot()
            .into_iter()
            .map(|resource| {
                json!({
                    "uri": resource.uri,
                    "name": resource.name,
                    "description": resource.description,
                    "mimeType": resource.mime_type,
                })
            })
            .collect::<Vec<_>>();
        json!({ "resources": resources }).to_string()
    }

    pub fn read_resource(&self, uri: &str) -> Result<String, String> {
        if !self
            .resources_snapshot()
            .iter()
            .any(|resource| resource.uri == uri)
        {
            return Err("resource URI was not returned by resources/list".to_string());
        }
        validate_resource_uri(uri, &self.root)?;
        let result =
            self.transport
                .request("resources/read", json!({ "uri": uri }), self.timeout)?;
        render_resource_result(&result)
    }

    /// `tools/call`; returns (output, is_error) like other tool runners.
    pub fn call_tool(&self, name: &str, arguments: Value) -> (String, bool) {
        let params = json!({ "name": name, "arguments": arguments });
        match self.transport.request("tools/call", params, self.timeout) {
            Ok(result) => parse_tool_result(&result),
            Err(error) => (json!({ "error": error }).to_string(), true),
        }
    }

    pub fn tools_snapshot(&self) -> Vec<McpToolInfo> {
        self.tools
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }

    pub fn read_only_hint(&self, tool: &str) -> Option<bool> {
        self.tools
            .lock()
            .ok()?
            .iter()
            .find(|info| info.name == tool)
            .map(|info| info.read_only_hint)
    }

    pub fn protocol_version(&self) -> String {
        self.protocol_version
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }

    pub fn server_info(&self) -> String {
        self.server_info
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }
}

fn format_server_info(initialize_result: &Value) -> String {
    let info = initialize_result.get("serverInfo");
    let field = |key: &str| {
        info.and_then(|info| info.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    format!("{} {}", field("name"), field("version"))
        .trim()
        .to_string()
}

fn parse_server_capabilities(initialize_result: &Value) -> ServerCapabilities {
    let capabilities = initialize_result.get("capabilities");
    ServerCapabilities {
        tools: capabilities.and_then(|value| value.get("tools")).is_some(),
        prompts: capabilities
            .and_then(|value| value.get("prompts"))
            .is_some(),
        resources: capabilities
            .and_then(|value| value.get("resources"))
            .is_some(),
    }
}

fn next_cursor(result: &Value) -> Option<String> {
    result
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_string)
}

fn log_refresh_error(server: &str, kind: &str, result: Result<usize, String>) {
    if let Err(error) = result {
        crate::log_warn!(
            "mcp",
            "list refresh failed",
            server = server,
            kind = kind,
            error = error
        );
    }
}

fn parse_tool_info(tool: &Value) -> McpToolInfo {
    McpToolInfo {
        name: tool
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        description: tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        input_schema: tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" })),
        read_only_hint: tool
            .get("annotations")
            .and_then(|annotations| annotations.get("readOnlyHint"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn parse_prompt_info(prompt: &Value) -> McpPromptInfo {
    let arguments = prompt
        .get("arguments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|argument| McpPromptArgument {
            name: string_field(argument, "name"),
            required: argument
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
        .filter(|argument| !argument.name.is_empty())
        .collect();
    McpPromptInfo {
        name: string_field(prompt, "name"),
        description: string_field(prompt, "description"),
        arguments,
    }
}

fn parse_resource_info(resource: &Value) -> McpResourceInfo {
    McpResourceInfo {
        uri: string_field(resource, "uri"),
        name: string_field(resource, "name"),
        description: string_field(resource, "description"),
        mime_type: string_field(resource, "mimeType"),
    }
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn validate_prompt_arguments(
    prompt: Option<&McpPromptInfo>,
    arguments: &Value,
) -> Result<(), String> {
    let prompt = prompt.ok_or_else(|| "unknown MCP prompt".to_string())?;
    let object = arguments
        .as_object()
        .ok_or_else(|| "MCP prompt arguments must be a JSON object".to_string())?;
    for argument in &prompt.arguments {
        if argument.required && !object.contains_key(&argument.name) {
            return Err(format!(
                "missing required prompt argument: {}",
                argument.name
            ));
        }
    }
    if object.values().any(|value| !value.is_string()) {
        return Err("MCP prompt argument values must be strings".to_string());
    }
    Ok(())
}

fn render_prompt_result(result: &Value) -> Result<String, String> {
    let mut output = String::new();
    if let Some(description) = result.get("description").and_then(Value::as_str) {
        append_limited(&mut output, description)?;
        output.push_str("\n\n");
    }
    let messages = result
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "prompts/get response missing messages".to_string())?;
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let content = message.get("content").unwrap_or(&Value::Null);
        let rendered = render_prompt_content(content);
        append_limited(&mut output, &format!("{role}:\n{rendered}\n\n"))?;
    }
    if output.trim().is_empty() {
        return Err("prompts/get returned no text content".to_string());
    }
    Ok(output.trim_end().to_string())
}

fn render_prompt_content(content: &Value) -> String {
    match content.get("type").and_then(Value::as_str) {
        Some("text") => string_field(content, "text"),
        Some("resource") => {
            let resource = content.get("resource").unwrap_or(&Value::Null);
            match resource.get("text").and_then(Value::as_str) {
                Some(text) => format!("[resource {}]\n{text}", string_field(resource, "uri")),
                None => format!(
                    "[binary resource {} omitted]",
                    string_field(resource, "uri")
                ),
            }
        }
        Some(kind @ ("image" | "audio")) => format!("[{kind} content omitted]"),
        Some(other) => format!("[unsupported MCP prompt content: {other}]"),
        None => "[invalid MCP prompt content]".to_string(),
    }
}

fn render_resource_result(result: &Value) -> Result<String, String> {
    let contents = result
        .get("contents")
        .and_then(Value::as_array)
        .ok_or_else(|| "resources/read response missing contents".to_string())?;
    let mut output = String::new();
    for content in contents {
        let uri = string_field(content, "uri");
        let text = content.get("text").and_then(Value::as_str).ok_or_else(|| {
            format!("resource {uri} is binary; only text resources are supported")
        })?;
        let mime = string_field(content, "mimeType");
        let heading = if mime.is_empty() {
            format!("[resource {uri}]\n")
        } else {
            format!("[resource {uri} ({mime})]\n")
        };
        append_limited(&mut output, &heading)?;
        append_limited(&mut output, text)?;
        output.push('\n');
    }
    if output.is_empty() {
        return Err("resources/read returned no contents".to_string());
    }
    Ok(output.trim_end().to_string())
}

fn append_limited(output: &mut String, text: &str) -> Result<(), String> {
    if output.len().saturating_add(text.len()) > MAX_MCP_CONTEXT_BYTES {
        return Err(format!(
            "MCP content exceeds the {} byte limit",
            MAX_MCP_CONTEXT_BYTES
        ));
    }
    output.push_str(text);
    Ok(())
}

fn validate_resource_uri(uri: &str, root: &Path) -> Result<(), String> {
    let Some(encoded_path) = uri.strip_prefix("file://") else {
        return Ok(());
    };
    if !encoded_path.starts_with('/') {
        return Err("file resource URI with an authority is not allowed".to_string());
    }
    let decoded = percent_decode(encoded_path)?;
    let candidate = PathBuf::from(decoded);
    let root = root.canonicalize().unwrap_or_else(|_| normalize_path(root));
    let candidate = candidate
        .canonicalize()
        .unwrap_or_else(|_| normalize_path(&candidate));
    if !candidate.starts_with(&root) {
        return Err(format!(
            "file resource escapes workspace root {}",
            root.display()
        ));
    }
    Ok(())
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            Component::Normal(part) => output.push(part),
        }
    }
    output
}

fn percent_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err("invalid percent escape in resource URI".to_string());
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .map_err(|_| "invalid percent escape in resource URI".to_string())?;
            output.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| "invalid percent escape in resource URI".to_string())?,
            );
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).map_err(|_| "resource URI path is not UTF-8".to_string())
}

/// Flatten a tools/call result: text items concatenated, binary/resource items
/// summarized as metadata, `isError` mapped to the error flag.
pub fn parse_tool_result(result: &Value) -> (String, bool) {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for item in content {
            parts.push(render_content_item(item));
        }
    }
    if parts.is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            parts.push(structured.to_string());
        }
    }
    let output = parts.join("\n");
    if output.is_empty() {
        return ("(no content)".to_string(), is_error);
    }
    (output, is_error)
}

fn render_content_item(item: &Value) -> String {
    let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
    match text("type") {
        "text" => text("text").to_string(),
        kind @ ("image" | "audio") => format!(
            "[{kind} {} ({} base64 bytes, not shown)]",
            text("mimeType"),
            text("data").len()
        ),
        "resource" => {
            let resource = item.get("resource");
            let uri = resource
                .and_then(|resource| resource.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            match resource
                .and_then(|resource| resource.get("text"))
                .and_then(Value::as_str)
            {
                Some(body) => format!("[resource {uri}]\n{body}"),
                None => format!("[resource {uri} (binary, not shown)]"),
            }
        }
        "resource_link" => format!("[resource link {}]", text("uri")),
        other => format!("[unsupported content type: {other}]"),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    /// Scripted transport state: canned initialize/tools/list pages and
    /// tools/call results, with a recorded request log. Tests keep a clone of
    /// the `Arc` to inspect traffic after the client takes ownership.
    pub(crate) struct FakeState {
        pub initialize_result: Mutex<Value>,
        pub list_pages: Mutex<Vec<Value>>,
        pub prompt_pages: Mutex<Vec<Value>>,
        pub resource_pages: Mutex<Vec<Value>>,
        pub call_result: Mutex<Value>,
        pub prompt_result: Mutex<Value>,
        pub resource_result: Mutex<Value>,
        pub requests: Mutex<Vec<(String, Value)>>,
        pub list_calls: Mutex<usize>,
        pub prompt_list_calls: Mutex<usize>,
        pub resource_list_calls: Mutex<usize>,
        pub list_changed: AtomicBool,
        pub prompts_changed: AtomicBool,
        pub resources_changed: AtomicBool,
    }

    pub(crate) struct FakeTransport(pub Arc<FakeState>);

    impl FakeState {
        pub(crate) fn new(list_pages: Vec<Value>, call_result: Value) -> Arc<Self> {
            Arc::new(Self {
                initialize_result: Mutex::new(json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "fake", "version": "0.1" }
                })),
                list_pages: Mutex::new(list_pages),
                prompt_pages: Mutex::new(vec![json!({ "prompts": [] })]),
                resource_pages: Mutex::new(vec![json!({ "resources": [] })]),
                call_result: Mutex::new(call_result),
                prompt_result: Mutex::new(json!({ "messages": [] })),
                resource_result: Mutex::new(json!({ "contents": [] })),
                requests: Mutex::new(Vec::new()),
                list_calls: Mutex::new(0),
                prompt_list_calls: Mutex::new(0),
                resource_list_calls: Mutex::new(0),
                list_changed: AtomicBool::new(false),
                prompts_changed: AtomicBool::new(false),
                resources_changed: AtomicBool::new(false),
            })
        }
    }

    impl McpTransport for FakeTransport {
        fn request(
            &self,
            method: &str,
            params: Value,
            _timeout: Duration,
        ) -> Result<Value, String> {
            let state = &self.0;
            state
                .requests
                .lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
            match method {
                "initialize" => Ok(state.initialize_result.lock().unwrap().clone()),
                "tools/list" => {
                    let pages = state.list_pages.lock().unwrap();
                    let mut calls = state.list_calls.lock().unwrap();
                    let page = pages
                        .get(*calls % pages.len().max(1))
                        .cloned()
                        .unwrap_or_else(|| json!({ "tools": [] }));
                    *calls += 1;
                    Ok(page)
                }
                "tools/call" => Ok(state.call_result.lock().unwrap().clone()),
                "prompts/list" => next_fake_page(&state.prompt_pages, &state.prompt_list_calls),
                "resources/list" => {
                    next_fake_page(&state.resource_pages, &state.resource_list_calls)
                }
                "prompts/get" => Ok(state.prompt_result.lock().unwrap().clone()),
                "resources/read" => Ok(state.resource_result.lock().unwrap().clone()),
                other => Err(format!("unexpected request: {other}")),
            }
        }

        fn notify(&self, method: &str, params: Value) -> Result<(), String> {
            self.0
                .requests
                .lock()
                .unwrap()
                .push((format!("notify:{method}"), params));
            Ok(())
        }

        fn take_tools_list_changed(&self) -> bool {
            self.0.list_changed.swap(false, Ordering::SeqCst)
        }

        fn take_prompts_list_changed(&self) -> bool {
            self.0.prompts_changed.swap(false, Ordering::SeqCst)
        }

        fn take_resources_list_changed(&self) -> bool {
            self.0.resources_changed.swap(false, Ordering::SeqCst)
        }
    }

    fn next_fake_page(pages: &Mutex<Vec<Value>>, calls: &Mutex<usize>) -> Result<Value, String> {
        let pages = pages.lock().unwrap();
        let mut calls = calls.lock().unwrap();
        let page = pages
            .get(*calls % pages.len().max(1))
            .cloned()
            .unwrap_or_else(|| json!({}));
        *calls += 1;
        Ok(page)
    }

    /// A connected-and-listed client over a `FakeTransport`.
    pub(crate) fn fake_client(server: &str, tools: Value, call_result: Value) -> McpClient {
        let state = FakeState::new(vec![json!({ "tools": tools })], call_result);
        let client = McpClient::new(
            server,
            Box::new(FakeTransport(state)),
            Duration::from_secs(5),
            Path::new("."),
        );
        client.initialize().unwrap();
        client.refresh_tools().unwrap();
        client
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FakeState, FakeTransport};
    use super::*;

    #[test]
    fn initialize_sends_handshake_then_initialized_notification() {
        let state = FakeState::new(vec![json!({ "tools": [] })], json!({}));
        let client = McpClient::new(
            "srv",
            Box::new(FakeTransport(state.clone())),
            Duration::from_secs(5),
            Path::new("."),
        );
        client.initialize().unwrap();

        let requests = state.requests.lock().unwrap();
        assert_eq!(requests[0].0, "initialize");
        assert_eq!(requests[0].1["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(requests[0].1["clientInfo"]["name"], "jucode");
        assert_eq!(
            requests[0].1["clientInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            requests[0].1["capabilities"],
            json!({ "roots": { "listChanged": false } })
        );
        assert_eq!(requests[1].0, "notify:notifications/initialized");
        assert_eq!(client.protocol_version(), MCP_PROTOCOL_VERSION);
        assert_eq!(client.server_info(), "fake 0.1");
    }

    #[test]
    fn initialize_records_the_servers_older_protocol_version() {
        let state = FakeState::new(vec![json!({ "tools": [] })], json!({}));
        state.initialize_result.lock().unwrap()["protocolVersion"] = json!("2025-03-26");
        let client = McpClient::new(
            "srv",
            Box::new(FakeTransport(state)),
            Duration::from_secs(5),
            Path::new("."),
        );
        client.initialize().unwrap();
        assert_eq!(client.protocol_version(), "2025-03-26");
    }

    #[test]
    fn refresh_tools_merges_paginated_pages() {
        let state = FakeState::new(
            vec![
                json!({
                    "tools": [{ "name": "a", "description": "A", "inputSchema": { "type": "object" } }],
                    "nextCursor": "page-2"
                }),
                json!({
                    "tools": [{
                        "name": "b",
                        "inputSchema": { "type": "object" },
                        "annotations": { "readOnlyHint": true }
                    }]
                }),
            ],
            json!({}),
        );
        let client = McpClient::new(
            "srv",
            Box::new(FakeTransport(state.clone())),
            Duration::from_secs(5),
            Path::new("."),
        );
        assert_eq!(client.refresh_tools().unwrap(), 2);
        let tools = client.tools_snapshot();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "a");
        assert!(!tools[0].read_only_hint);
        assert_eq!(tools[1].name, "b");
        assert!(tools[1].read_only_hint);
        assert_eq!(client.read_only_hint("b"), Some(true));
        assert_eq!(client.read_only_hint("missing"), None);

        let requests = state.requests.lock().unwrap();
        assert_eq!(requests[1].1["cursor"], "page-2");
    }

    #[test]
    fn call_tool_concatenates_text_content() {
        let client = test_support::fake_client(
            "srv",
            json!([]),
            json!({
                "content": [
                    { "type": "text", "text": "first" },
                    { "type": "text", "text": "second" }
                ]
            }),
        );
        let (output, is_error) = client.call_tool("t", json!({}));
        assert_eq!(output, "first\nsecond");
        assert!(!is_error);
    }

    #[test]
    fn call_tool_summarizes_binary_content_and_maps_is_error() {
        let (output, is_error) = parse_tool_result(&json!({
            "isError": true,
            "content": [
                { "type": "text", "text": "failed" },
                { "type": "image", "mimeType": "image/png", "data": "aaaa" },
                { "type": "resource", "resource": { "uri": "file:///x", "blob": "zz" } }
            ]
        }));
        assert!(is_error);
        assert!(output.contains("failed"));
        assert!(output.contains("[image image/png (4 base64 bytes, not shown)]"));
        assert!(output.contains("[resource file:///x (binary, not shown)]"));
    }

    #[test]
    fn parse_tool_result_falls_back_to_structured_content_then_placeholder() {
        let (output, _) = parse_tool_result(&json!({
            "content": [],
            "structuredContent": { "answer": 42 }
        }));
        assert_eq!(output, r#"{"answer":42}"#);
        let (output, is_error) = parse_tool_result(&json!({}));
        assert_eq!(output, "(no content)");
        assert!(!is_error);
    }

    #[test]
    fn embedded_text_resources_include_their_body() {
        let (output, _) = parse_tool_result(&json!({
            "content": [{
                "type": "resource",
                "resource": { "uri": "file:///a.txt", "text": "body text" }
            }]
        }));
        assert_eq!(output, "[resource file:///a.txt]\nbody text");
    }

    #[test]
    fn prompts_list_and_get_validate_arguments_and_render_messages() {
        let state = FakeState::new(vec![json!({ "tools": [] })], json!({}));
        state.initialize_result.lock().unwrap()["capabilities"] =
            json!({ "tools": {}, "prompts": { "listChanged": true } });
        *state.prompt_pages.lock().unwrap() = vec![json!({
            "prompts": [{
                "name": "review",
                "description": "Review a change",
                "arguments": [{ "name": "focus", "required": true }]
            }]
        })];
        *state.prompt_result.lock().unwrap() = json!({
            "description": "Review request",
            "messages": [
                { "role": "user", "content": { "type": "text", "text": "Check safety" } },
                { "role": "assistant", "content": { "type": "text", "text": "I will." } }
            ]
        });
        let client = McpClient::new(
            "srv",
            Box::new(FakeTransport(state.clone())),
            Duration::from_secs(5),
            Path::new("/workspace"),
        );
        client.initialize().unwrap();
        assert!(client.supports_prompts());
        assert_eq!(client.refresh_prompts().unwrap(), 1);
        let missing = client.get_prompt("review", json!({})).unwrap_err();
        assert!(missing.contains("focus"));
        let output = client
            .get_prompt("review", json!({ "focus": "security" }))
            .unwrap();
        assert!(output.contains("Review request"));
        assert!(output.contains("user:\nCheck safety"));
        assert!(output.contains("assistant:\nI will."));
        let requests = state.requests.lock().unwrap();
        assert!(requests.iter().any(|(method, params)| {
            method == "prompts/get" && params["arguments"]["focus"] == "security"
        }));
    }

    #[test]
    fn resources_list_and_read_are_bounded_and_workspace_scoped() {
        let state = FakeState::new(vec![json!({ "tools": [] })], json!({}));
        state.initialize_result.lock().unwrap()["capabilities"] =
            json!({ "tools": {}, "resources": { "listChanged": true } });
        *state.resource_pages.lock().unwrap() = vec![json!({
            "resources": [
                { "uri": "file:///workspace/readme.md", "name": "readme", "mimeType": "text/markdown" },
                { "uri": "file:///etc/passwd", "name": "outside" }
            ]
        })];
        *state.resource_result.lock().unwrap() = json!({
            "contents": [{
                "uri": "file:///workspace/readme.md",
                "mimeType": "text/markdown",
                "text": "hello"
            }]
        });
        let client = McpClient::new(
            "srv",
            Box::new(FakeTransport(state)),
            Duration::from_secs(5),
            Path::new("/workspace"),
        );
        client.initialize().unwrap();
        assert!(client.supports_resources());
        assert_eq!(client.refresh_resources().unwrap(), 2);
        assert!(client.list_resources_output().contains("readme.md"));
        let output = client.read_resource("file:///workspace/readme.md").unwrap();
        assert!(output.contains("hello"));
        let escape = client.read_resource("file:///etc/passwd").unwrap_err();
        assert!(escape.contains("escapes workspace root"), "{escape}");
        let unlisted = client
            .read_resource("file:///workspace/secret")
            .unwrap_err();
        assert!(unlisted.contains("resources/list"));
    }

    #[test]
    fn oversized_prompt_and_resource_text_is_rejected() {
        let huge = "x".repeat(MAX_MCP_CONTEXT_BYTES + 1);
        let prompt_error = render_prompt_result(&json!({
            "messages": [{ "role": "user", "content": { "type": "text", "text": huge } }]
        }))
        .unwrap_err();
        assert!(prompt_error.contains("byte limit"));
        let resource_error = render_resource_result(&json!({
            "contents": [{ "uri": "doc://large", "text": "x".repeat(MAX_MCP_CONTEXT_BYTES + 1) }]
        }))
        .unwrap_err();
        assert!(resource_error.contains("byte limit"));
    }

    #[test]
    fn list_changed_notifications_refresh_prompt_and_resource_caches() {
        let state = FakeState::new(vec![json!({ "tools": [] })], json!({}));
        state.initialize_result.lock().unwrap()["capabilities"] =
            json!({ "tools": {}, "prompts": {}, "resources": {} });
        *state.prompt_pages.lock().unwrap() =
            vec![json!({ "prompts": [{ "name": "new-prompt" }] })];
        *state.resource_pages.lock().unwrap() =
            vec![json!({ "resources": [{ "uri": "doc://new", "name": "new" }] })];
        let client = McpClient::new(
            "srv",
            Box::new(FakeTransport(state.clone())),
            Duration::from_secs(5),
            Path::new("/workspace"),
        );
        client.initialize().unwrap();
        state.prompts_changed.store(true, Ordering::SeqCst);
        state.resources_changed.store(true, Ordering::SeqCst);
        assert!(client.refresh_changed());
        assert_eq!(client.prompts_snapshot()[0].name, "new-prompt");
        assert_eq!(client.resources_snapshot()[0].uri, "doc://new");
        assert!(!client.refresh_changed());
    }
}
