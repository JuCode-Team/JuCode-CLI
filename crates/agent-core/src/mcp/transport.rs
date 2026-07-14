//! MCP transports: newline-delimited JSON-RPC over a child process (stdio) and
//! streamable HTTP (POST per message, JSON or SSE responses). No async — one
//! reader thread per stdio server, blocking reads for HTTP streams.

use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const CHILD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// One MCP server connection. Implementations must correlate concurrent
/// requests by id and surface `notifications/tools/list_changed`.
pub trait McpTransport: Send + Sync {
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String>;
    fn notify(&self, method: &str, params: Value) -> Result<(), String>;
    /// True once since the last call if the server signaled a tool-list change.
    fn take_tools_list_changed(&self) -> bool;
    /// Record the negotiated protocol version (sent as a header by HTTP).
    fn set_protocol_version(&self, _version: &str) {}
}

type PendingMap = Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>;
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// JSON-RPC 2.0 peer over any byte stream pair: monotonic request ids, a
/// reader thread routing responses to waiting callers, minimal handling of
/// server-initiated traffic (`ping` answered, other requests rejected,
/// `notifications/tools/list_changed` flagged).
pub struct JsonRpcPeer {
    writer: SharedWriter,
    pending: PendingMap,
    next_id: AtomicU64,
    tools_list_changed: Arc<AtomicBool>,
}

impl JsonRpcPeer {
    pub fn new(
        server: &str,
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> Self {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let tools_list_changed = Arc::new(AtomicBool::new(false));
        spawn_reader(
            server.to_string(),
            reader,
            Arc::clone(&writer),
            Arc::clone(&pending),
            Arc::clone(&tools_list_changed),
        );
        Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            tools_list_changed,
        }
    }

    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, tx);
        }
        let message = rpc_message(Some(id), method, params);
        if let Err(error) = write_json_line(&self.writer, &message) {
            self.forget(id);
            return Err(format!("failed to send {method}: {error}"));
        }
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.forget(id);
                Err(format!("{method} timed out after {}s", timeout.as_secs()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(format!("{method} failed: connection closed"))
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        write_json_line(&self.writer, &rpc_message(None, method, params))
            .map_err(|error| format!("failed to send {method}: {error}"))
    }

    pub fn take_tools_list_changed(&self) -> bool {
        self.tools_list_changed.swap(false, Ordering::SeqCst)
    }

    fn forget(&self, id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }
}

fn rpc_message(id: Option<u64>, method: &str, params: Value) -> Value {
    let mut message = Map::new();
    message.insert("jsonrpc".to_string(), json!("2.0"));
    if let Some(id) = id {
        message.insert("id".to_string(), json!(id));
    }
    message.insert("method".to_string(), json!(method));
    if !params.is_null() {
        message.insert("params".to_string(), params);
    }
    Value::Object(message)
}

fn write_json_line(writer: &Mutex<Box<dyn Write + Send>>, message: &Value) -> Result<(), String> {
    let mut writer = writer.lock().map_err(|_| "writer poisoned".to_string())?;
    writeln!(writer, "{message}").map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

fn spawn_reader(
    server: String,
    reader: impl Read + Send + 'static,
    writer: SharedWriter,
    pending: PendingMap,
    tools_list_changed: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut lines = BufReader::new(reader).lines();
        while let Some(Ok(line)) = lines.next() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                crate::log_warn!(
                    "mcp",
                    "discarding non-JSON line from server",
                    server = server.clone()
                );
                continue;
            };
            dispatch_incoming(&server, message, &writer, &pending, &tools_list_changed);
        }
        // EOF: unblock every waiting request by dropping its sender.
        if let Ok(mut pending) = pending.lock() {
            pending.clear();
        }
        crate::log_debug!("mcp", "reader thread finished", server = server);
    });
}

fn dispatch_incoming(
    server: &str,
    message: Value,
    writer: &Mutex<Box<dyn Write + Send>>,
    pending: &Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    tools_list_changed: &AtomicBool,
) {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        route_response(server, &message, pending);
        return;
    };
    match message.get("id").filter(|id| !id.is_null()) {
        Some(id) => {
            let _ = write_json_line(writer, &server_request_reply(server, method, id));
        }
        None => handle_notification(server, method, tools_list_changed),
    }
}

