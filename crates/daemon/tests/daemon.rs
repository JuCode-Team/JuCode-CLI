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
    start_daemon_with(state, agents, None)
}

fn start_daemon_with(state: PathBuf, agents: PathBuf, relay: Option<String>) -> Daemon {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let store = Store::open(state.clone()).unwrap();
    let token = store.token().unwrap();
    let agent_store = jucode_daemon::Agents::open(agents.clone()).unwrap();
    let web = state.parent().map(|root| root.join("web"));
    thread::spawn(move || jucode_daemon::serve(listener, store, agent_store, web, "test", relay));
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
    let restarted = start_daemon_on(state, daemon.agents.clone());
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
fn each_user_task_gets_a_new_session_that_reads_the_last_handoff() {
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

    // The turn's end has the title model write the run's handoff note into
    // its outcome (the test model echoes what it was asked).
    let mut listed = Value::Null;
    for _ in 0..50 {
        listed = request(
            &mut client,
            json!({ "op": "handoff_list", "agent": "route" }),
        );
        if listed["handoffs"][0]["session"] == session.as_str() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        listed["handoffs"][0]["session"],
        session.as_str(),
        "{listed}"
    );
    let note = listed["handoffs"][0]["text"].as_str().unwrap_or_default();
    assert!(note.contains("user said: Session:"), "{note}");

    // A new task is a new session, told what the last one concluded.
    client.send(json!({ "op": "message_send", "agent": "route", "body": "SYSTEM" }));
    let frames = client.until(delivered_to("route"));
    let next = frames.last().unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(next, session);
    let frames = client.until(ready(&next));
    let reply = reply_text(&frames, &next);
    assert!(reply.contains("<handoff task=\"task-"), "{reply}");
    assert!(reply.contains("user said: Session:"), "{reply}");

    // A reply names its session and continues it.
    client.send(json!({ "op": "message_send", "agent": "route", "body": "and another thing", "session": session }));
    let frames = client.until(delivered_to("route"));
    assert_eq!(frames.last().unwrap()["session"], session.as_str());

    // Hidden from a session list, it stays on its agent's page.
    client.send(json!({ "op": "session_meta", "session": session, "hidden": true }));
    // Ops of one client run in order: the list comes after the change.
    let listed = request(&mut client, json!({ "op": "session_list" }));
    assert!(listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["session"] == session.as_str() && s["agent"] == "route"));
}

#[test]
fn an_agent_proposes_a_schedule_that_stays_off_until_the_user_turns_it_on() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "planner", "Plans the week");
    create_agent(&mut client, "other", "Someone else");
    let theirs = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": {
            "agent": "other", "name": "theirs", "prompt": "x", "repeat": "daily", "time": "08:00",
        } }),
    );
    let theirs = theirs["schedule"]["id"].as_str().unwrap().to_string();

    let call = |body: Value| format!("CALL task {body}");
    client.send(
        json!({ "op": "message_send", "agent": "planner", "body": call(json!({
        // Filled the way some models fill every optional field.
        "action": "schedule", "id": "", "date": "", "days": [],
        "name": "每日巡检", "prompt": "check the deploys",
        "repeat": "daily", "time": "09:30",
    })) }),
    );
    let proposed = client.until(|frame| {
        frame["type"] == "schedules"
            && frame["schedules"]
                .as_array()
                .is_some_and(|list| list.iter().any(|s| s["agent"] == "planner"))
    });
    let list = proposed.last().unwrap()["schedules"]
        .as_array()
        .unwrap()
        .clone();
    let mine = list.iter().find(|s| s["agent"] == "planner").unwrap();
    assert_eq!(mine["name"], "每日巡检");
    assert_eq!(mine["enabled"], false);
    assert_eq!(mine["by_agent"], true);
    assert!(mine["next_run_at"].is_null());
    let mine = mine["id"].as_str().unwrap().to_string();

    // Another agent's task is out of reach.
    client.send(json!({ "op": "message_send", "agent": "planner", "body": call(json!({ "action": "unschedule", "id": theirs })) }));
    let frames = client.until(|frame| {
        frame["type"] == "tool_output" && frame["name"] == "task" && frame["is_error"] == true
    });
    assert!(
        frames
            .last()
            .unwrap()
            .to_string()
            .contains("unknown schedule"),
        "{:?}",
        frames.last()
    );

    // The user turns it on; an agent's later change switches it off again.
    let reply = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": { "id": mine, "enabled": true } }),
    );
    assert_eq!(reply["schedule"]["enabled"], true);
    assert!(reply["schedule"]["next_run_at"].as_u64().is_some());
    client.send(
        json!({ "op": "message_send", "agent": "planner", "body": call(json!({
        "action": "schedule", "id": mine, "time": "10:00", "enabled": true,
    })) }),
    );
    client.until(|frame| {
        frame["type"] == "schedules"
            && frame["schedules"].as_array().is_some_and(|list| {
                list.iter()
                    .any(|s| s["id"] == mine.as_str() && s["time"] == "10:00")
            })
    });
    let list = request(
        &mut client,
        json!({ "op": "schedule_list", "agent": "planner" }),
    );
    assert_eq!(list["schedules"][0]["enabled"], false);
}

