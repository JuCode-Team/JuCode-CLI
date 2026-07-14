//! MCP protocol client: initialize handshake, tool listing (with pagination),
//! and tool calls with result-content parsing. Transport-agnostic.

use crate::mcp::transport::McpTransport;
use serde_json::{json, Value};
use std::{sync::Mutex, time::Duration};

/// Protocol version this client speaks. An older version offered back by the
/// server in the initialize result is accepted and recorded.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Safety cap on tools/list pagination to survive a broken cursor loop.
const MAX_TOOL_LIST_PAGES: usize = 64;

#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// The server's `annotations.readOnlyHint`. Untrusted metadata — it only
    /// ever loosens approval in auto-edit mode, never grants full trust.
    pub read_only_hint: bool,
}

pub struct McpClient {
    server: String,
    transport: Box<dyn McpTransport>,
    timeout: Duration,
    protocol_version: Mutex<String>,
    server_info: Mutex<String>,
    tools: Mutex<Vec<McpToolInfo>>,
}

impl McpClient {
    pub fn new(server: &str, transport: Box<dyn McpTransport>, timeout: Duration) -> Self {
        Self {
            server: server.to_string(),
            transport,
            timeout,
            protocol_version: Mutex::new(MCP_PROTOCOL_VERSION.to_string()),
            server_info: Mutex::new(String::new()),
            tools: Mutex::new(Vec::new()),
        }
    }

    /// `initialize` request/response followed by the `notifications/initialized`
    /// notification. Records the server's (possibly older) protocol version.
    pub fn initialize(&self) -> Result<(), String> {
        let params = json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
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
        if result
            .get("capabilities")
            .and_then(|caps| caps.get("tools"))
            .is_none()
        {
            crate::log_warn!(
                "mcp",
                "server does not advertise the tools capability",
                server = self.server.clone()
            );
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

    /// Refresh the tool cache if the server signaled a list change.
    pub fn maybe_refresh_tools(&self) {
        if !self.transport.take_tools_list_changed() {
            return;
        }
        if let Err(error) = self.refresh_tools() {
            crate::log_warn!(
                "mcp",
                "tool list refresh failed",
                server = self.server.clone(),
                error = error
            );
        }
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
        pub call_result: Mutex<Value>,
        pub requests: Mutex<Vec<(String, Value)>>,
        pub list_calls: Mutex<usize>,
        pub list_changed: AtomicBool,
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
                call_result: Mutex::new(call_result),
                requests: Mutex::new(Vec::new()),
                list_calls: Mutex::new(0),
                list_changed: AtomicBool::new(false),
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
    }

    /// A connected-and-listed client over a `FakeTransport`.
    pub(crate) fn fake_client(server: &str, tools: Value, call_result: Value) -> McpClient {
        let state = FakeState::new(vec![json!({ "tools": tools })], call_result);
        let client = McpClient::new(
            server,
            Box::new(FakeTransport(state)),
            Duration::from_secs(5),
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
        assert_eq!(requests[0].1["capabilities"], json!({ "tools": {} }));
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
}
