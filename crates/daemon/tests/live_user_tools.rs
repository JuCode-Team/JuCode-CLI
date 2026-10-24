//! The daemon's own tools (`present_files`, `propose_schedule`, the session
//! tools) offered to the real Claude Code and Codex. Live: it spends model calls, so it only
//! runs when asked: `cargo test -p jucode-daemon --test live_user_tools -- --ignored`.

use jucode_daemon::Store;
use serde_json::{json, Value};
use std::{
    fs,
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

fn next_frame(socket: &mut Socket, deadline: Instant) -> Option<Value> {
    while Instant::now() < deadline {
        match socket.read() {
            Ok(Message::Text(text)) => return serde_json::from_str(&text).ok(),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => panic!("{e}"),
        }
    }
    None
}

/// The tool outputs a session's turn produced, by tool name, once the turn
/// is over and `last` was called.
fn run_turn(engine: &str, prompt: &str, last: &str) -> Vec<(String, String)> {
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("jucode-live-{engine}-{}-{run}", std::process::id()));
    let state = root.join("state").join("daemon");
    let dir = root.join("work");
    fs::create_dir_all(&dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let store = Store::open(state).unwrap();
    let token = store.token().unwrap();
    thread::spawn(move || jucode_daemon::serve(listener, store, "live", None));
    let (mut socket, _) = tungstenite::connect(format!("ws://{address}/?token={token}")).unwrap();
    if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(300);
    socket
        .send(Message::text(json!({ "op": "session_create", "id": 1, "cwd": dir, "engine": engine, "options": { "approval_mode": "full-auto" } }).to_string()))
        .unwrap();
    let mut session = String::new();
    let mut outputs = Vec::new();
    let mut sent = false;
    while let Some(frame) = next_frame(&mut socket, deadline) {
        if session.is_empty() && frame["id"] == 1 {
            session = frame["session"]
                .as_str()
                .expect("session created")
                .to_string();
            socket
                .send(Message::text(
                    json!({ "op": "watch", "session": session }).to_string(),
                ))
                .unwrap();
            socket
                .send(Message::text(
                    json!({ "op": "user_message", "session": session, "content": prompt })
                        .to_string(),
                ))
                .unwrap();
            sent = true;
            continue;
        }
        if !sent || frame["session"] != session.as_str() {
            continue;
        }
        match frame["type"].as_str().unwrap_or_default() {
            "tool_output" => outputs.push((
                frame["name"].as_str().unwrap_or_default().to_string(),
                frame["output"].as_str().unwrap_or_default().to_string(),
            )),
            "error" => panic!("{frame}"),
            "status" if frame["message"] == "ready" && outputs.iter().any(|(n, _)| n == last) => {
                break
            }
            _ => {}
        }
    }
    let _ = fs::remove_dir_all(&root);
    outputs
}

const PROMPT: &str = "Create a file report.html containing <h1>Hi</h1>. Then show it to me with the present_files tool. Then propose a scheduled task named Tests that runs `echo ok` every weekday at 09:00 with the propose_schedule tool. Do nothing else.";

fn check(outputs: &[(String, String)]) {
    let output = |name: &str| -> Value {
        let (_, body) = outputs
            .iter()
            .find(|(tool, _)| tool == name)
            .unwrap_or_else(|| panic!("no {name} call in {outputs:?}"));
        serde_json::from_str(body).unwrap_or_else(|_| panic!("{name}: {body}"))
    };
    let presented = output("present_files");
    assert!(
        presented["files"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("report.html"),
        "{presented}"
    );
    let proposed = output("propose_schedule");
    assert!(
        proposed["id"].as_str().unwrap().starts_with("prop-"),
        "{proposed}"
    );
    assert_eq!(proposed["repeat"], "weekdays", "{proposed}");
}

#[test]
#[ignore]
fn claude_code_calls_the_daemon_tools() {
    check(&run_turn("claude", PROMPT, "propose_schedule"));
}

#[test]
#[ignore]
fn codex_calls_the_daemon_tools() {
    check(&run_turn("codex", PROMPT, "propose_schedule"));
}

/// A session hands a task to a session on another engine and gets its reply.
fn check_handoff(parent: &str, child: &str) {
    let prompt = format!("Use start_session to start a {child} session with the prompt \"Reply with exactly the word PONG and nothing else.\" Then call wait_session on it until it is done, and tell me its reply. Do nothing else.");
    let outputs = run_turn(parent, &prompt, "wait_session");
    let started = outputs
        .iter()
        .find(|(n, _)| n == "start_session")
        .expect("start_session called");
    let started: Value =
        serde_json::from_str(&started.1).unwrap_or_else(|_| panic!("{}", started.1));
    assert_eq!(started["engine"], child, "{started}");
    let waited = outputs
        .iter()
        .rev()
        .find(|(n, _)| n == "wait_session")
        .unwrap();
    let waited: Value = serde_json::from_str(&waited.1).unwrap_or_else(|_| panic!("{}", waited.1));
    assert_eq!(waited["status"], "done", "{waited}");
    assert!(
        waited["reply"].as_str().unwrap().contains("PONG"),
        "{waited}"
    );
}

#[test]
#[ignore]
fn claude_code_hands_a_task_to_codex() {
    check_handoff("claude", "codex");
}

#[test]
#[ignore]
fn codex_hands_a_task_to_claude_code() {
    check_handoff("codex", "claude");
}

#[test]
#[ignore]
fn jucode_hands_a_task_to_codex() {
    check_handoff("jucode", "codex");
}
