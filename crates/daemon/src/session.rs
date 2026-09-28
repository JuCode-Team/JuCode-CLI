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
/// id once the engine is ready, or the open error.
pub fn spawn(
    hub: Arc<Hub>,
    cwd: PathBuf,
    resume: Option<String>,
    agent: Option<String>,
) -> Result<(String, Sender<Value>), String> {
    let (ops_tx, ops_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
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
        hub.session_ended(&id);
    });
    let id = ready_rx
        .recv()
        .map_err(|_| "session thread stopped while opening".to_string())??;
    Ok((id, ops_tx))
}

fn open(
    hub: &Arc<Hub>,
    cwd: PathBuf,
    resume: Option<&str>,
    agent: Option<&str>,
) -> Result<AgentCore, String> {
    let mut core = AgentCore::open(cwd)
        .map_err(|error| error.to_string())?
        .with_version(hub.version);
    // Nobody watches a session until a client asks to.
    core.set_attended(false);
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
        let session = core.session_id().to_string();
        core.set_host_extensions(crate::agent_tools::extensions(
            Arc::clone(hub),
            agent.id,
            session,
        ));
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
            publish(hub, id, event);
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
        if let (Some(action), Some(decision)) = (op["id"].as_str(), op["decision"].as_str()) {
            let _ = hub.store.record_decided(action, decision == "allow");
        }
    }
    let (quit, events) = protocol::apply_op(core, op);
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
    if let AgentEvent::ActionDeferred(action) = &event {
        if let Err(error) = hub.store.record_deferred(action) {
            hub.broadcast(&json!({
                    "type": "error",
                    "session": id,
                    "message": format!("failed to record deferred action {}: {error}", action.id),
            }));
        }
    }
    hub.broadcast(&session_event_json(id, event));
}
