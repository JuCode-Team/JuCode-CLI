//! The daemon end to end: real WebSocket clients against a daemon hosting
//! engines that talk to the scripted fake model.

#[path = "../../agent-core/tests/support/fake_model.rs"]
mod fake_model;

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

struct Daemon {
    address: String,
    token: String,
    state: PathBuf,
}

/// A daemon on a free loopback port with its own state directory. Daemons
/// from earlier tests keep running in this process, so sharing directories
/// would let their schedulers act on this test's data.
fn start_daemon() -> Daemon {
    start_daemon_on(temp_dir("daemon-state").join("daemon"))
}

/// A daemon on an existing directory, standing in for a restart.
fn start_daemon_on(state: PathBuf) -> Daemon {
    start_daemon_with(state, None)
}

fn start_daemon_with(state: PathBuf, relay: Option<String>) -> Daemon {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let store = Store::open(state.clone()).unwrap();
    let token = store.token().unwrap();
    let web = state.parent().map(|root| root.join("web"));
    thread::spawn(move || jucode_daemon::serve(listener, store, web, "test", relay));
    Daemon {
        address,
        token,
        state,
    }
}

struct Client {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl Client {
    fn connect(daemon: &Daemon) -> Self {
        Self::connect_with(daemon, &daemon.token)
    }

    fn connect_with(daemon: &Daemon, token: &str) -> Self {
        let (socket, _) =
            tungstenite::connect(format!("ws://{}/?token={token}", daemon.address)).unwrap();
        if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
            stream
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
        }
        let mut client = Self { socket };
        let hello = client.until(|frame| frame["type"] == "hello");
        assert_eq!(hello.last().unwrap()["protocol"], 2);
        client
    }

    fn send(&mut self, frame: Value) {
        self.socket.send(Message::text(frame.to_string())).unwrap();
    }

    /// Reads frames until `done` matches one; returns all frames read.
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

    fn create_session(&mut self, cwd: &PathBuf) -> String {
        self.send(json!({ "op": "session_create", "cwd": cwd, "id": 1 }));
        let frames = self.until(|frame| frame["type"] == "session_created");
        let session = frames.last().unwrap()["session"]
            .as_str()
            .unwrap()
            .to_string();
        self.send(json!({ "op": "set_approval_mode", "session": session, "mode": "manual" }));
        self.until(|frame| frame["type"] == "approval_mode" && frame["mode"] == "manual");
        session
    }
}

fn ready(session: &str) -> impl Fn(&Value) -> bool + '_ {
    move |frame| {
        frame["session"] == session && frame["type"] == "status" && frame["message"] == "ready"
    }
}

#[test]
fn a_wrong_token_is_refused() {
    let _guard = setup();
    let daemon = start_daemon();
    assert!(tungstenite::connect(format!("ws://{}/?token=nope", daemon.address)).is_err());
    assert!(tungstenite::connect(format!("ws://{}/", daemon.address)).is_err());
}

#[test]
fn an_unwatched_session_defers_and_keeps_running_after_the_client_leaves() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-unwatched");
    let mut client = Client::connect(&daemon);
    let session = client.create_session(&dir);

    client.send(json!({
        "op": "user_message",
        "session": session,
        "content": "RUN: printf ran > marker.txt",
    }));
    let frames = client.until(ready(&session));
    let deferred = frames
        .iter()
        .find(|frame| frame["type"] == "action_deferred")
        .expect("nobody watches, so the call is deferred")
        .clone();
    assert!(!dir.join("marker.txt").exists());
    drop(client);

    // A new client sees the session still hosted and the open action.
    let mut client = Client::connect(&daemon);
    client.send(json!({ "op": "session_list" }));
    let listed = client.until(|frame| frame["type"] == "sessions");
    let entry = listed.last().unwrap()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["session"] == session.as_str())
        .unwrap()
        .clone();
    assert_eq!(entry["open"], true);
    client.send(json!({ "op": "actions_list" }));
    let actions = client.until(|frame| frame["type"] == "actions");
    assert!(actions.last().unwrap()["actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action["id"] == deferred["id"]));

    client.send(json!({
        "op": "decide_action",
        "session": session,
        "action": deferred["id"],
        "decision": "allow",
    }));
    client.until(|frame| frame["type"] == "action_decided" && frame["id"] == deferred["id"]);
    client.until(ready(&session));
    assert_eq!(fs::read_to_string(dir.join("marker.txt")).unwrap(), "ran");
}

#[test]
fn a_watched_session_prompts_and_defers_when_its_watcher_leaves() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-watched");
    let mut observer = Client::connect(&daemon);
    let mut watcher = Client::connect(&daemon);
    let session = watcher.create_session(&dir);
    watcher.send(json!({ "op": "watch", "session": session }));
    watcher.until(|frame| frame["type"] == "attended" && frame["attended"] == true);

    watcher.send(json!({
        "op": "user_message",
        "session": session,
        "content": "RUN: printf later > marker.txt",
    }));
    watcher.until(|frame| frame["type"] == "approval_request");
    drop(watcher);

    // The prompt nobody can answer becomes a deferred action and the turn
    // finishes on its own.
    let frames = observer.until(ready(&session));
    let attended_off = frames
        .iter()
        .position(|frame| frame["type"] == "attended" && frame["attended"] == false)
        .expect("the last watcher leaving makes the session unattended");
    let deferred = frames
        .iter()
        .position(|frame| {
            frame["type"] == "action_deferred" && frame["session"] == session.as_str()
        })
        .expect("the waiting call is deferred");
    assert!(deferred < attended_off);
    assert!(!dir.join("marker.txt").exists());
}

#[test]
fn a_closed_session_reopens_with_its_open_actions() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-reopen");
    let mut client = Client::connect(&daemon);
    let session = client.create_session(&dir);
    client.send(json!({
        "op": "user_message",
        "session": session,
        "content": "RUN: printf reopened > marker.txt",
    }));
    let frames = client.until(ready(&session));
    let deferred = frames
        .iter()
        .find(|frame| frame["type"] == "action_deferred")
        .unwrap()
        .clone();
    client.send(json!({ "op": "session_close", "session": session }));
    client.until(|frame| frame["type"] == "session_closed");

    // A second daemon on a copy of the state stands in for a restart (the
    // first keeps running, and holds its state directory).
    let state = temp_dir("daemon-restarted").join("daemon");
    fs::create_dir_all(&state).unwrap();
    for entry in fs::read_dir(&daemon.state).unwrap().flatten() {
        if entry.path().is_file() {
            fs::copy(entry.path(), state.join(entry.file_name())).unwrap();
        }
    }
    let restarted = start_daemon_on(state);
    let mut client = Client::connect(&restarted);
    client.send(json!({ "op": "session_open", "session": session }));
    client.until(|frame| frame["type"] == "session_opened");
    client.send(json!({
        "op": "decide_action",
        "session": session,
        "action": deferred["id"],
        "decision": "allow",
    }));
    client.until(|frame| frame["type"] == "action_decided");
    client.until(ready(&session));
    assert_eq!(
        fs::read_to_string(dir.join("marker.txt")).unwrap(),
        "reopened"
    );
}

#[test]
fn watching_sends_that_client_a_snapshot_of_the_session() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-snapshot");
    let mut first = Client::connect(&daemon);
    let session = first.create_session(&dir);
    first.send(json!({ "op": "user_message", "session": session, "content": "hello there" }));
    first.until(ready(&session));

    // A client that attaches later gets the session identity and the
    // conversation so far, without asking the engine to start over.
    let mut late = Client::connect(&daemon);
    late.send(json!({ "op": "watch", "session": session }));
    let frames =
        late.until(|frame| frame["type"] == "transcript" && frame["session"] == session.as_str());
    let startup = frames
        .iter()
        .find(|frame| frame["type"] == "startup")
        .expect("snapshot starts with the startup state");
    assert_eq!(startup["session_id"], session.as_str());
    assert_eq!(startup["cwd"], dir.display().to_string());
    let transcript = frames.last().unwrap()["items"].as_array().unwrap().clone();
    assert!(transcript
        .iter()
        .any(|item| item["content"] == "hello there"));
}

#[test]
fn the_title_model_names_a_session_and_leaves_a_hand_set_title_alone() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-title");
    let mut client = Client::connect(&daemon);
    let session = client.create_session(&dir);
    let title_of = |frame: &Value| {
        frame["sessions"]
            .as_array()
            .and_then(|list| list.iter().find(|s| s["session"] == session.as_str()))
            .map(|s| s["title"].as_str().unwrap_or_default().to_string())
    };
    client.send(
        json!({ "op": "user_message", "session": session, "content": "fix the login redirect" }),
    );
    client.until(ready(&session));
    // The title model gets the project, the current title and the request
    // (the test model echoes what it was asked).
    client.until(|frame| {
        frame["type"] == "sessions"
            && title_of(frame).is_some_and(|title| title.starts_with("user said: Project:"))
    });

    client.send(json!({ "op": "session_meta", "session": session, "title": "my own title" }));
    client.until(|frame| {
        frame["type"] == "sessions" && title_of(frame).as_deref() == Some("my own title")
    });
    for message in ["and the logout", "and the signup"] {
        client.send(json!({ "op": "user_message", "session": session, "content": message }));
        client.until(ready(&session));
    }
    // Turn 3 was due for a new title; a hand-set one is kept.
    for _ in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let listed = request(&mut client, json!({ "op": "session_list" }));
        assert_eq!(title_of(&listed).as_deref(), Some("my own title"));
    }
}

#[test]
fn a_session_closed_before_its_first_message_reopens() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-empty");
    let mut client = Client::connect(&daemon);
    let session = client.create_session(&dir);
    client.send(json!({ "op": "session_close", "session": session }));
    client.until(|frame| frame["type"] == "session_closed");
    client.send(json!({ "op": "session_open", "session": session, "id": 7 }));
    let frames = client.until(|frame| frame["id"] == 7);
    assert_eq!(frames.last().unwrap()["type"], "session_opened");
}

#[test]
fn session_switching_commands_are_refused() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-switch");
    let mut client = Client::connect(&daemon);
    let session = client.create_session(&dir);
    client.send(json!({ "op": "command", "session": session, "input": "/new" }));
    let frames = client.until(|frame| frame["type"] == "error");
    assert!(frames.last().unwrap()["message"]
        .as_str()
        .unwrap()
        .contains("session_create"));
}

/// Text the model streamed in `frames` for `session`.
fn reply_text(frames: &[Value], session: &str) -> String {
    frames
        .iter()
        .filter(|frame| frame["session"] == session && frame["type"] == "assistant_delta")
        .filter_map(|frame| frame["delta"].as_str())
        .collect()
}

