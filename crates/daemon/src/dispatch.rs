//! Dispatch: the user hands over a batch of requests (from their phone,
//! away from the computer) without picking a project or session. A reserved
//! agent, the dispatcher, splits it into tasks, sends each to a session in
//! the right project (continuing one or starting one), and reports back
//! once they are done.
//!
//! Each dispatch is one session of the dispatcher. Its tools act on the
//! user's sessions; what a task's session does comes back to the
//! dispatcher as a message when its turn ends (see `observe`), so nothing
//! waits on a running turn. The user's choices hold whatever the model
//! does: every task runs in the permission mode the user picked, and in plan
//! mode no task starts before the user confirmed the plan.

use crate::{
    engines,
    hub::{lock, Hub},
    store::{now, write_private, Message},
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// The dispatcher agent's id (`Agents::list` leaves it out).
pub const AGENT: &str = "dispatch";
const FILE: &str = "dispatches.json";
/// Dispatches kept; older ones are dropped.
const KEEP: usize = 50;
/// How much of a task's last reply the dispatcher and the client get.
const REPLY_LIMIT: usize = 1500;
/// Permission modes a dispatch may run its tasks in (the jucode names; the
/// Claude Code and Codex adapters map them).
const MODES: [&str; 4] = ["manual", "auto-edit", "auto", "full-access"];

pub struct Dispatches {
    path: PathBuf,
    list: Mutex<Vec<Value>>,
}

impl Dispatches {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let list = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        Self {
            path,
            list: Mutex::new(list),
        }
    }

    pub fn json(&self) -> Value {
        json!({ "type": "dispatches", "dispatches": *lock(&self.list) })
    }

    fn get(&self, id: &str) -> Option<Value> {
        lock(&self.list).iter().find(|d| d["id"] == id).cloned()
    }

    /// Applies `change` to dispatch `id` and saves the list.
    fn update<T>(
        &self,
        id: &str,
        change: impl FnOnce(&mut Value) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut list = lock(&self.list);
        let dispatch = list
            .iter_mut()
            .find(|d| d["id"] == id)
            .ok_or_else(|| format!("unknown dispatch {id}"))?;
        let result = change(dispatch)?;
        dispatch["updated_at"] = json!(now());
        self.save(&list)?;
        Ok(result)
    }

    fn save(&self, list: &[Value]) -> Result<(), String> {
        write_private(&self.path, format!("{:#}\n", json!(list)).as_bytes())
            .map_err(|error| error.to_string())
    }

    /// The dispatch and task a session is running now, when it runs one
    /// (a session has at most one: see `plan` and `start_task`).
    fn task_of(&self, session: &str) -> Option<(String, u64)> {
        lock(&self.list).iter().find_map(|d| {
            d["tasks"].as_array()?.iter().find_map(|task| {
                (task["session"] == session && active(task)).then(|| {
                    (
                        d["id"].as_str().unwrap_or_default().to_string(),
                        task["id"].as_u64().unwrap_or(0),
                    )
                })
            })
        })
    }
}

/// `dispatch_send`: starts a dispatch of `text`.
/// `requirement`: the requirement the dispatch works on; its tasks' sessions
/// are linked to it.
pub fn start(
    hub: &Arc<Hub>,
    text: &str,
    plan: bool,
    mode: &str,
    requirement: Option<&str>,
) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("dispatch_send requires text".to_string());
    }
    if !MODES.contains(&mode) {
        return Err(format!("unknown permission mode {mode}"));
    }
    ensure_agent(hub)?;
    let session = hub.create_session(None, Some(AGENT), false)?;
    let dispatch = json!({
        "id": session,
        "text": text,
        "plan": plan,
        "mode": mode,
        "requirement": requirement,
        "status": "planning",
        "tasks": [],
        "summary": "",
        "created_at": now(),
        "updated_at": now(),
    });
    {
        let mut list = lock(&hub.dispatch.list);
        list.insert(0, dispatch.clone());
        list.truncate(KEEP);
        hub.dispatch.save(&list)?;
    }
    hub.broadcast(&hub.dispatch.json());
    hub.send_message(Message {
        id: hub.new_id("m"),
        to: AGENT.to_string(),
        from: "user".to_string(),
        body: text.to_string(),
        session: Some(session),
        reply_to: None,
        dedupe_key: None,
        at: now(),
    })?;
    Ok(json!({ "type": "dispatch_started", "dispatch": dispatch }))
}

