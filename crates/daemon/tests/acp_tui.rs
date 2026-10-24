//! A built-in ACP agent's conversation moves to the agent's own TUI and
//! back. Its own test binary: it puts a fake `omp` first on `PATH`.
#![cfg(unix)]

#[path = "../../agent-core/tests/support/fake_model.rs"]
mod fake_model;

use base64::{engine::general_purpose::STANDARD, Engine};
use fake_model::{setup, temp_dir};
use jucode_daemon::Store;
use serde_json::{json, Value};
use std::{
    fs,
    net::{TcpListener, TcpStream},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

struct Client {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl Client {
    fn send(&mut self, frame: Value) {
        self.socket.send(Message::text(frame.to_string())).unwrap();
    }

    fn until(&mut self, mut done: impl FnMut(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                    let finished = done(&frame);
                    seen.push(frame);
                    if finished {
                        return seen;
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("socket error {error}; frames: {seen:#?}"),
            }
        }
        panic!("timed out; frames: {seen:#?}");
    }

    fn request(&mut self, mut op: Value) -> Value {
        op["id"] = json!(1000);
        self.send(op);
        self.until(|frame| frame["id"] == 1000).pop().unwrap()
    }
}

/// Terminal output of `term` in `frame`, decoded.
fn term_text(frame: &Value, term: &str) -> String {
    if frame["type"] != "term_output" || frame["term"] != term {
        return String::new();
    }
    String::from_utf8_lossy(&STANDARD.decode(frame["data"].as_str().unwrap()).unwrap()).to_string()
}

#[test]
fn a_built_in_agents_conversation_moves_to_its_tui_and_back() {
    let _guard = setup();
    let support = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support");
    let bin = temp_dir("acp-tui-bin");
    std::os::unix::fs::symlink(support.join("fake_omp.sh"), bin.join("omp")).unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(bin).chain(std::env::split_paths(&path));
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
    let dir = temp_dir("acp-tui");
    std::env::set_var("FAKE_ACP", support.join("fake_acp.py"));
    std::env::set_var("FAKE_ACP_STATE", dir.join("agent-state.json"));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let store = Store::open(temp_dir("acp-tui-state").join("daemon")).unwrap();
    let token = store.token().unwrap();
    thread::spawn(move || jucode_daemon::serve(listener, store, "test", None));
    let (socket, _) = tungstenite::connect(format!("ws://{address}/?token={token}")).unwrap();
    if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
    }
    let mut desktop = Client { socket };

    let created = desktop.request(json!({ "op": "session_create", "cwd": dir, "engine": "acp",
        "options": { "agent": "omp", "approval_mode": "read-only" } }));
    let session = created["session"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    desktop.send(json!({ "op": "watch", "session": session }));
    desktop.send(json!({ "op": "user_message", "session": session, "content": "hi" }));
    desktop.until(|f| {
        f["session"] == session.as_str()
            && f["type"] == "assistant_delta"
            && f["delta"] == "ok: hi --approval-mode always-ask"
    });
    desktop.until(|f| {
        f["session"] == session.as_str() && f["type"] == "status" && f["message"] == "ready"
    });

    // The TUI resumes the agent's conversation in the session's mode.
    let opened = desktop
        .request(json!({ "op": "session_tui", "session": session, "cols": 100, "rows": 30 }));
    let term = opened["term"]
        .as_str()
        .unwrap_or_else(|| panic!("{opened}"))
        .to_string();
    let mut output = String::new();
    desktop.until(|f| {
        output += &term_text(f, &term);
        output.contains("TUI omp --approval-mode always-ask --resume agent-1")
    });
    desktop.send(
        json!({ "op": "term_input", "term": term, "data": STANDARD.encode("hello\nquit\n") }),
    );

    // Back on the agent, the turn typed in the TUI is in the conversation.
    let frames = desktop.until(|f| {
        f["session"] == session.as_str()
            && f["type"] == "transcript"
            && f["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|i| i["content"] == "hello"))
    });
    assert!(frames
        .iter()
        .any(|f| f["type"] == "surface" && f["surface"] == "gui"));
    let _ = fs::remove_dir_all(dir);
}
