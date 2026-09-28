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

#[test]
fn host_tools_run_in_the_host_and_host_prompt_reaches_the_model() {
    use jucode_agent_core::host::HostExtensions;
    use std::sync::{Arc, Mutex};
    let _guard = setup();
    let mut core = open(&temp_dir("host"), ApprovalMode::Manual);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    core.set_host_extensions(HostExtensions {
        tools: vec![serde_json::json!({
            "type": "function",
            "name": "note_down",
            "description": "Record a note.",
            "parameters": { "type": "object", "properties": { "text": { "type": "string" } } }
        })],
        run_tool: Arc::new(move |name, arguments| {
            seen.lock().unwrap().push(format!("{name} {arguments}"));
            ("noted".to_string(), false)
        }),
        prompt: Arc::new(|| "<host-marker>brief goes here</host-marker>".to_string()),
    });

    core.submit_user_message(r#"CALL note_down {"text":"hi"}"#.to_string());
    let events = pump(&mut core, is_ready);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![r#"note_down {"text":"hi"}"#.to_string()]
    );
    // No approval is asked for a host tool, and its output reaches the model.
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ApprovalRequest { .. })));
    assert!(assistant_text(&events).contains("noted"));

    core.submit_user_message("SYSTEM".to_string());
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("<host-marker>brief goes here</host-marker>"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_sandboxed_engine_runs_commands_inside_and_asks_to_leave() {
    use jucode_agent_core::sandbox::{CommandRule, RuleAction, SandboxMode, SandboxPolicy};
    let _guard = setup();
    let dir = temp_dir("sandboxed");
    fs::create_dir_all(dir.join(".git")).unwrap();
    // auto-edit asks for every shell command without a sandbox.
    let mut core = open(&dir, ApprovalMode::AutoEdit);
    core.set_sandbox(Some(SandboxPolicy {
        mode: SandboxMode::WorkspaceWrite,
        writable_dirs: Vec::new(),
        readable_dirs: Vec::new(),
        network: true,
        rules: vec![
            CommandRule {
                prefix: "touch".into(),
                action: RuleAction::Allow,
            },
            CommandRule {
                prefix: "rm -rf".into(),
                action: RuleAction::Forbid,
            },
        ],
    }));
    let approvals = |events: &[AgentEvent]| {
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ApprovalRequest { .. }))
            .count()
    };

    // Inside the sandbox: no approval, workspace writable, .git not.
    core.submit_user_message("RUN: printf ok > inside.txt; printf no > .git/config".to_string());
    let events = pump(&mut core, is_ready);
    assert_eq!(approvals(&events), 0);
    assert_eq!(fs::read_to_string(dir.join("inside.txt")).unwrap(), "ok");
    assert!(!dir.join(".git/config").exists());

    // Leaving the sandbox asks; once allowed it runs outside.
    core.submit_user_message(
        r#"CALL bash {"command":"printf yes > .git/config","escalate":true,"justification":"test"}"#.to_string(),
    );
    let events = pump(&mut core, |event| {
        matches!(event, AgentEvent::ApprovalRequest { .. })
    });
    let call_id = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ApprovalRequest { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .unwrap();
    core.approve(&call_id, true, false, None);
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join(".git/config")).unwrap(), "yes");

    // An allow rule lets that escalation through without asking.
    core.submit_user_message(
        r#"CALL bash {"command":"touch .git/marker","escalate":true}"#.to_string(),
    );
    let events = pump(&mut core, is_ready);
    assert_eq!(approvals(&events), 0);
    assert!(dir.join(".git/marker").exists());

    // A forbid rule never runs.
    fs::write(dir.join("keep.txt"), "keep").unwrap();
    core.submit_user_message(r#"CALL bash {"command":"rm -rf keep.txt"}"#.to_string());
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("forbidden"));
    assert!(dir.join("keep.txt").exists());

    // The model is told about the sandbox and gets the escalate parameter.
    core.submit_user_message("SYSTEM".to_string());
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("<sandbox mode=\"workspace-write\">"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_plain_engine_is_sandboxed_by_default_and_sandbox_switches_it() {
    let _guard = setup();
    let dir = temp_dir("plain-sandbox");
    fs::create_dir_all(dir.join(".git")).unwrap();
    // No agent, no set_sandbox: the config default applies.
    let mut core = open(&dir, ApprovalMode::FullAccess);
    core.submit_user_message("RUN: printf a > file.txt; printf b > .git/config".to_string());
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join("file.txt")).unwrap(), "a");
    assert!(!dir.join(".git/config").exists());

    let (_, events) = core.handle_command("/sandbox");
    let shown = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::Info(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    assert!(shown.starts_with("sandbox: workspace-write"), "{shown}");
    assert!(shown.contains("git push → ask"), "{shown}");

    core.handle_command("/sandbox full-access");
    core.submit_user_message("RUN: printf b > .git/config".to_string());
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join(".git/config")).unwrap(), "b");
}
