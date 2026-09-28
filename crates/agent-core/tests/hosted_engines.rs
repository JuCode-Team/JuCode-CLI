//! Engines hosted inside one process, the shape the daemon uses: each
//! `AgentCore` is opened on an explicit directory, and an unattended engine
//! defers gated calls instead of blocking on a prompt.
//!
//! The whole binary runs against a temporary HOME and a local fake
//! chat-completions server, so it never reads or writes the developer's
//! `~/.jucode` or calls a real provider.

use jucode_agent_core::{AgentCore, AgentEvent, ApprovalMode};
use serde_json::{json, Value};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

/// Tests share one HOME and one fake server; they run one at a time because
/// the engines share `~/.jucode/config.json`.
fn setup() -> std::sync::MutexGuard<'static, ()> {
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

fn temp_dir(label: &str) -> PathBuf {
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

/// Polls the engine until `done` matches an event (or times out), returning
/// every event seen.
fn pump(core: &mut AgentCore, done: impl Fn(&AgentEvent) -> bool) -> Vec<AgentEvent> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        for event in core.poll_events() {
            let finished = done(&event);
            seen.push(event);
            if finished {
                return seen;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out; events: {seen:#?}");
}

fn is_ready(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::Status(status) if status == "ready")
}

fn assistant_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::AssistantDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn open(cwd: &Path, mode: ApprovalMode) -> AgentCore {
    let mut core = AgentCore::open(cwd.to_path_buf()).unwrap();
    core.set_approval_mode(mode);
    core
}

#[test]
fn engines_in_one_process_work_in_their_own_directories() {
    let _guard = setup();
    let first_dir = temp_dir("first");
    let second_dir = temp_dir("second");
    let mut first = open(&first_dir, ApprovalMode::FullAccess);
    let mut second = open(&second_dir, ApprovalMode::FullAccess);

    first.submit_user_message("RUN: printf first > out.txt".to_string());
    second.submit_user_message("RUN: printf second > out.txt".to_string());
    pump(&mut first, is_ready);
    pump(&mut second, is_ready);

    assert_eq!(
        fs::read_to_string(first_dir.join("out.txt")).unwrap(),
        "first"
    );
    assert_eq!(
        fs::read_to_string(second_dir.join("out.txt")).unwrap(),
        "second"
    );
    assert_ne!(env::current_dir().unwrap(), first_dir);
}

#[test]
fn unattended_engine_defers_a_gated_call_and_runs_it_once_approved() {
    let _guard = setup();
    let dir = temp_dir("deferred");
    let mut core = open(&dir, ApprovalMode::Manual);
    core.set_attended(false);

    core.submit_user_message("RUN: printf ran > marker.txt".to_string());
    let events = pump(&mut core, is_ready);
    let action = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ActionDeferred(action) => Some(action.clone()),
            _ => None,
        })
        .expect("gated call is deferred, not prompted");
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ApprovalRequest { .. })));
    // The turn finished without waiting, and the model was told the call is
    // pending confirmation rather than denied.
    assert!(assistant_text(&events).contains("submitted for confirmation"));
    assert!(!dir.join("marker.txt").exists());
    assert_eq!(action.name, "bash");
    assert_eq!(action.cwd, dir);

    core.decide_action(&action.id, true);
    let events = pump(&mut core, is_ready);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ActionDecided { id, allow: true, is_error: false, .. } if *id == action.id
    )));
    assert_eq!(fs::read_to_string(dir.join("marker.txt")).unwrap(), "ran");
    // The outcome woke the session with a message the model answered.
    assert!(assistant_text(&events).contains(&format!("deferred action {} approved", action.id)));
}

#[test]
fn a_declined_deferred_action_is_reused_for_the_same_call() {
    let _guard = setup();
    let dir = temp_dir("declined");
    let mut core = open(&dir, ApprovalMode::Manual);
    core.set_attended(false);

    core.submit_user_message("RUN: printf no > marker.txt".to_string());
    let events = pump(&mut core, is_ready);
    let id = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ActionDeferred(action) => Some(action.id.clone()),
            _ => None,
        })
        .unwrap();
    core.decide_action(&id, false);
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("declined"));

    // The identical call is denied from the recorded decision: no new
    // deferred action, and nothing ran.
    core.submit_user_message("RUN: printf no > marker.txt".to_string());
    let events = pump(&mut core, is_ready);
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ActionDeferred(_))));
    assert!(assistant_text(&events).contains("denied by user"));
    assert!(!dir.join("marker.txt").exists());
}

#[test]
fn going_unattended_releases_a_call_waiting_on_a_prompt() {
    let _guard = setup();
    let dir = temp_dir("release");
    let mut core = open(&dir, ApprovalMode::Manual);

    core.submit_user_message("RUN: printf later > marker.txt".to_string());
    pump(&mut core, |event| {
        matches!(event, AgentEvent::ApprovalRequest { .. })
    });
    let events = core.set_attended(false);
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::ActionDeferred(_))));
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("submitted for confirmation"));
    assert!(!dir.join("marker.txt").exists());
}
