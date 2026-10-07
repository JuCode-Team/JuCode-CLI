//! Tools a long-lived agent's sessions get from the daemon, added to the
//! engine through `HostExtensions`: `message_agent`, `timer`, `schedule`,
//! `brief`, `question`, `report` and `requirements`.

use crate::{
    hub::Hub,
    store::{now, Message, Question, Report, Timer},
};
use jucode_agent_core::host::HostExtensions;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn extensions(hub: Arc<Hub>, agent: String, session: String) -> HostExtensions {
    if agent == crate::dispatch::AGENT {
        return dispatcher(hub, session);
    }
    let prompt_hub = Arc::clone(&hub);
    let prompt_agent = agent.clone();
    let prompt_session = session.clone();
    HostExtensions {
        tools: definitions(),
        run_tool: Arc::new(move |name, arguments| {
            let result = serde_json::from_str::<Value>(arguments)
                .map_err(|error| format!("invalid JSON arguments: {error}"))
                .map(without_empty)
                .and_then(|args| run(&hub, &agent, &session, name, &args));
            match result {
                Ok(output) => (output.to_string(), false),
                Err(error) => (json!({ "error": error }).to_string(), true),
            }
        }),
        prompt: Arc::new(move || {
            let mut prompt = prompt_hub.agents.prompt(&prompt_agent, &prompt_session);
            prompt.push_str(&project_prompt(&prompt_hub, &prompt_agent));
            prompt
        }),
        exclusive: false,
    }
}

/// The project the agent belongs to: its name and directories.
fn project_prompt(hub: &Hub, agent: &str) -> String {
    let Some(project) = hub
        .agents
        .get(agent)
        .and_then(|agent| agent.project)
        .and_then(|id| crate::projects::project(hub, &id))
    else {
        return String::new();
    };
    let dirs: Vec<&str> = project["dirs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    format!(
        "\n<project id=\"{}\" name=\"{}\">\nYou belong to this project; the `requirements` tool lists its requirements.\nMain directory: {}\nExtra directories: {}\n</project>",
        project["id"].as_str().unwrap_or_default(),
        project["name"].as_str().unwrap_or_default(),
        project["path"].as_str().unwrap_or_default(),
        if dirs.is_empty() {
            "none".to_string()
        } else {
            dirs.join(", ")
        }
    )
}

/// The dispatcher's tools (see `dispatch`), plus `question`.
fn dispatcher(hub: Arc<Hub>, session: String) -> HostExtensions {
    let prompt_hub = Arc::clone(&hub);
    let prompt_session = session.clone();
    let mut tools = crate::dispatch::definitions();
    tools.extend(
        definitions()
            .into_iter()
            .filter(|tool| tool["name"] == "question"),
    );
    HostExtensions {
        tools,
        run_tool: Arc::new(move |name, arguments| {
            let result = serde_json::from_str::<Value>(arguments)
                .map_err(|error| format!("invalid JSON arguments: {error}"))
                .map(without_empty)
                .and_then(
                    |args| match crate::dispatch::run(&hub, &session, name, &args) {
                        Some(result) => result,
                        None if name == "question" => {
                            run(&hub, crate::dispatch::AGENT, &session, name, &args)
                        }
                        None => Err(format!("unknown tool {name}")),
                    },
                );
            match result {
                Ok(output) => (output.to_string(), false),
                Err(error) => (json!({ "error": error }).to_string(), true),
            }
        }),
        prompt: Arc::new(move || crate::dispatch::prompt(&prompt_hub, &prompt_session)),
        exclusive: true,
    }
}

/// Some models fill every optional field with `""` or `[]` (a create
/// arriving with `"id": ""`); those mean "not given", same as leaving it out.
fn without_empty(mut args: Value) -> Value {
    if let Some(map) = args.as_object_mut() {
        map.retain(|_, value| {
            !(value.is_null()
                || value.as_str().is_some_and(|text| text.trim().is_empty())
                || value.as_array().is_some_and(Vec::is_empty))
        });
    }
    args
}