fn route_response(
    server: &str,
    message: &Value,
    pending: &Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
) {
    let Some(id) = message.get("id").and_then(Value::as_u64) else {
        crate::log_warn!("mcp", "response without usable id", server = server);
        return;
    };
    let sender = pending.lock().ok().and_then(|mut map| map.remove(&id));
    match sender {
        Some(sender) => {
            let _ = sender.send(response_result(message));
        }
        None => crate::log_warn!("mcp", "unmatched response id", server = server, id = id),
    }
}

/// Reply for a server-initiated request: `ping` gets an empty result, anything
/// else a method-not-found error (sampling, roots, elicitation unsupported).
pub(crate) fn server_request_reply(server: &str, method: &str, id: &Value) -> Value {
    if method == "ping" {
        return json!({ "jsonrpc": "2.0", "id": id, "result": {} });
    }
    crate::log_warn!(
        "mcp",
        "rejecting unsupported server request",
        server = server,
        method = method
    );
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": JSONRPC_METHOD_NOT_FOUND, "message": format!("method not supported: {method}") }
    })
}

pub(crate) fn handle_notification(server: &str, method: &str, tools_list_changed: &AtomicBool) {
    if method == "notifications/tools/list_changed" {
        tools_list_changed.store(true, Ordering::SeqCst);
    } else {
        crate::log_debug!(
            "mcp",
            "ignoring server notification",
            server = server,
            method = method
        );
    }
}

/// Extract the payload of a JSON-RPC response, mapping `error` to `Err`.
pub(crate) fn response_result(message: &Value) -> Result<Value, String> {
    if let Some(error) = message.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("server error {code}: {text}"));
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

/// stdio transport: a spawned child speaking newline-delimited JSON-RPC on
/// stdin/stdout. stderr is drained to debug logging. The child is killed on
/// drop and reaped with a short timeout.
pub struct StdioTransport {
    peer: JsonRpcPeer,
    child: Mutex<Child>,
    server: String,
}

impl StdioTransport {
    pub fn spawn(
        server: &str,
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<Self, String> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("failed to start MCP server {server}: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "server stdin is unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "server stdout is unavailable".to_string())?;
        if let Some(stderr) = child.stderr.take() {
            drain_stderr(server.to_string(), stderr);
        }
        Ok(Self {
            peer: JsonRpcPeer::new(server, stdout, stdin),
            child: Mutex::new(child),
            server: server.to_string(),
        })
    }
}

fn drain_stderr(server: String, stderr: impl Read + Send + 'static) {
    thread::spawn(move || {
        let mut lines = BufReader::new(stderr).lines();
        while let Some(Ok(line)) = lines.next() {
            crate::log_debug!("mcp", "server stderr", server = server.clone(), line = line);
        }
    });
}

impl McpTransport for StdioTransport {
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.peer.request(method, params, timeout)
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.peer.notify(method, params)
    }

    fn take_tools_list_changed(&self) -> bool {
        self.peer.take_tools_list_changed()
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        let Ok(mut child) = self.child.lock() else {
            return;
        };
        let _ = child.kill();
        let deadline = Instant::now() + CHILD_SHUTDOWN_TIMEOUT;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(_) => return,
            }
        }
        crate::log_warn!(
            "mcp",
            "server did not exit after kill",
            server = self.server.clone()
        );
    }
}

/// Streamable HTTP transport: every JSON-RPC message is POSTed to the server
/// URL; the response is either a single JSON body or an SSE stream read until
/// the matching response event. The standalone GET listening stream
/// (server-push) is intentionally unsupported.
pub struct HttpTransport {
    server: String,
    url: String,
    headers: BTreeMap<String, String>,
    agent: ureq::Agent,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    next_id: AtomicU64,
    tools_list_changed: AtomicBool,
}

