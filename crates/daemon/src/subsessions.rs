//! One session handing work to another: the session tools
//! (`start_session`, `wait_session`, `send_session`, see
//! `jucode_agent_core::user_tools`). A child is an ordinary session in the
//! parent's folder, on any engine, in the parent's permission mode; the user
//! sees it in the session list, can open it, and answers its approvals (the
//! parent only learns that it waits). The parent gets the child's last reply
//! once a turn ends. A child cannot start children of its own.
//!
//! What a child is doing is kept in memory, from the events it publishes
//! (`observe`); after a daemon restart the parent cannot wait on it again,
//! though the session itself stays.

use crate::{
    engines,
    hub::{lock, Hub},
};
use jucode_agent_core::user_tools::{SEND_SESSION, START_SESSION, WAIT_SESSION};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const DEFAULT_WAIT_SECS: u64 = 600;
const MAX_WAIT_SECS: u64 = 1800;
const POLL: Duration = Duration::from_millis(300);

/// A session started by another.
#[derive(Debug, Clone, Default)]
pub struct Child {
    pub parent: String,
    /// `running`, `waiting_approval` (the user must approve something),
    /// `done`, `failed` (why in `error`) or `closed` (its engine stopped).
    pub status: String,
    /// The last reply of its latest turn, so far.
    pub reply: String,
    pub error: Option<String>,
}

/// The session making the call.
pub struct Parent {
    pub session: String,
    pub cwd: PathBuf,
    /// Its permission mode now, in client names.
    pub mode: String,
    /// Claude / Codex: it runs on the JuCode gateway (its children on
    /// Claude Code or Codex do too); jucode sessions always do.
    pub gateway: bool,
}

