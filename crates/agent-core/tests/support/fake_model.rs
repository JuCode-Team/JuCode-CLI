//! A temporary HOME and a scripted local chat-completions server, shared by
//! the agent-core and daemon integration tests. Nothing here reads or writes
//! the developer's `~/.jucode` or calls a real provider.

use serde_json::{json, Value};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    thread,
};

/// Tests share one HOME and one fake server; they run one at a time because
/// the engines share `~/.jucode/config.json`.
pub fn setup() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    static INIT: OnceLock<()> = OnceLock::new();
    let guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    INIT.get_or_init(|| {
        let home = temp_dir("home");
        env::set_var("HOME", &home);
        env::remove_var("USERPROFILE");
        env::set_var("JUCODE_FAKE_KEY", "test-key");
        let base_url = start_fake_model();
        let profile = home.join(".jucode");
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            profile.join("config.json"),
            json!({
                "provider": "fake",
                "protocol": "chat",
                "model": "fake-model",
                "base_url": base_url,
                "api_key_env": "JUCODE_FAKE_KEY",
                "retry_attempts": 0,
                "include_project_instructions": false,
            })
            .to_string(),
        )
        .unwrap();
    });
    guard
}

pub fn temp_dir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = env::temp_dir().join(format!(
        "jucode-hosted-{}-{}-{label}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A scripted model: a user message `RUN: <command>` answers with one bash
/// call; anything after a tool result or a deferred-action message answers
/// with plain text naming what it saw.
fn start_fake_model() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let body = read_request_body(&mut stream);
                let reply = script(&body);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{reply}"
                );
            });
        }
    });
    format!("http://{address}/v1")
}

fn read_request_body(stream: &mut std::net::TcpStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap_or(Value::Null)
}

fn script(request: &Value) -> String {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or(Value::Null);
    let text = match &last["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    if last["role"] == "user" {
        if let Some(call) = text.strip_prefix("CALL ") {
            let (name, arguments) = call.split_once(' ').unwrap_or((call, "{}"));
            return sse(&[
                json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }] }, "finish_reason": null }] }),
                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
            ]);
        }
        if text == "SYSTEM" {
            let system = messages
                .iter()
                .find(|message| message["role"] == "system")
                .and_then(|message| message["content"].as_str())
                .unwrap_or_default();
            let tail: String = system
                .chars()
                .rev()
                .take(2000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return sse(&[
                json!({ "choices": [{ "index": 0, "delta": { "content": tail }, "finish_reason": null }] }),
                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
            ]);
        }
        if let Some(command) = text.strip_prefix("RUN: ") {
            let arguments = json!({ "command": command }).to_string();
            return sse(&[
                json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": { "name": "bash", "arguments": arguments }
                }] }, "finish_reason": null }] }),
                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
            ]);
        }
    }
    let reply = if last["role"] == "tool" {
        format!("tool said: {text}")
    } else {
        format!("user said: {text}")
    };
    sse(&[
        json!({ "choices": [{ "index": 0, "delta": { "content": reply }, "finish_reason": null }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
    ])
}

fn sse(chunks: &[Value]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&format!("data: {chunk}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}