#[test]
fn a_new_session_message_starts_a_conversation_and_the_log_shows_both() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "fresh", "Starts over when asked");
    client.send(json!({ "op": "message_send", "agent": "fresh", "body": "first" }));
    let frames = client.until(delivered_to("fresh"));
    let first = frames.last().unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    client.until(ready(&first));

    client.send(
        json!({ "op": "message_send", "agent": "fresh", "body": "second", "new_session": true }),
    );
    let frames = client.until(delivered_to("fresh"));
    let second = frames.last().unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first, second);

    client.send(json!({ "op": "message_list", "agent": "fresh" }));
    let frames = client.until(|frame| frame["type"] == "messages");
    let log = frames.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(log.len(), 2);
    assert_eq!(
        (log[0]["body"].as_str(), log[0]["session"].as_str()),
        (Some("second"), Some(second.as_str()))
    );
    assert_eq!(
        (log[1]["status"].as_str(), log[1]["from"].as_str()),
        (Some("delivered"), Some("user"))
    );
}

#[test]
fn a_timer_can_be_cancelled_from_the_desk_and_stays_cancelled() {
    let _guard = setup();
    let root = temp_dir("desk-cancel-timer");
    let state = root.join("daemon");
    fs::create_dir_all(&state).unwrap();
    let timers: String = ["t-cancel", "t-keep"]
        .iter()
        .map(|id| {
            format!(
                "{}\n",
                json!({
                    "kind": "set", "id": id, "agent": "ops", "session": "s1",
                    "fire_at": 4_000_000_000_000u64, "body": id, "at": 1,
                })
            )
        })
        .collect();
    fs::write(state.join("timers.jsonl"), timers).unwrap();
    let daemon = start_daemon_on(state.clone(), root.join("agents"));
    let mut client = Client::connect(&daemon);
    client.send(json!({ "op": "timer_cancel", "timer": "t-cancel", "id": 71 }));
    let frames = client.until(|frame| frame["id"] == 71);
    assert_eq!(frames.last().unwrap()["type"], "timer_cancelled");
    assert_eq!(frames.last().unwrap()["timer"], "t-cancel");
    client.send(json!({ "op": "timer_cancel", "timer": "t-cancel", "id": 72 }));
    let frames = client.until(|frame| frame["id"] == 72);
    assert_eq!(frames.last().unwrap()["type"], "error");
    drop(client);
    let mut client = Client::connect(&daemon);
    client.send(json!({ "op": "timer_list", "id": 73 }));
    let frames = client.until(|frame| frame["id"] == 73);
    let active = frames.last().unwrap()["timers"].as_array().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["timer"], "t-keep");
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
        "body": r#"CALL task {"action":"remind","in_seconds":1,"body":"check the deploy"}"#,
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
            &Value::Null,
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
        agents
            .create(id, id, &temp_dir(id), "Waits", &Value::Null)
            .unwrap();
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