/// `dispatch_confirm`: the user's decision on a plan waiting for one.
pub fn confirm(hub: &Arc<Hub>, id: &str, approve: bool, note: &str) -> Result<Value, String> {
    hub.dispatch.update(id, |d| {
        if d["status"] != "awaiting" {
            return Err("this dispatch is not waiting for a confirmation".to_string());
        }
        d["status"] = json!(if approve { "running" } else { "cancelled" });
        Ok(())
    })?;
    hub.broadcast(&hub.dispatch.json());
    let body = match (approve, note.trim()) {
        (true, "") => "The user confirmed the plan. Start the tasks now.".to_string(),
        (true, note) => format!("The user confirmed the plan, with this note:\n{note}\nStart the tasks now, taking the note into account."),
        (false, "") => "The user cancelled this dispatch. Start nothing; reply with one line acknowledging it.".to_string(),
        (false, note) => format!("The user cancelled this dispatch:\n{note}\nStart nothing; reply with one line acknowledging it."),
    };
    hub.send_message(Message {
        id: hub.new_id("m"),
        to: AGENT.to_string(),
        from: "user".to_string(),
        body,
        session: Some(id.to_string()),
        reply_to: None,
        dedupe_key: None,
        at: now(),
    })?;
    Ok(json!({ "type": "dispatch_confirmed", "dispatch": id }))
}

/// The dispatcher agent, created on first use. It works from the home
/// directory and has no files of its own to touch. A user's own agent of
/// that id (made before the id was reserved) is not taken over.
fn ensure_agent(hub: &Hub) -> Result<(), String> {
    if hub.agents.get(AGENT).is_some() {
        return match hub.agents.read_brief(AGENT, "role.md") {
            Ok(role) if role.trim() == ROLE => Ok(()),
            _ => Err(format!(
                "an agent with the id {AGENT} already exists; rename it to use dispatch"
            )),
        };
    }
    let home = engines::home();
    hub.agents
        .create(AGENT, "Dispatch", &home, ROLE, &json!({}))
        .map(|_| ())
}

const ROLE: &str = "Route the user's requests to the coding sessions on this computer.";

/// What the dispatcher is told every turn: how to work, and where this
/// dispatch stands.
pub fn prompt(hub: &Hub, session: &str) -> String {
    let Some(d) = hub.dispatch.get(session) else {
        return GUIDE.to_string();
    };
    let plan = if d["plan"] == true {
        "Plan mode is ON: after `plan`, end your turn. The user's decision arrives as a message; start no task before they confirm."
    } else {
        "Plan mode is OFF: after `plan`, start the tasks right away."
    };
    let mut tasks = String::new();
    for task in d["tasks"].as_array().into_iter().flatten() {
        tasks.push_str(&format!(
            "- task {} [{}] {} · project {} · session {}\n",
            task["id"],
            text(&task["status"]),
            text(&task["title"]),
            text(&task["project"]),
            task["session"].as_str().unwrap_or("(new)"),
        ));
    }
    if tasks.is_empty() {
        tasks.push_str("(no plan yet)\n");
    }
    format!(
        "{GUIDE}\n\n<dispatch>\nStatus: {}\n{plan}\nEvery task runs in permission mode `{}`, chosen by the user and applied by the system.\nTasks:\n{tasks}</dispatch>",
        text(&d["status"]),
        text(&d["mode"]),
    )
}

