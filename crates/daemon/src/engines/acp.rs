//! Agent Client Protocol agents (JSON-RPC 2.0 over stdio lines, protocol v1,
//! https://agentclientprotocol.com), stable schema only: the built-in
//! presets (`acp_presets`, by `options.agent`) and agents the desktop's
//! registry names by command.
//!
//! - Handshake: `initialize`, then `session/load` (its replay becomes the
//!   `transcript`) or `session/resume` to reopen the agent's conversation,
//!   else `session/new`.
//! - A turn is `session/prompt`, answered with its `stopReason` once it
//!   settles; `session/update` notifications stream the reply, thoughts, tool
//!   calls, plans, usage and the agent's commands and config options
//!   meanwhile. There is no turn-started notification.
//! - Permission prompts are `session/request_permission` requests answered
//!   with one of the options the agent offered, and form elicitations
//!   (`elicitation/create`, oh-my-pi's approvals) answered with the form.
//! - Model, effort and the agent's other settings are its config options
//!   (legacy `modes` when it has none); the client sees them as
//!   `model_status` and `session_options`, and what the session can do as
//!   `engine_caps`.
//! - One prompt at a time: messages sent during a turn run as the next turns;
//!   `steer` cancels the turn so a queued one runs now.

use super::{
    acp_presets::{self, Mode, Preset},
    claude, find_program, Adapter, Line, Options, Output,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use jucode_agent_core::queue_index;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Command,
};

pub const PROTOCOL_VERSION: u64 = 1;
/// Larger images are not sent.
const IMAGE_CAP: u64 = 20 * 1024 * 1024;

/// The built-in agent `options` names, if any.
pub fn preset(options: &Options) -> Result<Option<&'static Preset>, String> {
    let Some(id) = options.agent.as_deref() else {
        return Ok(None);
    };
    if options.runs_programs() {
        return Err("options.agent and options.command are exclusive".to_string());
    }
    acp_presets::find(id)
        .map(Some)
        .ok_or_else(|| format!("unknown ACP agent {id}"))
}

