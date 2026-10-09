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

/// A scripted model command that writes `text` to `file` in the session's
/// directory, in the shell the platform runs.
fn write_command(text: &str, file: &str) -> String {
    if cfg!(windows) {
        format!(
            "RUN: [System.IO.File]::WriteAllText((Join-Path (Get-Location) '{file}'), '{text}')"
        )
    } else {
        format!("RUN: printf {text} > {file}")
    }
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
        exclusive: false,
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

#[test]
fn an_exclusive_host_leaves_the_engine_no_tools_of_its_own() {
    use jucode_agent_core::host::HostExtensions;
    use std::sync::Arc;
    let _guard = setup();
    let dir = temp_dir("exclusive");
    let mut core = open(&dir, ApprovalMode::FullAccess);
    core.set_host_extensions(HostExtensions {
        tools: vec![serde_json::json!({
            "type": "function",
            "name": "note_down",
            "description": "Record a note.",
            "parameters": { "type": "object", "properties": {} }
        })],
        run_tool: Arc::new(|_, _| ("noted".to_string(), false)),
        prompt: Arc::new(String::new),
        exclusive: true,
    });
    core.submit_user_message(r#"CALL bash {"command":"touch made-by-bash"}"#.to_string());
    let events = pump(&mut core, is_ready);
    assert!(!dir.join("made-by-bash").exists());
    assert!(
        assistant_text(&events).contains("unknown tool"),
        "{}",
        assistant_text(&events)
    );
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

#[test]
fn subagents_command_saves_the_models_subagents_may_use() {
    let _guard = setup();
    let mut core = open(&temp_dir("subagents"), ApprovalMode::Manual);
    let config_path = std::path::PathBuf::from(env::var("HOME").unwrap())
        .join(".jucode")
        .join("config.json");
    let saved = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap()
    };

    let (_, events) = core.handle_command("/subagents add no-such-model search");
    assert!(
        matches!(&events[..], [AgentEvent::Error(error)] if error.contains("unknown model")),
        "{events:?}"
    );

    let (_, events) = core.handle_command("/subagents add fake-model wide code search");
    assert!(matches!(&events[..], [AgentEvent::Status(_)]), "{events:?}");
    assert_eq!(
        saved()["subagent_models"],
        serde_json::json!([{ "name": "fake-model", "description": "wide code search" }])
    );

    let (_, events) = core.handle_command("/subagents");
    assert!(
        matches!(&events[..], [AgentEvent::Info(info)] if info.contains("fake-model — wide code search")),
        "{events:?}"
    );

    core.handle_command("/subagents remove fake-model");
    assert_eq!(saved()["subagent_models"], serde_json::json!([]));
}

fn plan_event(events: &[AgentEvent]) -> Option<(String, String, String)> {
    events.iter().rev().find_map(|event| match event {
        AgentEvent::ProposedPlan {
            id, title, status, ..
        } => Some((id.clone(), title.clone(), status.clone())),
        _ => None,
    })
}

#[test]
fn plan_mode_refuses_changes_and_tells_the_model_to_plan() {
    let _guard = setup();
    let dir = temp_dir("plan-refuse");
    let mut core = open(&dir, ApprovalMode::Plan);

    core.submit_user_message(write_command("x", "marker.txt"));
    let events = pump(&mut core, is_ready);
    assert!(!dir.join("marker.txt").exists());
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ApprovalRequest { .. })));
    let reply = assistant_text(&events);
    assert!(reply.contains("plan mode"), "{reply}");
    assert!(reply.contains("propose_plan"), "{reply}");

    // The plan-mode rules are part of the prompt.
    core.submit_user_message("SYSTEM".to_string());
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("</plan_mode>"));
}

#[test]
fn a_proposed_plan_waits_then_runs_in_the_approved_mode() {
    let _guard = setup();
    let dir = temp_dir("plan-approve");
    let mut core = open(&dir, ApprovalMode::Plan);

    let args = serde_json::json!({ "title": "Add marker", "plan": "| File | Change |\n|---|---|\n| marker.txt | create |" });
    core.submit_user_message(format!("CALL propose_plan {args}"));
    let events = pump(&mut core, is_ready);
    let (id, title, status) = plan_event(&events).expect("proposed_plan event");
    assert_eq!((title.as_str(), status.as_str()), ("Add marker", "pending"));
    // Bare `/plan` shows the waiting plan again.
    let (_, events) = core.handle_command("/plan");
    assert_eq!(plan_event(&events).map(|plan| plan.0), Some(id.clone()));
    // The turn ended at the plan: the model did not get to answer the tool.
    assert!(!assistant_text(&events).contains("tool said"));

    // A revision keeps plan mode and asks the model again (the TUI's
    // `/plan` command, the text twin of `approve_plan`).
    let (_, events) = core.handle_command(&format!("/plan {id} revise also add a test"));
    assert_eq!(plan_event(&events).unwrap().2, "revising");
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("also add a test"));

    let (_, events) = core.handle_command(&format!("/plan {id} approve full-access keep it short"));
    assert_eq!(plan_event(&events).unwrap().2, "approved");
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ApprovalMode { mode } if mode == "full-access"
    )));
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("approved the plan"));
    assert!(assistant_text(&events).contains("Their notes: keep it short"));
    let again = core.approve_plan(&id, true, None, "");
    assert!(matches!(again.as_slice(), [AgentEvent::Error(_)]));
    let (_, events) = core.handle_command("/plan");
    assert!(plan_event(&events).is_none());

    // Reloading the session shows the plan once, with its latest status.
    let transcript = core.transcript_event();
    let AgentEvent::Transcript(items) = transcript else {
        panic!("transcript event")
    };
    let plans: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            jucode_agent_core::TranscriptItem::Plan { status, .. } => Some(status.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(plans, vec!["approved".to_string()]);
}