fn run(
    hub: &Arc<Hub>,
    agent: &str,
    session: &str,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    match name {
        "message_agent" => {
            let to = text("to").ok_or("message_agent requires to")?;
            let body = text("body").ok_or("message_agent requires body")?;
            let id = hub.new_id("m");
            hub.send_message(Message {
                id: id.clone(),
                to: to.clone(),
                from: format!("agent:{agent}"),
                body,
                session: None,
                reply_to: text("reply_to"),
                dedupe_key: None,
                at: now(),
            })?;
            Ok(json!({ "sent": id, "to": to }))
        }
        "timer" => match text("action").as_deref() {
            Some("set") => {
                let body = text("body").ok_or("timer set requires body")?;
                let fire_at = match (args["in_seconds"].as_u64(), args["at"].as_u64()) {
                    (Some(seconds), _) => now() + seconds * 1000,
                    (None, Some(unix_seconds)) => unix_seconds * 1000,
                    (None, None) => return Err("timer set requires in_seconds or at".to_string()),
                };
                let timer = Timer {
                    id: hub.new_id("t"),
                    agent: agent.to_string(),
                    // A new session only when asked: by default the timer
                    // comes back to the conversation that set it.
                    session: (args["new_session"] != true).then(|| session.to_string()),
                    fire_at,
                    body,
                };
                hub.set_timer(&timer)?;
                Ok(json!({ "timer": timer.id, "fire_at_unix": fire_at / 1000 }))
            }
            Some("list") => Ok(json!(hub
                .store
                .active_timers()
                .into_iter()
                .filter(|timer| timer.agent == agent)
                .map(|timer| json!({
                    "timer": timer.id,
                    "fire_at_unix": timer.fire_at / 1000,
                    "body": timer.body,
                    "session": timer.session,
                }))
                .collect::<Vec<_>>())),
            Some("cancel") => {
                let id = text("timer").ok_or("timer cancel requires timer")?;
                hub.cancel_timer(&id, Some(agent))?;
                Ok(json!({ "cancelled": id }))
            }
            _ => Err("timer action must be set, list or cancel".to_string()),
        },
        "schedule" => match text("action").as_deref() {
            Some("list") => Ok(hub.schedules_json(Some(agent))["schedules"].clone()),
            Some("create") if args["id"].is_null() => hub
                .propose_schedule(agent, args)
                .map(|schedule| json!({ "proposed": schedule.to_json(), "note": "switched off until the user turns it on" })),
            Some("create") => Err("create takes no id; use update".to_string()),
            Some("update") if args["id"].is_string() => hub
                .propose_schedule(agent, args)
                .map(|schedule| json!({ "proposed": schedule.to_json(), "note": "switched off until the user turns it on again" })),
            Some("update") => Err("update requires id".to_string()),
            Some("delete") => {
                let id = text("id").ok_or("delete requires id")?;
                hub.delete_schedule_by(&id, Some(agent))?;
                Ok(json!({ "deleted": id }))
            }
            _ => Err("schedule action must be list, create, update or delete".to_string()),
        },
        "brief" => match text("action").as_deref() {
            Some("read") => {
                let file = text("file").ok_or("brief read requires file")?;
                Ok(json!({ "file": file, "content": hub.agents.read_brief(agent, &file)? }))
            }
            Some("write") => {
                let file = text("file").ok_or("brief write requires file")?;
                let content = text("content").ok_or("brief write requires content")?;
                hub.agents.write_brief(agent, &file, &content)?;
                Ok(json!({ "written": file }))
            }
            Some("list") => Ok(json!({ "memory": hub.agents.memory_files(agent) })),
            _ => Err("brief action must be read, write or list".to_string()),
        },
        "question" => {
            let title = text("title").ok_or("question requires title")?;
            let importance = text("importance").unwrap_or_else(|| "normal".to_string());
            if !matches!(importance.as_str(), "low" | "normal" | "high") {
                return Err("importance must be low, normal or high".to_string());
            }
            let question = Question {
                id: hub.new_id("q"),
                agent: agent.to_string(),
                session: session.to_string(),
                title,
                body: text("body").unwrap_or_default(),
                assumption: text("assumption").unwrap_or_default(),
                default_action: text("default").unwrap_or_default(),
                importance,
                // Models fill optional fields with 0; that means no deadline,
                // not an answer due this instant.
                due_at: args["due_in_seconds"]
                    .as_u64()
                    .filter(|seconds| *seconds > 0)
                    .map(|seconds| now() + seconds * 1000),
                asked_at: now(),
            };
            hub.ask(&question)?;
            Ok(json!({
                "question": question.id,
                "note": "Recorded for the user. Carry on with work that does not depend on the answer, under your stated assumption; the answer (or the deadline passing) arrives in this conversation as a message."
            }))
        }
        "report" => {
            let report = Report {
                id: hub.new_id("r"),
                agent: agent.to_string(),
                session: session.to_string(),
                title: text("title").ok_or("report requires title")?,
                body: text("body").unwrap_or_default(),
                at: now(),
                read: false,
            };
            hub.post_report(&report)?;
            Ok(json!({ "report": report.id }))
        }
        "requirements" => {
            let project = hub.agents.get(agent).and_then(|agent| agent.project);
            crate::requirements::tool(hub, agent, project.as_deref(), session, args)
        }
        other => Err(format!("unknown tool {other}")),
    }
}

