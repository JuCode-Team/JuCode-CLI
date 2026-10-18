//! The built-in ACP agents installed on this machine, live, through a real
//! daemon. Every test is ignored: they run the agents on their own models
//! (or the JuCode gateway) and cost a few tiny prompts each. Run with
//! `cargo test -p jucode-daemon --test live_acp -- --ignored --nocapture`.
//! An agent that is not installed is skipped with a note. Set
//! `JUCODE_LIVE_ACP_MODEL_<ID>` (`OPENCODE`, `OMP`, `PI`) to a model value
//! from the agent's `/model` list to run on a cheap one, and
//! `JUCODE_LIVE_ACP_GATEWAY=1` to run the full test of omp or Pi on the
//! JuCode gateway when their own providers do not answer.

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

const DEADLINE: Duration = Duration::from_secs(300);

struct Client {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

fn temp_dir(label: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "jucode-live-acp-{label}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A daemon on a free loopback port with its own state, and a local client.
fn start(label: &str) -> Client {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let store = Store::open(temp_dir(label).join("state").join("daemon")).unwrap();
    let token = store.token().unwrap();
    thread::spawn(move || jucode_daemon::serve(listener, store, None, "live", None));
    let (socket, _) = tungstenite::connect(format!("ws://{address}/?token={token}")).unwrap();
    if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
    }
    let mut client = Client { socket, next_id: 1 };
    client.until(|f| f["type"] == "hello");
    client
}

impl Client {
    fn send(&mut self, frame: Value) {
        self.socket.send(Message::text(frame.to_string())).unwrap();
    }

    fn until(&mut self, mut done: impl FnMut(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + DEADLINE;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                    if frame["type"] == "error" {
                        println!("error: {}", frame["message"]);
                    }
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
                Err(error) => panic!("socket error {error}"),
            }
        }
        panic!(
            "timed out; last frames: {:#?}",
            &seen[seen.len().saturating_sub(20)..]
        );
    }

    fn request(&mut self, mut op: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        op["id"] = json!(id);
        self.send(op);
        self.until(|f| f["id"] == id).pop().unwrap()
    }
}

fn of<'a>(session: &'a str, kind: &'a str) -> impl Fn(&Value) -> bool + 'a {
    move |f| f["session"] == session && f["type"] == kind
}

/// The end of a turn: `ready` after something of the turn arrived.
fn turn_done(session: &str) -> impl FnMut(&Value) -> bool + '_ {
    let mut started = false;
    move |f| {
        started |= f["session"] == session && f["type"] == "user_message";
        started && f["session"] == session && f["type"] == "status" && f["message"] == "ready"
    }
}