#[test]
fn an_action_can_be_decided_after_its_session_closed() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-decide-closed");
    let mut client = Client::connect(&daemon);
    let session = client.create_session(&dir);
    client.send(json!({
        "op": "user_message", "session": session, "content": "RUN: printf later > marker.txt",
    }));
    let frames = client.until(ready(&session));
    let action = frames
        .iter()
        .find(|frame| frame["type"] == "action_deferred")
        .expect("nobody watches, so the call is deferred")
        .clone();
    client.send(json!({ "op": "session_close", "session": session }));
    client.until(|frame| frame["type"] == "session_closed");

    client.send(json!({
        "op": "decide_action", "session": session, "action": action["id"], "decision": "allow",
    }));
    client.until(|frame| frame["type"] == "action_decided");
    client.until(ready(&session));
    assert_eq!(fs::read_to_string(dir.join("marker.txt")).unwrap(), "later");
}

fn http(daemon: &Daemon, request: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = TcpStream::connect(&daemon.address).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, response)
}

fn pair_request(code: &str) -> String {
    let body = json!({ "code": code, "name": "phone" }).to_string();
    format!(
        "POST /api/pair HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn the_remote_page_is_served_over_plain_http() {
    let _guard = setup();
    let daemon = start_daemon();
    let web = daemon.state.parent().unwrap().join("web");
    fs::create_dir_all(web.join("_app/immutable")).unwrap();
    fs::write(web.join("index.html"), "<html>remote</html>").unwrap();
    fs::write(web.join("_app/immutable/app.js"), "console.log(1)").unwrap();

    let (status, response) = http(&daemon, "GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(status, 302);
    assert!(response.contains("Location: /remote"));
    // A page route of the single-page app gets index.html.
    let (status, response) = http(&daemon, "GET /remote HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(status, 200);
    assert!(response.contains("text/html") && response.ends_with("<html>remote</html>"));
    let (status, response) = http(
        &daemon,
        "GET /_app/immutable/app.js HTTP/1.1\r\nHost: x\r\n\r\n",
    );
    assert_eq!(status, 200);
    assert!(response.contains("text/javascript") && response.contains("immutable"));
    let (status, _) = http(&daemon, "GET /../daemon/token HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(status, 404);
    let (status, _) = http(&daemon, "GET /missing.js HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(status, 404);
}

#[test]
fn a_device_pairs_with_a_code_from_the_desktop_until_revoked() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut desktop = Client::connect(&daemon);
    desktop.send(json!({ "op": "pair_start", "id": 1 }));
    let frames = desktop.until(|frame| frame["id"] == 1);
    let code = frames.last().unwrap()["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 8);

    let (status, _) = http(&daemon, &pair_request("WRONG123"));
    assert_eq!(status, 403);
    let (status, response) = http(&daemon, &pair_request(&code.to_lowercase()));
    assert_eq!(status, 200, "{response}");
    let body: Value = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let token = body["token"].as_str().unwrap().to_string();
    let device = body["device"].as_str().unwrap().to_string();
    // The code works once.
    let (status, _) = http(&daemon, &pair_request(&code));
    assert_eq!(status, 403);

    // The phone reaches sessions, but cannot pair more devices.
    let mut phone = Client::connect_with(&daemon, &token);
    phone.send(json!({ "op": "session_list", "id": 2 }));
    assert_eq!(
        phone.until(|frame| frame["id"] == 2).last().unwrap()["type"],
        "sessions"
    );
    phone.send(json!({ "op": "pair_start", "id": 3 }));
    assert_eq!(
        phone.until(|frame| frame["id"] == 3).last().unwrap()["type"],
        "error"
    );

    desktop.send(json!({ "op": "device_list", "id": 4 }));
    let frames = desktop.until(|frame| frame["id"] == 4);
    assert_eq!(frames.last().unwrap()["devices"][0]["name"], "phone");
    desktop.send(json!({ "op": "device_revoke", "device": device, "id": 5 }));
    desktop.until(|frame| frame["id"] == 5);

    // The open connection is dropped and the token no longer connects.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match phone.socket.read() {
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => break,
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
            Ok(_) => {}
        }
        assert!(Instant::now() < deadline, "revoked device still connected");
    }
    assert!(tungstenite::connect(format!("ws://{}/?token={token}", daemon.address)).is_err());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn an_unattended_escalation_becomes_a_pending_action() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    let dir = temp_dir("daemon-escalator");
    fs::create_dir_all(dir.join(".git")).unwrap();
    let session = client.create_session(&dir);
    // auto-edit: an escalation asks a person (no safety model involved).
    client.send(json!({ "op": "set_approval_mode", "session": session, "mode": "auto-edit" }));
    client.until(|frame| frame["type"] == "approval_mode" && frame["mode"] == "auto-edit");
    client.send(json!({
        "op": "user_message",
        "session": session,
        "content": r#"CALL bash {"command":"printf x > .git/config","escalate":true,"justification":"set up the repo"}"#,
    }));
    let frames = client.until(ready(&session));
    let deferred = frames
        .iter()
        .find(|frame| frame["type"] == "action_deferred")
        .expect("nobody watches, so the escalation waits on the desk");
    assert!(deferred["arguments"].as_str().unwrap().contains("escalate"));
    assert!(!dir.join(".git/config").exists());
}

#[test]
fn a_chat_session_runs_in_the_chats_directory_with_the_chat_prompt() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    client.send(json!({ "op": "session_create", "chat": true, "id": 1 }));
    let frames = client.until(|frame| frame["type"] == "session_created");
    let session = frames.last().unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();

    client.send(json!({ "op": "session_list" }));
    let listed = client.until(|frame| frame["type"] == "sessions");
    let entry = listed.last().unwrap()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["session"] == session.as_str())
        .unwrap()
        .clone();
    assert_eq!(entry["chat"], true);
    let cwd = PathBuf::from(entry["cwd"].as_str().unwrap());
    assert!(cwd.is_dir() && cwd.ends_with(".jucode/chats"));

    client.send(json!({ "op": "watch", "session": session }));
    client.send(json!({ "op": "user_message", "session": session, "content": "SYSTEM" }));
    let frames = client.until(ready(&session));
    let reply: String = frames
        .iter()
        .filter(|frame| frame["session"] == session.as_str())
        .filter_map(|frame| frame["delta"].as_str().or(frame["text"].as_str()))
        .collect();
    assert!(
        reply.contains(jucode_agent_core::chat::CHAT_TOOL_GUIDANCE),
        "{reply}"
    );
}

/// The relay's end of the host connection, in-process.
struct FakeRelay {
    host: WebSocket<TcpStream>,
}

impl FakeRelay {
    fn send(&mut self, kind: u8, stream: u32, payload: &[u8]) {
        let mut frame = vec![kind];
        frame.extend_from_slice(&stream.to_be_bytes());
        frame.extend_from_slice(payload);
        self.host.send(Message::binary(frame)).unwrap();
    }

    /// The next binary frame from the host: (kind, stream, payload).
    fn next(&mut self) -> (u8, u32, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match self.host.read() {
                Ok(Message::Binary(bytes)) => {
                    let stream = u32::from_be_bytes(bytes[1..5].try_into().unwrap());
                    return (bytes[0], stream, bytes[5..].to_vec());
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("host connection failed: {error}"),
            }
        }
        panic!("no frame from the host");
    }

    /// The next frame on `stream`, skipping other streams' traffic.
    fn next_on(&mut self, stream: u32) -> (u8, Vec<u8>) {
        loop {
            let (kind, from, payload) = self.next();
            if from == stream {
                return (kind, payload);
            }
        }
    }

    /// Opens a stream and runs the client's handshake; returns msg 2's
    /// payload and the client transport.
    fn connect(
        &mut self,
        stream: u32,
        client_key: &[u8],
        host_key: &[u8],
        hello: Value,
    ) -> (Value, jucode_daemon::noise::Transport) {
        use jucode_daemon::noise;
        self.send(1, stream, &[]);
        let mut initiator = noise::initiator(client_key, host_key).unwrap();
        let mut buffer = vec![0u8; noise::MAX_MESSAGE];
        let written = initiator
            .write_message(hello.to_string().as_bytes(), &mut buffer)
            .unwrap();
        self.send(2, stream, &buffer[..written]);
        let (kind, message) = self.next_on(stream);
        assert_eq!(kind, 2);
        let read = initiator.read_message(&message, &mut buffer).unwrap();
        let reply = serde_json::from_slice(&buffer[..read]).unwrap();
        (reply, noise::Transport::new(initiator).unwrap())
    }
}

#[test]
fn a_relay_client_pairs_gets_hello_and_is_dropped_on_revoke() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    use sha2::{Digest, Sha256};

    let _guard = setup();
    let relay = TcpListener::bind("127.0.0.1:0").unwrap();
    let root = temp_dir("relay-state");
    let daemon = start_daemon_with(
        root.join("daemon"),
        Some(format!("ws://{}/relay/v1", relay.local_addr().unwrap())),
    );
    let mut desktop = Client::connect(&daemon);
    desktop.send(json!({ "op": "relay_status", "id": 1 }));
    let status = desktop.until(|frame| frame["id"] == 1).pop().unwrap();
    assert_eq!(
        (status["enabled"].clone(), status["connected"].clone()),
        (json!(false), json!(false))
    );
    desktop.send(json!({ "op": "pair_link", "id": 2 }));
    assert_eq!(
        desktop.until(|frame| frame["id"] == 2).pop().unwrap()["type"],
        "error"
    );
    desktop.send(json!({ "op": "relay_set", "enabled": true, "id": 3 }));
    assert_eq!(
        desktop.until(|frame| frame["id"] == 3).pop().unwrap()["enabled"],
        true
    );
    desktop.send(json!({ "op": "pair_link", "id": 4 }));
    let reply = desktop.until(|frame| frame["id"] == 4).pop().unwrap();
    let link = reply["link"].as_str().unwrap();
    let pair = link.split_once("/remote#pair=").unwrap().1;
    let parts: Vec<&str> = pair.split('.').collect();
    let (host_id, host_key, code) = (
        parts[0],
        URL_SAFE_NO_PAD.decode(parts[1]).unwrap(),
        parts[2],
    );
    assert_eq!(code, reply["code"].as_str().unwrap());

    // The daemon connects and proves its identity.
    let (tcp, _) = relay.accept().unwrap();
    let mut path = String::new();
    // The error type is fixed by tungstenite's handshake callback.
    #[allow(clippy::result_large_err)]
    let record_path = |request: &tungstenite::handshake::server::Request,
                       response: tungstenite::handshake::server::Response| {
        path = request.uri().path().to_string();
        Ok(response)
    };
    let mut host = tungstenite::accept_hdr(tcp, record_path).unwrap();
    assert_eq!(path, "/relay/v1/host");
    let nonce = [7u8; 32];
    host.send(Message::text(
        json!({ "t": "challenge", "nonce": URL_SAFE_NO_PAD.encode(nonce) }).to_string(),
    ))
    .unwrap();
    let auth: Value = serde_json::from_str(host.read().unwrap().to_text().unwrap()).unwrap();
    let public: [u8; 32] = URL_SAFE_NO_PAD
        .decode(auth["pub"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let signature: [u8; 64] = URL_SAFE_NO_PAD
        .decode(auth["sig"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    VerifyingKey::from_bytes(&public)
        .unwrap()
        .verify(
            &[b"jucode-relay-v1:".as_slice(), &nonce].concat(),
            &Signature::from_bytes(&signature),
        )
        .unwrap();
    assert_eq!(
        URL_SAFE_NO_PAD.encode(&Sha256::digest(public)[..16]),
        host_id
    );
    host.send(Message::text(
        json!({ "t": "ready", "host": host_id }).to_string(),
    ))
    .unwrap();
    host.get_ref()
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut relay = FakeRelay { host };

    // A new phone pairs with the code and gets the usual greeting.
    let (phone_key, _) = jucode_daemon::noise::generate_keypair().unwrap();
    let (reply, mut phone) = relay.connect(
        1,
        &phone_key,
        &host_key,
        json!({ "name": "phone", "pair": code }),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["name"], "phone");
    let device = reply["device"].as_str().unwrap().to_string();
    let (kind, message) = relay.next_on(1);
    assert_eq!(kind, 2);
    let hello: Value = serde_json::from_slice(&phone.open(&message).unwrap().unwrap()).unwrap();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["protocol"], 2);

    // The same phone reconnects without a code; it may not manage devices.
    let (reply, mut again) = relay.connect(2, &phone_key, &host_key, json!({ "name": "phone" }));
    assert_eq!(reply["device"], device.as_str());
    for message in again
        .seal(
            json!({ "op": "device_list", "id": 9 })
                .to_string()
                .as_bytes(),
        )
        .unwrap()
    {
        relay.send(2, 2, &message);
    }
    loop {
        let (kind, message) = relay.next_on(2);
        assert_eq!(kind, 2);
        let frame: Value = serde_json::from_slice(&again.open(&message).unwrap().unwrap()).unwrap();
        if frame["id"] == 9 {
            assert_eq!(frame["type"], "error");
            break;
        }
    }

    // An unknown key without a code is refused and closed.
    let (stranger_key, _) = jucode_daemon::noise::generate_keypair().unwrap();
    let (reply, _) = relay.connect(3, &stranger_key, &host_key, json!({ "name": "x" }));
    assert_eq!(reply["ok"], false);
    assert_eq!(relay.next_on(3), (3, Vec::new()));

    desktop.send(json!({ "op": "relay_status", "id": 5 }));
    assert_eq!(
        desktop.until(|frame| frame["id"] == 5).pop().unwrap()["connected"],
        true
    );

    // Revoking the device closes both of its streams.
    desktop.send(json!({ "op": "device_revoke", "device": device, "id": 6 }));
    desktop.until(|frame| frame["id"] == 6);
    let mut closed = Vec::new();
    while closed.len() < 2 {
        let (kind, stream, _) = relay.next();
        if kind == 3 {
            closed.push(stream);
        }
    }
    closed.sort();
    assert_eq!(closed, [1, 2]);
}

fn git(dir: &PathBuf, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Sends `op` with a fresh id and returns the reply to it.
fn request(client: &mut Client, mut op: Value) -> Value {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    op["id"] = json!(id);
    client.send(op);
    client.until(|frame| frame["id"] == id).pop().unwrap()
}

#[test]
fn projects_are_shared_and_their_files_readable_but_nothing_else() {
    let _guard = setup();
    let daemon = start_daemon();
    let project = temp_dir("daemon-project");
    fs::create_dir_all(&project).unwrap();
    git(&project, &["init", "-q"]);
    fs::write(project.join(".gitignore"), "*.log\n").unwrap();
    fs::write(project.join("a.txt"), "hello\n").unwrap();
    fs::write(project.join("noise.log"), "x").unwrap();
    let outside = temp_dir("daemon-outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "no").unwrap();

    let mut desktop = Client::connect(&daemon);
    let mut phone = Client::connect(&daemon);
    let added = request(
        &mut desktop,
        json!({ "op": "project_add", "path": project, "workspace_name": "Mine" }),
    );
    assert_eq!(added["type"], "workspaces", "{added}");
    assert_eq!(added["rev"], 1);
    assert_eq!(added["workspaces"][0]["name"], "Mine");
    let real = project.canonicalize().unwrap();
    assert_eq!(added["workspaces"][0]["projects"][0]["path"], json!(real));
    // Everyone hears about it.
    phone.until(|frame| frame["type"] == "workspaces" && frame["rev"] == 1);

    let listing = request(&mut phone, json!({ "op": "fs_list", "path": project }));
    let names: Vec<&str> = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, [".gitignore", "a.txt"], "{listing}");

    let file = request(
        &mut phone,
        json!({ "op": "fs_read", "path": project.join("a.txt") }),
    );
    assert_eq!(file["text"], "hello\n");
    let refused = request(
        &mut phone,
        json!({ "op": "fs_read", "path": outside.join("secret.txt") }),
    );
    assert_eq!(refused["type"], "error");
    let escape = request(
        &mut phone,
        json!({ "op": "fs_read", "path": project.join("../").join(outside.file_name().unwrap()).join("secret.txt") }),
    );
    assert_eq!(escape["type"], "error");

    let status = request(&mut phone, json!({ "op": "git_status", "path": project }));
    assert_eq!(status["repo"], true);
    assert!(status["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file["path"] == "a.txt" && file["status"] == "??"));
    let diff = request(
        &mut phone,
        json!({ "op": "git_diff", "path": project, "file": "a.txt" }),
    );
    assert!(diff["diff"].as_str().unwrap().contains("+hello"), "{diff}");

    // A save based on an old list is refused; one on the current list wins.
    let stale = request(
        &mut desktop,
        json!({ "op": "workspaces_set", "rev": 0, "workspaces": [] }),
    );
    assert_eq!(stale["type"], "error");
    let workspace = added["workspaces"][0]["id"].as_str().unwrap().to_string();
    let project_id = added["workspaces"][0]["projects"][0]["id"].clone();
    let removed = request(
        &mut desktop,
        json!({ "op": "project_remove", "workspace": workspace, "project": project_id }),
    );
    assert_eq!(removed["workspaces"][0]["projects"], json!([]));
    assert!(
        project.join("a.txt").exists(),
        "removing a project keeps its files"
    );
}

#[test]
fn a_new_project_folder_is_made_under_home_and_credentials_stay_unreadable() {
    let _guard = setup();
    let daemon = start_daemon();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let parent = home.join(format!("code-{}", std::process::id()));
    fs::create_dir_all(&parent).unwrap();
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::write(home.join(".ssh/id_test"), "key").unwrap();
    let mut phone = Client::connect(&daemon);

    let dirs = request(
        &mut phone,
        json!({ "op": "fs_list", "path": home, "dirs_only": true }),
    );
    let names: Vec<&str> = dirs["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&parent.file_name().unwrap().to_str().unwrap()));
    assert!(!names.contains(&".ssh"), "{names:?}");
    let files = request(&mut phone, json!({ "op": "fs_list", "path": home }));
    assert_eq!(files["type"], "error", "only folders outside projects");

    let bad = request(
        &mut phone,
        json!({ "op": "project_create", "parent": parent, "name": "../x" }),
    );
    assert_eq!(bad["type"], "error");
    let made = request(
        &mut phone,
        json!({ "op": "project_create", "parent": parent, "name": "fresh", "git_init": true }),
    );
    assert_eq!(
        made["workspaces"][0]["projects"][0]["name"], "fresh",
        "{made}"
    );
    assert!(parent.join("fresh/.git").is_dir());

    // Even with the home directory as a project, credentials stay out.
    request(&mut phone, json!({ "op": "project_add", "path": home }));
    let key = request(
        &mut phone,
        json!({ "op": "fs_read", "path": home.join(".ssh/id_test") }),
    );
    assert!(
        key["message"].as_str().unwrap().contains("protected"),
        "{key}"
    );
}

#[test]
fn sessions_carry_titles_and_archive_state_and_history_reopens_unknown_ones() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-history");
    let mut client = Client::connect(&daemon);
    let session = client.create_session(&dir);

    client.send(
        json!({ "op": "session_meta", "session": session, "title": "Fix login", "archived": true }),
    );
    let frames = client.until(|frame| {
        frame["type"] == "sessions"
            && frame["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["session"] == session.as_str() && s["title"] == "Fix login")
    });
    let listed = frames.last().unwrap()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session"] == session.as_str())
        .cloned()
        .unwrap();
    assert_eq!(listed["archived"], true);

    let history = request(&mut client, json!({ "op": "session_history", "cwd": dir }));
    let entry = history["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session"] == session.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("{history}"));
    assert_eq!(entry["title"], "Fix login");
    assert_eq!(entry["open"], true);

    client.send(json!({ "op": "session_close", "session": session }));
    client.until(|frame| frame["type"] == "session_closed");
    // Another daemon never hosted it, but finds it saved in that directory.
    let other = start_daemon();
    let mut fresh = Client::connect(&other);
    let unknown = request(
        &mut fresh,
        json!({ "op": "session_open", "session": session }),
    );
    assert_eq!(unknown["type"], "error");
    let opened = request(
        &mut fresh,
        json!({ "op": "session_open", "session": session, "cwd": dir }),
    );
    assert_eq!(opened["type"], "session_opened", "{opened}");
}

/// Points the daemon's Claude Code engine at the fake CLI; returns the file
/// its starts are logged to.
fn fake_claude() -> PathBuf {
    let log = temp_dir("fake-claude").with_extension("log");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_claude.py");
    std::env::set_var("CLAUDE_BIN", script);
    std::env::set_var("FAKE_CLAUDE_LOG", &log);
    log
}

fn starts(log: &PathBuf) -> Vec<Vec<String>> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn claude_reply(frames: &[Value], session: &str) -> String {
    frames
        .iter()
        .filter(|f| f["session"] == session && f["type"] == "assistant_delta")
        .map(|f| f["delta"].as_str().unwrap().to_string())
        .collect()
}

fn is_ready(session: &str) -> impl Fn(&Value) -> bool + '_ {
    move |f| f["session"] == session && f["type"] == "status" && f["message"] == "ready"
}

/// A title from the first message "hello", or the one the title model
/// wrote after the first turn (the test model echoes its request).
fn titled(title: &Value) -> bool {
    title == "hello"
        || title
            .as_str()
            .is_some_and(|t| t.starts_with("user said: Project:"))
}

/// The end of a turn: `ready` after the reply (the first turn's init also
/// says ready).
fn turn_done(session: &str) -> impl FnMut(&Value) -> bool + '_ {
    let mut replied = false;
    move |f| {
        replied |= f["session"] == session && f["type"] == "assistant_delta";
        replied && is_ready(session)(f)
    }
}

/// Terminal output of `term` among `frames`, decoded.
fn term_text(frames: &[Value], term: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    frames
        .iter()
        .filter(|f| f["type"] == "term_output" && f["term"] == term)
        .map(|f| {
            String::from_utf8_lossy(&STANDARD.decode(f["data"].as_str().unwrap()).unwrap())
                .to_string()
        })
        .collect()
}

fn type_in(client: &mut Client, term: &str, text: &str) {
    use base64::{engine::general_purpose::STANDARD, Engine};
    client.send(json!({ "op": "term_input", "term": term, "data": STANDARD.encode(text) }));
}

#[test]
fn a_claude_conversation_moves_to_its_tui_and_back() {
    let _guard = setup();
    let log = fake_claude();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-claude-tui");
    fs::create_dir_all(&dir).unwrap();
    let mut desktop = Client::connect(&daemon);
    let created = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "claude", "options": { "approval_mode": "auto-edit" } }),
    );
    let session = created["session"].as_str().unwrap().to_string();
    desktop.send(json!({ "op": "watch", "session": session }));
    desktop.until(is_ready(&session));
    desktop.send(json!({ "op": "user_message", "session": session, "content": "hello" }));
    desktop.until(turn_done(&session));

    // The engine stops and Claude Code's TUI resumes the conversation.
    let opened = request(
        &mut desktop,
        json!({ "op": "session_tui", "session": session, "cols": 100, "rows": 30 }),
    );
    assert_eq!(opened["type"], "term_opened", "{opened}");
    let term = opened["term"].as_str().unwrap().to_string();
    let frames = desktop.until(|f| f["session"] == session.as_str() && f["type"] == "surface");
    assert_eq!(frames.last().unwrap()["surface"], "tui");
    let started = format!("TUI {session}");
    desktop.until(|f| term_text(std::slice::from_ref(f), &term).contains(&started));
    let args = &starts(&log)[1];
    assert!(
        args.windows(2).any(|w| w == ["--resume", session.as_str()]),
        "{args:?}"
    );
    assert!(!args.contains(&"--print".to_string()), "{args:?}");
    // The session stays listed and open; its GUI ops wait for the TUI.
    desktop.send(json!({ "op": "user_message", "session": session, "content": "too soon" }));
    desktop.until(|f| f["session"] == session.as_str() && f["type"] == "error");

    type_in(&mut desktop, &term, "from the tui\n");
    let frames =
        desktop.until(|f| term_text(std::slice::from_ref(f), &term).contains("tui: from the tui"));
    assert!(!frames.is_empty());
    type_in(&mut desktop, &term, "exit\n");

    // Back in the GUI: the engine resumed, with what the TUI added.
    let frames = desktop.until(|f| f["session"] == session.as_str() && f["type"] == "transcript");
    assert!(frames
        .iter()
        .any(|f| f["type"] == "surface" && f["surface"] == "gui"));
    let transcript = frames.last().unwrap();
    assert!(
        transcript["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["content"] == "tui: from the tui"),
        "{transcript}"
    );
    desktop.send(json!({ "op": "user_message", "session": session, "content": "again" }));
    let frames = desktop.until(turn_done(&session));
    assert_eq!(claude_reply(&frames, &session), "ok: again");
    let args = &starts(&log)[2];
    assert!(
        args.windows(2).any(|w| w == ["--resume", session.as_str()]),
        "{args:?}"
    );
    assert!(args.contains(&"--print".to_string()));
}

#[test]
fn a_new_claude_conversation_starts_in_its_tui() {
    let _guard = setup();
    let log = fake_claude();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-claude-tui-new");
    fs::create_dir_all(&dir).unwrap();
    let mut desktop = Client::connect(&daemon);
    let created = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "claude", "options": { "approval_mode": "auto-edit" } }),
    );
    let session = created["session"].as_str().unwrap().to_string();
    desktop.send(json!({ "op": "watch", "session": session }));
    desktop.until(is_ready(&session));
    // Nothing saved yet: the TUI starts the conversation under its id.
    let opened = request(
        &mut desktop,
        json!({ "op": "session_tui", "session": session, "cols": 100, "rows": 30 }),
    );
    let term = opened["term"].as_str().unwrap().to_string();
    desktop
        .until(|f| term_text(std::slice::from_ref(f), &term).contains(&format!("TUI {session}")));
    let args = &starts(&log)[1];
    assert!(
        args.windows(2)
            .any(|w| w == ["--session-id", session.as_str()]),
        "{args:?}"
    );
    // Quit without a word: the engine starts the conversation again.
    type_in(&mut desktop, &term, "exit\n");
    desktop.until(|f| {
        f["session"] == session.as_str() && f["type"] == "surface" && f["surface"] == "gui"
    });
    desktop.send(json!({ "op": "user_message", "session": session, "content": "hi" }));
    let frames = desktop.until(turn_done(&session));
    assert_eq!(claude_reply(&frames, &session), "ok: hi");
    let args = &starts(&log)[2];
    assert!(
        args.windows(2)
            .any(|w| w == ["--session-id", session.as_str()]),
        "{args:?}"
    );
}

