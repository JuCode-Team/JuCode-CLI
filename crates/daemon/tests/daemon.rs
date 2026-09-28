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
    agents: PathBuf,
}

/// A daemon on a free loopback port with its own state and agents
/// directories. Daemons from earlier tests keep running in this process, so
/// sharing directories would let their schedulers act on this test's data.
fn start_daemon() -> Daemon {
    let root = temp_dir("daemon-state");
    start_daemon_on(root.join("daemon"), root.join("agents"))
}

/// A daemon on existing directories, standing in for a restart.
fn start_daemon_on(state: PathBuf, agents: PathBuf) -> Daemon {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let store = Store::open(state.clone()).unwrap();
    let token = store.token().unwrap();
    let agent_store = jucode_daemon::Agents::open(agents.clone()).unwrap();
    thread::spawn(move || jucode_daemon::serve(listener, store, agent_store, "test"));
    Daemon {
        address,
        token,
        state,
        agents,
    }
}

struct Client {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl Client {
    fn connect(daemon: &Daemon) -> Self {
        let (socket, _) =
            tungstenite::connect(format!("ws://{}/?token={}", daemon.address, daemon.token))
                .unwrap();
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
        "id": deferred["id"],
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

    // A second daemon on the same state stands in for a restart.
    let restarted = start_daemon_on(daemon.state.clone(), daemon.agents.clone());
    let mut client = Client::connect(&restarted);
    client.send(json!({ "op": "session_open", "session": session }));
    client.until(|frame| frame["type"] == "session_opened");
    client.send(json!({
        "op": "decide_action",
        "session": session,
        "id": deferred["id"],
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

fn create_agent(client: &mut Client, id: &str, role: &str) -> PathBuf {
    let dir = temp_dir(&format!("agent-{id}"));
    client.send(json!({
        "op": "agent_create", "agent": id, "name": id, "cwd": dir, "role": role,
    }));
    client.until(|frame| frame["type"] == "agent_created" && frame["agent"]["id"] == id);
    dir
}

fn delivered_to(agent: &str) -> impl Fn(&Value) -> bool + '_ {
    move |frame| frame["type"] == "message_delivered" && frame["agent"] == agent
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
fn a_user_message_to_an_agent_opens_a_session_and_follow_ups_continue_it() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "route", "Keeps the build green");

    client.send(json!({ "op": "message_send", "agent": "route", "body": "SYSTEM" }));
    let frames = client.until(delivered_to("route"));
    let session = frames.last().unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    let frames = client.until(ready(&session));
    // The agent's brief is part of every turn's system prompt.
    let reply = reply_text(&frames, &session);
    assert!(reply.contains("<agent id=\"route\""), "{reply}");
    assert!(reply.contains("Keeps the build green"), "{reply}");

    client.send(json!({ "op": "message_send", "agent": "route", "body": "and another thing" }));
    let frames = client.until(delivered_to("route"));
    assert_eq!(frames.last().unwrap()["session"], session.as_str());
}

#[test]
fn a_timer_set_by_an_agent_wakes_it_with_nobody_connected() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "waker", "Checks back later");
    client.send(json!({
        "op": "message_send",
        "agent": "waker",
        "body": r#"CALL timer {"action":"set","in_seconds":1,"body":"check the deploy"}"#,
    }));
    let frames = client.until(delivered_to("waker"));
    let session = frames.last().unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    client.until(ready(&session));
    drop(client);

    // Nobody is connected when the timer fires; a client that attaches
    // afterwards finds the timer's message answered in the same session.
    thread::sleep(Duration::from_millis(3500));
    let mut late = Client::connect(&daemon);
    late.send(json!({ "op": "watch", "session": session }));
    let frames =
        late.until(|frame| frame["type"] == "transcript" && frame["session"] == session.as_str());
    let transcript = frames.last().unwrap()["items"].to_string();
    assert!(transcript.contains("fired"), "{transcript}");
    assert!(transcript.contains("check the deploy"), "{transcript}");
    late.send(json!({ "op": "timer_list" }));
    let timers = late.until(|frame| frame["type"] == "timers");
    assert!(!timers.last().unwrap()["timers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|timer| timer["agent"] == "waker"));
}

#[test]
fn agents_message_each_other_into_a_new_session() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "asker", "Asks for help");
    create_agent(&mut client, "helper", "Helps");
    client.send(json!({
        "op": "message_send",
        "agent": "asker",
        "body": r#"CALL message_agent {"to":"helper","body":"please look at the logs"}"#,
    }));
    let frames = client.until(delivered_to("helper"));
    let delivery = frames.last().unwrap().clone();
    assert_eq!(delivery["from"], "agent:asker");
    let session = delivery["session"].as_str().unwrap().to_string();
    let frames = client.until(ready(&session));
    let reply = reply_text(&frames, &session);
    assert!(reply.contains("message from agent asker"), "{reply}");
    assert!(reply.contains("please look at the logs"), "{reply}");
}

#[test]
fn a_message_with_a_seen_dedupe_key_is_delivered_once() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "once", "Does things once");
    for expected in [false, true] {
        client.send(json!({
            "op": "message_send", "agent": "once", "body": "hi", "dedupe_key": "im-42", "id": 9,
        }));
        let frames = client.until(|frame| frame["id"] == 9);
        assert_eq!(frames.last().unwrap()["duplicate"], expected);
    }
    let sessions = fs::read_to_string(daemon.state.join("messages.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| line.contains("\"delivered\"") || line.contains("im-42"))
        .count();
    // One message line and one delivery line.
    assert_eq!(sessions, 2);
}