/// Sends `body` to a fresh agent and returns the session it landed in once
/// that run has finished.
fn run_agent(client: &mut Client, agent: &str, body: &str) -> (String, Vec<Value>) {
    client.send(json!({ "op": "message_send", "agent": agent, "body": body }));
    let frames = client.until(delivered_to(agent));
    let session = frames.last().unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    let mut all = frames;
    all.extend(client.until(ready(&session)));
    (session, all)
}

#[test]
fn an_answered_question_wakes_the_session_that_asked() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "asks", "Needs decisions");
    let (session, frames) = run_agent(
        &mut client,
        "asks",
        r#"CALL question {"title":"Ship to prod?","assumption":"not yet","default":"wait","importance":"high"}"#,
    );
    let listed = frames
        .iter()
        .rev()
        .find(|frame| frame["type"] == "questions")
        .expect("the new question is broadcast");
    let question = listed["questions"][0].clone();
    assert_eq!(question["title"], "Ship to prod?");
    assert_eq!(question["session"], session.as_str());
    assert_eq!(question["importance"], "high");

    client.send(json!({
        "op": "question_answer", "question": question["id"], "answer": "yes, ship it", "id": 3,
    }));
    let mut frames = client.until(|frame| frame["id"] == 3 && frame["type"] == "question_answered");
    frames.extend(client.until(ready(&session)));
    let delivery = frames
        .iter()
        .find(|frame| frame["type"] == "message_delivered")
        .expect("the answer is delivered");
    assert_eq!(delivery["session"], session.as_str());
    let reply = reply_text(&frames, &session);
    assert!(reply.contains("A: yes, ship it"), "{reply}");

    client.send(
        json!({ "op": "question_answer", "question": question["id"], "answer": "no", "id": 4 }),
    );
    let frames = client.until(|frame| frame["id"] == 4);
    assert_eq!(frames.last().unwrap()["type"], "error");
}

#[test]
fn an_unanswered_question_falls_back_to_its_default_at_the_deadline() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "waits", "Waits politely");
    let (session, _) = run_agent(
        &mut client,
        "waits",
        r#"CALL question {"title":"Which region?","default":"use eu","due_in_seconds":1}"#,
    );
    let frames = client.until(delivered_to("waits"));
    assert_eq!(
        frames.last().unwrap()["from"]
            .as_str()
            .unwrap()
            .split(':')
            .next(),
        Some("question")
    );
    let frames = client.until(ready(&session));
    let reply = reply_text(&frames, &session);
    assert!(reply.contains("No answer by the deadline"), "{reply}");
    assert!(reply.contains("use eu"), "{reply}");
    client.send(json!({ "op": "question_list", "id": 5 }));
    let frames = client.until(|frame| frame["id"] == 5);
    assert_eq!(frames.last().unwrap()["questions"], json!([]));
}

#[test]
fn a_runs_write_up_is_a_report_listed_until_read() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "reporter", "Reports");
    // A run's longer write-up (`finish` with details) is kept as a report.
    let (_, frames) = run_agent(
        &mut client,
        "reporter",
        r#"CALL finish {"verdict":"done","summary":"Login fixed","details":"tests: 12 pass"}"#,
    );
    let posted = frames
        .iter()
        .find(|frame| frame["type"] == "report_posted")
        .expect("the report is broadcast")["report"]
        .clone();
    assert_eq!(posted["body"], "tests: 12 pass");
    assert!(!posted["title"].as_str().unwrap_or_default().is_empty());
    assert_eq!(posted["read"], false);
    // Posting a report started no further delivery.
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "message_delivered")
            .count(),
        1
    );

    client.send(json!({ "op": "report_read", "report": posted["id"], "id": 6 }));
    client.until(|frame| frame["id"] == 6);
    client.send(json!({ "op": "report_list", "id": 7 }));
    let frames = client.until(|frame| frame["id"] == 7);
    let reports = frames.last().unwrap()["reports"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["read"], true);
}