#[test]
fn a_running_turn_moves_to_the_tui_only_when_forced() {
    let _guard = setup();
    let _log = fake_claude();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-claude-tui-force");
    fs::create_dir_all(&dir).unwrap();
    let mut desktop = Client::connect(&daemon);
    let created = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "claude", "options": { "approval_mode": "manual" } }),
    );
    let session = created["session"].as_str().unwrap().to_string();
    desktop.send(json!({ "op": "watch", "session": session }));
    desktop.until(is_ready(&session));
    // The turn waits on an approval: it is still running.
    desktop.send(json!({ "op": "user_message", "session": session, "content": "use a tool" }));
    desktop.until(|f| f["session"] == session.as_str() && f["type"] == "approval_request");

    let refused = request(
        &mut desktop,
        json!({ "op": "session_tui", "session": session, "cols": 100, "rows": 30 }),
    );
    assert_eq!(refused["type"], "error", "{refused}");

    desktop.send(
        json!({ "op": "session_tui", "session": session, "cols": 100, "rows": 30, "force": true }),
    );
    // Every client sees the turn end, and the switch.
    let frames = desktop.until(|f| f["session"] == session.as_str() && f["type"] == "surface");
    assert!(frames.iter().any(|f| f["type"] == "term_opened"));
    assert!(frames
        .iter()
        .any(|f| f["type"] == "status" && f["message"] == "interrupted"));
    let surface = frames.last().unwrap();
    assert_eq!(surface["surface"], "tui");
    // Closing the TUI hands the conversation back, its engine resumed.
    desktop.send(json!({ "op": "term_close", "term": surface["term"] }));
    desktop.until(|f| {
        f["session"] == session.as_str() && f["type"] == "surface" && f["surface"] == "gui"
    });
}