#[test]
fn a_timer_due_while_the_daemon_was_down_fires_on_start() {
    let _guard = setup();
    // An agent and a timer written before this daemon starts, already due.
    let root = temp_dir("daemon-down");
    let agents = jucode_daemon::Agents::open(root.join("agents")).unwrap();
    agents
        .create(
            "sleeper",
            "sleeper",
            &temp_dir("agent-sleeper"),
            "Was asleep",
        )
        .unwrap();
    let timers = root.join("daemon").join("timers.jsonl");
    fs::create_dir_all(timers.parent().unwrap()).unwrap();
    let line = json!({
        "kind": "set", "id": "t-overdue", "agent": "sleeper", "session": null,
        "fire_at": 1, "body": "catch up", "at": 1,
    });
    let mut existing = fs::read_to_string(&timers).unwrap_or_default();
    existing.push_str(&format!("{line}\n"));
    fs::write(&timers, existing).unwrap();

    let daemon = start_daemon_on(root.join("daemon"), root.join("agents"));
    let mut client = Client::connect(&daemon);
    let frames = client.until(delivered_to("sleeper"));
    assert_eq!(frames.last().unwrap()["from"], "timer:t-overdue");
}

#[test]
fn at_most_four_runs_are_in_progress_at_once() {
    let _guard = setup();
    let root = temp_dir("daemon-slots");
    let agents = jucode_daemon::Agents::open(root.join("agents")).unwrap();
    let ids = ["slot-a", "slot-b", "slot-c", "slot-d", "slot-e"];
    for id in ids {
        agents.create(id, id, &temp_dir(id), "Waits").unwrap();
        // Run shell commands without asking, so each run takes real time.
        let settings = root.join("agents").join(id).join("agent.json");
        let text = fs::read_to_string(&settings).unwrap();
        fs::write(&settings, text.replace("\"auto\"", "\"full-access\"")).unwrap();
    }
    let daemon = start_daemon_on(root.join("daemon"), root.join("agents"));
    let mut client = Client::connect(&daemon);
    for id in ids {
        client.send(json!({ "op": "message_send", "agent": id, "body": "RUN: sleep 2" }));
    }
    let mut delivered = Vec::new();
    let mut finished = 0;
    let mut finished_before_fifth = 0;
    client.until(|frame| {
        if frame["type"] == "message_delivered" {
            delivered.push(frame["session"].as_str().unwrap().to_string());
            if delivered.len() == 5 {
                finished_before_fifth = finished;
            }
        }
        if frame["type"] == "status" && frame["message"] == "ready" {
            finished += 1;
        }
        delivered.len() == 5
    });
    // The fifth message waited for one of the first four runs to end.
    assert!(
        finished_before_fifth >= 1,
        "fifth delivered with {finished_before_fifth} runs finished"
    );
}