#[test]
fn an_action_can_be_decided_after_its_session_closed() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    let dir = create_agent(&mut client, "careful", "Asks before shell commands");
    client.send(
        json!({ "op": "agent_update", "agent": "careful", "approval_mode": "manual", "id": 8 }),
    );
    client.until(|frame| frame["id"] == 8);
    let (session, frames) = run_agent(&mut client, "careful", "RUN: printf later > marker.txt");
    let actions = frames
        .iter()
        .rev()
        .find(|frame| frame["type"] == "actions")
        .expect("the deferred action is broadcast");
    let action = actions["actions"][0].clone();
    assert_eq!(action["session_id"], session.as_str());
    client.send(json!({ "op": "session_close", "session": session }));
    client.until(|frame| frame["type"] == "session_closed");

    client.send(json!({
        "op": "decide_action", "session": session, "action": action["id"], "decision": "allow",
    }));
    client.until(|frame| frame["type"] == "action_decided");
    client.until(ready(&session));
    assert_eq!(fs::read_to_string(dir.join("marker.txt")).unwrap(), "later");
}

#[test]
fn the_agent_page_reads_the_brief_and_changes_settings() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "paged", "Has a page");
    client.send(json!({ "op": "agent_update", "agent": "paged", "name": "Paged", "enabled": false, "id": 1 }));
    let frames = client.until(|frame| frame["id"] == 1);
    assert_eq!(frames.last().unwrap()["agent"]["enabled"], false);
    client.send(json!({ "op": "agent_get", "agent": "paged", "id": 2 }));
    let frames = client.until(|frame| frame["id"] == 2);
    let page = frames.last().unwrap();
    assert_eq!(page["agent"]["name"], "Paged");
    assert_eq!(page["brief"]["role.md"], "Has a page\n");
    // A disabled agent's messages wait, and go once it is enabled again.
    client.send(json!({ "op": "message_send", "agent": "paged", "body": "hi", "id": 3 }));
    client.until(|frame| frame["id"] == 3);
    thread::sleep(Duration::from_millis(1500));
    let log = fs::read_to_string(daemon.state.join("messages.jsonl")).unwrap();
    assert!(
        !log.contains("undeliverable") && !log.contains("delivered"),
        "{log}"
    );
    client.send(json!({ "op": "agent_update", "agent": "paged", "enabled": true, "id": 4 }));
    client.until(delivered_to("paged"));
}

/// Runs schedule `id` now and returns the session its message reached once
/// that run has finished.
fn run_schedule(client: &mut Client, agent: &str, id: &str) -> (String, Vec<Value>) {
    client.send(json!({ "op": "schedule_run", "schedule": id, "id": "run" }));
    let mut replied = false;
    let mut session: Option<String> = None;
    let frames = client.until(|frame| {
        if frame["id"] == "run" {
            assert_eq!(frame["type"], "schedule_started", "{frame}");
            assert_eq!(frame["schedule"], id);
            replied = true;
        }
        if delivered_to(agent)(frame) {
            assert_eq!(frame["from"], format!("schedule:{id}"));
            session = frame["session"].as_str().map(str::to_string);
        }
        replied
            && session
                .as_deref()
                .is_some_and(|session| ready(session)(frame))
    });
    (session.unwrap(), frames)
}