/// A phone on a relay stream: sends ops and reads frames, Noise-sealed.
struct RelayPhone {
    stream: u32,
    transport: jucode_daemon::noise::Transport,
}

impl RelayPhone {
    fn send(&mut self, relay: &mut FakeRelay, frame: Value) {
        for message in self.transport.seal(frame.to_string().as_bytes()).unwrap() {
            relay.send(2, self.stream, &message);
        }
    }

    /// Reads frames until `done` matches one; returns all frames read.
    fn until(&mut self, relay: &mut FakeRelay, mut done: impl FnMut(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let (kind, message) = relay.next_on(self.stream);
            assert_eq!(kind, 2, "{seen:#?}");
            let Some(plain) = self.transport.open(&message).unwrap() else {
                continue;
            };
            let frame: Value = serde_json::from_slice(&plain).unwrap();
            let finished = done(&frame);
            seen.push(frame);
            if finished {
                return seen;
            }
        }
    }
}

/// A daemon whose relay is a fake one, with a desktop client and a phone
/// paired through the relay that got its `hello`.
fn relay_phone(label: &str) -> (Daemon, Client, FakeRelay, RelayPhone) {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    let relay = TcpListener::bind("127.0.0.1:0").unwrap();
    let root = temp_dir(label);
    let daemon = start_daemon_with(
        root.join("daemon"),
        Some(format!("ws://{}/relay/v1", relay.local_addr().unwrap())),
    );
    let mut desktop = Client::connect(&daemon);
    request(&mut desktop, json!({ "op": "relay_set", "enabled": true }));
    let reply = request(&mut desktop, json!({ "op": "pair_link" }));
    let pair = reply["link"]
        .as_str()
        .unwrap()
        .split_once("/remote#pair=")
        .unwrap()
        .1;
    let parts: Vec<&str> = pair.split('.').collect();
    let (host_key, code) = (URL_SAFE_NO_PAD.decode(parts[1]).unwrap(), parts[2]);
    // Other tests' daemons in this process may reach a reused port.
    let mut host = loop {
        let (tcp, _) = relay.accept().unwrap();
        if let Ok(host) = tungstenite::accept(tcp) {
            break host;
        }
    };
    host.send(Message::text(
        json!({ "t": "challenge", "nonce": URL_SAFE_NO_PAD.encode([7u8; 32]) }).to_string(),
    ))
    .unwrap();
    host.read().unwrap();
    host.send(Message::text(
        json!({ "t": "ready", "host": parts[0] }).to_string(),
    ))
    .unwrap();
    host.get_ref()
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut relay = FakeRelay { host };
    let (phone_key, _) = jucode_daemon::noise::generate_keypair().unwrap();
    let (reply, transport) = relay.connect(
        1,
        &phone_key,
        &host_key,
        json!({ "name": "phone", "pair": code }),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let mut phone = RelayPhone {
        stream: 1,
        transport,
    };
    phone.until(&mut relay, |f| f["type"] == "hello");
    (daemon, desktop, relay, phone)
}

#[test]
fn a_paired_phone_views_and_drives_a_session_tui_through_the_relay() {
    let _guard = setup();
    let _log = fake_claude();
    let (_daemon, mut desktop, mut relay, mut phone) = relay_phone("relay-tui-state");

    // The desktop moves a conversation to its TUI.
    let dir = temp_dir("daemon-relay-tui");
    let created = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "claude", "options": { "approval_mode": "auto-edit" } }),
    );
    let session = created["session"].as_str().unwrap().to_string();
    desktop.send(json!({ "op": "watch", "session": session }));
    desktop.until(is_ready(&session));
    desktop.send(json!({ "op": "user_message", "session": session, "content": "hello" }));
    desktop.until(turn_done(&session));
    let opened = request(
        &mut desktop,
        json!({ "op": "session_tui", "session": session, "cols": 100, "rows": 30 }),
    );
    let term = opened["term"].as_str().unwrap().to_string();
    let banner = format!("TUI {session}");
    desktop.until(|f| term_text(std::slice::from_ref(f), &term).contains(&banner));

    // The phone learns the terminal from its snapshot and attaches.
    phone.send(&mut relay, json!({ "op": "watch", "session": session }));
    let frames = phone.until(&mut relay, |f| {
        f["session"] == session.as_str() && f["type"] == "surface"
    });
    let surface = frames.last().unwrap();
    assert_eq!(
        (surface["surface"].as_str(), surface["term"].as_str()),
        (Some("tui"), Some(term.as_str()))
    );
    phone.send(
        &mut relay,
        json!({ "op": "term_attach", "term": term, "cols": 60, "rows": 20, "id": 5 }),
    );
    let attached = phone.until(&mut relay, |f| f["id"] == 5).pop().unwrap();
    assert_eq!(attached["type"], "term_attached", "{attached}");
    let replay = phone.until(&mut relay, |f| {
        term_text(std::slice::from_ref(f), &term).contains(&banner)
    });
    assert!(replay.iter().all(|f| f["replay"] == true), "{replay:#?}");

    // What the phone types reaches the TUI; both see its answer.
    use base64::{engine::general_purpose::STANDARD, Engine};
    phone.send(
        &mut relay,
        json!({ "op": "term_input", "term": term, "data": STANDARD.encode("from the phone\n") }),
    );
    let mut written = 0;
    phone.until(&mut relay, |f| {
        if f["type"] == "term_output" {
            written += STANDARD.decode(f["data"].as_str().unwrap()).unwrap().len();
        }
        term_text(std::slice::from_ref(f), &term).contains("tui: from the phone")
    });
    desktop.until(|f| term_text(std::slice::from_ref(f), &term).contains("tui: from the phone"));
    phone.send(
        &mut relay,
        json!({ "op": "term_ack", "term": term, "bytes": written }),
    );

    // The phone leaving only detaches it.
    relay.send(3, 1, &[]);
    type_in(&mut desktop, &term, "still here\n");
    desktop.until(|f| term_text(std::slice::from_ref(f), &term).contains("tui: still here"));
    desktop.send(json!({ "op": "term_close", "term": term }));
    desktop.until(|f| {
        f["session"] == session.as_str() && f["type"] == "surface" && f["surface"] == "gui"
    });
}