fn definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": "question",
            "description": "Ask the user something only they can decide, without stopping: the question waits for them while you continue under `assumption`. Their answer, or the deadline passing (then you go with `default`), arrives here as a message. Use it for real decisions, not for confirmations you can reason out yourself.",
            "parameters": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "The question in one line." },
                    "body": { "type": "string", "description": "Context and the options you see." },
                    "assumption": { "type": "string", "description": "What you assume while waiting." },
                    "default": { "type": "string", "description": "What you will do if nobody answers in time." },
                    "due_in_seconds": { "type": "integer", "minimum": 0, "description": "Deadline; omit (or 0) to wait indefinitely." },
                    "importance": { "type": "string", "enum": ["low", "normal", "high"] }
                },
                "required": ["title"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "report",
            "description": "Tell the user what you finished or found, for them to read later on their desk. It wakes nobody. Report outcomes worth their attention (done, blocked, something they should know), not every step.",
            "parameters": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "The outcome in one line." },
                    "body": { "type": "string", "description": "Details: what changed, how it was checked, what is left." }
                },
                "required": ["title"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "message_agent",
            "description": "Send a message to another long-lived agent (listed in <other_agents>; not a subagent). It is delivered into that agent's work, which starts or continues a session there. Use it to hand off work or ask for something in their area.",
            "parameters": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Agent id." },
                    "body": { "type": "string", "description": "Self-contained message." },
                    "reply_to": { "type": "string", "description": "Id of a message you received, to answer in the same conversation." }
                },
                "required": ["to", "body"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "timer",
            "description": "Come back to something later. `set` delivers `body` to you as a message after `in_seconds` (or at unix time `at`), into this conversation unless `new_session` is true; it fires even if nobody has the app open. `list` shows your pending timers, `cancel` removes one.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["set", "list", "cancel"] },
                    "in_seconds": { "type": "integer", "minimum": 0 },
                    "at": { "type": "integer", "description": "Unix time in seconds." },
                    "body": { "type": "string", "description": "What to do when it fires." },
                    "new_session": { "type": "boolean" },
                    "timer": { "type": "string", "description": "Timer id, for cancel." }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "schedule",
            "description": "Propose recurring work for yourself: a task that runs `prompt` at set local times, each run in a new session told what the last run concluded. What you create or update stays switched off until the user turns it on in the app, so tell them why in a `report`. Use `timer` for a one-off return instead. `list` shows your scheduled tasks; `delete` removes one of yours.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "create", "update", "delete"] },
                    "id": { "type": "string", "description": "Schedule id, for update and delete." },
                    "name": { "type": "string", "description": "Short name, e.g. 每日巡检." },
                    "prompt": { "type": "string", "description": "The self-contained message each run receives." },
                    "repeat": { "type": "string", "enum": ["once", "hourly", "daily", "weekdays", "weekly"] },
                    "time": { "type": "string", "description": "Local HH:MM (24-hour); hourly uses only the minute." },
                    "days": { "type": "array", "items": { "type": "integer", "minimum": 0, "maximum": 6 }, "description": "weekly: 0 is Sunday." },
                    "date": { "type": "string", "description": "once: YYYY-MM-DD." }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "brief",
            "description": "Read or rewrite your own brief: role.md, capabilities.md, policy.md, state.md, or a memory/<topic>.md note. `write` replaces the whole file. `list` shows your memory files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["read", "write", "list"] },
                    "file": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "requirements",
            "description": "The user's requirements (what they mean to get done). `list` shows those of your project (`project`: another project's id, or `none` for unassigned ones; `state` filters). `get` shows one in full: words, progress, start gate, pending proposal. `propose_create` and `propose_close` only propose: the user is asked, and nothing changes until they accept. Do not propose the same thing twice; a pending proposal shows in `list`. `progress` adds to a requirement's progress record directly: list items are appended once each.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "propose_create", "propose_close", "progress"] },
                    "requirement": { "type": "string", "description": "Requirement id (R-12), for get, propose_close and progress." },
                    "project": { "type": "string", "description": "For list: a project id, or none for unassigned requirements." },
                    "state": { "type": "string", "enum": ["idea", "open", "done", "parked", "proposed"], "description": "For list." },
                    "text": { "type": "string", "description": "For propose_create: the requirement in the user's language." },
                    "reason": { "type": "string", "description": "For propose_create and propose_close: why, in one line the user reads." },
                    "outcome": { "type": "string", "enum": ["done", "parked"], "description": "For propose_close." },
                    "decided": { "type": "array", "items": { "type": "string" } },
                    "done": { "type": "array", "items": { "type": "string" } },
                    "doing": { "type": "array", "items": { "type": "string" } },
                    "blocked": { "type": "array", "items": { "type": "string" } },
                    "next": { "type": "array", "items": { "type": "string" } },
                    "note": { "type": "string", "description": "For progress: a note that replaces the previous one." }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        }),
    ]
}
