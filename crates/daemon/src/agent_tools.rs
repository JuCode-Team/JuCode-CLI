//! Tools a long-lived agent's sessions get from the daemon, added to the
//! engine through `HostExtensions`: `message_agent`, `timer` and `brief`.

use crate::{
    hub::Hub,
    store::{now, Message, Timer},
};
use jucode_agent_core::host::HostExtensions;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn extensions(hub: Arc<Hub>, agent: String, session: String) -> HostExtensions {
    let prompt_hub = Arc::clone(&hub);
    let prompt_agent = agent.clone();
    HostExtensions {
        tools: definitions(),
        run_tool: Arc::new(move |name, arguments| {
            let result = serde_json::from_str::<Value>(arguments)
                .map_err(|error| format!("invalid JSON arguments: {error}"))
                .and_then(|args| run(&hub, &agent, &session, name, &args));
            match result {
                Ok(output) => (output.to_string(), false),
                Err(error) => (json!({ "error": error }).to_string(), true),
            }
        }),
        prompt: Arc::new(move || prompt_hub.agents.prompt(&prompt_agent)),
    }
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
                if !hub
                    .store
                    .active_timers()
                    .iter()
                    .any(|timer| timer.id == id && timer.agent == agent)
                {
                    return Err(format!("no active timer {id}"));
                }
                hub.store
                    .record_timer_done(&id, "cancelled")
                    .map_err(|error| error.to_string())?;
                Ok(json!({ "cancelled": id }))
            }
            _ => Err("timer action must be set, list or cancel".to_string()),
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
        other => Err(format!("unknown tool {other}")),
    }
}

fn definitions() -> Vec<Value> {
    vec![
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
    ]
}