const GUIDE: &str = "You are the dispatcher of JuCode. The user is away from this computer and hands you a batch of requests in one message. You do not write code yourself: you have no file or shell tools. You route the work to the user's coding sessions, then report back.

How to work:
1. Call `workspace_overview` to see the projects and their recent sessions.
2. Split the request into tasks. One task is one coherent piece of work in one project; keep steps that depend on each other in the same task. Do not split finer than the user did.
3. Pick where each task runs. Continue an existing session when the request clearly carries on its work (the same feature, bug or thread, judging by its title and last reply). Otherwise start a new session in the project. A task must name a project path from `workspace_overview`, or an existing session.
4. Call `plan` with all tasks.
5. Start each task with `start_task`. Its prompt goes to a coding agent that has not seen the user's message: make it self-contained, in the user's language, with the goal, the context from the user's words, constraints, and how to tell it is done. End it with: \"When you are done, reply with three short lines: what changed, how you verified it, and anything left open.\"
6. Never tell a session to push, deploy, publish, delete data or spend money unless the user asked for exactly that.
7. If the request is unclear about where or what (two projects fit, say), call `question` with your best guess as the assumption and the default, and carry on with it.
8. Results arrive as messages starting with [task ...]. A task that failed or stopped short may get one corrective `start_task`; one waiting for the user's approval is theirs to decide.
9. When every task has finished or failed, call `finish` with the report for the user, in their language: the overall outcome in one line, then one short line per task (done, failed, or waiting for them), then anything they must do. No more than that.
Keep your own replies short; the user reads them on a phone.";

/// The dispatcher's tools (with `question` from the agent tools).
pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": "workspace_overview",
            "description": "The user's projects on this computer, each with its recent sessions: id, title, engine, last activity, whether it is running now, and the end of its last reply when known.",
            "parameters": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "type": "function",
            "name": "plan",
            "description": "Record the tasks of this dispatch (replaces an earlier plan that has not started). In plan mode the user is shown the plan and asked to confirm; end your turn afterwards.",
            "parameters": {
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string", "description": "A short name for the task, in the user's language." },
                                "project": { "type": "string", "description": "The project path it runs in (from workspace_overview). Leave out when continuing a session." },
                                "session": { "type": "string", "description": "An existing session to continue. Leave out to start a new one." },
                                "engine": { "type": "string", "enum": ["jucode", "claude", "codex"], "description": "For a new session: the coding agent to run. Default jucode." }
                            },
                            "required": ["title"]
                        }
                    }
                },
                "required": ["tasks"]
            }
        }),
        json!({
            "type": "function",
            "name": "start_task",
            "description": "Send a task's prompt to its session (starting the session when the task has none yet). Returns at once; the session's result arrives later as a [task ...] message.",
            "parameters": {
                "type": "object",
                "properties": {
                    "task": { "type": "integer", "description": "The task number from the plan." },
                    "prompt": { "type": "string", "description": "The self-contained instruction for the coding agent." }
                },
                "required": ["task", "prompt"]
            }
        }),
        json!({
            "type": "function",
            "name": "read_task",
            "description": "A task's state now: its status, whether its session is running, and the end of its last reply.",
            "parameters": {
                "type": "object",
                "properties": { "task": { "type": "integer" } },
                "required": ["task"]
            }
        }),
        json!({
            "type": "function",
            "name": "finish",
            "description": "End the dispatch with the report for the user (it is also sent to their phone).",
            "parameters": {
                "type": "object",
                "properties": { "summary": { "type": "string" } },
                "required": ["summary"]
            }
        }),
    ]
}

/// Runs one dispatcher tool; None for a name that is not one of them.
pub fn run(
    hub: &Arc<Hub>,
    session: &str,
    name: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    Some(match name {
        "workspace_overview" => Ok(overview(hub)),
        "plan" => plan(hub, session, args),
        "start_task" => start_task(hub, session, args),
        "read_task" => read_task(hub, session, args),
        "finish" => finish(hub, session, args),
        _ => return None,
    })
}