impl HttpTransport {
    pub fn new(server: &str, url: &str, headers: &BTreeMap<String, String>) -> Self {
        Self {
            server: server.to_string(),
            url: url.to_string(),
            headers: headers.clone(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .build(),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            next_id: AtomicU64::new(1),
            tools_list_changed: AtomicBool::new(false),
        }
    }

    fn post(&self, body: &Value, timeout: Duration) -> Result<ureq::Response, String> {
        let mut request = self
            .agent
            .post(&self.url)
            .timeout(timeout)
            .set("content-type", "application/json")
            .set("accept", "application/json, text/event-stream");
        for (name, value) in &self.headers {
            request = request.set(name, value);
        }
        if let Some(session_id) = self.session_id.lock().ok().and_then(|id| id.clone()) {
            request = request.set("mcp-session-id", &session_id);
        }
        if let Some(version) = self.protocol_version.lock().ok().and_then(|v| v.clone()) {
            request = request.set("mcp-protocol-version", &version);
        }
        let response = match request.send_string(&body.to_string()) {
            Ok(response) => response,
            Err(ureq::Error::Status(code, response)) => {
                let body = response.into_string().unwrap_or_default();
                let snippet: String = body.chars().take(200).collect();
                return Err(format!("HTTP {code}: {snippet}"));
            }
            Err(error) => return Err(format!("HTTP request failed: {error}")),
        };
        if let Some(session_id) = response.header("mcp-session-id") {
            if let Ok(mut slot) = self.session_id.lock() {
                *slot = Some(session_id.to_string());
            }
        }
        Ok(response)
    }

    /// Best-effort reply to a server request received on an SSE stream.
    fn post_reply(&self, reply: &Value) {
        if let Err(error) = self.post(reply, Duration::from_secs(10)) {
            crate::log_debug!(
                "mcp",
                "failed to answer server request",
                server = self.server.clone(),
                error = error
            );
        }
    }

    fn handle_stream_message(&self, message: &Value) {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            crate::log_warn!(
                "mcp",
                "unmatched response on event stream",
                server = self.server.clone()
            );
            return;
        };
        match message.get("id").filter(|id| !id.is_null()) {
            Some(id) => self.post_reply(&server_request_reply(&self.server, method, id)),
            None => handle_notification(&self.server, method, &self.tools_list_changed),
        }
    }
}

impl McpTransport for HttpTransport {
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = rpc_message(Some(id), method, params);
        let response = self.post(&body, timeout)?;
        let content_type = response
            .header("content-type")
            .unwrap_or_default()
            .to_string();
        if content_type.contains("text/event-stream") {
            let reader = BufReader::new(response.into_reader());
            let message = read_sse_until(reader, |message| {
                if message.get("method").is_none()
                    && message.get("id").and_then(Value::as_u64) == Some(id)
                {
                    return true;
                }
                self.handle_stream_message(message);
                false
            })?;
            return response_result(&message);
        }
        if content_type.contains("application/json") {
            let body = response.into_string().map_err(|error| error.to_string())?;
            let message =
                serde_json::from_str::<Value>(&body).map_err(|error| error.to_string())?;
            return response_result(&message);
        }
        Err(format!(
            "{method}: server sent no usable response (content-type '{content_type}')"
        ))
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        // Notifications expect 202 Accepted (or any 2xx); the body is ignored.
        self.post(&rpc_message(None, method, params), Duration::from_secs(30))
            .map(|_| ())
    }

    fn take_tools_list_changed(&self) -> bool {
        self.tools_list_changed.swap(false, Ordering::SeqCst)
    }

    fn set_protocol_version(&self, version: &str) {
        if let Ok(mut slot) = self.protocol_version.lock() {
            *slot = Some(version.to_string());
        }
    }
}