#[test]
fn a_paired_phone_opens_a_shell_that_dies_with_it() {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let _guard = setup();
    let (_daemon, mut desktop, mut relay, mut phone) = relay_phone("relay-shell-state");
    // Only known directories: a session's makes this one known.
    let dir = temp_dir("daemon-relay-shell");
    fs::create_dir_all(&dir).unwrap();
    let refused = request(&mut desktop, json!({ "op": "term_open", "cwd": dir }));
    assert_eq!(refused["type"], "error", "{refused}");
    desktop.create_session(&dir);

    phone.send(
        &mut relay,
        json!({ "op": "term_open", "cwd": dir, "cols": 80, "rows": 24, "id": 5 }),
    );
    let opened = phone.until(&mut relay, |f| f["id"] == 5).pop().unwrap();
    assert_eq!(opened["type"], "term_opened", "{opened}");
    let term = opened["term"].as_str().unwrap().to_string();
    // It is the phone's alone.
    let refused = request(&mut desktop, json!({ "op": "term_attach", "term": term }));
    assert_eq!(refused["message"], format!("unknown terminal {term}"));

    phone.send(
        &mut relay,
        json!({ "op": "term_input", "term": term, "data": STANDARD.encode("echo $$ > shell.pid; echo hi-$((40+2))\n") }),
    );
    let mut written = 0;
    phone.until(&mut relay, |f| {
        if f["type"] == "term_output" {
            written += STANDARD.decode(f["data"].as_str().unwrap()).unwrap().len();
        }
        term_text(std::slice::from_ref(f), &term).contains("hi-42")
    });
    phone.send(
        &mut relay,
        json!({ "op": "term_ack", "term": term, "bytes": written }),
    );
    let pid = fs::read_to_string(dir.join("shell.pid")).unwrap();

    // The phone leaving kills its shell.
    relay.send(3, 1, &[]);
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .status()
        .unwrap()
        .success()
    {
        assert!(Instant::now() < deadline, "shell {pid} still running");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_jucode_conversation_moves_to_its_tui_and_back() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-jucode-tui");
    fs::create_dir_all(&dir).unwrap();
    // The jucode TUI stands in as a script echoing what it is typed.
    let script = dir.join("tui.sh");
    fs::write(&script, "#!/bin/sh\necho \"TUI $*\"\nwhile read line; do echo \"got: $line\"; [ \"$line\" = exit ] && exit 0; done\n").unwrap();
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&script)
        .status()
        .unwrap();
    std::env::set_var("JUCODE_BIN", &script);
    let mut desktop = Client::connect(&daemon);
    let session = desktop.create_session(&dir);
    desktop.send(json!({ "op": "watch", "session": session }));
    let opened = request(
        &mut desktop,
        json!({ "op": "session_tui", "session": session }),
    );
    std::env::remove_var("JUCODE_BIN");
    let term = opened["term"]
        .as_str()
        .unwrap_or_else(|| panic!("{opened}"))
        .to_string();
    let resume = format!("TUI --resume {session}");
    desktop.until(|f| term_text(std::slice::from_ref(f), &term).contains(&resume));
    type_in(&mut desktop, &term, "exit\n");
    let frames = desktop.until(|f| f["session"] == session.as_str() && f["type"] == "transcript");
    assert!(frames
        .iter()
        .any(|f| f["type"] == "surface" && f["surface"] == "gui"));
    // The engine has the session again.
    desktop.send(json!({ "op": "user_message", "session": session, "content": "hi" }));
    desktop.until(ready(&session));
}