#[test]
fn a_schedule_runs_now_into_a_new_session_or_the_last_one() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "tickets", "Works the ticket queue");
    let reply = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": { "agent": "nobody", "name": "x", "prompt": "x", "repeat": "daily", "time": "11:00" } }),
    );
    assert_eq!(reply["type"], "error");
    let reply = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": {
            "agent": "tickets", "name": "工单处理", "prompt": "work the queue",
            "repeat": "weekly", "time": "11:00", "days": [],
        } }),
    );
    assert_eq!(reply["type"], "error", "a weekly schedule needs days");
    let reply = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": {
            "agent": "tickets", "name": "工单处理", "prompt": "work the queue",
            "repeat": "daily", "time": "11:00",
        } }),
    );
    assert_eq!(reply["type"], "schedule_saved", "{reply}");
    let schedule = reply["schedule"].clone();
    let id = schedule["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("sch-"));
    assert_eq!(schedule["new_session"], true);
    assert!(schedule["next_run_at"].as_u64().unwrap() > 0);
    assert!(daemon.agents.join("tickets/schedules.json").exists());
    let list = request(
        &mut client,
        json!({ "op": "schedule_list", "agent": "tickets" }),
    );
    assert_eq!(list["schedules"].as_array().unwrap().len(), 1);
    // A client that connects now gets the list right away.
    let mut late = Client::connect(&daemon);
    late.until(|frame| frame["type"] == "schedules" && frame["schedules"][0]["id"] == id.as_str());

    let (first, frames) = run_schedule(&mut client, "tickets", &id);
    let reply = reply_text(&frames, &first);
    assert!(reply.contains("定时任务「工单处理」"), "{reply}");
    assert!(reply.contains("work the queue"), "{reply}");
    let list = request(&mut client, json!({ "op": "schedule_list" }));
    let listed = &list["schedules"][0];
    assert_eq!(listed["last_session"], first.as_str());
    assert!(listed["last_run_at"].as_u64().is_some());
    assert_eq!(listed["next_run_at"], schedule["next_run_at"]);

    // Continuing: the next run goes into the last run's session.
    let reply = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": { "id": id, "new_session": false } }),
    );
    assert_eq!(reply["schedule"]["name"], "工单处理");
    // Runs are told apart by the second they start in.
    thread::sleep(Duration::from_millis(1100));
    let (second, _) = run_schedule(&mut client, "tickets", &id);
    assert_eq!(second, first);

    request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": { "id": id, "new_session": true } }),
    );
    thread::sleep(Duration::from_millis(1100));
    let (third, _) = run_schedule(&mut client, "tickets", &id);
    assert_ne!(third, first);

    // `id` alone names the schedule (and is echoed as the request id).
    client.send(json!({ "op": "schedule_delete", "id": id }));
    let frames = client.until(|frame| frame["type"] == "schedule_deleted");
    assert_eq!(frames.last().unwrap()["id"], id.as_str());
    let list = request(&mut client, json!({ "op": "schedule_list" }));
    assert_eq!(list["schedules"], json!([]));
}

#[test]
fn a_schedule_due_while_the_daemon_was_down_fires_once_on_start() {
    let _guard = setup();
    let root = temp_dir("schedule-down");
    let agents = jucode_daemon::Agents::open(root.join("agents")).unwrap();
    agents
        .create(
            "nightly",
            "nightly",
            &temp_dir("agent-nightly"),
            "Runs at night",
            &Value::Null,
        )
        .unwrap();
    // Due long ago: many runs were missed.
    let saved = json!([{
        "id": "sch-missed", "agent": "nightly", "name": "nightly", "prompt": "catch up",
        "enabled": true, "repeat": "hourly", "time": "00:00", "days": [], "date": null,
        "new_session": true, "created_at": 1, "last_run_at": null, "last_session": null,
        "next_run_at": 1,
    }]);
    fs::write(
        root.join("agents/nightly/schedules.json"),
        saved.to_string(),
    )
    .unwrap();

    let daemon = start_daemon_on(root.join("daemon"), root.join("agents"));
    let mut client = Client::connect(&daemon);
    let frames = client.until(delivered_to("nightly"));
    assert_eq!(frames.last().unwrap()["from"], "schedule:sch-missed");
    let list = request(&mut client, json!({ "op": "schedule_list" }));
    let schedule = &list["schedules"][0];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(schedule["next_run_at"].as_u64().unwrap() > now);
    thread::sleep(Duration::from_millis(1500));
    let fired = fs::read_to_string(daemon.state.join("messages.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| line.contains("\"kind\":\"message\"") && line.contains("sch-missed"))
        .count();
    assert_eq!(fired, 1);
}