fn overview(hub: &Hub) -> Value {
    let sessions = hub.sessions_json();
    let sessions: Vec<&Value> = sessions["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["agent"].is_null() && s["chat"] != true && s["archived"] != true)
        .collect();
    let workspaces = hub.store.workspaces();
    let mut projects = Vec::new();
    for ws in workspaces["workspaces"].as_array().into_iter().flatten() {
        for project in ws["projects"].as_array().into_iter().flatten() {
            let path = text(&project["path"]);
            if path.is_empty() || projects.iter().any(|p: &Value| p["path"] == path) {
                continue;
            }
            let mut recent: Vec<&&Value> = sessions
                .iter()
                .filter(|s| same_dir(text(&s["cwd"]), path))
                .collect();
            recent.sort_by_key(|s| std::cmp::Reverse(s["updated_at"].as_u64().unwrap_or(0)));
            let recent: Vec<Value> = recent
                .into_iter()
                .take(8)
                .map(|s| {
                    let id = text(&s["session"]);
                    json!({
                        "session": id,
                        "title": s["title"],
                        "engine": s["engine"],
                        "updated_at": s["updated_at"],
                        "running": hub.is_busy(id),
                        "last_reply": hub.last_reply(id).map(|r| clip(&r, 300)),
                    })
                })
                .collect();
            projects.push(json!({
                "name": project["name"],
                "path": path,
                "workspace": ws["name"],
                "sessions": recent,
            }));
        }
    }
    json!({ "projects": projects })
}

fn plan(hub: &Hub, session: &str, args: &Value) -> Result<Value, String> {
    let d = hub
        .dispatch
        .get(session)
        .ok_or("this session is not a dispatch")?;
    if d["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|task| task["status"] != "planned")
    {
        return Err("tasks have started; the plan can no longer change".to_string());
    }
    if !matches!(text(&d["status"]), "planning" | "awaiting") {
        return Err(format!("this dispatch is {}", text(&d["status"])));
    }
    let records = hub.store.sessions();
    let projects = project_paths(hub);
    let mut tasks = Vec::new();
    for (index, task) in args["tasks"].as_array().into_iter().flatten().enumerate() {
        let title = text(&task["title"]).trim();
        if title.is_empty() {
            return Err(format!("task {} has no title", index + 1));
        }
        let (project, existing, engine) = match task["session"].as_str() {
            Some(id) => {
                let record = records
                    .iter()
                    .find(|r| r.id == id && r.agent.is_none())
                    .ok_or_else(|| format!("unknown session {id}"))?;
                if record.engine.as_deref() == Some("acp") {
                    return Err(format!(
                        "task {}: session {id} runs an ACP agent, which cannot be continued; start a new session",
                        index + 1
                    ));
                }
                if tasks.iter().any(|t: &Value| t["session"] == id) {
                    return Err(format!(
                        "task {}: session {id} already has a task; merge them",
                        index + 1
                    ));
                }
                if hub.dispatch.task_of(id).is_some() {
                    return Err(format!(
                        "task {}: session {id} is running another dispatch's task; start a new session",
                        index + 1
                    ));
                }
                (
                    record.cwd.display().to_string(),
                    Some(id.to_string()),
                    record
                        .engine
                        .clone()
                        .unwrap_or_else(|| "jucode".to_string()),
                )
            }
            None => {
                let path = text(&task["project"]);
                let project = projects.iter().find(|p| same_dir(p, path)).ok_or_else(|| {
                    format!(
                        "task {}: {path:?} is not one of the user's projects",
                        index + 1
                    )
                })?;
                let engine = task["engine"].as_str().unwrap_or("jucode");
                if !matches!(engine, "jucode" | "claude" | "codex") {
                    return Err(format!("task {}: unknown engine {engine}", index + 1));
                }
                (project.clone(), None, engine.to_string())
            }
        };
        tasks.push(json!({
            "id": index + 1,
            "title": title,
            "project": project,
            "session": existing,
            "engine": engine,
            "status": "planned",
            "reply": "",
        }));
    }
    if tasks.is_empty() {
        return Err("plan requires at least one task".to_string());
    }
    let wait = d["plan"] == true;
    hub.dispatch.update(session, |d| {
        d["tasks"] = json!(tasks);
        d["status"] = json!(if wait { "awaiting" } else { "running" });
        Ok(())
    })?;
    hub.broadcast(&hub.dispatch.json());
    if wait {
        hub.notify(
            "派发计划待确认",
            &tasks
                .iter()
                .map(|t| text(&t["title"]).to_string())
                .collect::<Vec<_>>()
                .join("；"),
            session,
        );
        Ok(
            json!({ "note": "The plan is shown to the user. End your turn now; their decision arrives as a message." }),
        )
    } else {
        Ok(json!({ "note": "Recorded. Start the tasks now." }))
    }
}

fn start_task(hub: &Arc<Hub>, session: &str, args: &Value) -> Result<Value, String> {
    let d = hub
        .dispatch
        .get(session)
        .ok_or("this session is not a dispatch")?;
    match text(&d["status"]) {
        // "failed": the dispatcher failed once and a task result woke it.
        "running" | "failed" => {}
        "awaiting" => {
            return Err(
                "the user has not confirmed the plan yet; end your turn and wait".to_string(),
            )
        }
        status => return Err(format!("this dispatch is {status}")),
    }
    let number = args["task"].as_u64().ok_or("start_task requires task")?;
    let prompt = text(&args["prompt"]).trim().to_string();
    if prompt.is_empty() {
        return Err("start_task requires prompt".to_string());
    }
    let task = d["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["id"] == number)
        .cloned()
        .ok_or_else(|| format!("unknown task {number}"))?;
    if active(&task) {
        return Err(format!("task {number} is still running"));
    }
    let mode = text(&d["mode"]).to_string();
    let engine = engines::Kind::parse(text(&task["engine"]))?;
    let options = || engines::Options {
        approval_mode: Some(mode.clone()),
        ..engines::Options::default()
    };
    let target = match task["session"].as_str() {
        Some(id) if hub.is_busy(id) => {
            return Err(format!(
                "session {id} is running a turn of the user's; start this task later or in a new session"
            ))
        }
        Some(id) => {
            hub.open_engine_session(id, None, engine, options())?;
            id.to_string()
        }
        None => hub.create_engine_session(
            Some(PathBuf::from(text(&task["project"]))),
            None,
            false,
            engine,
            options(),
        )?,
    };
    // The user's mode, whatever the session ran in before (the jucode
    // engine takes no start options; a Claude session switching in or out
    // of full access restarts before it takes the prompt).
    hub.forward(&target, json!({ "op": "set_approval_mode", "mode": mode }))?;
    // Marked before sending, so the session's first events find the task.
    let mark = |status: &str, reply: &str| {
        hub.dispatch.update(session, |d| {
            if let Some(t) = d["tasks"]
                .as_array_mut()
                .and_then(|tasks| tasks.iter_mut().find(|t| t["id"] == number))
            {
                t["session"] = json!(target);
                t["status"] = json!(status);
                t["reply"] = json!(reply);
            }
            Ok(())
        })
    };
    mark("sent", "")?;
    if let Some(requirement) = d["requirement"].as_str() {
        if let Err(error) = crate::requirements::link(hub, requirement, &target) {
            jucode_agent_core::log_warn!("daemon", "dispatch task not linked", error = error);
        }
    }
    if let Err(error) = hub.send_to_session(&target, &prompt) {
        let _ = mark("failed", &error);
        hub.broadcast(&hub.dispatch.json());
        return Err(error);
    }
    hub.broadcast(&hub.dispatch.json());
    Ok(json!({ "session": target, "note": "Sent. Its result arrives as a [task ...] message." }))
}

fn read_task(hub: &Hub, session: &str, args: &Value) -> Result<Value, String> {
    let d = hub
        .dispatch
        .get(session)
        .ok_or("this session is not a dispatch")?;
    let number = args["task"].as_u64().ok_or("read_task requires task")?;
    let task = d["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["id"] == number)
        .ok_or_else(|| format!("unknown task {number}"))?;
    let target = task["session"].as_str();
    Ok(json!({
        "task": number,
        "status": task["status"],
        "session": target,
        "running": target.is_some_and(|s| hub.is_busy(s)),
        "last_reply": target.and_then(|s| hub.last_reply(s)).map(|r| clip(&r, REPLY_LIMIT)),
    }))
}

fn finish(hub: &Hub, session: &str, args: &Value) -> Result<Value, String> {
    let summary = text(&args["summary"]).trim().to_string();
    if summary.is_empty() {
        return Err("finish requires summary".to_string());
    }
    hub.dispatch.update(session, |d| match text(&d["status"]) {
        "planning" | "running" | "failed" => {
            d["status"] = json!("done");
            d["summary"] = json!(summary);
            Ok(())
        }
        "awaiting" => Err("the plan is waiting for the user; end your turn instead".to_string()),
        status => Err(format!("this dispatch is {status}; end your turn")),
    })?;
    hub.broadcast(&hub.dispatch.json());
    hub.notify("派发已完成", &summary, session);
    Ok(json!({ "note": "Sent to the user. End your turn with one short line." }))
}

/// A session event: a task's session started, finished, failed or waits
/// for the user. Its dispatcher hears about it as a message.
pub fn observe(hub: &Arc<Hub>, session: &str, event: &Value) {
    // Checked first: this sees every event of every session (deltas too).
    let kind = text(&event["type"]);
    let ready = kind == "status" && event["message"] == "ready";
    let waiting = matches!(kind, "approval_request" | "action_deferred");
    if !(kind == "user_message" || ready || kind == "error" || waiting) {
        return;
    }
    // The dispatcher itself failed (its retries spent): the dispatch shows
    // it; a later task result wakes the dispatcher again.
    if kind == "error" && hub.dispatch.get(session).is_some() {
        let failed = hub.dispatch.update(session, |d| {
            let open = matches!(text(&d["status"]), "planning" | "running");
            if open {
                d["status"] = json!("failed");
            }
            Ok(open)
        });
        if failed == Ok(true) {
            hub.broadcast(&hub.dispatch.json());
            hub.notify("派发出错", text(&event["message"]), session);
        }
        return;
    }
    let Some((dispatch, number)) = hub.dispatch.task_of(session) else {
        return;
    };
    let reply = hub.last_reply(session).map(|r| clip(&r, REPLY_LIMIT));
    let change = hub.dispatch.update(&dispatch, |d| {
        let Some(task) = d["tasks"]
            .as_array_mut()
            .and_then(|tasks| tasks.iter_mut().find(|t| t["id"] == number))
        else {
            return Ok(None);
        };
        let status = text(&task["status"]).to_string();
        let next = match (status.as_str(), kind) {
            // The engine took the prompt: from now on "ready" ends the turn.
            ("sent", "user_message") => "running",
            ("sent" | "running" | "waiting", "error") => "failed",
            ("running" | "waiting", _) if ready => "done",
            ("sent" | "running", _) if waiting => "waiting",
            _ => return Ok(None),
        };
        task["status"] = json!(next);
        if let Some(reply) = &reply {
            task["reply"] = json!(reply);
        }
        let title = text(&task["title"]).to_string();
        Ok(Some((next, title)))
    });
    let Ok(Some((status, title))) = change else {
        return;
    };
    hub.broadcast(&hub.dispatch.json());
    let body = match status {
        "running" => return,
        "done" => format!(
            "Task {number} ({title}) finished in session {session}.\nEnd of its reply:\n{}",
            reply.as_deref().unwrap_or("(no reply text)")
        ),
        "failed" => format!(
            "Task {number} ({title}) failed in session {session}: {}",
            text(&event["message"])
        ),
        _ => {
            hub.notify("派发任务等待你确认", &title, &dispatch);
            format!("Task {number} ({title}) is waiting for the user to approve an action in session {session}. It continues once they decide.")
        }
    };
    let _ = hub.send_message(Message {
        id: hub.new_id("m"),
        to: AGENT.to_string(),
        from: format!("task:{number}"),
        body,
        session: Some(dispatch),
        reply_to: None,
        dedupe_key: None,
        at: now(),
    });
}

/// A task whose session is working on it (or waits for the user).
fn active(task: &Value) -> bool {
    matches!(text(&task["status"]), "sent" | "running" | "waiting")
}

fn project_paths(hub: &Hub) -> Vec<String> {
    hub.store.workspaces()["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|ws| ws["projects"].as_array().into_iter().flatten())
        .map(|p| text(&p["path"]).to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn same_dir(a: &str, b: &str) -> bool {
    a.trim_end_matches(['/', '\\']) == b.trim_end_matches(['/', '\\'])
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn clip(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    format!("…{}", text.chars().skip(count - limit).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{store::Store, Agents};
    use std::sync::mpsc;

    fn hub(label: &str) -> (Arc<Hub>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "jucode-dispatch-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        let project = dir.join("app");
        fs::create_dir_all(&project).unwrap();
        hub.store
            .update_workspaces(|list| {
                list.push(json!({ "id": "w", "name": "W", "projects": [
                    { "id": "p", "name": "app", "path": project.display().to_string() }
                ] }));
                Ok(())
            })
            .unwrap();
        (hub, project)
    }

    /// A session hosted on a channel instead of an engine.
    fn hosted(hub: &Arc<Hub>, id: &str, cwd: &Path, agent: Option<&str>) -> mpsc::Receiver<Value> {
        hub.store
            .record_engine_session(id, cwd, agent, None, false)
            .unwrap();
        let (ops, rx) = mpsc::channel();
        hub.host(
            id.to_string(),
            ops,
            cwd.to_path_buf(),
            hub.next_generation(),
        );
        rx
    }

    /// A dispatch whose dispatcher session is hosted on a channel.
    fn dispatch(hub: &Arc<Hub>, plan: bool, mode: &str) -> mpsc::Receiver<Value> {
        ensure_agent(hub).unwrap();
        let rx = hosted(hub, "d1", &engines::home(), Some(AGENT));
        lock(&hub.dispatch.list).push(json!({
            "id": "d1", "text": "x", "plan": plan, "mode": mode,
            "status": "planning", "tasks": [], "summary": "",
        }));
        rx
    }

    #[test]
    fn a_session_takes_one_task_at_a_time() {
        let (hub, project) = hub("one");
        let _dispatcher = dispatch(&hub, false, "auto");
        let _target = hosted(&hub, "s-old", &project, None);
        let twice = json!({ "tasks": [
            { "title": "a", "session": "s-old" },
            { "title": "b", "session": "s-old" },
        ] });
        assert!(plan(&hub, "d1", &twice)
            .unwrap_err()
            .contains("already has a task"));

        // The user's own turn is running there.
        let once = json!({ "tasks": [{ "title": "a", "session": "s-old" }] });
        plan(&hub, "d1", &once).unwrap();
        hub.set_busy("s-old", true);
        let busy = start_task(&hub, "d1", &json!({ "task": 1, "prompt": "go" })).unwrap_err();
        assert!(busy.contains("running a turn"), "{busy}");
        hub.set_busy("s-old", false);
        start_task(&hub, "d1", &json!({ "task": 1, "prompt": "go" })).unwrap();

        // Another dispatch cannot plan on it while the task runs.
        let _second = hosted(&hub, "d2", &engines::home(), Some(AGENT));
        lock(&hub.dispatch.list).insert(
            0,
            json!({
                "id": "d2", "text": "y", "plan": false, "mode": "auto",
                "status": "planning", "tasks": [], "summary": "",
            }),
        );
        let taken = plan(&hub, "d2", &once).unwrap_err();
        assert!(taken.contains("another dispatch"), "{taken}");
    }

    #[test]
    fn plan_mode_holds_every_task_until_the_user_confirms() {
        let (hub, project) = hub("plan");
        let _dispatcher = dispatch(&hub, true, "auto");
        let path = project.display().to_string();
        assert!(plan(
            &hub,
            "d1",
            &json!({ "tasks": [{ "title": "t", "project": "/elsewhere" }] })
        )
        .unwrap_err()
        .contains("not one of the user's projects"));
        plan(
            &hub,
            "d1",
            &json!({ "tasks": [{ "title": "修复登录", "project": path }] }),
        )
        .unwrap();
        assert_eq!(hub.dispatch.get("d1").unwrap()["status"], "awaiting");
        let refused = start_task(&hub, "d1", &json!({ "task": 1, "prompt": "go" })).unwrap_err();
        assert!(refused.contains("not confirmed"), "{refused}");
        assert!(finish(&hub, "d1", &json!({ "summary": "done" })).is_err());
    }

    #[test]
    fn a_task_runs_in_the_users_mode_and_its_end_reaches_the_dispatcher() {
        let (hub, project) = hub("run");
        let dispatcher = dispatch(&hub, false, "full-access");
        let target = hosted(&hub, "s-old", &project, None);
        plan(
            &hub,
            "d1",
            &json!({ "tasks": [{ "title": "修复登录", "session": "s-old" }] }),
        )
        .unwrap();
        assert_eq!(hub.dispatch.get("d1").unwrap()["status"], "running");
        start_task(&hub, "d1", &json!({ "task": 1, "prompt": "修复登录跳转" })).unwrap();
        let sent: Vec<Value> = target.try_iter().collect();
        assert_eq!(
            sent[0],
            json!({ "op": "set_approval_mode", "mode": "full-access" })
        );
        assert_eq!(sent[1]["op"], "user_message");
        assert_eq!(sent[1]["content"], "修复登录跳转");
        // A second start while it runs is refused.
        assert!(start_task(&hub, "d1", &json!({ "task": 1, "prompt": "again" })).is_err());

        // A "ready" before the engine took the prompt does not end the task.
        hub.observe("s-old", &json!({ "type": "status", "message": "ready" }));
        assert_eq!(
            hub.dispatch.get("d1").unwrap()["tasks"][0]["status"],
            "sent"
        );
        hub.observe(
            "s-old",
            &json!({ "type": "user_message", "content": "修复登录跳转" }),
        );
        hub.observe(
            "s-old",
            &json!({ "type": "assistant_delta", "delta": "已修复，测试通过" }),
        );
        hub.set_busy("s-old", false);
        hub.observe("s-old", &json!({ "type": "status", "message": "ready" }));
        let task = &hub.dispatch.get("d1").unwrap()["tasks"][0];
        assert_eq!(task["status"], "done");
        assert_eq!(task["reply"], "已修复，测试通过");
        let delivered: Vec<Value> = dispatcher.try_iter().collect();
        let content = delivered.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(content.starts_with("[task 1 update"), "{content}");
        assert!(content.contains("已修复，测试通过"), "{content}");

        finish(&hub, "d1", &json!({ "summary": "登录已修复" })).unwrap();
        assert_eq!(hub.dispatch.get("d1").unwrap()["status"], "done");
    }
}