pub fn command(options: &Options) -> Result<Command, String> {
    if let Some(preset) = preset(options)? {
        return acp_presets::command(preset, options);
    }
    let program = options.command.as_deref().unwrap_or_default();
    let path = Path::new(program);
    let program = if path.components().count() > 1 || path.is_absolute() {
        path.to_path_buf()
    } else {
        find_program(program, &[])
    };
    let mut command = Command::new(program);
    command.args(&options.args);
    Ok(command)
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn block_text(block: &Value) -> &str {
    match text(&block["type"]) {
        "text" => text(&block["text"]),
        "resource_link" => text(&block["uri"]),
        "resource" => text(&block["resource"]["uri"]),
        _ => "",
    }
}

/// A tool call's `diff` content as a unified-diff-ish string.
fn diff_text(item: &Value) -> String {
    let path = text(&item["path"]);
    let mut lines = vec![format!("--- {path}"), format!("+++ {path}")];
    if let Some(old) = item["oldText"].as_str().filter(|old| !old.is_empty()) {
        lines.extend(old.split('\n').map(|line| format!("-{line}")));
    }
    lines.extend(
        text(&item["newText"])
            .split('\n')
            .map(|line| format!("+{line}")),
    );
    lines.join("\n")
}

fn image_mime(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

/// An image file as a prompt block; None when it is unreadable or too big.
fn image_block(path: &str) -> Option<Value> {
    let size = std::fs::metadata(path).ok()?.len();
    if size > IMAGE_CAP {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    Some(json!({ "type": "image", "mimeType": image_mime(path), "data": STANDARD.encode(data) }))
}

/// A select option's choices, groups flattened.
fn choices(option: &Value) -> Vec<&Value> {
    option["options"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|choice| match choice["options"].as_array() {
            Some(group) => group.iter().collect(),
            None => vec![choice],
        })
        .collect()
}

fn choice_values(option: Option<&Value>) -> Vec<Value> {
    option
        .map(|option| {
            choices(option)
                .into_iter()
                .map(|c| c["value"].clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Effort values that leave the level to the agent (OpenCode's "default",
/// oh-my-pi's "auto"): the unset state, not a level of their own.
const UNSET_EFFORTS: &[&str] = &["default", "auto"];

/// An effort option's levels, weakest first as agents list them, and its
/// unset value if it has one.
fn effort_levels(option: Option<&Value>) -> (Vec<Value>, &'static str) {
    let mut unset = "";
    let mut levels = Vec::new();
    for value in choice_values(option) {
        match UNSET_EFFORTS.iter().find(|u| value == **u) {
            Some(u) => unset = u,
            None => levels.push(value),
        }
    }
    (levels, unset)
}

/// A model's name without the provider prefix of its value, as the other
/// engines name models ("deepseek/DeepSeek V4 Pro" for
/// "deepseek/deepseek-v4-pro" is "DeepSeek V4 Pro").
fn model_name<'a>(value: &str, name: &'a str) -> &'a str {
    value
        .split_once('/')
        .and_then(|(provider, _)| name.strip_prefix(provider)?.strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(name)
}

#[derive(Default)]
struct Tool {
    name: String,
    kind: String,
    path: String,
    text: String,
    diff: String,
}

impl Tool {
    fn output(&self) -> String {
        let mut out = json!({ "kind": self.kind });
        for (key, value) in [
            ("path", &self.path),
            ("content", &self.text),
            ("diff", &self.diff),
        ] {
            if !value.is_empty() {
                out[key] = json!(value);
            }
        }
        out.to_string()
    }

    fn absorb(&mut self, content: &Value) {
        for item in content.as_array().into_iter().flatten() {
            match text(&item["type"]) {
                "content" => {
                    let chunk = block_text(&item["content"]);
                    if !chunk.is_empty() {
                        if !self.text.is_empty() {
                            self.text.push('\n');
                        }
                        self.text.push_str(chunk);
                    }
                }
                "diff" => {
                    if self.path.is_empty() {
                        self.path = text(&item["path"]).to_string();
                    }
                    if !self.diff.is_empty() {
                        self.diff.push('\n');
                    }
                    self.diff.push_str(&diff_text(item));
                }
                _ => {}
            }
        }
    }
}

/// An open question from the agent, by the reply it needs.
enum Ask {
    /// `session/request_permission`: the options offered.
    Permission(Vec<Value>),
    /// `elicitation/create`: the form as shown.
    Form(Value),
}

pub struct Acp {
    cwd: PathBuf,
    next_id: u64,
    /// Request id → method and params.
    pending: HashMap<u64, (String, Value)>,
    session: Option<String>,
    /// The agent's conversation to reopen.
    resume: Option<String>,
    /// The model to switch to once open.
    model: Option<String>,
    /// The thinking effort to switch to once open (after the model: its
    /// levels can depend on it).
    effort: Option<String>,
    preset: Option<&'static Preset>,
    /// The approval mode the agent was launched in (presets).
    mode: Mode,
    agent: Value,
    auth: Value,
    load_session: bool,
    resume_session: bool,
    image_prompts: bool,
    /// The agent's config options; legacy `modes` when it sends none.
    config: Vec<Value>,
    modes: Value,
    commands: Vec<Value>,
    usage_size: u64,
    usage_seen: bool,
    /// The last `engine_caps` sent.
    caps_sent: Value,
    /// A `session/load` replay: the transcript so far, and its last plan.
    replay: Option<Vec<Value>>,
    replay_plan: Option<Value>,
    prompt_in_flight: bool,
    busy_announced: bool,
    /// The running turn produced a reply, thought or tool call.
    replied: bool,
    /// Messages waiting for the session or the running turn: text, images.
    queued: VecDeque<(String, Vec<String>)>,
    session_failed: bool,
    /// Synthetic call id → (request id, what was asked).
    approvals: HashMap<String, (Value, Ask)>,
    approval_seq: u64,
    tools: HashMap<String, Tool>,
}

impl Acp {
    pub fn new(cwd: &Path, options: &Options) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            next_id: 0,
            pending: HashMap::new(),
            session: None,
            resume: options.resume.clone(),
            model: options.model.clone(),
            effort: options.effort.clone().filter(|e| !e.is_empty()),
            preset: options.agent.as_deref().and_then(acp_presets::find),
            mode: Mode::of(options.approval_mode.as_deref()),
            agent: Value::Null,
            auth: Value::Null,
            load_session: false,
            resume_session: false,
            image_prompts: false,
            config: Vec::new(),
            modes: Value::Null,
            commands: Vec::new(),
            usage_size: 0,
            usage_seen: false,
            caps_sent: Value::Null,
            replay: None,
            replay_plan: None,
            prompt_in_flight: false,
            busy_announced: false,
            replied: false,
            queued: VecDeque::new(),
            session_failed: false,
            approvals: HashMap::new(),
            approval_seq: 0,
            tools: HashMap::new(),
        }
    }

    fn request(&mut self, method: &str, params: Value) -> String {
        self.next_id += 1;
        self.pending
            .insert(self.next_id, (method.to_string(), params.clone()));
        json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params })
            .to_string()
    }

    fn agent_name(&self) -> String {
        [&self.agent["title"], &self.agent["name"]]
            .into_iter()
            .map(text)
            .find(|name| !name.is_empty())
            .or(self.preset.map(|preset| preset.name))
            .unwrap_or("the agent")
            .to_string()
    }

    /// A prompt for the agent, now: its blocks (images as data) and the
    /// `user_message` clients see.
    fn send(&mut self, content: String, images: Vec<String>) -> Output {
        let mut blocks = vec![json!({ "type": "text", "text": content })];
        let mut events = Vec::new();
        if !images.is_empty() && !self.image_prompts {
            events.push(json!({ "type": "info", "message": format!("[acp] this agent does not take images; {} not sent", images.len()) }));
        } else if !images.is_empty() {
            let sent: Vec<Value> = images.iter().filter_map(|path| image_block(path)).collect();
            if sent.len() < images.len() {
                events.push(json!({ "type": "info", "message": format!("[acp] {} images unreadable or over 20 MB; not sent", images.len() - sent.len()) }));
            }
            blocks.extend(sent);
        }
        events.push(json!({ "type": "user_message", "content": content }));
        self.prompt_in_flight = true;
        self.busy_announced = false;
        self.replied = false;
        let session = self.session.clone();
        Output {
            frames: vec![self.request(
                "session/prompt",
                json!({ "sessionId": session, "prompt": blocks }),
            )],
            events,
        }
    }

    /// A prompt now, or once the session is open and the running turn ended.
    fn submit(&mut self, content: String, images: Vec<String>) -> Output {
        if self.session.is_some() && !self.prompt_in_flight {
            return self.send(content, images);
        }
        self.queued.push_back((content, images));
        let mut output = Output::events(vec![
            self.pending_event(),
            json!({ "type": "status", "message": format!("queued: {}", self.queued.len()) }),
        ]);
        if self.session.is_none() && self.session_failed {
            self.session_failed = false;
            output.frames.push(self.new_session());
        }
        output
    }

    fn pending_event(&self) -> Value {
        let texts: Vec<&str> = self.queued.iter().map(|(text, _)| text.as_str()).collect();
        json!({ "type": "pending_messages", "messages": texts })
    }

    /// The next queued message starts, if the session is open.
    fn next_queued(&mut self) -> Option<Output> {
        self.session.as_ref()?;
        let (content, images) = self.queued.pop_front()?;
        let mut output = self.send(content, images);
        output.events.insert(0, self.pending_event());
        output.events.push(json!({ "type": "connecting" }));
        Some(output)
    }

    fn new_session(&mut self) -> String {
        let cwd = self.cwd.clone();
        self.request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
    }

    /// Opens the conversation: the one to reopen when the agent can, else a
    /// new one (and the user is told).
    fn open(&mut self) -> Output {
        let cwd = self.cwd.clone();
        let params = |id: &str| json!({ "sessionId": id, "cwd": cwd, "mcpServers": [] });
        match self.resume.clone() {
            Some(id) if self.load_session => {
                self.replay = Some(Vec::new());
                Output {
                    frames: vec![self.request("session/load", params(&id))],
                    events: vec![],
                }
            }
            Some(id) if self.resume_session => Output {
                frames: vec![self.request("session/resume", params(&id))],
                events: vec![],
            },
            Some(_) => Output {
                frames: vec![self.new_session()],
                events: vec![
                    json!({ "type": "info", "message": "[acp] this agent cannot reopen conversations; a new one started" }),
                ],
            },
            None => Output {
                frames: vec![self.new_session()],
                events: vec![],
            },
        }
    }

    /// The first config option of a category that is a select.
    fn select(&self, category: &str) -> Option<&Value> {
        self.config
            .iter()
            .find(|option| option["category"] == category && option["type"] == "select")
    }

    fn model_status(&self) -> Option<Value> {
        let model = self.select("model")?;
        let current = &model["currentValue"];
        let label = choices(model)
            .into_iter()
            .find(|choice| &choice["value"] == current)
            .map_or("", |choice| {
                model_name(text(current), text(&choice["name"]))
            });
        let effort = self.select("thought_level");
        let (efforts, unset) = effort_levels(effort);
        Some(json!({
            "type": "model_status", "provider": "acp", "model": text(current), "model_label": label,
            "reasoning_effort": effort.map_or("", |e| text(&e["currentValue"])),
            "reasoning_efforts": efforts, "default_effort": unset,
            "context_window": self.usage_size, "context_limit": 0,
            "state": if self.prompt_in_flight { "streaming" } else { "ready" },
        }))
    }

    fn model_view(&self) -> Option<Value> {
        let model = self.select("model")?;
        let current = &model["currentValue"];
        let effort = self.select("thought_level");
        let (efforts, _) = effort_levels(effort);
        let row = |value: &Value, label: &Value| json!({ "model": value, "label": label, "active": value == current, "context_window": 0, "max_output_tokens": 0, "reasoning_efforts": efforts });
        let mut rows: Vec<Value> = choices(model)
            .into_iter()
            .map(|choice| {
                let value = &choice["value"];
                row(
                    value,
                    &json!(model_name(text(value), text(&choice["name"]))),
                )
            })
            .collect();
        if !rows.iter().any(|row| row["active"] == true) {
            let mut unlisted = row(current, current);
            unlisted["listed"] = json!(false);
            rows.insert(0, unlisted);
        }
        Some(json!({
            "type": "model_view", "models": rows,
            "active_effort": effort.map_or("", |e| text(&e["currentValue"])),
        }))
    }

    /// The agent's settings besides model and effort.
    fn session_options(&self) -> Value {
        let main: Vec<&Value> = [self.select("model"), self.select("thought_level")]
            .into_iter()
            .flatten()
            .map(|option| &option["id"])
            .collect();
        let options: Vec<Value> = if self.config.is_empty() {
            self.modes["availableModes"]
                .as_array()
                .map(|modes| {
                    let choices: Vec<Value> = modes
                        .iter()
                        .map(|m| json!({ "value": m["id"], "name": m["name"], "description": m["description"] }))
                        .collect();
                    vec![json!({
                        "id": "mode", "name": "Mode", "description": null, "category": "mode",
                        "type": "select", "value": self.modes["currentModeId"], "choices": choices,
                    })]
                })
                .unwrap_or_default()
        } else {
            self.config
                .iter()
                .filter(|option| !main.contains(&&option["id"]))
                .map(|option| {
                    let mut entry = json!({
                        "id": option["id"], "name": option["name"], "description": option["description"],
                        "category": option["category"], "type": option["type"], "value": option["currentValue"],
                    });
                    if option["type"] == "select" {
                        entry["choices"] = choices(option)
                            .into_iter()
                            .map(|c| json!({ "value": c["value"], "name": c["name"], "description": c["description"] }))
                            .collect();
                    }
                    entry
                })
                .collect()
        };
        json!({ "type": "session_options", "options": options })
    }

    fn command_list(&self) -> Value {
        let commands: Vec<Value> = self
            .commands
            .iter()
            .map(|c| json!({ "command": format!("/{}", text(&c["name"])), "marker": null, "args": text(&c["input"]["hint"]), "description": c["description"] }))
            .collect();
        json!({ "type": "command_list", "commands": commands })
    }

    /// `engine_caps` when it changed since last sent.
    fn caps_changed(&mut self) -> Option<Value> {
        let caps = json!({
            "type": "engine_caps",
            "agent": { "id": self.preset.map(|p| p.id), "name": self.agent_name(), "version": self.agent["version"] },
            "approvals": self.preset.map(|p| p.approvals),
            "gateway": self.preset.is_some_and(|p| p.gateway),
            "caps": {
                "interrupt": true,
                "approvalModes": self.preset.is_some_and(|p| p.approvals),
                "modelPicker": self.select("model").is_some(),
                "contextUsage": self.usage_seen,
                "slashCommands": !self.commands.is_empty(),
                "transcriptReplay": self.load_session,
            },
        });
        (caps != self.caps_sent).then(|| {
            self.caps_sent = caps.clone();
            caps
        })
    }

    /// The config options changed: what clients see of them.
    fn config_changed(&mut self, options: &Value) -> Vec<Value> {
        if let Some(options) = options.as_array() {
            self.config = options.clone();
        }
        let mut events: Vec<Value> = self.model_status().into_iter().collect();
        events.push(self.session_options());
        events.extend(self.caps_changed());
        events
    }

    /// `session/set_config_option` for option `id` (a select's value or a
    /// boolean).
    fn set_option(&mut self, id: &Value, value: &Value) -> String {
        let session = self.session.clone();
        let mut params = json!({ "sessionId": session, "configId": id, "value": value });
        if value.is_boolean() {
            params["type"] = json!("boolean");
        }
        self.request("session/set_config_option", params)
    }

    /// The conversation is open: its state, then the first queued message.
    fn opened(&mut self, result: &Value) -> Output {
        self.config = result["configOptions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        self.modes = result["modes"].clone();
        let mut events = Vec::new();
        if let Some(mut items) = self.replay.take() {
            self.tools.clear();
            for item in &mut items {
                if let Some(item) = item.as_object_mut() {
                    item.remove("call_id");
                }
            }
            events.push(json!({ "type": "transcript", "items": items }));
            events.extend(self.replay_plan.take());
        }
        events.push(json!({
            "type": "startup", "model": self.select("model").map_or("", |m| text(&m["currentValue"])),
            "cwd": self.cwd, "session_id": "", "context_window": self.usage_size,
        }));
        events.extend(self.model_status());
        events.push(self.session_options());
        if !self.commands.is_empty() {
            events.push(self.command_list());
        }
        if self.preset.is_some_and(|p| p.approvals) {
            events.push(json!({ "type": "approval_mode", "mode": self.mode.name() }));
        }
        events.extend(self.caps_changed());
        events.push(json!({ "type": "status", "message": "ready" }));
        let mut frames = Vec::new();
        // The model asked for, as the agent names it (`provider/model`).
        let wanted = self.model.clone().and_then(|model| {
            let option = self.select("model")?;
            let choices = choices(option);
            let value = choices
                .iter()
                .map(|c| &c["value"])
                .find(|v| text(v) == model)
                .or_else(|| {
                    choices
                        .iter()
                        .map(|c| &c["value"])
                        .find(|v| text(v).ends_with(&format!("/{model}")))
                })
                .cloned()
                .unwrap_or(json!(model));
            (value != option["currentValue"]).then(|| (option["id"].clone(), value))
        });
        if let Some((id, value)) = wanted {
            frames.push(self.set_option(&id, &value));
        }
        let effort = self.effort.clone().and_then(|effort| {
            let option = self.select("thought_level")?;
            (option["currentValue"] != effort.as_str())
                .then(|| option["id"].clone())
                .map(|id| (id, effort))
        });
        if let Some((id, effort)) = effort {
            frames.push(self.set_option(&id, &json!(effort)));
        }
        let mut output = Output { events, frames };
        if let Some(next) = self.next_queued() {
            output.events.extend(next.events);
            output.frames.extend(next.frames);
        }
        output
    }

    /// The turn settled: start the next queued prompt, or go ready.
    fn settle(&mut self) -> Output {
        self.prompt_in_flight = false;
        self.busy_announced = false;
        self.next_queued().unwrap_or_else(|| {
            Output::events(vec![json!({ "type": "status", "message": "ready" })])
        })
    }

    /// A `session/load` replay: the conversation as transcript items.
    fn on_replay(&mut self, update: &Value) {
        let Some(items) = self.replay.as_mut() else {
            return;
        };
        let kind = text(&update["sessionUpdate"]);
        match kind {
            "user_message_chunk" | "agent_message_chunk" => {
                let role = if kind == "user_message_chunk" {
                    "user"
                } else {
                    "assistant"
                };
                let chunk = block_text(&update["content"]);
                match items.last_mut() {
                    Some(last) if last["role"] == role => {
                        last["content"] = json!(format!("{}{chunk}", text(&last["content"])));
                    }
                    _ if chunk.is_empty() => {}
                    _ => items.push(json!({ "role": role, "content": chunk })),
                }
            }
            "tool_call" | "tool_call_update" => {
                let id = text(&update["toolCallId"]).to_string();
                if id.is_empty() {
                    return;
                }
                let tool = self.tools.entry(id.clone()).or_insert_with(|| Tool {
                    name: "tool".to_string(),
                    kind: "other".to_string(),
                    ..Tool::default()
                });
                let set = |field: &mut String, value: &str| {
                    if !value.is_empty() {
                        *field = value.to_string();
                    }
                };
                set(&mut tool.kind, text(&update["kind"]));
                if tool.name == "tool" {
                    set(&mut tool.name, text(&update["kind"]));
                }
                set(&mut tool.name, text(&update["title"]));
                set(&mut tool.path, text(&update["locations"][0]["path"]));
                tool.absorb(&update["content"]);
                let item = json!({ "role": "tool", "name": tool.name, "output": tool.output(), "call_id": id });
                match items.iter_mut().find(|item| item["call_id"] == id.as_str()) {
                    Some(known) => *known = item,
                    None => items.push(item),
                }
            }
            "plan" => self.replay_plan = Some(plan(update)),
            _ => {}
        }
    }

    fn on_update(&mut self, update: &Value) -> Vec<Value> {
        let kind = text(&update["sessionUpdate"]);
        if self.replay.is_some()
            && matches!(
                kind,
                "user_message_chunk"
                    | "agent_message_chunk"
                    | "agent_thought_chunk"
                    | "tool_call"
                    | "tool_call_update"
                    | "plan"
            )
        {
            self.on_replay(update);
            return vec![];
        }
        match kind {
            // Outside a turn (pi-acp's startup banner): not a reply.
            "agent_message_chunk" | "agent_thought_chunk" if !self.prompt_in_flight => vec![],
            "agent_message_chunk" => match block_text(&update["content"]) {
                "" => vec![],
                chunk => {
                    self.replied = true;
                    vec![json!({ "type": "assistant_delta", "delta": chunk })]
                }
            },
            "agent_thought_chunk" => match block_text(&update["content"]) {
                "" => vec![],
                chunk => {
                    self.replied = true;
                    vec![json!({ "type": "reasoning_delta", "delta": chunk })]
                }
            },
            "tool_call" => {
                let id = text(&update["toolCallId"]).to_string();
                if id.is_empty() {
                    return vec![];
                }
                self.replied = true;
                let name = [text(&update["title"]), text(&update["kind"])]
                    .into_iter()
                    .find(|n| !n.is_empty())
                    .unwrap_or("tool");
                let mut tool = Tool {
                    name: name.to_string(),
                    kind: match text(&update["kind"]) {
                        "" => "other",
                        kind => kind,
                    }
                    .to_string(),
                    path: text(&update["locations"][0]["path"]).to_string(),
                    ..Tool::default()
                };
                tool.absorb(&update["content"]);
                let mut events = vec![
                    json!({ "type": "tool_start", "call_id": id, "name": tool.name }),
                    json!({ "type": "tool_update", "call_id": id, "output": tool.output() }),
                ];
                let status = text(&update["status"]);
                if matches!(status, "completed" | "failed") {
                    events.push(json!({ "type": "tool_output", "call_id": id, "name": tool.name, "output": tool.output(), "is_error": status == "failed" }));
                } else {
                    self.tools.insert(id, tool);
                }
                events
            }
            "tool_call_update" => {
                let id = text(&update["toolCallId"]).to_string();
                let Some(tool) = self.tools.get_mut(&id) else {
                    return vec![];
                };
                if !text(&update["title"]).is_empty() {
                    tool.name = text(&update["title"]).to_string();
                }
                if !text(&update["locations"][0]["path"]).is_empty() {
                    tool.path = text(&update["locations"][0]["path"]).to_string();
                }
                tool.absorb(&update["content"]);
                let status = text(&update["status"]);
                if matches!(status, "completed" | "failed") {
                    let tool = self.tools.remove(&id).unwrap_or_default();
                    vec![
                        json!({ "type": "tool_output", "call_id": id, "name": tool.name, "output": tool.output(), "is_error": status == "failed" }),
                    ]
                } else {
                    vec![json!({ "type": "tool_update", "call_id": id, "output": tool.output() })]
                }
            }
            "plan" => vec![plan(update)],
            "config_option_update" => self.config_changed(&update["configOptions"]),
            "current_mode_update" if self.config.is_empty() => {
                self.modes["currentModeId"] = update["currentModeId"].clone();
                vec![self.session_options()]
            }
            "usage_update" => {
                let mut usage = json!({ "type": "context_usage", "tokens": update["used"] });
                if update["cost"]["currency"] == "USD" {
                    usage["cost"] = update["cost"]["amount"].clone();
                }
                let mut events = vec![usage];
                self.usage_seen = true;
                let size = update["size"].as_u64().unwrap_or(0);
                if size != self.usage_size {
                    self.usage_size = size;
                    events.extend(self.model_status());
                }
                events.extend(self.caps_changed());
                events
            }
            "available_commands_update" => {
                self.commands = update["availableCommands"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let mut events = vec![self.command_list()];
                events.extend(self.caps_changed());
                events
            }
            "notice" => {
                let mut message = format!("[acp] {}", text(&update["title"]));
                if !text(&update["description"]).is_empty() {
                    message.push('\n');
                    message.push_str(text(&update["description"]));
                }
                let kind = if update["severity"] == "error" {
                    "error"
                } else {
                    "info"
                };
                vec![json!({ "type": kind, "message": message })]
            }
            _ => vec![],
        }
    }

    fn on_response(&mut self, id: u64, result: &Value, error: &Value) -> Output {
        let Some((method, params)) = self.pending.remove(&id) else {
            return Output::default();
        };
        if !error.is_null() {
            return self.on_error(&method, error);
        }
        match method.as_str() {
            "initialize" => {
                let caps = &result["agentCapabilities"];
                self.image_prompts = caps["promptCapabilities"]["image"] == true;
                self.load_session = caps["loadSession"] == true;
                self.resume_session = caps["sessionCapabilities"]["resume"].is_object();
                self.agent = result["agentInfo"].clone();
                self.auth = result["authMethods"][0].clone();
                self.open()
            }
            "session/new" => {
                self.session = result["sessionId"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                self.opened(result)
            }
            "session/load" | "session/resume" => {
                self.session = self.resume.clone();
                self.opened(result)
            }
            "session/set_config_option" => {
                Output::events(self.config_changed(&result["configOptions"]))
            }
            "session/set_mode" => {
                self.modes["currentModeId"] = params["modeId"].clone();
                Output::events(vec![self.session_options()])
            }
            "session/prompt" => {
                let mut output = Output::default();
                match text(&result["stopReason"]) {
                    "refusal" => output.events.push(json!({ "type": "info", "message": "[acp] the agent refused this request" })),
                    reason @ ("max_tokens" | "max_turn_requests") => {
                        output.events.push(json!({ "type": "info", "message": format!("[acp] the turn stopped at a limit ({reason})") }))
                    }
                    "cancelled" => output.events.push(json!({ "type": "status", "message": "interrupted" })),
                    "end_turn" if !self.replied => output.events.push(json!({ "type": "info", "message": "[acp] the agent ended the turn without a reply" })),
                    _ => {}
                }
                let settled = self.settle();
                output.events.extend(settled.events);
                output.frames.extend(settled.frames);
                output
            }
            _ => Output::default(),
        }
    }

    fn on_error(&mut self, method: &str, error: &Value) -> Output {
        let message = match text(&error["message"]) {
            "" => format!("JSON-RPC error {}", error["code"]),
            message => message.to_string(),
        };
        let message = if method == "session/new" && error["code"] == -32000 {
            let how = [&self.auth["description"], &self.auth["name"]]
                .into_iter()
                .map(text)
                .find(|how| !how.is_empty())
                .unwrap_or(&message);
            format!("[acp] {} needs sign-in: {how}", self.agent_name())
        } else {
            format!("[acp] {message}")
        };
        let mut output = Output::events(vec![json!({ "type": "error", "message": message })]);
        match method {
            "session/prompt" => {
                let settled = self.settle();
                output.events.extend(settled.events);
                output.frames.extend(settled.frames);
            }
            // The conversation cannot be reopened: a new one, as the user is told.
            "session/load" | "session/resume" => {
                self.replay = None;
                self.replay_plan = None;
                output.events.push(json!({ "type": "info", "message": "[acp] this agent cannot reopen conversations; a new one started" }));
                output.frames.push(self.new_session());
            }
            "session/new" | "initialize" => {
                if !self.queued.is_empty() {
                    self.queued.clear();
                    output.events.push(json!({ "type": "error", "message": "The agent could not open a session; the messages waiting for it were not sent" }));
                    output.events.push(self.pending_event());
                }
                self.session_failed = method == "session/new";
                output
                    .events
                    .push(json!({ "type": "status", "message": "ready" }));
            }
            _ => {}
        }
        output
    }

    fn on_request(&mut self, id: &Value, method: &str, params: &Value) -> Output {
        let form = method == "elicitation/create"
            && matches!(params["mode"].as_str(), None | Some("form"));
        if method != "session/request_permission" && !form {
            return Output {
                frames: vec![json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unsupported by client: {method}") } }).to_string()],
                events: vec![json!({ "type": "info", "message": format!("[acp] unsupported request: {method}") })],
            };
        }
        self.approval_seq += 1;
        let call = format!("acp-approval-{}", self.approval_seq);
        if form {
            // A lone unnamed field is asked by the message itself.
            let message = text(&params["message"]);
            let mut schema = params["requestedSchema"].clone();
            if let Some(fields) = schema["properties"].as_object_mut() {
                if fields.len() == 1 {
                    if let Some(field) = fields.values_mut().next() {
                        if text(&field["title"]).is_empty() {
                            field["title"] = json!(message);
                        }
                    }
                }
            }
            let event = json!({
                "type": "approval_request", "call_id": call, "name": "mcp_elicitation",
                "summary": message, "subagent_id": null, "hunks": null,
                "questions": claude::elicitation_questions(&schema),
            });
            self.approvals.insert(call, (id.clone(), Ask::Form(schema)));
            return Output::events(vec![event]);
        }
        let options = params["options"].as_array().cloned().unwrap_or_default();
        self.approvals
            .insert(call.clone(), (id.clone(), Ask::Permission(options)));
        let tool = &params["toolCall"];
        let mut summary = text(&tool["title"]).to_string();
        for item in tool["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "diff")
        {
            summary.push('\n');
            summary.push_str(&diff_text(item));
        }
        let kind = match text(&tool["kind"]) {
            "" => "tool",
            kind => kind,
        };
        if summary.is_empty() {
            summary = kind.to_string();
        }
        Output::events(vec![
            json!({ "type": "approval_request", "call_id": call, "name": kind, "summary": summary, "subagent_id": null, "hunks": null }),
        ])
    }

    fn command(&mut self, input: &str) -> Result<Output, String> {
        let mut words = input.split_whitespace();
        let name = words.next().unwrap_or_default();
        if name == "/model" {
            let unavailable = || format!("{input} is not available in this agent session");
            let (Some(value), effort) = (words.next(), words.next()) else {
                return self
                    .model_view()
                    .map(|view| Output::events(vec![view]))
                    .ok_or_else(unavailable);
            };
            let model = self.select("model").cloned().ok_or_else(unavailable)?;
            let mut frames = Vec::new();
            if model["currentValue"] != value {
                frames.push(self.set_option(&model["id"], &json!(value)));
            }
            let effort = effort.zip(self.select("thought_level").cloned());
            if let Some((effort, option)) = effort {
                if option["currentValue"] != effort {
                    frames.push(self.set_option(&option["id"], &json!(effort)));
                }
            }
            return Ok(Output {
                frames,
                events: vec![],
            });
        }
        let known = self
            .commands
            .iter()
            .any(|c| name.strip_prefix('/') == Some(text(&c["name"])));
        if !known {
            return Err(format!("{name} is not available in this agent session"));
        }
        Ok(self.submit(input.trim().to_string(), Vec::new()))
    }

    fn set_session_option(&mut self, op: &Value) -> Result<Output, String> {
        let id = text(&op["id"]);
        let value = &op["value"];
        if self.config.is_empty() && id == "mode" && self.modes.is_object() {
            let session = self.session.clone();
            let frame = self.request(
                "session/set_mode",
                json!({ "sessionId": session, "modeId": value }),
            );
            return Ok(Output {
                frames: vec![frame],
                events: vec![],
            });
        }
        if !self.config.iter().any(|option| option["id"] == id) {
            return Err(format!("this agent has no option {id}"));
        }
        Ok(Output {
            frames: vec![self.set_option(&op["id"], value)],
            events: vec![],
        })
    }
}

fn plan(update: &Value) -> Value {
    let plan: Vec<Value> = update["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| json!({ "step": text(&entry["content"]), "status": match text(&entry["status"]) { "" => "pending", s => s } }))
        .collect();
    json!({ "type": "plan", "plan": plan })
}

impl Adapter for Acp {
    fn start(&mut self) -> Vec<String> {
        vec![self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientInfo": { "name": "jucode", "title": "JuCode", "version": env!("CARGO_PKG_VERSION") },
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                    "elicitation": { "form": {} },
                    "session": { "configOptions": { "boolean": {} }, "notices": {} },
                },
            }),
        )]
    }

    fn translate(&mut self, line: Line) -> Output {
        let frame = match line {
            // Diagnostics only; kept for an exit message (Session::note_stderr).
            Line::Stderr(_) => return Output::default(),
            Line::Frame(frame) => frame,
        };
        // No turn-started notification: the first frame after a prompt
        // means the turn is running.
        let announce = self.prompt_in_flight && !self.busy_announced;
        if announce {
            self.busy_announced = true;
        }
        let id = &frame["id"];
        let has_id = id.is_u64() || id.is_string();
        let mut output = match frame["method"].as_str() {
            Some(method) if has_id => self.on_request(id, method, &frame["params"]),
            Some("session/update") => Output::events(self.on_update(&frame["params"]["update"])),
            // The agent withdrew a question: it no longer waits for the answer.
            Some("$/cancel_request") => {
                let request = &frame["params"]["requestId"];
                self.approvals.retain(|_, (asked, _)| asked != request);
                Output::default()
            }
            Some(_) => Output::default(),
            None => match id.as_u64() {
                Some(id) => self.on_response(id, &frame["result"], &frame["error"]),
                None => Output::default(),
            },
        };
        if announce {
            output.events.insert(0, json!({ "type": "connecting" }));
        }
        output
    }

    fn encode(&mut self, op: &Value) -> Result<Output, String> {
        let frames = match text(&op["op"]) {
            "user_message" => {
                let images = op["images"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|image| text(image).to_string())
                    .collect();
                return Ok(self.submit(text(&op["content"]).to_string(), images));
            }
            "command" => return self.command(text(&op["input"])),
            "set_session_option" => return self.set_session_option(op),
            "set_approval_mode" if self.preset.is_some_and(|p| p.approvals) => {
                // A different mode restarts the agent (`restart_for`).
                return Ok(Output::events(vec![
                    json!({ "type": "approval_mode", "mode": self.mode.name() }),
                ]));
            }
            "approve" => {
                let call = text(&op["call_id"]);
                let Some((request, ask)) = self.approvals.remove(call) else {
                    return Err(format!("no open approval {call}"));
                };
                let deny = op["decision"] == "deny";
                let result = match ask {
                    Ask::Form(_) if deny => json!({ "action": "decline" }),
                    Ask::Form(schema) => {
                        json!({ "action": "accept", "content": claude::elicitation_content(&schema, &op["answers"]) })
                    }
                    Ask::Permission(options) => {
                        let kinds: &[&str] = if deny {
                            &["reject_once", "reject_always"]
                        } else if op["always"] == true {
                            &["allow_always", "allow_once"]
                        } else {
                            &["allow_once", "allow_always"]
                        };
                        let option = kinds.iter().find_map(|kind| {
                            options
                                .iter()
                                .find(|o| o["kind"] == *kind && !text(&o["optionId"]).is_empty())
                        });
                        let outcome = match option {
                            Some(option) => {
                                json!({ "outcome": "selected", "optionId": option["optionId"] })
                            }
                            None => json!({ "outcome": "cancelled" }),
                        };
                        json!({ "outcome": outcome })
                    }
                };
                vec![json!({ "jsonrpc": "2.0", "id": request, "result": result }).to_string()]
            }
            // A queued message (`index`, the next by default, negative from
            // the last) goes now: the
            // turn is cancelled and, once it settles, that message starts
            // the next one, ahead of the others waiting.
            "steer" | "interrupt" => {
                let Some(session) = self.session.clone() else {
                    return Ok(Output::default());
                };
                let steer = op["op"] == "steer";
                if steer {
                    let Some(index) = queue_index(op["index"].as_i64(), self.queued.len()) else {
                        return Ok(Output::default());
                    };
                    if !self.prompt_in_flight {
                        return Ok(Output::default());
                    }
                    if let Some(chosen) = self.queued.remove(index) {
                        self.queued.push_front(chosen);
                    }
                }
                let mut frames: Vec<String> = self
                    .approvals
                    .drain()
                    .map(|(_, (request, ask))| {
                        let result = match ask {
                            Ask::Permission(_) => json!({ "outcome": { "outcome": "cancelled" } }),
                            Ask::Form(_) => json!({ "action": "cancel" }),
                        };
                        json!({ "jsonrpc": "2.0", "id": request, "result": result }).to_string()
                    })
                    .collect();
                if !steer {
                    self.queued.clear();
                }
                frames.push(json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": session } }).to_string());
                return Ok(Output {
                    frames,
                    events: vec![self.pending_event()],
                });
            }
            // A waiting message taken back: `index`, negative from the last.
            "unqueue" => {
                let Some((content, images)) = queue_index(op["index"].as_i64(), self.queued.len())
                    .and_then(|i| self.queued.remove(i))
                else {
                    return Ok(Output::default());
                };
                return Ok(Output::events(vec![
                    self.pending_event(),
                    json!({ "type": "unqueued", "content": content, "images": images }),
                ]));
            }
            "shutdown" => vec![],
            other => return Err(format!("ACP agents do not support {other}")),
        };
        Ok(Output {
            events: Vec::new(),
            frames,
        })
    }

    fn busy(&self) -> bool {
        self.prompt_in_flight
    }

    /// Presets take their approval mode as a launch flag: another mode
    /// restarts the agent, which reopens the conversation.
    fn restart_for(&self, op: &Value) -> Option<Options> {
        let switch = op["op"] == "set_approval_mode"
            && self.preset.is_some_and(|p| p.approvals)
            && Mode::of(op["mode"].as_str()) != self.mode;
        switch.then(|| Options {
            approval_mode: op["mode"].as_str().map(str::to_string),
            ..Options::default()
        })
    }

    fn conversation(&self) -> Option<String> {
        self.session.clone()
    }

    /// The agent restores its model when it reopens a conversation; this
    /// keeps an idle close from switching back to the model it started on.
    fn model(&self) -> Option<String> {
        self.select("model")
            .and_then(|option| option["currentValue"].as_str())
            .map(str::to_string)
    }

    fn approval_mode(&self) -> Option<String> {
        self.preset
            .filter(|p| p.approvals)
            .map(|_| self.mode.name().to_string())
    }

    fn mode_applies_live(&self) -> bool {
        false
    }

    fn has_gateway(&self) -> bool {
        self.preset.is_some_and(|p| p.gateway)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(acp: &mut Acp, value: Value) -> Output {
        acp.translate(Line::Frame(value))
    }

    fn update(acp: &mut Acp, update: Value) -> Output {
        frame(
            acp,
            json!({ "method": "session/update", "params": { "sessionId": "s1", "update": update } }),
        )
    }

    fn sent(frames: &[String]) -> Vec<Value> {
        frames
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect()
    }

    fn types(events: &[Value]) -> Vec<&str> {
        events.iter().map(|e| text(&e["type"])).collect()
    }

    fn event<'a>(events: &'a [Value], kind: &str) -> &'a Value {
        events
            .iter()
            .find(|e| e["type"] == kind)
            .unwrap_or_else(|| panic!("no {kind} in {events:#?}"))
    }

    fn config() -> Value {
        json!([
            { "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "jucode/gpt-6",
              "options": [{ "group": "jucode", "name": "JuCode", "options": [
                  { "value": "jucode/gpt-6", "name": "GPT-6" }, { "value": "jucode/claude-5", "name": "Claude 5" }] }] },
            { "id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": "low",
              "options": [{ "value": "low", "name": "Low" }, { "value": "high", "name": "High" }] },
            { "id": "mode", "name": "Session Mode", "category": "mode", "type": "select", "currentValue": "build",
              "options": [{ "value": "build", "name": "build" }, { "value": "plan", "name": "plan" }] },
            { "id": "x", "name": "Auto compact", "type": "boolean", "currentValue": true },
        ])
    }

    /// An adapter through `initialize` with `caps`, before the open answers.
    fn started(options: Options, caps: Value) -> (Acp, Vec<Value>) {
        let mut a = Acp::new(Path::new("/p"), &options);
        a.start();
        let out = frame(
            &mut a,
            json!({ "id": 1, "result": { "protocolVersion": 1, "agentCapabilities": caps,
                "agentInfo": { "name": "omp", "title": "Oh My Pi", "version": "18.1.21" },
                "authMethods": [{ "id": "agent", "name": "Agent login", "description": "Run omp login" }] } }),
        );
        (a, sent(&out.frames))
    }

    fn omp() -> Options {
        Options {
            agent: Some("omp".into()),
            approval_mode: Some("read-only".into()),
            ..Options::default()
        }
    }

    /// An open omp session with `config()`.
    fn opened() -> Acp {
        let (mut a, _) = started(omp(), json!({ "loadSession": true }));
        frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "s1", "configOptions": config() } }),
        );
        a
    }

    #[test]
    fn the_handshake_offers_form_elicitation_boolean_options_and_notices() {
        let mut a = Acp::new(Path::new("/p"), &Options::default());
        let init = &sent(&a.start())[0];
        assert_eq!(init["method"], "initialize");
        let caps = &init["params"]["clientCapabilities"];
        assert_eq!(caps["elicitation"], json!({ "form": {} }));
        assert_eq!(caps["session"]["configOptions"]["boolean"], json!({}));
        assert_eq!(caps["session"]["notices"], json!({}));
        assert_eq!(caps["terminal"], false);
        assert_eq!(init["params"]["clientInfo"]["name"], "jucode");
    }

    #[test]
    fn the_model_and_effort_to_start_with_are_set_once_open() {
        // A new session's picks made before it started (the desktop's draft).
        let (mut a, _) = started(
            Options {
                model: Some("jucode/claude-5".into()),
                effort: Some("high".into()),
                ..omp()
            },
            json!({}),
        );
        let open = frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "s1", "configOptions": config() } }),
        );
        let frames = sent(&open.frames);
        let set: Vec<(&Value, &Value)> = frames
            .iter()
            .map(|f| (&f["params"]["configId"], &f["params"]["value"]))
            .collect();
        assert_eq!(
            set,
            [
                (&json!("model"), &json!("jucode/claude-5")),
                (&json!("effort"), &json!("high"))
            ]
        );
        // What it already runs asks for nothing.
        let (mut a, _) = started(
            Options {
                effort: Some("low".into()),
                ..omp()
            },
            json!({}),
        );
        let open = frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "s1", "configOptions": config() } }),
        );
        assert!(open.frames.is_empty());
    }

    #[test]
    fn an_open_session_reports_model_options_mode_and_caps() {
        let (mut a, frames) = started(
            Options {
                model: Some("claude-5".into()),
                ..omp()
            },
            json!({ "loadSession": true }),
        );
        assert_eq!(frames[0]["method"], "session/new");
        assert_eq!(frames[0]["params"]["mcpServers"], json!([]));
        let open = frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "s1", "configOptions": config() } }),
        );
        assert_eq!(
            types(&open.events),
            [
                "startup",
                "model_status",
                "session_options",
                "approval_mode",
                "engine_caps",
                "status"
            ]
        );
        assert_eq!(open.events[0]["model"], "jucode/gpt-6");
        let status = &open.events[1];
        assert_eq!(status["model_label"], "GPT-6");
        assert_eq!(status["reasoning_effort"], "low");
        assert_eq!(status["reasoning_efforts"], json!(["low", "high"]));
        let options = &open.events[2]["options"];
        assert_eq!(
            options.as_array().unwrap().len(),
            2,
            "model and effort left out"
        );
        assert_eq!(options[0]["choices"][1]["value"], "plan");
        assert_eq!(options[1]["type"], "boolean");
        assert_eq!(options[1]["value"], true);
        assert!(options[1].get("choices").is_none());
        assert_eq!(open.events[3]["mode"], "read-only");
        assert_eq!(
            open.events[4],
            json!({ "type": "engine_caps",
                "agent": { "id": "omp", "name": "Oh My Pi", "version": "18.1.21" },
                "approvals": true, "gateway": true,
                "caps": { "interrupt": true, "approvalModes": true, "modelPicker": true, "contextUsage": false,
                          "slashCommands": false, "transcriptReplay": true } })
        );
        // The asked-for model, as the agent names it.
        let set = &sent(&open.frames)[0];
        assert_eq!(set["method"], "session/set_config_option");
        assert_eq!(set["params"]["configId"], "model");
        assert_eq!(set["params"]["value"], "jucode/claude-5");
        assert_eq!(a.conversation().as_deref(), Some("s1"));
    }

    #[test]
    fn a_custom_agent_has_no_preset_metadata() {
        let (mut a, _) = started(
            Options {
                command: Some("agent".into()),
                ..Options::default()
            },
            json!({}),
        );
        let open = frame(&mut a, json!({ "id": 2, "result": { "sessionId": "s1" } }));
        assert!(!types(&open.events).contains(&"model_status"));
        assert!(!types(&open.events).contains(&"approval_mode"));
        let caps = event(&open.events, "engine_caps");
        assert_eq!(caps["agent"]["id"], Value::Null);
        assert_eq!(caps["approvals"], Value::Null);
        assert_eq!(caps["gateway"], false);
        assert_eq!(caps["caps"]["modelPicker"], false);
        assert!(a
            .encode(&json!({ "op": "set_approval_mode", "mode": "full-access" }))
            .is_err());
        assert!(a
            .restart_for(&json!({ "op": "set_approval_mode", "mode": "full-access" }))
            .is_none());
        assert!(a
            .encode(&json!({ "op": "command", "input": "/model" }))
            .is_err());
    }

    #[test]
    fn config_updates_show_the_model_unlisted_values_and_flips() {
        let mut a = opened();
        let mut options = config();
        options[0]["currentValue"] = json!("other/model");
        options.as_array_mut().unwrap().remove(1);
        let out = update(
            &mut a,
            json!({ "sessionUpdate": "config_option_update", "configOptions": options }),
        );
        assert_eq!(types(&out.events), ["model_status", "session_options"]);
        assert_eq!(out.events[0]["model"], "other/model");
        assert_eq!(out.events[0]["model_label"], "");
        assert_eq!(out.events[0]["reasoning_efforts"], json!([]));
        let view = a
            .encode(&json!({ "op": "command", "input": "/model" }))
            .unwrap();
        let rows = &view.events[0]["models"];
        assert_eq!(rows[0]["model"], "other/model");
        assert_eq!(rows[0]["listed"], false);
        assert_eq!(rows[1]["label"], "GPT-6");
        assert_eq!(rows.as_array().unwrap().len(), 3);
        // No model option any more: the picker goes.
        let out = update(
            &mut a,
            json!({ "sessionUpdate": "config_option_update", "configOptions": [] }),
        );
        assert_eq!(types(&out.events), ["session_options", "engine_caps"]);
        assert_eq!(out.events[1]["caps"]["modelPicker"], false);
    }

    #[test]
    fn model_and_effort_switches_are_config_option_requests() {
        let mut a = opened();
        let out = a
            .encode(&json!({ "op": "command", "input": "/model jucode/claude-5 high" }))
            .unwrap();
        let frames = sent(&out.frames);
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0]["params"],
            json!({ "sessionId": "s1", "configId": "model", "value": "jucode/claude-5" })
        );
        assert_eq!(frames[1]["params"]["configId"], "effort");
        assert_eq!(frames[1]["params"]["value"], "high");
        // The answer carries the options as they are now.
        let mut options = config();
        options[0]["currentValue"] = json!("jucode/claude-5");
        let id = frames[0]["id"].clone();
        let done = frame(
            &mut a,
            json!({ "id": id, "result": { "configOptions": options } }),
        );
        assert_eq!(done.events[0]["model"], "jucode/claude-5");
        // The same model and effort again ask for nothing.
        let same = a
            .encode(&json!({ "op": "command", "input": "/model jucode/claude-5 low" }))
            .unwrap();
        assert!(same.frames.is_empty());
    }

    #[test]
    fn an_agent_default_effort_is_the_unset_state_and_names_drop_the_provider() {
        // OpenCode 2.0: "default" is listed after the strongest level, and
        // names repeat the provider the value starts with.
        let mut a = opened();
        let options = json!([
            { "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "deepseek/deepseek-flash",
              "options": [{ "value": "deepseek/deepseek-flash", "name": "deepseek/DeepSeek V4.1 Flash" },
                          { "value": "openai/gpt-5", "name": "GPT-5" }] },
            { "id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": "default",
              "options": [{ "value": "none", "name": "None" }, { "value": "low", "name": "Low" },
                          { "value": "high", "name": "High" }, { "value": "max", "name": "Max" },
                          { "value": "default", "name": "Default" }] },
        ]);
        let out = update(
            &mut a,
            json!({ "sessionUpdate": "config_option_update", "configOptions": options }),
        );
        let status = event(&out.events, "model_status");
        assert_eq!(status["model_label"], "DeepSeek V4.1 Flash");
        assert_eq!(status["reasoning_effort"], "default");
        assert_eq!(
            status["reasoning_efforts"],
            json!(["none", "low", "high", "max"])
        );
        assert_eq!(status["default_effort"], "default");
        let view = a
            .encode(&json!({ "op": "command", "input": "/model" }))
            .unwrap();
        let rows = &view.events[0]["models"];
        assert_eq!(rows[0]["label"], "DeepSeek V4.1 Flash");
        assert_eq!(rows[1]["label"], "GPT-5");
        assert_eq!(
            rows[0]["reasoning_efforts"],
            json!(["none", "low", "high", "max"])
        );
        // Back to the agent's default is a pick like any level.
        let frames = sent(
            &a.encode(&json!({ "op": "command", "input": "/model deepseek/deepseek-flash high" }))
                .unwrap()
                .frames,
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["params"]["value"], "high");
        // An effort option without one has no unset value.
        let out = update(
            &mut a,
            json!({ "sessionUpdate": "config_option_update", "configOptions": config() }),
        );
        assert_eq!(event(&out.events, "model_status")["default_effort"], "");
    }

    #[test]
    fn session_options_set_selects_booleans_and_legacy_modes() {
        let mut a = opened();
        let select = sent(
            &a.encode(&json!({ "op": "set_session_option", "id": "mode", "value": "plan" }))
                .unwrap()
                .frames,
        );
        assert_eq!(select[0]["method"], "session/set_config_option");
        assert!(select[0]["params"].get("type").is_none());
        let boolean = sent(
            &a.encode(&json!({ "op": "set_session_option", "id": "x", "value": false }))
                .unwrap()
                .frames,
        );
        assert_eq!(boolean[0]["params"]["type"], "boolean");
        assert_eq!(boolean[0]["params"]["value"], false);
        assert!(a
            .encode(&json!({ "op": "set_session_option", "id": "nope", "value": 1 }))
            .is_err());

        // An agent with only legacy modes.
        let (mut b, _) = started(omp(), json!({}));
        let open = frame(
            &mut b,
            json!({ "id": 2, "result": { "sessionId": "s2", "modes": { "currentModeId": "ask",
                "availableModes": [{ "id": "ask", "name": "Ask" }, { "id": "code", "name": "Code" }] } } }),
        );
        let options = &event(&open.events, "session_options")["options"];
        assert_eq!(options[0]["id"], "mode");
        assert_eq!(options[0]["value"], "ask");
        let set = sent(
            &b.encode(&json!({ "op": "set_session_option", "id": "mode", "value": "code" }))
                .unwrap()
                .frames,
        );
        assert_eq!(set[0]["method"], "session/set_mode");
        assert_eq!(set[0]["params"]["modeId"], "code");
        let done = frame(&mut b, json!({ "id": set[0]["id"], "result": {} }));
        assert_eq!(done.events[0]["options"][0]["value"], "code");
        let changed = update(
            &mut b,
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "ask" }),
        );
        assert_eq!(changed.events[0]["options"][0]["value"], "ask");
    }

    #[test]
    fn usage_updates_feed_context_usage_and_the_window() {
        let mut a = opened();
        let first = update(
            &mut a,
            json!({ "sessionUpdate": "usage_update", "used": 1200, "size": 200000, "cost": { "amount": 0.25, "currency": "USD" } }),
        );
        assert_eq!(
            types(&first.events),
            ["context_usage", "model_status", "engine_caps"]
        );
        assert_eq!(
            first.events[0],
            json!({ "type": "context_usage", "tokens": 1200, "cost": 0.25 })
        );
        assert_eq!(first.events[1]["context_window"], 200000);
        assert_eq!(first.events[2]["caps"]["contextUsage"], true);
        let next = update(
            &mut a,
            json!({ "sessionUpdate": "usage_update", "used": 1500, "size": 200000, "cost": { "amount": 9, "currency": "EUR" } }),
        );
        assert_eq!(
            next.events,
            [json!({ "type": "context_usage", "tokens": 1500 })]
        );
    }

    #[test]
    fn agent_commands_are_listed_and_run_as_prompts() {
        let mut a = opened();
        let out = update(
            &mut a,
            json!({ "sessionUpdate": "available_commands_update", "availableCommands": [
                { "name": "review", "description": "Review changes", "input": { "hint": "path" } }] }),
        );
        assert_eq!(
            out.events[0]["commands"],
            json!([{ "command": "/review", "marker": null, "args": "path", "description": "Review changes" }])
        );
        assert_eq!(out.events[1]["caps"]["slashCommands"], true);
        let run = a
            .encode(&json!({ "op": "command", "input": "/review src" }))
            .unwrap();
        assert_eq!(
            sent(&run.frames)[0]["params"]["prompt"][0]["text"],
            "/review src"
        );
        assert_eq!(event(&run.events, "user_message")["content"], "/review src");
        assert!(a
            .encode(&json!({ "op": "command", "input": "/compact" }))
            .is_err());
    }

    #[test]
    fn replies_outside_a_turn_are_dropped() {
        let mut a = opened();
        let banner = update(
            &mut a,
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "pi v1.2" } }),
        );
        assert!(banner.events.is_empty());
    }

    #[test]
    fn a_reopened_conversation_replays_into_one_transcript() {
        let (mut a, frames) = started(
            Options {
                resume: Some("s1".into()),
                ..omp()
            },
            json!({ "loadSession": true, "sessionCapabilities": { "resume": {} } }),
        );
        assert_eq!(frames[0]["method"], "session/load");
        assert_eq!(frames[0]["params"]["sessionId"], "s1");
        let mut live = Vec::new();
        for u in [
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "fix " } }),
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "it" } }),
            json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "hmm" } }),
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "on " } }),
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "it" } }),
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit a.rs", "kind": "edit", "status": "pending" }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [{ "type": "content", "content": { "type": "text", "text": "done" } }] }),
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "fixed" } }),
            json!({ "sessionUpdate": "plan", "entries": [{ "content": "step", "status": "completed" }] }),
        ] {
            live.extend(update(&mut a, u).events);
        }
        assert!(live.is_empty(), "{live:#?}");
        let open = frame(
            &mut a,
            json!({ "id": 2, "result": { "configOptions": config() } }),
        );
        assert_eq!(types(&open.events)[..3], ["transcript", "plan", "startup"]);
        assert_eq!(
            open.events[0]["items"],
            json!([
                { "role": "user", "content": "fix it" },
                { "role": "assistant", "content": "on it" },
                { "role": "tool", "name": "Edit a.rs", "output": r#"{"content":"done","kind":"edit"}"# },
                { "role": "assistant", "content": "fixed" },
            ])
        );
        assert_eq!(a.conversation().as_deref(), Some("s1"));
    }

    #[test]
    fn reopening_falls_back_from_load_to_resume_to_a_new_conversation() {
        let resume = || Options {
            resume: Some("s1".into()),
            ..omp()
        };
        let (_, frames) = started(resume(), json!({ "sessionCapabilities": { "resume": {} } }));
        assert_eq!(frames[0]["method"], "session/resume");
        // No way to reopen: a new conversation, and the user is told.
        let mut a = Acp::new(Path::new("/p"), &resume());
        a.start();
        let out = frame(
            &mut a,
            json!({ "id": 1, "result": { "agentCapabilities": {} } }),
        );
        assert_eq!(sent(&out.frames)[0]["method"], "session/new");
        assert_eq!(
            out.events[0]["message"],
            "[acp] this agent cannot reopen conversations; a new one started"
        );
        // A failed load: the error, the same notice and a new conversation.
        let (mut b, _) = started(resume(), json!({ "loadSession": true }));
        let failed = frame(
            &mut b,
            json!({ "id": 2, "error": { "code": -32002, "message": "no such session" } }),
        );
        assert_eq!(types(&failed.events), ["error", "info"]);
        assert_eq!(sent(&failed.frames)[0]["method"], "session/new");
        let open = frame(&mut b, json!({ "id": 3, "result": { "sessionId": "s9" } }));
        assert!(!types(&open.events).contains(&"transcript"));
        assert_eq!(b.conversation().as_deref(), Some("s9"));
    }

    #[test]
    fn an_agent_that_needs_sign_in_says_how() {
        let (mut a, _) = started(omp(), json!({}));
        let out = frame(
            &mut a,
            json!({ "id": 2, "error": { "code": -32000, "message": "Authentication required" } }),
        );
        assert_eq!(
            out.events[0]["message"],
            "[acp] Oh My Pi needs sign-in: Run omp login"
        );
    }

    #[test]
    fn a_form_elicitation_is_an_approval_card_answered_with_the_form() {
        let mut a = opened();
        let ask = |a: &mut Acp, id: u64| {
            frame(
                a,
                json!({ "id": id, "method": "elicitation/create", "params": { "sessionId": "s1", "mode": "form",
                    "message": "Allow tool: edit\nFile: b.txt",
                    "requestedSchema": { "type": "object", "properties": { "value": { "type": "string", "enum": ["Approve", "Deny"] } } } } }),
            )
            .events
            .remove(0)
        };
        let card = ask(&mut a, 40);
        assert_eq!(card["name"], "mcp_elicitation");
        assert_eq!(card["summary"], "Allow tool: edit\nFile: b.txt");
        assert_eq!(
            card["questions"][0]["question"],
            "Allow tool: edit\nFile: b.txt"
        );
        assert_eq!(card["questions"][0]["options"][0]["label"], "Approve");
        let accept = sent(
            &a.encode(
                &json!({ "op": "approve", "call_id": card["call_id"], "decision": "allow",
                "answers": { "Allow tool: edit\nFile: b.txt": "Approve" } }),
            )
            .unwrap()
            .frames,
        );
        assert_eq!(accept[0]["id"], 40);
        assert_eq!(
            accept[0]["result"],
            json!({ "action": "accept", "content": { "value": "Approve" } })
        );
        let card = ask(&mut a, 41);
        let decline = sent(
            &a.encode(&json!({ "op": "approve", "call_id": card["call_id"], "decision": "deny" }))
                .unwrap()
                .frames,
        );
        assert_eq!(decline[0]["result"], json!({ "action": "decline" }));
        // Other modes are not supported.
        let url = frame(
            &mut a,
            json!({ "id": 42, "method": "elicitation/create", "params": { "mode": "url", "url": "https://x" } }),
        );
        assert_eq!(sent(&url.frames)[0]["error"]["code"], -32601);
        // A withdrawn question is dropped.
        let card = ask(&mut a, 43);
        frame(
            &mut a,
            json!({ "method": "$/cancel_request", "params": { "requestId": 43 } }),
        );
        assert!(a
            .encode(&json!({ "op": "approve", "call_id": card["call_id"], "decision": "allow" }))
            .is_err());
    }

    #[test]
    fn interrupt_answers_open_questions_clears_the_queue_and_cancels() {
        let mut a = opened();
        a.encode(&json!({ "op": "user_message", "content": "one" }))
            .unwrap();
        let queued = a
            .encode(&json!({ "op": "user_message", "content": "two" }))
            .unwrap();
        assert_eq!(
            queued.events,
            [
                json!({ "type": "pending_messages", "messages": ["two"] }),
                json!({ "type": "status", "message": "queued: 1" })
            ]
        );
        frame(
            &mut a,
            json!({ "id": 9, "method": "session/request_permission", "params": { "toolCall": { "title": "ls" }, "options": [] } }),
        );
        frame(
            &mut a,
            json!({ "id": 10, "method": "elicitation/create", "params": { "message": "ok?", "requestedSchema": { "properties": {} } } }),
        );
        let out = a.encode(&json!({ "op": "interrupt" })).unwrap();
        let frames = sent(&out.frames);
        let answer = |id: u64| frames.iter().find(|f| f["id"] == id).unwrap()["result"].clone();
        assert_eq!(answer(9), json!({ "outcome": { "outcome": "cancelled" } }));
        assert_eq!(answer(10), json!({ "action": "cancel" }));
        assert_eq!(frames.last().unwrap()["method"], "session/cancel");
        assert_eq!(
            out.events,
            [json!({ "type": "pending_messages", "messages": [] })]
        );
        let prompt = a
            .pending
            .iter()
            .find(|(_, (m, _))| m == "session/prompt")
            .map(|(id, _)| *id)
            .unwrap();
        let done = frame(
            &mut a,
            json!({ "id": prompt, "result": { "stopReason": "cancelled" } }),
        );
        assert_eq!(
            done.events,
            [
                json!({ "type": "status", "message": "interrupted" }),
                json!({ "type": "status", "message": "ready" })
            ]
        );
        assert!(!a.busy());
    }

    #[test]
    fn steering_cancels_the_turn_and_the_chosen_message_runs_next() {
        let mut a = opened();
        a.encode(&json!({ "op": "user_message", "content": "one" }))
            .unwrap();
        for text in ["two", "three"] {
            a.encode(&json!({ "op": "user_message", "content": text }))
                .unwrap();
        }
        let out = a.encode(&json!({ "op": "steer", "index": -1 })).unwrap();
        assert_eq!(
            sent(&out.frames).last().unwrap()["method"],
            "session/cancel"
        );
        assert_eq!(
            out.events,
            [json!({ "type": "pending_messages", "messages": ["three", "two"] })]
        );
        let prompt = a
            .pending
            .iter()
            .find(|(_, (m, _))| m == "session/prompt")
            .map(|(id, _)| *id)
            .unwrap();
        let done = frame(
            &mut a,
            json!({ "id": prompt, "result": { "stopReason": "cancelled" } }),
        );
        let next = sent(&done.frames);
        assert_eq!(next[0]["method"], "session/prompt");
        assert_eq!(next[0]["params"]["prompt"][0]["text"], "three");
    }

    #[test]
    fn a_waiting_message_can_be_taken_back() {
        let mut a = opened();
        for text in ["one", "two", "three"] {
            a.encode(&json!({ "op": "user_message", "content": text }))
                .unwrap();
        }
        let back = a.encode(&json!({ "op": "unqueue", "index": -1 })).unwrap();
        assert_eq!(
            back.events,
            [
                json!({ "type": "pending_messages", "messages": ["two"] }),
                json!({ "type": "unqueued", "content": "three", "images": [] }),
            ]
        );
    }

    #[test]
    fn a_turn_without_a_reply_says_so() {
        let mut a = opened();
        let sent_prompt = sent(
            &a.encode(&json!({ "op": "user_message", "content": "hi" }))
                .unwrap()
                .frames,
        );
        let done = frame(
            &mut a,
            json!({ "id": sent_prompt[0]["id"], "result": { "stopReason": "end_turn" } }),
        );
        assert_eq!(
            done.events[1]["message"],
            "[acp] the agent ended the turn without a reply"
        );
    }

    #[test]
    fn images_go_as_base64_blocks_when_the_agent_takes_them() {
        let dir = std::env::temp_dir().join(format!("jucode-acp-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("a.png");
        std::fs::write(&image, [1u8, 2, 3]).unwrap();
        let image = image.display().to_string();
        let (mut a, _) = started(omp(), json!({ "promptCapabilities": { "image": true } }));
        frame(&mut a, json!({ "id": 2, "result": { "sessionId": "s1" } }));
        let out = a
            .encode(&json!({ "op": "user_message", "content": "look", "images": [image, "/no/such.png"] }))
            .unwrap();
        let blocks = &sent(&out.frames)[0]["params"]["prompt"];
        assert_eq!(
            blocks[1],
            json!({ "type": "image", "mimeType": "image/png", "data": "AQID" })
        );
        assert_eq!(blocks.as_array().unwrap().len(), 2);
        assert_eq!(types(&out.events), ["info", "user_message"]);

        let (mut b, _) = started(omp(), json!({}));
        frame(&mut b, json!({ "id": 2, "result": { "sessionId": "s1" } }));
        let out = b
            .encode(&json!({ "op": "user_message", "content": "look", "images": [image] }))
            .unwrap();
        assert_eq!(
            sent(&out.frames)[0]["params"]["prompt"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            out.events[0]["message"],
            "[acp] this agent does not take images; 1 not sent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_approval_mode_restarts_a_preset_and_the_same_one_is_acknowledged() {
        let mut a = opened();
        assert_eq!(a.approval_mode().as_deref(), Some("read-only"));
        assert!(!a.mode_applies_live());
        let same = json!({ "op": "set_approval_mode", "mode": "manual" });
        assert!(a.restart_for(&same).is_none());
        assert_eq!(
            a.encode(&same).unwrap().events,
            [json!({ "type": "approval_mode", "mode": "read-only" })]
        );
        for mode in ["auto-edit", "full-access"] {
            let restart = a
                .restart_for(&json!({ "op": "set_approval_mode", "mode": mode }))
                .unwrap();
            assert_eq!(restart.approval_mode.as_deref(), Some(mode));
        }
        // Pi never asks: it has no modes to switch.
        let pi = Acp::new(
            Path::new("/p"),
            &Options {
                agent: Some("pi".into()),
                ..Options::default()
            },
        );
        assert!(pi
            .restart_for(&json!({ "op": "set_approval_mode", "mode": "full-access" }))
            .is_none());
        assert!(pi.has_gateway());
    }

    #[test]
    fn prompts_queue_behind_the_handshake_and_the_running_turn() {
        let (mut a, _) = started(Options::default(), json!({}));
        assert!(a
            .encode(&json!({ "op": "user_message", "content": "one" }))
            .unwrap()
            .frames
            .is_empty());
        let opened = frame(&mut a, json!({ "id": 2, "result": { "sessionId": "s1" } }));
        assert_eq!(
            sent(&opened.frames)[0]["params"]["prompt"][0]["text"],
            "one"
        );
        assert!(types(&opened.events).contains(&"user_message"));
        assert!(a.busy());
        assert!(a
            .encode(&json!({ "op": "user_message", "content": "two" }))
            .unwrap()
            .frames
            .is_empty());

        let chunk = update(
            &mut a,
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" } }),
        );
        assert_eq!(chunk.events[0]["type"], "connecting");
        assert_eq!(chunk.events[1]["delta"], "hi");
        // The turn settles and the queued message starts.
        let settled = frame(
            &mut a,
            json!({ "id": 3, "result": { "stopReason": "end_turn" } }),
        );
        assert_eq!(
            sent(&settled.frames)[0]["params"]["prompt"][0]["text"],
            "two"
        );
        assert_eq!(
            types(&settled.events),
            ["pending_messages", "user_message", "connecting"]
        );
        frame(
            &mut a,
            json!({ "id": 4, "result": { "stopReason": "end_turn" } }),
        );
        assert!(!a.busy());
    }

    #[test]
    fn permissions_pick_the_offered_option_and_tools_fill_their_cards() {
        let mut a = Acp::new(Path::new("/p"), &Options::default());
        let ask = frame(
            &mut a,
            json!({ "id": 9, "method": "session/request_permission", "params": {
            "toolCall": { "title": "Edit a.rs", "kind": "edit", "content": [{ "type": "diff", "path": "a.rs", "oldText": "x", "newText": "y" }] },
            "options": [{ "optionId": "no", "kind": "reject_once" }, { "optionId": "yes", "kind": "allow_once" }, { "optionId": "all", "kind": "allow_always" }],
        } }),
        );
        let request = &ask.events[0];
        assert!(text(&request["summary"]).contains("-x\n+y"));
        let answer = sent(&a.encode(&json!({ "op": "approve", "call_id": request["call_id"], "decision": "allow", "always": true })).unwrap().frames);
        assert_eq!(answer[0]["result"]["outcome"]["optionId"], "all");

        let start = update(
            &mut a,
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "ls", "kind": "execute" }),
        );
        assert_eq!(start.events[0]["type"], "tool_start");
        let done = update(
            &mut a,
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed", "content": [{ "type": "content", "content": { "type": "text", "text": "a.rs" } }] }),
        );
        assert_eq!(done.events[0]["type"], "tool_output");
        assert!(text(&done.events[0]["output"]).contains("a.rs"));
    }

    #[test]
    fn notices_are_info_or_errors() {
        let mut a = opened();
        let out = update(
            &mut a,
            json!({ "sessionUpdate": "notice", "severity": "warning", "title": "Low credit", "description": "Top up" }),
        );
        assert_eq!(
            out.events,
            [json!({ "type": "info", "message": "[acp] Low credit\nTop up" })]
        );
        let out = update(
            &mut a,
            json!({ "sessionUpdate": "notice", "severity": "error", "title": "Down" }),
        );
        assert_eq!(out.events[0]["type"], "error");
    }

    #[test]
    fn preset_options_never_carry_a_command() {
        let both = Options {
            agent: Some("omp".into()),
            command: Some("/bin/sh".into()),
            ..Options::default()
        };
        assert_eq!(
            preset(&both).err().as_deref(),
            Some("options.agent and options.command are exclusive")
        );
        let unknown = Options {
            agent: Some("deepseek".into()),
            ..Options::default()
        };
        assert_eq!(
            preset(&unknown).err().as_deref(),
            Some("unknown ACP agent deepseek")
        );
    }
}