#[test]
fn an_agent_is_deleted_only_while_idle_and_takes_its_schedules_along() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "doomed", "Goes away");
    create_agent(&mut client, "stays", "Stays");
    request(
        &mut client,
        json!({ "op": "agent_update", "agent": "doomed", "approval_mode": "full-access" }),
    );
    for agent in ["doomed", "stays"] {
        let reply = request(
            &mut client,
            json!({ "op": "schedule_save", "schedule": {
                "agent": agent, "name": "n", "prompt": "p", "repeat": "hourly", "time": "00:30",
            } }),
        );
        assert_eq!(reply["type"], "schedule_saved", "{reply}");
    }
    let reply = request(
        &mut client,
        json!({ "op": "timer_list", "agent": "doomed" }),
    );
    assert_eq!(reply["timers"], json!([]));

    client.send(json!({ "op": "message_send", "agent": "doomed", "body": "RUN: sleep 3" }));
    let frames = client.until(delivered_to("doomed"));
    let session = frames.last().unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    let reply = request(
        &mut client,
        json!({ "op": "agent_delete", "agent": "doomed" }),
    );
    assert_eq!(reply["type"], "error", "refused while running: {reply}");
    assert!(daemon.agents.join("doomed").exists());
    client.until(ready(&session));

    let reply = request(
        &mut client,
        json!({ "op": "agent_delete", "agent": "doomed" }),
    );
    assert_eq!(reply["type"], "agent_deleted", "{reply}");
    assert_eq!(reply["agent"], "doomed");
    assert!(!daemon.agents.join("doomed").exists());
    let list = request(&mut client, json!({ "op": "schedule_list" }));
    let owners: Vec<&Value> = list["schedules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|schedule| &schedule["agent"])
        .collect();
    assert_eq!(owners, vec!["stays"]);
    let agents = request(&mut client, json!({ "op": "agent_list" }));
    assert!(!agents["agents"].to_string().contains("doomed"));

    // A message for the deleted agent is refused; its old sessions stay listed.
    let reply = request(
        &mut client,
        json!({ "op": "message_send", "agent": "doomed", "body": "hi" }),
    );
    assert_eq!(reply["type"], "error");
    let sessions = request(&mut client, json!({ "op": "session_list" }));
    assert!(sessions["sessions"].to_string().contains(&session));
}

#[test]
fn agent_memory_is_read_by_file_name_only() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "keeper", "Keeps notes");
    fs::write(
        daemon.agents.join("keeper/memory/deploy.md"),
        "use make ship",
    )
    .unwrap();
    fs::write(daemon.agents.join("keeper/secret.md"), "nope").unwrap();
    let page = request(&mut client, json!({ "op": "agent_get", "agent": "keeper" }));
    assert_eq!(page["memory"], json!(["memory/deploy.md"]));
    let reply = request(
        &mut client,
        json!({ "op": "agent_memory_read", "agent": "keeper", "file": "deploy.md" }),
    );
    assert_eq!(reply["type"], "agent_memory", "{reply}");
    assert_eq!(reply["content"], "use make ship");
    assert_eq!(reply["file"], "deploy.md");
    for bad in [
        "../x",
        "../secret.md",
        "../../keeper/secret.md",
        "missing.md",
        "deploy",
    ] {
        let reply = request(
            &mut client,
            json!({ "op": "agent_memory_read", "agent": "keeper", "file": bad }),
        );
        assert_eq!(reply["type"], "error", "{bad}: {reply}");
    }

    // Renaming and rewriting the role.
    let reply = request(
        &mut client,
        json!({ "op": "agent_update", "agent": "keeper", "name": " " }),
    );
    assert_eq!(reply["type"], "error");
    let reply = request(
        &mut client,
        json!({ "op": "agent_update", "agent": "keeper", "name": "Keeper", "role": "Keeps better notes" }),
    );
    assert_eq!(reply["agent"]["name"], "Keeper");
    let page = request(&mut client, json!({ "op": "agent_get", "agent": "keeper" }));
    assert_eq!(page["brief"]["role.md"], "Keeps better notes\n");
}