/// Whether the preset can start here; says why not.
fn ready(client: &mut Client, id: &str) -> bool {
    let presets = client.request(json!({ "op": "acp_presets" }));
    let preset = presets["presets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .unwrap()
        .clone();
    println!("{id}: {preset}");
    if preset["status"] != "ready" {
        println!(
            "skipped: {id} is {} (install: {})",
            preset["status"], preset["install"]
        );
        return false;
    }
    true
}

/// A session of preset `id` in a new directory holding `b.txt`.
fn create(client: &mut Client, id: &str, gateway: bool) -> (String, PathBuf) {
    let dir = temp_dir(id);
    fs::write(dir.join("b.txt"), "original\n").unwrap();
    let created = client.request(json!({
        "op": "session_create", "cwd": dir, "engine": "acp",
        "options": { "agent": id, "approval_mode": "read-only", "jucode_gateway": gateway },
    }));
    let session = created["session"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    client.send(json!({ "op": "watch", "session": session }));
    (session, dir)
}

fn prompt(client: &mut Client, session: &str, text: &str) -> Vec<Value> {
    client.send(json!({ "op": "user_message", "session": session, "content": text }));
    client.until(turn_done(session))
}

/// Asks for `b.txt` to hold `word`, allowing or refusing every approval:
/// how many were asked, and the turn's frames.
fn edit(client: &mut Client, session: &str, word: &str, allow: bool) -> (usize, Vec<Value>) {
    client.send(json!({ "op": "user_message", "session": session,
        "content": format!("Use your edit or write tool to replace the whole content of b.txt with exactly the word: {word}") }));
    let mut asked = 0;
    let mut finished = turn_done(session);
    let mut frames = Vec::new();
    loop {
        let batch = client.until(|f| of(session, "approval_request")(f) || finished(f));
        let last = batch.last().unwrap().clone();
        frames.extend(batch);
        if last["type"] != "approval_request" {
            return (asked, frames);
        }
        asked += 1;
        println!("asked: {} (allow: {allow})", last["summary"]);
        let decision = if allow { "allow" } else { "deny" };
        let mut answer = json!({ "op": "approve", "session": session, "call_id": last["call_id"], "decision": decision });
        if last["name"] == "mcp_elicitation" {
            let question = &last["questions"][0];
            let label = if allow { "Approve" } else { "Deny" };
            let choice = question["options"]
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["label"] == label)
                .or(question["options"].get(0))
                .map(|o| o["label"].clone())
                .unwrap();
            answer["answers"] = json!({ question["question"].as_str().unwrap(): choice });
        }
        client.send(answer);
    }
}

fn run(id: &str) {
    let mut client = start(id);
    if !ready(&mut client, id) {
        return;
    }
    // 1. Open: caps and the model.
    let gateway = std::env::var_os("JUCODE_LIVE_ACP_GATEWAY").is_some();
    let (session, dir) = create(&mut client, id, gateway);
    // Caps can change while the conversation opens; the last before `ready`
    // stand.
    let opened = client.until(|f| of(&session, "status")(f) && f["message"] == "ready");
    let caps = opened
        .iter()
        .rfind(|f| of(&session, "engine_caps")(f))
        .unwrap_or_else(|| panic!("no engine_caps: {opened:#?}"))
        .clone();
    for f in &opened {
        println!("open: {}", f["type"]);
    }
    println!("caps: {caps}");
    assert_eq!(caps["caps"]["interrupt"], true);
    assert_eq!(caps["caps"]["transcriptReplay"], true);
    assert_eq!(caps["caps"]["modelPicker"], true);
    let status = opened
        .iter()
        .find(|f| f["type"] == "model_status")
        .unwrap_or_else(|| panic!("no model_status: {opened:#?}"));
    assert!(!status["model"].as_str().unwrap_or_default().is_empty());
    if let Ok(model) = std::env::var(format!("JUCODE_LIVE_ACP_MODEL_{}", id.to_uppercase())) {
        client.send(
            json!({ "op": "command", "session": session, "input": format!("/model {model}") }),
        );
        client.until(|f| of(&session, "model_status")(f) && f["model"] == model.as_str());
    }

    // A setting besides the model (OpenCode's and omp's mode): switched there
    // and back.
    let options = opened
        .iter()
        .rfind(|f| of(&session, "session_options")(f))
        .map(|f| f["options"].clone())
        .unwrap_or_default();
    println!("session options: {options}");
    if let Some(option) = options
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["type"] == "select" && o["choices"].as_array().is_some_and(|c| c.len() > 1))
    {
        let first = option["value"].clone();
        let other = option["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["value"].clone())
            .find(|v| *v != first)
            .unwrap();
        for value in [other, first] {
            client.send(json!({ "op": "set_session_option", "session": session, "id": option["id"], "value": value }));
            client.until(|f| {
                of(&session, "session_options")(f)
                    && f["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|o| o["id"] == option["id"] && o["value"] == value)
            });
            println!("{} = {value}", option["id"]);
        }
    }

    // 2. A reply, streamed, and the context it used.
    let frames = prompt(&mut client, &session, "Reply with exactly: hello");
    let reply: String = frames
        .iter()
        .filter(|f| of(&session, "assistant_delta")(f))
        .filter_map(|f| f["delta"].as_str())
        .collect();
    println!("reply: {reply:?}");
    assert!(!reply.is_empty(), "{frames:#?}");
    assert!(frames.iter().any(of(&session, "context_usage")));

    // 2b. The conversation in the agent's own TUI, quit there, and back.
    let opened =
        client.request(json!({ "op": "session_tui", "session": session, "cols": 100, "rows": 30 }));
    let term = opened["term"]
        .as_str()
        .unwrap_or_else(|| panic!("{opened}"))
        .to_string();
    client.until(|f| f["type"] == "term_output" && f["term"] == term.as_str());
    thread::sleep(Duration::from_secs(3));
    let quit = if id == "opencode" { "\u{3}" } else { "\u{4}" };
    for _ in 0..2 {
        let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, quit);
        client.send(json!({ "op": "term_input", "term": term, "data": data }));
        thread::sleep(Duration::from_secs(1));
    }
    // Still running: closed from here.
    thread::sleep(Duration::from_secs(3));
    client.send(json!({ "op": "term_close", "term": term }));
    client.until(|f| of(&session, "surface")(f) && f["surface"] == "gui");
    client.until(|f| {
        of(&session, "transcript")(f)
            && f["items"].as_array().is_some_and(|items| {
                items.iter().any(|i| {
                    i["role"] == "user"
                        && i["content"]
                            .as_str()
                            .is_some_and(|c| c.contains("Reply with exactly: hello"))
                })
            })
    });
    client.until(|f| of(&session, "status")(f) && f["message"] == "ready");

    // 3. An edit: asked first, unless the agent never asks; then one refused.
    let (asked, frames) = edit(&mut client, &session, "changed", true);
    if caps["approvals"] == true {
        assert!(asked > 0, "no approval asked: {frames:#?}");
        assert!(frames.iter().any(of(&session, "tool_output")));
        assert_eq!(
            fs::read_to_string(dir.join("b.txt")).unwrap().trim(),
            "changed"
        );
        let (asked, frames) = edit(&mut client, &session, "refused", false);
        assert!(asked > 0, "no approval asked: {frames:#?}");
        assert_eq!(
            fs::read_to_string(dir.join("b.txt")).unwrap().trim(),
            "changed"
        );
        // Another approval mode restarts the agent on the same conversation
        // and model.
        client.send(json!({ "op": "set_approval_mode", "session": session, "mode": "auto-edit" }));
        let frames = client.until(|f| {
            of(&session, "transcript")(f)
                && f["items"].as_array().is_some_and(|items| {
                    items
                        .iter()
                        .any(|i| i["role"] == "user" && i["content"] == "Reply with exactly: hello")
                })
        });
        let frames = [
            frames,
            client.until(|f| of(&session, "status")(f) && f["message"] == "ready"),
        ]
        .concat();
        for f in frames
            .iter()
            .filter(|f| f["type"] == "approval_mode" || f["type"] == "model_status")
        {
            println!("after the mode switch: {f}");
        }
    } else {
        assert_eq!(caps["approvals"], false);
        assert_eq!(asked, 0);
    }

    // 4. A long turn, interrupted.
    client.send(json!({ "op": "user_message", "session": session,
        "content": "Count from 1 to 400, one number per line, nothing else." }));
    client.until(of(&session, "assistant_delta"));
    client.send(json!({ "op": "interrupt", "session": session }));
    client.until(|f| of(&session, "status")(f) && f["message"] == "interrupted");
    client.until(|f| of(&session, "status")(f) && f["message"] == "ready");

    // 5. Closed and reopened, the agent replays the conversation.
    client.send(json!({ "op": "session_close", "session": session }));
    client.until(|f| f["type"] == "session_closed" && f["session"] == session.as_str());
    let reopened = client.request(json!({
        "op": "session_open", "session": session, "engine": "acp",
        "options": { "agent": id, "approval_mode": "read-only", "jucode_gateway": gateway },
    }));
    assert_eq!(reopened["type"], "session_opened", "{reopened}");
    client.send(json!({ "op": "watch", "session": session }));
    client.until(|f| {
        of(&session, "transcript")(f)
            && f["items"].as_array().is_some_and(|items| {
                items.iter().any(|i| {
                    i["role"] == "user"
                        && i["content"]
                            .as_str()
                            .is_some_and(|c| c.contains("Reply with exactly: hello"))
                })
            })
    });
    client.send(json!({ "op": "session_close", "session": session }));
}

/// On the JuCode gateway: a reply, the agent's model file in JuCode's own
/// directory for it, and the conversation kept when switched off it.
fn gateway(id: &str) {
    if jucode_agent_core::jucode_gateway_credentials().is_err() {
        println!("skipped: not signed in to JuCode");
        return;
    }
    let mut client = start(id);
    if !ready(&mut client, id) {
        return;
    }
    let (session, _) = create(&mut client, id, true);
    client.until(of(&session, "engine_caps"));
    let frames = prompt(&mut client, &session, "Reply with exactly: hello");
    assert!(
        frames.iter().any(of(&session, "assistant_delta")),
        "{frames:#?}"
    );
    let file = if id == "omp" {
        "models.yml"
    } else {
        "models.json"
    };
    let dir = home().join(".jucode/daemon/acp").join(id);
    assert!(dir.join(file).is_file(), "{}", dir.display());
    // Off the gateway, the agent reopens the same conversation.
    client.send(json!({ "op": "set_gateway", "session": session, "gateway": false }));
    client.until(|f| {
        of(&session, "transcript")(f)
            && f["items"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|i| i["role"] == "user" && i["content"] == "Reply with exactly: hello")
            })
    });
    client.send(json!({ "op": "session_close", "session": session }));
}