/// Runs a session tool for `parent`: the JSON output, or what the model
/// should fix. `wait_session` blocks until the child's turn ends or the
/// wait does.
pub fn call(hub: &Arc<Hub>, parent: &Parent, name: &str, args: &Value) -> Result<Value, String> {
    if lock(&hub.children).contains_key(&parent.session) {
        return Err(
            "a session started by another session cannot start or drive sessions: do the task yourself"
                .to_string(),
        );
    }
    let text = |key: &str| args[key].as_str().map(str::trim).unwrap_or_default();
    match name {
        START_SESSION => start(
            hub,
            parent,
            text("engine"),
            text("prompt"),
            text("title"),
            text("model"),
        ),
        WAIT_SESSION => {
            let secs = args["timeout_seconds"]
                .as_u64()
                .unwrap_or(DEFAULT_WAIT_SECS)
                .clamp(10, MAX_WAIT_SECS);
            wait(hub, parent, text("session"), Duration::from_secs(secs))
        }
        SEND_SESSION => send(hub, parent, text("session"), text("prompt")),
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn start(
    hub: &Arc<Hub>,
    parent: &Parent,
    engine: &str,
    prompt: &str,
    title: &str,
    model: &str,
) -> Result<Value, String> {
    if prompt.is_empty() {
        return Err("prompt must not be empty".to_string());
    }
    if !matches!(engine, "claude" | "codex" | "jucode") {
        return Err("engine must be claude, codex or jucode".to_string());
    }
    let kind = engines::Kind::parse(engine)?;
    let mode = crate::requirements::engine_mode(kind, &parent.mode).unwrap_or(if kind.is_none() {
        "manual"
    } else {
        "read-only"
    });
    let options = engines::Options {
        approval_mode: Some(mode.to_string()),
        model: (!model.is_empty()).then(|| model.to_string()),
        gateway: kind.map(|_| parent.gateway),
        ..engines::Options::default()
    };
    // The child belongs to its parent's project, wherever the parent runs.
    let project = crate::projects::of_session(hub, &parent.session);
    let session = hub.create_engine_session(
        Some(parent.cwd.clone()),
        None,
        false,
        kind,
        options,
        Some(&project),
    )?;
    let title = match title {
        "" => prompt
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(60)
            .collect(),
        title => title.to_string(),
    };
    let _ = hub.store.record_session_meta(
        &session,
        &json!({ "title": title, "parent": parent.session }),
    );
    lock(&hub.children).insert(
        session.clone(),
        Child {
            parent: parent.session.clone(),
            status: "running".to_string(),
            ..Child::default()
        },
    );
    // The jucode engine takes no start options.
    let sent = hub
        .forward(&session, json!({ "op": "set_approval_mode", "mode": mode }))
        .and_then(|()| hub.send_to_session(&session, prompt));
    if let Err(error) = sent {
        lock(&hub.children).remove(&session);
        return Err(error);
    }
    hub.broadcast(&hub.sessions_json());
    Ok(json!({ "session": session, "engine": engine, "title": title, "status": "running" }))
}

/// The child `session` of `parent`, or why it is not one.
fn child(hub: &Hub, parent: &Parent, session: &str) -> Result<Child, String> {
    lock(&hub.children)
        .get(session)
        .filter(|child| child.parent == parent.session)
        .cloned()
        .ok_or_else(|| {
            format!("{session} is not a session you started (or the app restarted since)")
        })
}

fn report(session: &str, child: &Child) -> Value {
    let mut out = json!({ "session": session, "status": child.status });
    match child.status.as_str() {
        "done" => out["reply"] = json!(child.reply),
        "waiting_approval" => {
            out["note"] =
                json!("It waits for the user to approve a step in the app; wait again later.")
        }
        "running" => out["note"] = json!("Still working; call wait_session again."),
        _ => {}
    }
    if let Some(error) = &child.error {
        out["error"] = json!(error);
    }
    out
}

fn wait(hub: &Hub, parent: &Parent, session: &str, timeout: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let child = child(hub, parent, session)?;
        if child.status != "running" || Instant::now() >= deadline {
            return Ok(report(session, &child));
        }
        thread::sleep(POLL);
    }
}

fn send(hub: &Hub, parent: &Parent, session: &str, prompt: &str) -> Result<Value, String> {
    if prompt.is_empty() {
        return Err("prompt must not be empty".to_string());
    }
    let child = child(hub, parent, session)?;
    match child.status.as_str() {
        "running" | "waiting_approval" => {
            return Err("it is still on its last task: wait_session first".to_string())
        }
        "closed" => return Err("that session has closed".to_string()),
        _ => {}
    }
    set(hub, session, |child| {
        child.status = "running".to_string();
        child.reply.clear();
        child.error = None;
    });
    hub.send_to_session(session, prompt)?;
    Ok(json!({ "session": session, "status": "running" }))
}

fn set(hub: &Hub, session: &str, change: impl FnOnce(&mut Child)) {
    if let Some(child) = lock(&hub.children).get_mut(session) {
        change(child);
    }
}

/// A session event: how the child in `session`, if it is one, is going.
/// `over`: a turn ended, `failed` with this error.
pub(crate) fn observe(hub: &Hub, session: &str, event: &Value, over: bool, failed: Option<&str>) {
    let mut children = lock(&hub.children);
    let Some(child) = children.get_mut(session) else {
        return;
    };
    match event["type"].as_str().unwrap_or_default() {
        "approval_request" | "action_deferred" => child.status = "waiting_approval".to_string(),
        "action_decided" | "tool_start" => child.status = "running".to_string(),
        // The turn's last reply is what the parent gets.
        "assistant_start" => {
            child.reply.clear();
            child.status = "running".to_string();
        }
        "assistant_delta" => {
            if child.status == "waiting_approval" {
                child.status = "running".to_string();
            }
            child
                .reply
                .push_str(event["delta"].as_str().unwrap_or_default());
        }
        _ => {}
    }
    if over && child.status != "closed" {
        match failed {
            Some(error) => {
                child.status = "failed".to_string();
                child.error = Some(error.to_string());
            }
            None => child.status = "done".to_string(),
        }
    }
}

/// The engine of `session` stopped: a child there can't go on.
pub(crate) fn ended(hub: &Hub, session: &str) {
    set(hub, session, |child| {
        if matches!(child.status.as_str(), "running" | "waiting_approval") {
            child.status = "closed".to_string();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent() -> Parent {
        Parent {
            session: "p".to_string(),
            cwd: PathBuf::from("/"),
            mode: "auto".to_string(),
            gateway: false,
        }
    }

    fn hub_with_child() -> Arc<Hub> {
        let dir = std::env::temp_dir().join(format!("jucode-subsessions-{}", std::process::id()));
        let hub = Hub::new(crate::store::Store::open(dir).unwrap(), "test", None);
        lock(&hub.children).insert(
            "c".to_string(),
            Child {
                parent: "p".to_string(),
                status: "running".to_string(),
                ..Child::default()
            },
        );
        hub
    }

    #[test]
    fn a_child_reports_its_last_reply_once_its_turn_ends() {
        let hub = hub_with_child();
        observe(
            &hub,
            "c",
            &json!({ "type": "assistant_start" }),
            false,
            None,
        );
        observe(
            &hub,
            "c",
            &json!({ "type": "assistant_delta", "delta": "looking" }),
            false,
            None,
        );
        observe(&hub, "c", &json!({ "type": "tool_start" }), false, None);
        let early = wait(&hub, &parent(), "c", Duration::ZERO).unwrap();
        assert_eq!(early["status"], "running");
        observe(
            &hub,
            "c",
            &json!({ "type": "approval_request" }),
            false,
            None,
        );
        assert_eq!(
            wait(&hub, &parent(), "c", Duration::from_secs(5)).unwrap()["status"],
            "waiting_approval"
        );
        observe(
            &hub,
            "c",
            &json!({ "type": "assistant_start" }),
            false,
            None,
        );
        observe(
            &hub,
            "c",
            &json!({ "type": "assistant_delta", "delta": "all good" }),
            true,
            None,
        );
        let done = wait(&hub, &parent(), "c", Duration::from_secs(5)).unwrap();
        assert_eq!(
            (done["status"].as_str(), done["reply"].as_str()),
            (Some("done"), Some("all good"))
        );
    }

    #[test]
    fn only_the_parent_drives_a_child_and_a_child_drives_none() {
        let hub = hub_with_child();
        let other = Parent {
            session: "q".to_string(),
            ..parent()
        };
        assert!(call(&hub, &other, WAIT_SESSION, &json!({ "session": "c" })).is_err());
        let child = Parent {
            session: "c".to_string(),
            ..parent()
        };
        let refused = call(
            &hub,
            &child,
            START_SESSION,
            &json!({ "engine": "codex", "prompt": "x" }),
        );
        assert!(refused.unwrap_err().contains("cannot start"));
        assert!(send(&hub, &parent(), "c", "more")
            .unwrap_err()
            .contains("wait_session first"));
        ended(&hub, "c");
        assert_eq!(
            wait(&hub, &parent(), "c", Duration::ZERO).unwrap()["status"],
            "closed"
        );
    }
}