/// Read SSE events (concatenated `data:` lines per event) until `found`
/// accepts one; returns that event's JSON payload.
fn read_sse_until(
    reader: impl BufRead,
    mut found: impl FnMut(&Value) -> bool,
) -> Result<Value, String> {
    let mut data: Vec<String> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|error| format!("event stream read failed: {error}"))?;
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            continue;
        }
        if !line.is_empty() {
            continue; // event:/id:/retry:/comment lines are irrelevant here
        }
        if data.is_empty() {
            continue;
        }
        let payload = data.join("\n");
        data.clear();
        let Ok(message) = serde_json::from_str::<Value>(&payload) else {
            crate::log_warn!("mcp", "discarding non-JSON SSE event");
            continue;
        };
        if found(&message) {
            return Ok(message);
        }
    }
    Err("event stream ended before the response arrived".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// In-memory byte pipe: `ChannelWriter` feeds `ChannelReader` over mpsc.
    struct ChannelReader {
        rx: mpsc::Receiver<Vec<u8>>,
        buffer: Vec<u8>,
        pos: usize,
    }

    struct ChannelWriter {
        tx: mpsc::Sender<Vec<u8>>,
    }

    impl Read for ChannelReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.buffer.len() {
                match self.rx.recv() {
                    Ok(bytes) => {
                        self.buffer = bytes;
                        self.pos = 0;
                    }
                    Err(_) => return Ok(0),
                }
            }
            let n = out.len().min(self.buffer.len() - self.pos);
            out[..n].copy_from_slice(&self.buffer[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl Write for ChannelWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.tx
                .send(bytes.to_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "closed"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A peer plus the fake server's view of both stream ends.
    fn peer_pair() -> (JsonRpcPeer, BufReader<ChannelReader>, ChannelWriter) {
        let (to_server_tx, to_server_rx) = mpsc::channel();
        let (to_client_tx, to_client_rx) = mpsc::channel();
        let peer = JsonRpcPeer::new(
            "test",
            ChannelReader {
                rx: to_client_rx,
                buffer: Vec::new(),
                pos: 0,
            },
            ChannelWriter { tx: to_server_tx },
        );
        let server_reader = BufReader::new(ChannelReader {
            rx: to_server_rx,
            buffer: Vec::new(),
            pos: 0,
        });
        (peer, server_reader, ChannelWriter { tx: to_client_tx })
    }

    fn read_message(reader: &mut BufReader<ChannelReader>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn responses_are_matched_by_id_even_out_of_order() {
        let (peer, mut server_reader, mut server_writer) = peer_pair();
        let peer = Arc::new(peer);
        let first = {
            let peer = Arc::clone(&peer);
            thread::spawn(move || peer.request("alpha", json!({}), Duration::from_secs(5)))
        };
        let request_1 = read_message(&mut server_reader);
        let second = {
            let peer = Arc::clone(&peer);
            thread::spawn(move || peer.request("beta", json!({}), Duration::from_secs(5)))
        };
        let request_2 = read_message(&mut server_reader);
        assert_eq!(request_1["method"], "alpha");
        assert_eq!(request_2["method"], "beta");

        // Answer in reverse order; each caller must still get its own result.
        writeln!(
            server_writer,
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"which":"beta"}}}}"#,
            request_2["id"]
        )
        .unwrap();
        writeln!(
            server_writer,
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"which":"alpha"}}}}"#,
            request_1["id"]
        )
        .unwrap();
        assert_eq!(first.join().unwrap().unwrap()["which"], "alpha");
        assert_eq!(second.join().unwrap().unwrap()["which"], "beta");
    }

    #[test]
    fn error_responses_become_err_results() {
        let (peer, mut server_reader, mut server_writer) = peer_pair();
        let handle = thread::spawn(move || {
            let request = read_message(&mut server_reader);
            writeln!(
                server_writer,
                r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":-32000,"message":"boom"}}}}"#,
                request["id"]
            )
            .unwrap();
        });
        let error = peer
            .request("explode", json!({}), Duration::from_secs(5))
            .unwrap_err();
        assert!(error.contains("-32000"), "{error}");
        assert!(error.contains("boom"), "{error}");
        handle.join().unwrap();
    }

    #[test]
    fn server_ping_gets_empty_result() {
        let (_peer, mut server_reader, mut server_writer) = peer_pair();
        writeln!(
            server_writer,
            r#"{{"jsonrpc":"2.0","id":"srv-1","method":"ping"}}"#
        )
        .unwrap();
        let reply = read_message(&mut server_reader);
        assert_eq!(reply["id"], "srv-1");
        assert_eq!(reply["result"], json!({}));
    }

    #[test]
    fn unknown_server_request_is_rejected_with_method_not_found() {
        let (_peer, mut server_reader, mut server_writer) = peer_pair();
        writeln!(
            server_writer,
            r#"{{"jsonrpc":"2.0","id":7,"method":"sampling/createMessage","params":{{}}}}"#
        )
        .unwrap();
        let reply = read_message(&mut server_reader);
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["error"]["code"], JSONRPC_METHOD_NOT_FOUND);
    }

    #[test]
    fn tools_list_changed_notification_sets_flag_once() {
        let (peer, _server_reader, mut server_writer) = peer_pair();
        assert!(!peer.take_tools_list_changed());
        writeln!(
            server_writer,
            r#"{{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}}"#
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !peer.take_tools_list_changed() {
            assert!(Instant::now() < deadline, "flag never set");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!peer.take_tools_list_changed());
    }

    #[test]
    fn request_times_out_without_response() {
        let (peer, _server_reader, _server_writer) = peer_pair();
        let error = peer
            .request("slow", json!({}), Duration::from_millis(50))
            .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
    }

    #[test]
    fn closed_stream_fails_pending_request() {
        let (peer, mut server_reader, server_writer) = peer_pair();
        let peer = Arc::new(peer);
        let pending = {
            let peer = Arc::clone(&peer);
            thread::spawn(move || peer.request("orphan", json!({}), Duration::from_secs(5)))
        };
        // The request is registered once the server sees it; EOF must then
        // unblock the waiting caller instead of leaving it to time out.
        let request = read_message(&mut server_reader);
        assert_eq!(request["method"], "orphan");
        drop(server_writer);
        let error = pending.join().unwrap().unwrap_err();
        assert!(error.contains("connection closed"), "{error}");
    }

    #[test]
    fn notifications_omit_id_and_null_params() {
        let (peer, mut server_reader, _server_writer) = peer_pair();
        peer.notify("notifications/initialized", Value::Null)
            .unwrap();
        let message = read_message(&mut server_reader);
        assert_eq!(message["method"], "notifications/initialized");
        assert!(message.get("id").is_none());
        assert!(message.get("params").is_none());
    }

    #[test]
    fn sse_reader_joins_multiline_data_and_skips_other_fields() {
        let stream = "event: message\nid: 3\ndata: {\"jsonrpc\":\"2.0\",\n\
                      data: \"id\":1,\"result\":{\"ok\":true}}\n\n";
        let message = read_sse_until(stream.as_bytes(), |value| {
            value.get("id").and_then(Value::as_u64) == Some(1)
        })
        .unwrap();
        assert_eq!(message["result"]["ok"], true);
    }

    #[test]
    fn sse_reader_errors_when_stream_ends_without_match() {
        let stream = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n";
        let error = read_sse_until(stream.as_bytes(), |_| false).unwrap_err();
        assert!(error.contains("ended before"), "{error}");
    }

    #[test]
    fn http_transport_captures_session_id_and_reads_sse_responses() {
        use std::net::{TcpListener, TcpStream};

        fn accept_and_read(listener: &TcpListener) -> (TcpStream, HashMap<String, String>, Value) {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut headers = HashMap::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
                }
            }
            let length: usize = headers.get("content-length").unwrap().parse().unwrap();
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            (stream, headers, serde_json::from_slice(&body).unwrap())
        }

        fn respond(stream: &mut TcpStream, content_type: &str, extra: &str, body: &str) {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            // First request: plain JSON response carrying a session id.
            let (mut stream, headers, request) = accept_and_read(&listener);
            assert!(!headers.contains_key("mcp-session-id"));
            assert_eq!(
                headers.get("authorization").map(String::as_str),
                Some("Bearer secret")
            );
            assert!(headers.get("accept").unwrap().contains("text/event-stream"));
            let body =
                json!({ "jsonrpc": "2.0", "id": request["id"], "result": { "mode": "json" } })
                    .to_string();
            respond(
                &mut stream,
                "application/json",
                "mcp-session-id: sess-42\r\n",
                &body,
            );

            // Second request: the captured session id must come back; reply
            // over SSE with a notification before the matching response.
            let (mut stream, headers, request) = accept_and_read(&listener);
            assert_eq!(
                headers.get("mcp-session-id").map(String::as_str),
                Some("sess-42")
            );
            let response =
                json!({ "jsonrpc": "2.0", "id": request["id"], "result": { "mode": "sse" } });
            let body = format!(
                "data: {}\n\ndata: {}\n\n",
                json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }),
                response
            );
            respond(&mut stream, "text/event-stream", "", &body);
        });

        let headers = BTreeMap::from([("Authorization".to_string(), "Bearer secret".to_string())]);
        let transport = HttpTransport::new("srv", &format!("http://{addr}/mcp"), &headers);
        let first = transport
            .request("initialize", json!({}), Duration::from_secs(5))
            .unwrap();
        assert_eq!(first["mode"], "json");
        let second = transport
            .request("tools/list", json!({}), Duration::from_secs(5))
            .unwrap();
        assert_eq!(second["mode"], "sse");
        assert!(transport.take_tools_list_changed());
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stdio_transport_round_trips_against_a_shell_server() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!(
            "jucode-mcp-stdio-test-{}",
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
    *'"ping"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}' ;;
    *'"echo"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"echoed":true}}' ;;
  esac
done
"##,
        )
        .unwrap();
        let transport = StdioTransport::spawn(
            "shell",
            "/bin/sh",
            &[script.display().to_string()],
            &BTreeMap::new(),
            &dir,
        )
        .unwrap();
        let pong = transport
            .request("ping", json!({}), Duration::from_secs(5))
            .unwrap();
        assert_eq!(pong, json!({}));
        let echoed = transport
            .request("echo", json!({ "x": 1 }), Duration::from_secs(5))
            .unwrap();
        assert_eq!(echoed["echoed"], true);
        drop(transport); // must kill and reap the child
        let _ = fs::remove_dir_all(dir);
    }
}
