//! One hosted session: an engine on its own thread, fed ops through a
//! channel and polled every 30 ms, the same loop `jucode serve` runs.

use crate::hub::Hub;
use jucode_agent_core::{
    protocol::{self, session_event_json},
    AgentCore, AgentEvent, ApprovalMode,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::Duration,
};

/// Opens an engine in `cwd` (resuming `resume` when given) on a new thread,
/// set up for `agent` when the session belongs to one. Returns the session
/// id once the engine is ready, with the thread's generation (see
/// `Hub::session_ended`), or the open error.
pub fn spawn(
    hub: Arc<Hub>,
    cwd: PathBuf,
    resume: Option<String>,
    agent: Option<String>,
) -> Result<(String, Sender<Value>, u64), String> {
    let (ops_tx, ops_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let generation = hub.next_generation();
    thread::spawn(move || {
        let core = match open(&hub, cwd, resume.as_deref(), agent.as_deref()) {
            Ok(core) => core,
            Err(error) => {
                let _ = ready_tx.send(Err(error));
                return;
            }
        };
        let id = core.session_id().to_string();
        let _ = ready_tx.send(Ok(id.clone()));
        run(&hub, core, &id, ops_rx);
        hub.session_ended(&id, generation);
    });
    let id = ready_rx
        .recv()
        .map_err(|_| "session thread stopped while opening".to_string())??;
    Ok((id, ops_tx, generation))
}

fn open(
    hub: &Arc<Hub>,
    cwd: PathBuf,
    resume: Option<&str>,
    agent: Option<&str>,
) -> Result<AgentCore, String> {
    let dirs = crate::projects::extra_dirs(hub, &cwd);
    let mut core = AgentCore::open(cwd)
        .map_err(|error| error.to_string())?
        .with_version(hub.version);
    // Nobody watches a session until a client asks to.
    core.set_attended(false);
    core.set_tag_turns(true);
    let result = match resume {
        // Persist the new session right away: a session closed before its
        // first message must still reopen by id.
        None => core.save_session().map_err(|error| error.to_string()),
        Some(id) => resume_session(hub, &mut core, id),
    };
    result?;
    if let Some(agent) = agent.and_then(|id| hub.agents.get(id)) {
        if let Ok(mode) = ApprovalMode::parse(&agent.approval_mode) {
            core.set_approval_mode(mode);
        }
        // Refuse to start rather than run an agent's commands unsandboxed.
        let policy = agent.policy()?;
        policy.check_available()?;
        core.set_sandbox(Some(policy));
        let session = core.session_id().to_string();
        core.set_host_extensions(crate::agent_tools::extensions(
            Arc::clone(hub),
            agent.id,
            session,
        ));
    }
    // After an agent's sandbox, which replaces the default one.
    core.add_writable_dirs(&dirs);
    if resume.is_some_and(|id| crate::requirements::gated(hub, id)) {
        core.set_approval_mode(ApprovalMode::Manual);
    }
    Ok(core)
}

fn resume_session(hub: &Hub, core: &mut AgentCore, id: &str) -> Result<(), String> {
    let (_, events) = core.handle_command(&format!("/resume {id}"));
    if core.session_id() != id {
        let reason = events
            .into_iter()
            .find_map(|event| match event {
                AgentEvent::Error(message) => Some(message),
                _ => None,
            })
            .unwrap_or_else(|| format!("could not resume {id}"));
        return Err(reason);
    }
    let open = hub
        .store
        .open_actions()
        .into_iter()
        .filter(|action| action.session_id == id)
        .collect();
    core.restore_deferred_actions(open);
    Ok(())
}

fn run(hub: &Hub, mut core: AgentCore, id: &str, ops: Receiver<Value>) {
    for event in core.startup_events() {
        publish(hub, id, event);
    }
    let mut last_status = None;
    loop {
        loop {
            match ops.try_recv() {
                Ok(op) if op["op"] == "tui" => match terminal(hub, core, id, &op, &ops) {
                    Some(next) => core = next,
                    None => return,
                },
                Ok(op) => {
                    if apply(hub, &mut core, id, &op) {
                        return;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        for event in core.poll_events() {
            // Usage goes on the turn that spent it (see crate::usage).
            let turn = matches!(event, AgentEvent::Usage { .. })
                .then(|| core.turn_tag().map(str::to_string))
                .flatten();
            publish_on(hub, id, event, turn);
        }
        let status = session_event_json(id, core.model_status_event());
        // Reconciled every tick, so a message that never started a run (an
        // engine error) does not hold a running slot.
        hub.set_busy(id, status["state"] != "ready");
        if last_status.as_ref() != Some(&status) {
            hub.broadcast(&status);
            last_status = Some(status);
        }
        thread::sleep(Duration::from_millis(30));
    }
}

/// The conversation in the jucode TUI, on a terminal for the client that
/// asked: the engine here lets go of the session while the TUI has it, and
/// opens it again once the TUI exits. None when the session should stop.
fn terminal(
    hub: &Hub,
    core: AgentCore,
    id: &str,
    request: &Value,
    ops: &Receiver<Value>,
) -> Option<AgentCore> {
    let client = request["client"].as_u64().unwrap_or(0);
    let refuse = |message: String| {
        let mut error = json!({ "type": "error", "message": message });
        if !request["id"].is_null() {
            error["id"] = request["id"].clone();
        }
        hub.send_to(client, &error);
    };
    let mut core = core;
    if hub.is_busy(id) {
        // `force`: the user agreed to cut the running turn short.
        if request["force"] != true {
            refuse("the running turn must end first".to_string());
            return Some(core);
        }
        for event in core.interrupt() {
            publish(hub, id, event);
        }
    }
    // An agent's session runs as the agent set it up (tools, sandbox).
    let agent = hub
        .store
        .sessions()
        .into_iter()
        .any(|r| r.id == id && r.agent.is_some());
    // The jucode TUI is this binary, or the one JUCODE_BIN names.
    let exe = std::env::var_os("JUCODE_BIN")
        .map(std::path::PathBuf::from)
        .map_or_else(std::env::current_exe, Ok);
    let (Some(arc), Ok(exe), false) = (hub.handle(), exe, agent) else {
        refuse("this session cannot open in the terminal".to_string());
        return Some(core);
    };
    let cwd = core.cwd().to_path_buf();
    // Lets go of the session, its lock with it.
    drop(core);
    let (exit_hub, exit_session) = (Arc::clone(&arc), id.to_string());
    let on_exit: Box<dyn FnOnce() + Send> = Box::new(move || {
        let _ = exit_hub.forward(&exit_session, json!({ "op": "tui_exit" }));
    });
    let mut command = std::process::Command::new(exe);
    command.args(["--resume", id]);
    let tui = crate::terminal::tui(&command, &cwd);
    match crate::terminal::open_command(&arc, client, request, tui, Some(on_exit)) {
        Ok(term) => {
            let surface = json!({ "type": "surface", "surface": "tui", "term": term, "client": client, "session": id });
            hub.broadcast(&surface);
            loop {
                let Ok(op) = ops.recv() else {
                    crate::terminal::kill(hub, &term);
                    return None;
                };
                match op["op"].as_str().unwrap_or_default() {
                    "tui_exit" => break,
                    "shutdown" => {
                        crate::terminal::kill(hub, &term);
                        return None;
                    }
                    "snapshot" => {
                        if let Some(watcher) = op["client"].as_u64() {
                            hub.send_to(watcher, &surface);
                        }
                    }
                    "set_attended" => {}
                    _ => hub.broadcast(&json!({ "type": "error", "session": id, "message": "the conversation is open in its terminal: exit the TUI to continue here" })),
                }
            }
        }
        Err(error) => refuse(error),
    }
    match open(&arc, cwd, Some(id), None) {
        Ok(core) => {
            hub.broadcast(&json!({ "type": "surface", "surface": "gui", "session": id }));
            for event in core.state_events() {
                publish(hub, id, event);
            }
            publish(hub, id, core.transcript_event());
            Some(core)
        }
        Err(error) => {
            hub.broadcast(&json!({ "type": "error", "session": id, "message": error }));
            None
        }
    }
}

/// Applies one op; returns true when the session should stop.
fn apply(hub: &Hub, core: &mut AgentCore, id: &str, op: &Value) -> bool {
    if op["op"] == "snapshot" {
        if let Some(client) = op["client"].as_u64() {
            send_snapshot(hub, core, id, client);
        }
        return false;
    }
    if let Some(reason) = rejected(op) {
        publish(hub, id, AgentEvent::Error(reason));
        return false;
    }
    // Recorded before the engine runs an approved action: if the daemon
    // stops mid-run, the action is not offered again and run twice.
    if op["op"] == "decide_action" {
        if let (Some(action), Some(decision)) = (op["action"].as_str(), op["decision"].as_str()) {
            let _ = hub.store.record_decided(action, decision == "allow");
        }
    }
    hub.note_sent_images(id, op);
    let (quit, events) = protocol::apply_op(core, op);
    if op["claimed"] == true {
        hub.release_claim(id);
    }
    for event in events {
        publish(hub, id, event);
    }
    quit
}

/// Everything a client needs to show a session it starts watching: the
/// startup batch, the conversation so far and the current model status.
/// Sent to that client only; the others already have it.
fn send_snapshot(hub: &Hub, core: &AgentCore, id: &str, client: u64) {
    let mut events = core.state_events();
    events.push(core.transcript_event());
    events.push(AgentEvent::Attended(core.attended()));
    for event in events {
        hub.send_to(client, &session_event_json(id, event));
    }
}

/// Commands that would switch the engine to another session. A hosted
/// engine keeps one session for its whole life, so clients open another
/// session through the daemon instead.
fn rejected(op: &Value) -> Option<String> {
    if op["op"] != "command" {
        return None;
    }
    let input = op["input"].as_str().unwrap_or_default().trim();
    let mut parts = input.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("/new"), _) => {
            Some("/new is not available in a daemon session; use session_create".to_string())
        }
        (Some("/resume"), Some(_)) => {
            Some("/resume <id> is not available in a daemon session; use session_open".to_string())
        }
        _ => None,
    }
}

/// Records deferred-action events before sending any event to clients, so
/// an action a client sees is always one the daemon can restore.
fn publish(hub: &Hub, id: &str, event: AgentEvent) {
    publish_on(hub, id, event, None);
}

/// `publish`, naming the turn a usage event belongs to.
fn publish_on(hub: &Hub, id: &str, event: AgentEvent, turn: Option<String>) {
    if let AgentEvent::ActionDeferred(action) = &event {
        if let Err(error) = hub.store.record_deferred(action) {
            hub.broadcast(&json!({
                    "type": "error",
                    "session": id,
                    "message": format!("failed to record deferred action {}: {error}", action.id),
            }));
        }
    }
    let actions_changed = matches!(
        event,
        AgentEvent::ActionDeferred(_) | AgentEvent::ActionDecided { .. }
    );
    let mut json = session_event_json(id, event);
    hub.attach_sent_images(id, &mut json);
    if let Some(turn) = turn {
        json["turn"] = json!(turn);
    }
    hub.observe(id, &json);
    hub.broadcast(&json);
    if actions_changed {
        hub.broadcast(&hub.actions_json());
    }
}