/// One plain HTTP request; returns (status, headers and body as text).
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

    // The phone reaches sessions and agents, but cannot pair more devices.
    let mut phone = Client::connect_with(&daemon, &token);
    phone.send(json!({ "op": "agent_list", "id": 2 }));
    assert_eq!(
        phone.until(|frame| frame["id"] == 2).last().unwrap()["type"],
        "agents"
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
fn an_agent_writes_its_rw_directories_and_not_its_ro_ones() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    let dir = create_agent(&mut client, "deployer", "Deploys");
    fs::create_dir_all(dir.join(".git")).unwrap();
    let deploy = temp_dir("deploy-scripts");
    let logs = temp_dir("prod-logs");
    fs::write(logs.join("app.log"), "boot ok").unwrap();
    client.send(json!({
        "op": "agent_update", "agent": "deployer", "id": 1,
        "directories": [{ "path": deploy, "mode": "rw" }, { "path": logs, "mode": "ro" }],
    }));
    let frames = client.until(|frame| frame["id"] == 1);
    assert_eq!(
        frames.last().unwrap()["agent"]["sandbox"],
        "workspace-write"
    );

    let script = format!(
        "RUN: printf a > src.txt; printf b > {}/run.sh; printf c > {}/app.log; printf d > .git/config; cat {}/app.log > seen.txt",
        deploy.display(),
        logs.display(),
        logs.display()
    );
    let (_, frames) = run_agent(&mut client, "deployer", &script);
    // auto mode, inside the sandbox: nothing asked, nothing deferred.
    assert!(!frames
        .iter()
        .any(|frame| frame["type"] == "approval_request" || frame["type"] == "action_deferred"));
    assert_eq!(fs::read_to_string(dir.join("src.txt")).unwrap(), "a");
    assert_eq!(fs::read_to_string(deploy.join("run.sh")).unwrap(), "b");
    assert_eq!(fs::read_to_string(logs.join("app.log")).unwrap(), "boot ok");
    assert!(!dir.join(".git/config").exists());
    assert_eq!(fs::read_to_string(dir.join("seen.txt")).unwrap(), "boot ok");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn an_unattended_escalation_becomes_a_pending_action() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    let dir = create_agent(&mut client, "escalator", "Needs git");
    fs::create_dir_all(dir.join(".git")).unwrap();
    // auto-edit: an escalation asks a person (no safety model involved).
    client.send(json!({ "op": "agent_update", "agent": "escalator", "approval_mode": "auto-edit", "id": 1 }));
    client.until(|frame| frame["id"] == 1);
    let (_, frames) = run_agent(
        &mut client,
        "escalator",
        r#"CALL bash {"command":"printf x > .git/config","escalate":true,"justification":"set up the repo"}"#,
    );
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
        root.join("agents"),
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
fn only_the_desktop_starts_acp_agents_which_run_like_any_session() {
    let _guard = setup();
    let daemon = start_daemon();
    let dir = temp_dir("daemon-acp");
    fs::create_dir_all(&dir).unwrap();
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_acp.py");
    let options =
        json!({ "command": script, "args": ["--flag"], "env": { "FAKE_ACP_GREETING": "hey" } });

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
    assert_eq!(refused["type"], "error", "{refused}");
    let refused = request(
        &mut phone,
        json!({ "op": "session_create", "cwd": dir, "engine": "claude", "options": { "bin": script } }),
    );
    assert_eq!(refused["type"], "error", "{refused}");
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
    phone.until(|f| f["session"] == session.as_str() && f["type"] == "attended");
    phone.send(json!({ "op": "user_message", "session": session, "content": "hi" }));
    let frames = phone.until(turn_done(&session));
    assert_eq!(claude_reply(&frames, &session), "ok: hi --flag hey");

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

/// `agent`'s tasks once `done` holds for them, waiting up to five seconds
/// (a run ends just after its session reports ready).
fn tasks_when(client: &mut Client, agent: &str, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let start = Instant::now();
    loop {
        let reply = request(client, json!({ "op": "task_list", "agent": agent }));
        let tasks = reply["tasks"].as_array().cloned().unwrap_or_default();
        if done(&tasks) {
            return tasks;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "{tasks:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn every_message_is_a_run_of_a_task_that_concludes() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "worker", "Does tasks");

    // A new task; the agent concludes its run itself.
    let (session, _) = run_agent(
        &mut client,
        "worker",
        r#"CALL finish {"verdict":"quiet","summary":"nothing new"}"#,
    );
    let tasks = tasks_when(&mut client, "worker", |tasks| {
        tasks.len() == 1 && tasks[0]["latest_run"]["status"] == "succeeded"
    });
    let task = &tasks[0];
    let id = task["id"].as_str().unwrap().to_string();
    assert_eq!(task["session"], session.as_str());
    assert_eq!(task["state"], "done");
    assert_eq!(task["trigger"]["kind"], "manual");
    assert_eq!(task["quiet_runs"], 1);
    let outcome = &task["latest_run"]["outcome"];
    assert_eq!(outcome["verdict"], "quiet");
    assert_eq!(outcome["summary"], "nothing new");
    assert_eq!(outcome["source"], "agent");

    // A message to the task continues its session as another run.
    client.send(json!({ "op": "message_send", "task": id, "body": "and the logs?" }));
    let frames = client.until(delivered_to("worker"));
    assert_eq!(frames.last().unwrap()["session"], session.as_str());
    client.until(ready(&session));
    let tasks = tasks_when(&mut client, "worker", |tasks| {
        tasks[0]["runs"] == 2 && tasks[0]["latest_run"]["ended_at"].is_u64()
    });
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["latest_run"]["trigger"], "reply");
    assert_eq!(tasks[0]["latest_run"]["outcome"]["source"], "inferred");

    // The same question asked twice stays one, and the task needs the user.
    for _ in 0..2 {
        client.send(json!({ "op": "message_send", "task": id,
            "body": r#"CALL question {"title":"Refund this order?","key":"refund"}"# }));
        client.until(delivered_to("worker"));
        client.until(ready(&session));
    }
    let tasks = tasks_when(&mut client, "worker", |tasks| {
        tasks[0]["runs"] == 4 && tasks[0]["latest_run"]["ended_at"].is_u64()
    });
    assert_eq!(tasks[0]["open_questions"], 1);
    assert_eq!(tasks[0]["state"], "needs_you");
    let detail = request(&mut client, json!({ "op": "task_get", "task": id }));
    assert_eq!(detail["questions"].as_array().unwrap().len(), 1);
    assert_eq!(detail["runs"].as_array().unwrap().len(), 4);

    // Closed by the user, then reopened.
    let reply = request(&mut client, json!({ "op": "task_close", "task": id }));
    assert_eq!(reply["type"], "task_closed");
    let tasks = tasks_when(&mut client, "worker", |tasks| tasks[0]["state"] == "closed");
    assert_eq!(tasks[0]["closed_by"], "user");
    let reply = request(&mut client, json!({ "op": "task_reopen", "task": id }));
    assert_eq!(reply["type"], "task_reopened");
    tasks_when(&mut client, "worker", |tasks| {
        tasks[0]["state"] == "needs_you"
    });
}

#[test]
fn a_scheduled_run_is_a_run_of_its_schedule_named_after_it() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "patrol", "Checks things");
    let reply = request(
        &mut client,
        json!({ "op": "schedule_save", "schedule": {
            "agent": "patrol", "name": "巡检（午夜）", "prompt": "check", "repeat": "daily", "time": "00:00",
        } }),
    );
    let schedule = reply["schedule"]["id"].as_str().unwrap().to_string();
    let (session, _) = run_schedule(&mut client, "patrol", &schedule);
    let tasks = tasks_when(&mut client, "patrol", |tasks| {
        tasks.len() == 1 && tasks[0]["latest_run"]["ended_at"].is_u64()
    });
    assert_eq!(tasks[0]["id"], schedule.as_str());
    assert_eq!(tasks[0]["title"], "巡检（午夜）");
    assert_eq!(tasks[0]["trigger"]["kind"], "repeat");
    assert_eq!(tasks[0]["state"], "waiting");
    assert_eq!(tasks[0]["latest_run"]["trigger"], "schedule");
    assert_eq!(tasks[0]["latest_run"]["session"], session.as_str());
    // The session keeps the task's name: the title model leaves it alone.
    thread::sleep(Duration::from_millis(500));
    let sessions = request(&mut client, json!({ "op": "session_list" }));
    let record = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == session.as_str() || s["session"] == session.as_str())
        .cloned()
        .unwrap_or_default();
    assert_eq!(record["title"], "巡检（午夜）", "{record}");
}
