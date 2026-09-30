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

    // A second daemon on the same state stands in for a restart.
    let restarted = start_daemon_on(daemon.state.clone(), daemon.agents.clone());
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
fn a_report_is_listed_until_read_and_wakes_nobody() {
    let _guard = setup();
    let daemon = start_daemon();
    let mut client = Client::connect(&daemon);
    create_agent(&mut client, "reporter", "Reports");
    let (_, frames) = run_agent(
        &mut client,
        "reporter",
        r#"CALL report {"title":"Login fixed","body":"tests: 12 pass"}"#,
    );
    let posted = frames
        .iter()
        .find(|frame| frame["type"] == "report_posted")
        .expect("the report is broadcast")["report"]
        .clone();
    assert_eq!(posted["title"], "Login fixed");
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
    // A disabled agent takes no new messages.
    client.send(json!({ "op": "message_send", "agent": "paged", "body": "hi", "id": 3 }));
    client.until(|frame| frame["id"] == 3);
    thread::sleep(Duration::from_millis(1500));
    let log = fs::read_to_string(daemon.state.join("messages.jsonl")).unwrap();
    assert!(log.contains("undeliverable"), "{log}");
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

/// The end of a turn: `ready` after the reply (the first turn's init also
/// says ready).
fn turn_done(session: &str) -> impl FnMut(&Value) -> bool + '_ {
    let mut replied = false;
    move |f| {
        replied |= f["session"] == session && f["type"] == "assistant_delta";
        replied && is_ready(session)(f)
    }
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
    let history = request(&mut desktop, json!({ "op": "session_history", "cwd": dir }));
    assert!(
        history["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["session"] == session.as_str()
                && s["engine"] == "claude"
                && s["title"] == "hello"),
        "{history}"
    );
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
    client.send(json!({ "op": "user_message", "session": session, "content": "first" }));
    client.until(turn_done(&session));

    client.send(json!({ "op": "set_approval_mode", "session": session, "mode": "full-access" }));
    client.until(|f| {
        f["session"] == session.as_str() && f["type"] == "approval_mode" && f["mode"] == "full-auto"
    });
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
        json!([{ "role": "user", "content": "first" }, { "role": "assistant", "content": "ok: first" }])
    );
    assert!(starts(&log)
        .pop()
        .unwrap()
        .windows(2)
        .any(|w| w == ["--resume", session.as_str()]));
}