#[test]
fn subagents_report_their_work_to_the_agent_trace() {
    let _guard = setup();
    let dir = temp_dir("trace");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let args = serde_json::json!({ "task_name": "lister", "message": "RUN: ls" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    // Whole batches: the lifecycle events follow `ready` in the same batch.
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !events.iter().any(is_ready) && Instant::now() < deadline {
        events.extend(core.poll_events());
        thread::sleep(Duration::from_millis(10));
    }
    // The child runs on its own thread; wait until it has finished.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let AgentEvent::AgentRuns(rows) = core.agent_runs_event() else {
            unreachable!()
        };
        if rows
            .iter()
            .any(|row| row["state"] != "running" && row["state"] != "pending")
        {
            break;
        }
        events.extend(core.poll_events());
        thread::sleep(Duration::from_millis(20));
    }

    let lifecycle = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::SubagentLifecycle {
                label, tool_use_id, ..
            } => Some((label.clone(), tool_use_id.clone())),
            _ => None,
        })
        .expect("subagent_lifecycle");
    assert_eq!(lifecycle, ("lister".to_string(), "call_1".to_string()));
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::AgentRuns(_))));

    let AgentEvent::AgentRuns(rows) = core.agent_runs_event() else {
        unreachable!()
    };
    let row = rows
        .iter()
        .find(|row| row["label"] == "lister")
        .expect("row");
    assert_eq!(row["id"], "/root/lister");
    assert_eq!(row["tool_use_id"], "call_1");
    assert_eq!(row["type"], "subagent");
    assert_eq!(row["prompt"], "RUN: ls");
    assert!(row["started_at"].as_u64().unwrap() > 1_700_000_000_000);

    let AgentEvent::SubagentTranscript {
        items: Some(items), ..
    } = core.subagent_transcript_event("/root/lister")
    else {
        panic!("transcript")
    };
    // The child may still be working when the parent's turn ends and closes
    // it, so only its task is certain.
    assert_eq!(
        items[0],
        serde_json::json!({ "role": "user", "content": "RUN: ls" })
    );
    let AgentEvent::SubagentTranscript { items: None, .. } =
        core.subagent_transcript_event("/root/nobody")
    else {
        panic!("unknown agent")
    };

    // A later turn keeps the earlier turn's agent in the trace.
    core.submit_user_message("hello".to_string());
    pump(&mut core, is_ready);
    let AgentEvent::AgentRuns(rows) = core.agent_runs_event() else {
        unreachable!()
    };
    assert!(rows.iter().any(|row| row["label"] == "lister"));
    assert!(matches!(
        core.subagent_transcript_event("/root/lister"),
        AgentEvent::SubagentTranscript { items: Some(_), .. }
    ));
}

#[test]
fn a_steered_message_joins_the_running_turn_without_stopping_its_tool() {
    let _guard = setup();
    let dir = temp_dir("steer");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    // A command that takes a moment, so the message arrives while it runs.
    let slow = if cfg!(windows) {
        "RUN: Start-Sleep -Milliseconds 800; Set-Content done.txt ok".to_string()
    } else {
        "RUN: sleep 0.8 && printf ok > done.txt".to_string()
    };
    core.submit_user_message(slow);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    while Instant::now() < deadline
        && !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { .. }))
    {
        events.extend(core.poll_events());
        thread::sleep(Duration::from_millis(5));
    }
    core.submit_user_message("also check the weather".to_string());
    events.extend(core.steer());
    events.extend(pump(&mut core, is_ready));

    // The tool was not killed: its file exists.
    assert_eq!(fs::read_to_string(dir.join("done.txt")).unwrap(), "ok");
    // The message reached the model in the same turn (it answered it), and
    // shows as the user's message.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::UserMessage(m) if m == "also check the weather")));
    assert!(assistant_text(&events).contains("also check the weather"));
    let turns = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Status(s) if s == "ready"))
        .count();
    assert_eq!(turns, 1, "one turn, not a restart");
}

#[test]
fn an_interrupt_requeues_unread_steers_ahead_of_later_messages() {
    let _guard = setup();
    let dir = temp_dir("steer-interrupt");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let slow = if cfg!(windows) {
        "RUN: Start-Sleep -Seconds 5".to_string()
    } else {
        "RUN: sleep 5".to_string()
    };
    core.submit_user_message(slow);
    pump(&mut core, |e| matches!(e, AgentEvent::ToolStart { .. }));
    // Steered while the tool runs, so the model has not read it yet.
    core.submit_user_message("first".to_string());
    core.steer();
    core.submit_user_message("second".to_string());
    let events = core.interrupt();
    // Shown again, ahead of the message sent after it.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::PendingMessages(texts) if *texts == ["first", "second"]
    )));

    let mut seen = Vec::new();
    while !seen
        .iter()
        .any(|e| matches!(e, AgentEvent::UserMessage(m) if m == "second"))
    {
        seen.extend(pump(&mut core, is_ready));
    }
    let order: Vec<&str> = seen
        .iter()
        .filter_map(|e| match e {
            AgentEvent::UserMessage(m) => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["first", "second"]);
}
