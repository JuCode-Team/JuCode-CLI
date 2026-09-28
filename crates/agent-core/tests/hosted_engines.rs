//! Engines hosted inside one process, the shape the daemon uses: each
//! `AgentCore` is opened on an explicit directory, and an unattended engine
//! defers gated calls instead of blocking on a prompt.
//!
//! The whole binary runs against a temporary HOME and a local fake
//! chat-completions server, so it never reads or writes the developer's
//! `~/.jucode` or calls a real provider.

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{setup, temp_dir};
use jucode_agent_core::{AgentCore, AgentEvent, ApprovalMode};
use std::{
    env, fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

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

#[test]
fn config_changes_from_two_engines_both_land() {
    let _guard = setup();
    let mut first = open(&temp_dir("config-first"), ApprovalMode::Manual);
    let mut second = open(&temp_dir("config-second"), ApprovalMode::Manual);
    let server = |name: &str| {
        serde_json::json!({
            "name": name, "transport": "stdio", "command": "true", "enabled": false,
        })
    };
    // `second` loaded the config before `first` changed it; its save must
    // not drop `first`'s server.
    first.mcp_set(&server("from_first"));
    second.mcp_set(&server("from_second"));

    let config_path = std::path::PathBuf::from(env::var("HOME").unwrap())
        .join(".jucode")
        .join("config.json");
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let names: Vec<&str> = saved["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert!(names.contains(&"from_first"), "{names:?}");
    assert!(names.contains(&"from_second"), "{names:?}");

    second.mcp_remove("from_first");
    second.mcp_remove("from_second");
}