#[test]
fn a_claude_session_runs_turns_and_shares_approvals_with_every_client() {
    let _guard = setup();
    let log = fake_claude();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-claude");
    fs::create_dir_all(&dir).unwrap();
    let mut desktop = Client::connect(&daemon);
    let created = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "claude", "options": { "approval_mode": "auto-edit" } }),
    );
    let session = created["session"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    assert_eq!(
        session.len(),
        36,
        "a claude session id is its conversation uuid"
    );
    desktop.send(json!({ "op": "watch", "session": session }));
    desktop.until(is_ready(&session));
    let args = &starts(&log)[0];
    assert!(
        args.windows(2)
            .any(|w| w == ["--session-id", session.as_str()]),
        "{args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w == ["--permission-mode", "acceptEdits"]),
        "{args:?}"
    );

    desktop.send(json!({ "op": "user_message", "session": session, "content": "hello" }));
    let frames = desktop.until(turn_done(&session));
    assert!(frames
        .iter()
        .any(|f| f["type"] == "user_message" && f["content"] == "hello"));
    assert_eq!(claude_reply(&frames, &session), "ok: hello");

    // A turn waits on a permission prompt; a phone that starts watching sees
    // the conversation and the open prompt, and answers it.
    desktop.send(json!({ "op": "user_message", "session": session, "content": "use a tool" }));
    desktop.until(|f| f["session"] == session.as_str() && f["type"] == "approval_request");
    let mut phone = Client::connect(&daemon);
    phone.send(json!({ "op": "watch", "session": session }));
    let snapshot =
        phone.until(|f| f["session"] == session.as_str() && f["type"] == "approval_request");
    let transcript = snapshot.iter().find(|f| f["type"] == "transcript").unwrap();
    assert_eq!(
        transcript["items"][1],
        json!({ "role": "assistant", "content": "ok: hello" })
    );
    let call = snapshot.last().unwrap()["call_id"].clone();
    phone
        .send(json!({ "op": "approve", "session": session, "call_id": call, "decision": "allow" }));
    let frames = desktop.until(turn_done(&session));
    let output = frames.iter().find(|f| f["type"] == "tool_output").unwrap();
    assert!(
        output["output"]
            .as_str()
            .unwrap()
            .contains("\"stdout\":\"hi\""),
        "{output}"
    );

    let listed = request(&mut desktop, json!({ "op": "session_list" }));
    let entry = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session"] == session.as_str())
        .unwrap()
        .clone();
    assert_eq!(entry["engine"], "claude");
    // Titled after its first message, the same on every client (or already
    // by the title model once the first turn ended).
    assert!(titled(&entry["title"]), "{entry}");
    let history = request(&mut desktop, json!({ "op": "session_history", "cwd": dir }));
    assert!(
        history["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["session"] == session.as_str()
                && s["engine"] == "claude"
                && titled(&s["title"])),
        "{history}"
    );

    // Removed from the lists everywhere; the conversation stays on disk.
    desktop.send(json!({ "op": "session_meta", "session": session, "hidden": true }));
    desktop.until(|f| {
        f["type"] == "sessions"
            && !f["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["session"] == session.as_str())
    });
    let history = request(&mut desktop, json!({ "op": "session_history", "cwd": dir }));
    assert!(!history["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["session"] == session.as_str()));
}

#[test]
fn full_access_restarts_claude_on_the_same_conversation_and_reopening_resumes_it() {
    let _guard = setup();
    let log = fake_claude();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-claude-restart");
    fs::create_dir_all(&dir).unwrap();
    let mut client = Client::connect(&daemon);
    let created = request(
        &mut client,
        json!({ "op": "session_create", "cwd": dir, "engine": "claude" }),
    );
    let session = created["session"].as_str().unwrap().to_string();
    client.send(json!({ "op": "watch", "session": session }));
    client.until(is_ready(&session));
    // Closed before its first turn: nothing saved, so it starts again.
    client.send(json!({ "op": "session_close", "session": session }));
    client.until(|f| f["type"] == "session_closed" && f["session"] == session.as_str());
    request(
        &mut client,
        json!({ "op": "session_open", "session": session }),
    );
    let again = starts(&log).pop().unwrap();
    assert!(
        again
            .windows(2)
            .any(|w| w == ["--session-id", session.as_str()]),
        "{again:?}"
    );
    client.send(json!({ "op": "watch", "session": session }));
    client.until(is_ready(&session));

    client.send(json!({ "op": "user_message", "session": session, "content": "first" }));
    client.until(turn_done(&session));

    // A message right behind the switch runs in the new mode.
    client.send(json!({ "op": "set_approval_mode", "session": session, "mode": "full-access" }));
    client.send(json!({ "op": "user_message", "session": session, "content": "which mode" }));
    let frames = client.until(turn_done(&session));
    assert_eq!(claude_reply(&frames, &session), "mode: bypassPermissions");
    let restarted = starts(&log).pop().unwrap();
    assert!(
        restarted.contains(&"--dangerously-skip-permissions".to_string()),
        "{restarted:?}"
    );
    assert!(
        restarted
            .windows(2)
            .any(|w| w == ["--resume", session.as_str()]),
        "{restarted:?}"
    );

    client.send(json!({ "op": "session_close", "session": session }));
    client.until(|f| f["type"] == "session_closed" && f["session"] == session.as_str());
    let opened = request(
        &mut client,
        json!({ "op": "session_open", "session": session }),
    );
    assert_eq!(opened["type"], "session_opened", "{opened}");
    client.send(json!({ "op": "watch", "session": session }));
    let frames = client.until(|f| f["session"] == session.as_str() && f["type"] == "transcript");
    assert_eq!(
        frames.last().unwrap()["items"],
        json!([
            { "role": "user", "content": "first" },
            { "role": "assistant", "content": "ok: first" },
            { "role": "user", "content": "which mode" },
            { "role": "assistant", "content": "mode: bypassPermissions" },
        ])
    );
    assert!(starts(&log)
        .pop()
        .unwrap()
        .windows(2)
        .any(|w| w == ["--resume", session.as_str()]));
}

fn fake_codex() -> PathBuf {
    let log = temp_dir("fake-codex").with_extension("log");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_codex.py");
    std::env::set_var("CODEX_BIN", script);
    std::env::set_var("FAKE_CODEX_LOG", &log);
    log
}

#[test]
fn a_codex_session_is_named_by_its_thread_and_resumes_with_its_history() {
    let _guard = setup();
    fake_codex();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-codex");
    fs::create_dir_all(&dir).unwrap();
    let mut desktop = Client::connect(&daemon);
    let created = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "codex" }),
    );
    let session = created["session"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    assert!(
        session.starts_with("th-"),
        "the session is the codex thread: {session}"
    );
    desktop.send(json!({ "op": "watch", "session": session }));
    desktop.until(|f| f["session"] == session.as_str() && f["type"] == "attended");

    desktop.send(json!({ "op": "user_message", "session": session, "content": "hello" }));
    let frames = desktop.until(turn_done(&session));
    assert_eq!(claude_reply(&frames, &session), "ok: hello");

    desktop.send(json!({ "op": "user_message", "session": session, "content": "use a tool" }));
    let asked =
        desktop.until(|f| f["session"] == session.as_str() && f["type"] == "approval_request");
    let call = asked.last().unwrap()["call_id"].clone();
    desktop
        .send(json!({ "op": "approve", "session": session, "call_id": call, "decision": "allow" }));
    let frames = desktop.until(turn_done(&session));
    assert!(frames
        .iter()
        .any(|f| f["type"] == "tool_output" && f["is_error"] == false));

    let history = request(&mut desktop, json!({ "op": "session_history", "cwd": dir }));
    assert!(
        history["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["session"] == session.as_str()
                && s["engine"] == "codex"
                && titled(&s["title"])),
        "{history}"
    );

    desktop.send(json!({ "op": "session_close", "session": session }));
    desktop.until(|f| f["type"] == "session_closed" && f["session"] == session.as_str());
    request(
        &mut desktop,
        json!({ "op": "session_open", "session": session }),
    );
    desktop.send(json!({ "op": "watch", "session": session }));
    let frames = desktop.until(|f| f["session"] == session.as_str() && f["type"] == "transcript");
    let items = frames.last().unwrap()["items"].as_array().unwrap().clone();
    assert_eq!(items[0], json!({ "role": "user", "content": "hello" }));
    assert!(items
        .iter()
        .any(|i| i["role"] == "tool" && i["name"] == "bash"));
}

#[test]
fn the_desktop_saves_mcp_servers_for_every_session_without_one() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-mcp");
    let mut desktop = Client::connect(&daemon);
    let session = desktop.create_session(&dir);
    let server =
        json!({ "name": "probe", "transport": "stdio", "command": "false", "enabled": false });

    let saved = request(&mut desktop, json!({ "op": "mcp_set", "server": server }));
    assert_eq!(saved["type"], "mcp_saved", "{saved}");
    let config = fs::read_to_string(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap()
            .join(".jucode/config.json"),
    )
    .unwrap();
    assert!(config.contains("\"probe\""), "{config}");
    // The open session applies it too.
    desktop.until(|f| {
        f["session"] == session.as_str()
            && f["type"] == "mcp_servers"
            && f["servers"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["name"] == "probe"))
    });

    // A paired device may not name a command to run.
    desktop.send(json!({ "op": "pair_start", "id": 1 }));
    let code = desktop.until(|f| f["id"] == 1).pop().unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, response) = http(&daemon, &pair_request(&code));
    let body: Value = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let mut phone = Client::connect_with(&daemon, body["token"].as_str().unwrap());
    for op in [
        json!({ "op": "mcp_set", "server": server }),
        json!({ "op": "mcp_set", "session": session, "server": server }),
    ] {
        let refused = request(&mut phone, op);
        assert_eq!(refused["type"], "error", "{refused}");
    }

    let removed = request(&mut desktop, json!({ "op": "mcp_remove", "name": "probe" }));
    assert_eq!(removed["type"], "mcp_saved", "{removed}");
    let unknown = request(&mut desktop, json!({ "op": "mcp_remove", "name": "probe" }));
    assert_eq!(unknown["type"], "error");
}

#[test]
fn only_the_desktop_names_an_acp_command_and_the_agents_run_like_any_session() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-acp");
    fs::create_dir_all(&dir).unwrap();
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_acp.py");
    let state = dir.join("agent-state.json");
    let options = json!({ "command": script, "args": ["--flag"],
        "env": { "FAKE_ACP_GREETING": "hey", "FAKE_ACP_STATE": state } });

    // A paired device may not name a command to run.
    let mut desktop = Client::connect(&daemon);
    desktop.send(json!({ "op": "pair_start", "id": 1 }));
    let code = desktop.until(|f| f["id"] == 1).pop().unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, response) = http(&daemon, &pair_request(&code));
    let body: Value = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let mut phone = Client::connect_with(&daemon, body["token"].as_str().unwrap());
    let refused = request(
        &mut phone,
        json!({ "op": "session_create", "cwd": dir, "engine": "acp", "options": options }),
    );
    assert_eq!(
        refused["message"], "only the desktop can choose what an engine runs",
        "{refused}"
    );
    let refused = request(
        &mut phone,
        json!({ "op": "session_create", "cwd": dir, "engine": "acp", "options": { "agent": "omp", "args": ["x"] } }),
    );
    assert_eq!(
        refused["message"], "only the desktop can choose what an engine runs",
        "{refused}"
    );
    // A built-in agent, by id, is the daemon's to resolve: past the gate.
    let resolved = request(
        &mut phone,
        json!({ "op": "session_create", "cwd": dir, "engine": "acp", "options": { "agent": "deepseek" } }),
    );
    assert_eq!(
        resolved["message"], "unknown ACP agent deepseek",
        "{resolved}"
    );
    let named = request(
        &mut phone,
        json!({ "op": "session_create", "cwd": dir, "engine": "acp", "options": {} }),
    );
    assert_eq!(
        named["message"], "an ACP session needs options.agent or options.command",
        "{named}"
    );
    let refused = request(
        &mut phone,
        json!({ "op": "session_create", "cwd": dir, "engine": "claude", "options": { "bin": script } }),
    );
    assert_eq!(refused["type"], "error", "{refused}");
    // A built-in agent is named, never given a command.
    let both = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "acp", "options": { "agent": "omp", "command": script } }),
    );
    assert_eq!(
        both["message"], "options.agent and options.command are exclusive",
        "{both}"
    );
    let unknown = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "acp", "options": { "agent": "deepseek" } }),
    );
    assert_eq!(
        unknown["message"], "unknown ACP agent deepseek",
        "{unknown}"
    );
    let bad_env = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "acp", "options": { "command": script, "env": { "DYLD_INSERT_LIBRARIES": "x" } } }),
    );
    assert_eq!(bad_env["type"], "error");

    let created = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "acp", "options": options }),
    );
    let session = created["session"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    phone.send(json!({ "op": "watch", "session": session }));
    let watched = phone.until(|f| f["session"] == session.as_str() && f["type"] == "engine_caps");
    let caps = watched.last().unwrap();
    assert_eq!(caps["agent"]["name"], "fake");
    assert_eq!(caps["caps"]["transcriptReplay"], true);
    assert_eq!(caps["caps"]["modelPicker"], true);
    phone.send(json!({ "op": "user_message", "session": session, "content": "hi" }));
    let frames = phone.until(turn_done(&session));
    assert_eq!(claude_reply(&frames, &session), "ok: hi --flag hey");
    assert!(
        frames
            .iter()
            .any(|f| f["type"] == "context_usage" && f["tokens"] == 100),
        "{frames:#?}"
    );

    // The phone answers the agent's permission prompt.
    phone.send(json!({ "op": "user_message", "session": session, "content": "use a tool" }));
    let asked =
        phone.until(|f| f["session"] == session.as_str() && f["type"] == "approval_request");
    let call = asked.last().unwrap()["call_id"].clone();
    phone
        .send(json!({ "op": "approve", "session": session, "call_id": call, "decision": "allow" }));
    let frames = phone.until(turn_done(&session));
    assert!(
        frames
            .iter()
            .any(|f| f["type"] == "tool_output" && f["is_error"] == false),
        "{frames:#?}"
    );

    // Closed and reopened, the agent replays the conversation. A desktop
    // registered agent reopens only where its command is known.
    desktop.send(json!({ "op": "session_close", "session": session }));
    desktop.until(|f| f["type"] == "session_closed" && f["session"] == session.as_str());
    let refused = request(
        &mut phone,
        json!({ "op": "session_open", "session": session }),
    );
    assert_eq!(
        refused["message"], "this agent's session can only be reopened on the desktop",
        "{refused}"
    );
    // Naming a built-in agent does not turn it into one.
    let refused = request(
        &mut phone,
        json!({ "op": "session_open", "session": session, "engine": "acp", "options": { "agent": "omp" } }),
    );
    assert_eq!(
        refused["message"], "this agent's session can only be reopened on the desktop",
        "{refused}"
    );
    let opened = request(
        &mut desktop,
        json!({ "op": "session_open", "session": session, "engine": "acp", "options": options }),
    );
    assert_eq!(opened["type"], "session_opened", "{opened}");
    desktop.send(json!({ "op": "watch", "session": session }));
    let replayed = desktop.until(|f| {
        f["session"] == session.as_str()
            && f["type"] == "transcript"
            && f["items"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|i| i["role"] == "user" && i["content"] == "hi")
            })
    });
    let items = replayed.last().unwrap()["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(items[1]["content"], "ok: hi --flag hey");
    assert_eq!(items[2]["content"], "use a tool");
    desktop.send(json!({ "op": "user_message", "session": session, "content": "again" }));
    let frames = desktop.until(turn_done(&session));
    assert_eq!(claude_reply(&frames, &session), "ok: again --flag hey");
}

