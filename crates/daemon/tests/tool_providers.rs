//! Real daemon + API-key engine traffic, using an isolated HOME and fake API.
#[path = "../../agent-core/tests/support/fake_model.rs"]
mod fake_model;

use jucode_daemon::Store;
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    thread,
    time::Duration,
};
use tungstenite::Message;

#[test]
fn tool_configuration_is_isolated_and_hosted_api_traffic_is_metered_once() {
    let _guard = fake_model::setup();
    let api = TcpListener::bind("127.0.0.1:0").unwrap();
    let api_url = format!("http://{}/v1", api.local_addr().unwrap());
    let upstream = thread::spawn(move || loop {
        let (mut stream, _) = api.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut headers = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            headers.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        // Ignore unrelated local dev-server discovery probes.
        if !headers.starts_with("POST /v1/chat/completions ") {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            continue;
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let events = [
            json!({"choices":[{"index":0,"delta":{"content":"proxy works"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":60},"completion_tokens":15}}),
        ];
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        for event in events {
            write!(stream, "data: {event}\n\n")
                .unwrap_or_else(|e| panic!("{e}; received head: {headers}"));
        }
        stream.write_all(b"data: [DONE]\n\n").unwrap();
        break headers;
    });
    let config_path =
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".jucode/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    config["base_url"] = json!(api_url);
    fs::write(&config_path, config.to_string()).unwrap();
    let state = fake_model::temp_dir("provider-daemon").join("daemon");
    let store = Store::open(state.clone()).unwrap();
    let token = store.token().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || jucode_daemon::serve(listener, store, None, "test", None));
    let (mut socket, _) = tungstenite::connect(format!("ws://{address}/?token={token}")).unwrap();
    if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_ref() {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
    }
    let read_until = |socket: &mut tungstenite::WebSocket<_>, id: u64| -> Value {
        loop {
            if let Message::Text(text) = socket.read().unwrap() {
                let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                if frame["id"] == id {
                    assert_ne!(frame["type"], "error", "{frame}");
                    return frame;
                }
            }
        }
    };
    for (tool, format) in [("claude", "anthropic"), ("codex", "responses")] {
        socket.send(Message::text(json!({"id":10,"op":"tool_provider_save","tool":tool,"provider":{"id":"custom","name":tool,"base_url":"https://example.com/v1","format":format,"model":"custom-model","key":format!("{tool}-secret")}}).to_string())).unwrap();
        let reply = read_until(&mut socket, 10);
        assert_eq!(reply["config"]["providers"][0]["name"], tool);
        assert!(!reply.to_string().contains("secret"));
    }
    let cwd = fake_model::temp_dir("provider-session");
    socket
        .send(Message::text(
            json!({"id":11,"op":"session_create","cwd":cwd}).to_string(),
        ))
        .unwrap();
    let session = read_until(&mut socket, 11)["session"]
        .as_str()
        .unwrap()
        .to_string();
    socket
        .send(Message::text(
            json!({"op":"user_message","session":session,"content":"hello"}).to_string(),
        ))
        .unwrap();
    loop {
        if let Message::Text(text) = socket.read().unwrap() {
            let frame: Value = serde_json::from_str(text.as_str()).unwrap();
            assert_ne!(frame["type"], "error", "{frame}");
            if frame["type"] == "status" && frame["message"] == "ready" {
                break;
            }
        }
    }
    let headers = upstream.join().unwrap().to_ascii_lowercase();
    assert!(
        headers.starts_with("post /v1/chat/completions "),
        "{headers}"
    );
    assert!(headers.contains("authorization: bearer test-key"));
    assert!(!headers.contains("jgw-"));
    let mut records = Vec::new();
    for _ in 0..100 {
        records = fs::read_to_string(state.join("usage.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect();
        if !records.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["channel"], "jucode:fake");
    assert_eq!(records[0]["channel_kind"], "local");
    assert_eq!(records[0]["input_tokens"], 100);
    assert_eq!(records[0]["cached_input_tokens"], 60);
    assert_eq!(records[0]["output_tokens"], 15);
    assert_eq!(records[0]["requests"], 1);
    assert_eq!(records[0]["cwd"], json!(cwd));
    assert!(!fs::read_to_string(state.join("usage.jsonl"))
        .unwrap()
        .contains("test-key"));
}