/// The effort control without a prompt: the levels the agent offers (its
/// unset value apart), a level picked and the unset value picked back, each
/// confirmed by the agent's options.
fn efforts(id: &str) {
    let mut client = start(id);
    if !ready(&mut client, id) {
        return;
    }
    let (session, _) = create(&mut client, id, false);
    let opened = client.until(|f| of(&session, "status")(f) && f["message"] == "ready");
    let status = opened
        .iter()
        .rfind(|f| of(&session, "model_status")(f))
        .unwrap_or_else(|| panic!("no model_status: {opened:#?}"))
        .clone();
    println!("model_status: {status}");
    let model = status["model"].as_str().unwrap().to_string();
    let levels = status["reasoning_efforts"].as_array().unwrap().clone();
    let unset = status["default_effort"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!levels.is_empty(), "{status}");
    assert!(
        levels.iter().all(|l| l != "default" && l != "auto"),
        "{status}"
    );
    let mut pick = |effort: &str| {
        client.send(json!({ "op": "command", "session": session, "input": format!("/model {model} {effort}") }));
        let seen =
            client.until(|f| of(&session, "model_status")(f) && f["reasoning_effort"] == effort);
        println!("set {effort}: {}", seen.last().unwrap());
    };
    let level = levels
        .iter()
        .find(|l| **l != status["reasoning_effort"])
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    pick(&level);
    if !unset.is_empty() {
        pick(&unset);
    }
    client.send(json!({ "op": "session_close", "session": session }));
}

#[test]
#[ignore]
fn opencode_efforts() {
    efforts("opencode");
}

#[test]
#[ignore]
fn omp_efforts() {
    efforts("omp");
}

#[test]
#[ignore]
fn pi_efforts() {
    efforts("pi");
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap())
}

#[test]
#[ignore]
fn opencode() {
    run("opencode");
}

#[test]
#[ignore]
fn omp() {
    run("omp");
}

#[test]
#[ignore]
fn pi() {
    run("pi");
}

#[test]
#[ignore]
fn omp_on_the_gateway() {
    gateway("omp");
}

#[test]
#[ignore]
fn pi_on_the_gateway() {
    gateway("pi");
}