/// A one-route HTTP server standing in for the JuCode API: every request
/// gets `body` as JSON.
fn fake_jucode_api(body: Value) -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                line.clear();
            }
            let body = body.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{address}")
}

fn set_jucode_api_url(url: &str) {
    let path = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".jucode/config.json");
    let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    config["jucode_api_url"] = json!(url);
    fs::write(&path, config.to_string()).unwrap();
}

#[test]
fn the_desktop_lists_and_installs_skills_into_the_engines_directory() {
    let _guard = setup();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    set_jucode_api_url(&fake_jucode_api(json!({
        "skills": [{
            "id": "review",
            "name": "Review",
            "description": "Review code",
            "content": "Be strict.",
            "tags": ["code"]
        }],
        "default_skill_ids": ["review"]
    })));
    let daemon = start_daemon();
    let mut desktop = Client::connect(&daemon);
    let entry = |catalog: &Value, source: &str, id: &str| {
        catalog["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|skill| skill["source"] == source && skill["id"] == id)
            .unwrap_or_else(|| panic!("{source}:{id} in {catalog}"))
            .clone()
    };

    let catalog = request(
        &mut desktop,
        json!({ "op": "skills_catalog", "backend": "jucode" }),
    );
    assert_eq!(catalog["type"], "skills_catalog", "{catalog}");
    assert_eq!(catalog["warnings"], json!([]));
    let jucode_dir = home.join(".jucode/skills");
    assert_eq!(catalog["installDir"], json!(jucode_dir));
    let review = entry(&catalog, "jucode", "review");
    assert_eq!(
        (&review["isDefault"], &review["installed"], &review["tags"]),
        (&json!(true), &json!(false), &json!(["code"]))
    );
    let pdf = entry(&catalog, "anthropic", "pdf");
    assert_eq!(pdf["redistributable"], false);
    assert!(pdf["homepage"]
        .as_str()
        .unwrap()
        .starts_with("https://github.com/anthropics/skills/tree/"));
    let refused = request(
        &mut desktop,
        json!({ "op": "skill_install", "source": "anthropic", "skill": "pdf" }),
    );
    assert_eq!(refused["type"], "error", "{refused}");

    let installed = request(
        &mut desktop,
        json!({ "op": "skill_install", "source": "jucode", "skill": "review", "backend": "jucode" }),
    );
    assert_eq!(installed["type"], "skill_installed", "{installed}");
    assert_eq!(installed["path"], json!(jucode_dir.join("review")));
    assert!(fs::read_to_string(jucode_dir.join("review/SKILL.md"))
        .unwrap()
        .contains("Be strict."));
    let catalog = request(
        &mut desktop,
        json!({ "op": "skills_catalog", "backend": "jucode" }),
    );
    assert_eq!(entry(&catalog, "jucode", "review")["installed"], true);

    // Claude Code sessions read their own directory.
    let catalog = request(
        &mut desktop,
        json!({ "op": "skills_catalog", "backend": "claude" }),
    );
    assert_eq!(catalog["installDir"], json!(home.join(".claude/skills")));
    assert_eq!(entry(&catalog, "jucode", "review")["installed"], false);
    for op in [
        json!({ "op": "skill_install", "source": "elsewhere", "skill": "review" }),
        json!({ "op": "skill_install", "source": "jucode", "skill": "missing" }),
        json!({ "op": "skill_install", "source": "anthropic", "skill": "../escape" }),
    ] {
        assert_eq!(request(&mut desktop, op)["type"], "error");
    }

    // A paired device cannot browse or install.
    desktop.send(json!({ "op": "pair_start", "id": 1 }));
    let code = desktop.until(|f| f["id"] == 1).pop().unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, response) = http(&daemon, &pair_request(&code));
    let body: Value = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let mut phone = Client::connect_with(&daemon, body["token"].as_str().unwrap());
    for op in ["skills_catalog", "skill_install"] {
        let refused = request(
            &mut phone,
            json!({ "op": op, "source": "jucode", "skill": "review" }),
        );
        assert_eq!(refused["type"], "error", "{refused}");
    }

    // Without the JuCode marketplace the Anthropic catalog still lists.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    set_jucode_api_url(&format!("http://{}", closed.local_addr().unwrap()));
    drop(closed);
    let catalog = request(
        &mut desktop,
        json!({ "op": "skills_catalog", "backend": "jucode" }),
    );
    assert_eq!(
        catalog["warnings"].as_array().unwrap().len(),
        1,
        "{catalog}"
    );
    entry(&catalog, "anthropic", "frontend-design");
    assert!(catalog["skills"]
        .as_array()
        .unwrap()
        .iter()
        .all(|skill| skill["source"] == "anthropic"));
    let _ = fs::remove_dir_all(jucode_dir.join("review"));
}

/// Requirement `id` in a `requirements` frame.
fn requirement<'a>(frame: &'a Value, id: &str) -> Option<&'a Value> {
    (frame["type"] == "requirements")
        .then(|| frame["requirements"].as_array())
        .flatten()
        .and_then(|list| list.iter().find(|r| r["id"] == id))
}

#[test]
fn a_message_shows_its_images_to_every_client() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-images");
    fs::create_dir_all(&dir).unwrap();
    let mut client = Client::connect(&daemon);
    let shot = request(
        &mut client,
        json!({ "op": "upload", "name": "shot.png", "data": STANDARD.encode(b"png bytes"), "last": true }),
    );
    let shot = shot["path"].as_str().unwrap().to_string();
    let session = client.create_session(&dir);
    client.send(
        json!({ "op": "user_message", "session": session, "content": "look", "images": [shot] }),
    );
    let frames = client.until(|f| f["session"] == session && f["type"] == "user_message");
    assert_eq!(frames.last().unwrap()["images"], json!([shot]));
    client.until(ready(&session));

    // A client that comes later sees them in the conversation so far.
    let mut phone = Client::connect(&daemon);
    phone.send(json!({ "op": "watch", "session": session }));
    let frames = phone.until(|f| f["session"] == session && f["type"] == "transcript");
    let items = frames.last().unwrap()["items"].as_array().unwrap().clone();
    assert!(items
        .iter()
        .any(|i| i["role"] == "user" && i["images"] == json!([shot])));

    // The remote page reads the image itself; nothing outside is readable.
    let image = request(&mut phone, json!({ "op": "fs_image", "path": shot }));
    assert_eq!(
        image["data"],
        format!("data:image/png;base64,{}", STANDARD.encode(b"png bytes"))
    );
    let outside = request(
        &mut phone,
        json!({ "op": "fs_image", "path": daemon.state.join("token") }),
    );
    assert_eq!(outside["type"], "error");
}

#[test]
fn a_requirement_follows_its_sessions_and_continues_in_them() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-requirement");
    let mut client = Client::connect(&daemon);
    let png = format!("data:image/png;base64,{}", STANDARD.encode(b"png bytes"));
    let shot = request(
        &mut client,
        json!({ "op": "upload", "name": "shot.png", "data": STANDARD.encode(b"png bytes"), "last": true }),
    );
    let shot = shot["path"].as_str().unwrap().to_string();
    // Only an upload is taken in, nothing else on the computer.
    let outside = request(
        &mut client,
        json!({ "op": "requirement_create", "text": "steal", "images": [daemon.state.join("token")] }),
    );
    assert_eq!(outside["type"], "error");

    fs::create_dir_all(&dir).unwrap();
    let added = request(&mut client, json!({ "op": "project_add", "path": dir }));
    let project = added["workspaces"][0]["projects"][0]["id"].clone();
    let created = request(
        &mut client,
        json!({ "op": "requirement_create", "text": "export sessions as markdown", "project": project, "images": [shot] }),
    );
    let r = &created["requirement"];
    let id = r["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("R-"));
    assert_eq!(r["state"], "idea");
    assert_eq!(r["title"], "export sessions as markdown");
    assert_eq!(r["project"], project);
    let image = request(
        &mut client,
        json!({ "op": "requirement_image", "requirement": id, "index": 0 }),
    );
    assert_eq!(image["data"], png);

    // Started from the phone: a new session in its project, linked to it,
    // that first only explains what it understood and waits.
    let started = request(
        &mut client,
        json!({ "op": "requirement_reply", "requirement": id, "lang": "en" }),
    );
    let first = started["session"].as_str().unwrap().to_string();
    let frames = client.until(|f| requirement(f, &id).is_some_and(|r| r["status"] == "confirm"));
    let shown = requirement(frames.last().unwrap(), &id).unwrap();
    assert_eq!(shown["state"], "open");
    assert_eq!(shown["sessions"], json!([first]));
    assert_eq!(shown["session_states"][&first], "idle");
    assert_eq!(shown["gate"]["session"], first);
    assert_eq!(shown["gate"]["stage"], "understand");
    let reply = shown["last_reply"].as_str().unwrap();
    assert!(
        reply.ends_with("Then stop and wait for my confirmation."),
        "{reply}"
    );

    // Until confirmed it stays read-only, whatever a client asks for.
    client.send(json!({ "op": "watch", "session": first }));
    client.send(json!({ "op": "set_approval_mode", "session": first, "mode": "full-access" }));
    let frames = client.until(|f| {
        f["type"] == "approval_mode" && f["session"] == first.as_str() && f["mode"] != "auto"
    });
    assert_eq!(frames.last().unwrap()["mode"], "manual");

    // Confirmed, it goes to work in the chosen mode.
    let confirmed = request(
        &mut client,
        json!({ "op": "requirement_confirm", "requirement": id, "lang": "en" }),
    );
    assert_eq!(confirmed["stage"], "go", "{confirmed}");
    let frames = client.until(|f| requirement(f, &id).is_some_and(|r| r["status"] == "review"));
    let shown = requirement(frames.last().unwrap(), &id).unwrap();
    assert!(shown.get("gate").is_none());
    assert!(shown["last_reply"]
        .as_str()
        .unwrap()
        .contains("Now implement it."));

    // Feedback goes to the latest session; asked for, a new one starts.
    let again = request(
        &mut client,
        json!({ "op": "requirement_reply", "requirement": id, "text": "and the images" }),
    );
    assert_eq!(again["session"], first);
    client.until(|f| {
        requirement(f, &id).is_some_and(|r| {
            r["status"] == "review"
                && r["last_reply"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("and the images")
        })
    });
    let fresh = request(
        &mut client,
        json!({ "op": "requirement_reply", "requirement": id, "text": "start over", "new_session": true }),
    );
    let second = fresh["session"].as_str().unwrap().to_string();
    assert_ne!(second, first);
    let frames = client.until(|f| {
        requirement(f, &id)
            .is_some_and(|r| r["status"] == "confirm" && r["sessions"] == json!([first, second]))
    });
    assert!(
        requirement(frames.last().unwrap(), &id).unwrap()["last_reply"]
            .as_str()
            .unwrap()
            .contains("start over")
    );

    // A session works on one requirement at a time.
    let other = request(
        &mut client,
        json!({ "op": "requirement_create", "text": "second" }),
    );
    let other = other["requirement"]["id"].as_str().unwrap().to_string();
    request(
        &mut client,
        json!({ "op": "requirement_link", "requirement": other, "session": second }),
    );
    let listed = request(&mut client, json!({ "op": "requirement_list" }));
    assert_eq!(
        requirement(&listed, &id).unwrap()["sessions"],
        json!([first])
    );
    assert_eq!(
        requirement(&listed, &other).unwrap()["sessions"],
        json!([second])
    );
    assert_eq!(requirement(&listed, &other).unwrap()["state"], "open");

    let done = request(
        &mut client,
        json!({ "op": "requirement_update", "requirement": id, "state": "done" }),
    );
    assert_eq!(done["requirement"]["state"], "done");
    let bad = request(
        &mut client,
        json!({ "op": "requirement_update", "requirement": id, "state": "gone" }),
    );
    assert_eq!(bad["type"], "error");
    request(
        &mut client,
        json!({ "op": "requirement_delete", "requirement": id }),
    );
    let listed = request(&mut client, json!({ "op": "requirement_list" }));
    assert!(requirement(&listed, &id).is_none());
    let kept = daemon.state.parent().unwrap().join("uploads/requirements");
    assert!(!kept.join(&id).exists());
}

#[test]
fn a_file_uploads_in_chunks_outside_the_daemon_state() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    let part = request(
        &mut client,
        json!({ "op": "upload", "name": "notes.md", "data": STANDARD.encode(b"# one\n") }),
    );
    assert_eq!(part["type"], "upload_part");
    let upload = part["upload"].as_str().unwrap().to_string();
    let stale = request(
        &mut client,
        json!({ "op": "upload", "upload": upload, "offset": 0, "data": STANDARD.encode(b"x") }),
    );
    assert_eq!(stale["type"], "error");
    let done = request(
        &mut client,
        json!({ "op": "upload", "upload": upload, "offset": 6, "data": STANDARD.encode(b"two\n"), "last": true }),
    );
    assert_eq!(done["type"], "uploaded");
    assert_eq!(done["size"], 10);
    assert_eq!(done["image"], false);
    let path = PathBuf::from(done["path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(&path).unwrap(), "# one\ntwo\n");
    assert!(path.starts_with(daemon.state.parent().unwrap().join("uploads")));
}

#[test]
fn a_mode_codex_takes_per_turn_waits_and_full_access_answers_the_open_approval() {
    let _guard = setup();
    fake_codex();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-codex-mode");
    fs::create_dir_all(&dir).unwrap();
    let mut desktop = Client::connect(&daemon);
    let created = request(
        &mut desktop,
        json!({ "op": "session_create", "cwd": dir, "engine": "codex" }),
    );
    let session = created["session"].as_str().unwrap().to_string();
    desktop.send(json!({ "op": "watch", "session": session }));
    desktop.until(|f| f["session"] == session.as_str() && f["type"] == "attended");

    desktop.send(json!({ "op": "user_message", "session": session, "content": "use a tool" }));
    desktop.until(|f| f["session"] == session.as_str() && f["type"] == "approval_request");
    // Mid-turn the switch waits for the turn; full access needs no decision,
    // so the open approval is allowed and the turn goes on.
    desktop.send(json!({ "op": "set_approval_mode", "session": session, "mode": "full-access" }));
    let frames = desktop.until(turn_done(&session));
    assert!(frames
        .iter()
        .any(|f| f["type"] == "approval_mode_pending" && f["mode"] == "full-access"));
    assert!(frames
        .iter()
        .any(|f| f["type"] == "tool_output" && f["is_error"] == false));
    desktop.until(|f| {
        f["session"] == session.as_str()
            && f["type"] == "approval_mode_pending"
            && f["mode"].is_null()
    });

    // Between turns it applies at once: nothing waits.
    desktop.send(json!({ "op": "set_approval_mode", "session": session, "mode": "manual" }));
    desktop.send(json!({ "op": "user_message", "session": session, "content": "hello" }));
    let frames = desktop.until(turn_done(&session));
    assert!(!frames.iter().any(|f| f["type"] == "approval_mode_pending"));
}

#[test]
fn session_usage_restores_only_its_persisted_turns_with_correlated_replies() {
    let _guard = setup();
    let daemon = start_daemon();
    let row = |session: &str, turn: &str, cost: f64| {
        json!({
            "session": session, "turn_id": turn, "channel_kind": "local",
            "input_tokens": 100, "output_tokens": 10, "requests": 1,
            "estimated_cost_usd": cost, "ended_at": 123,
        })
    };
    fs::write(
        daemon.state.join("usage.jsonl"),
        format!("{}\n{}\n", row("s1", "t1", 0.12), row("other", "t2", 99.0)),
    )
    .unwrap();
    let mut client = Client::connect(&daemon);
    client.send(json!({ "op": "session_usage", "session": "s1", "id": 81 }));
    let frames = client.until(|f| f["id"] == 81);
    let usage = frames.last().unwrap();
    assert_eq!(usage["type"], "session_usage");
    assert_eq!(usage["session"], "s1");
    assert_eq!(usage["totals"]["estimated_cost_usd"], 0.12);
    assert_eq!(usage["totals"]["input_tokens"], 100);
    assert_eq!(usage["turns"].as_array().unwrap().len(), 1);
    assert_eq!(usage["turns"][0]["turn_id"], "t1");
    client.send(json!({ "op": "session_usage", "id": 82 }));
    assert_eq!(
        client.until(|f| f["id"] == 82).last().unwrap()["type"],
        "error"
    );
}

/// Scheduled task `id` once `done` holds for it, waiting up to ten seconds.
fn schedule_when(client: &mut Client, id: &str, done: impl Fn(&Value) -> bool) -> Value {
    let start = Instant::now();
    loop {
        let listed = request(client, json!({ "op": "schedule_list" }));
        let entry = listed["schedules"]
            .as_array()
            .and_then(|list| list.iter().find(|s| s["id"] == id))
            .cloned()
            .unwrap_or(Value::Null);
        if done(&entry) {
            return entry;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "{entry}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Every frame up to the reply to `op`, the reply last.
fn request_frames(client: &mut Client, mut op: Value) -> Vec<Value> {
    op["id"] = json!("frames");
    client.send(op);
    client.until(|frame| frame["id"] == "frames")
}

fn session_entry(client: &mut Client, session: &str) -> Value {
    let listed = request(client, json!({ "op": "session_list" }));
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["session"] == session)
        .cloned()
        .unwrap_or(Value::Null)
}

#[test]
fn a_scheduled_task_runs_in_a_new_session_and_archives_the_last_run() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-schedule");
    let mut client = Client::connect(&daemon);
    let fields =
        json!({ "name": "巡检", "prompt": "check the logs", "repeat": "daily", "time": "09:00" });
    // It cannot be switched on before it has a directory to run in.
    let refused = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": fields }),
    );
    assert_eq!(refused["type"], "error");
    let mut with_dir = fields.clone();
    with_dir["cwd"] = json!(dir);
    let saved = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": with_dir }),
    );
    let id = saved["schedule"]["id"].as_str().unwrap().to_string();
    assert_eq!(saved["schedule"]["engine"], "jucode");
    assert!(saved["schedule"]["next_run_at"].is_u64());

    let run = |client: &mut Client| {
        let started = request(client, json!({ "op": "schedule_run", "schedule": id }));
        assert_eq!(started["type"], "schedule_started", "{started}");
        let session = started["session"].as_str().unwrap().to_string();
        let frames = client.until(ready(&session));
        assert_eq!(reply_text(&frames, &session), "user said: check the logs");
        schedule_when(client, &id, |s| {
            s["last_run"]["session"] == session.as_str() && s["last_run"]["status"] == "done"
        });
        session
    };
    let first = run(&mut client);
    let entry = session_entry(&mut client, &first);
    assert_eq!(entry["schedule"], id.as_str());
    assert!(entry["title"].as_str().unwrap().starts_with("巡检 · "));

    // The next run archives this one, which has nothing left to do.
    let second = run(&mut client);
    assert_eq!(session_entry(&mut client, &first)["archived"], true);
    assert_eq!(session_entry(&mut client, &second)["archived"], false);

    let usage = request(
        &mut client,
        json!({ "op": "schedule_usage", "schedule": id }),
    );
    assert_eq!(usage["type"], "schedule_usage");
}

#[test]
fn a_run_waiting_for_an_approval_says_so_and_holds_the_next_time() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-schedule-wait");
    let mut client = Client::connect(&daemon);
    let saved = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": {
            "name": "清理", "prompt": "RUN: printf x > marker.txt", "repeat": "hourly",
            "time": "00:30", "cwd": dir, "mode": "ask",
        } }),
    );
    let id = saved["schedule"]["id"].as_str().unwrap().to_string();
    let started = request(&mut client, json!({ "op": "schedule_run", "schedule": id }));
    let session = started["session"].as_str().unwrap().to_string();
    let frames = client.until(|frame| frame["type"] == "schedule_event");
    let event = frames.last().unwrap();
    assert_eq!(
        (event["kind"].as_str(), event["session"].as_str()),
        (Some("waiting"), Some(session.as_str()))
    );
    client.until(ready(&session));
    schedule_when(&mut client, &id, |s| s["last_run"]["status"] == "waiting");
    assert!(!dir.join("marker.txt").exists());
    let again = request(&mut client, json!({ "op": "schedule_run", "schedule": id }));
    assert_eq!(again["type"], "error", "the last run still waits");
}

#[test]
fn a_run_that_cannot_start_fails_and_tells_the_clients() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-schedule-gone");
    let mut client = Client::connect(&daemon);
    let saved = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": {
            "name": "日报", "prompt": "write", "repeat": "daily", "time": "18:00", "cwd": dir,
        } }),
    );
    let id = saved["schedule"]["id"].as_str().unwrap().to_string();
    fs::remove_dir_all(&dir).unwrap();
    let frames = request_frames(&mut client, json!({ "op": "schedule_run", "schedule": id }));
    assert_eq!(frames.last().unwrap()["type"], "error");
    let event = frames
        .iter()
        .find(|frame| frame["type"] == "schedule_event")
        .expect("the clients hear of it");
    assert_eq!(event["kind"], "failed");
    let failed = schedule_when(&mut client, &id, |s| s["last_run"]["status"] == "failed");
    assert!(failed["last_run"]["error"]
        .as_str()
        .unwrap()
        .contains("not a directory"));
}
