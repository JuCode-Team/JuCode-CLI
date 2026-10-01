//! Agent Client Protocol agents (JSON-RPC 2.0 over stdio lines, protocol v1,
//! https://agentclientprotocol.com): `jucode acp`, `gemini --experimental-acp`
//! and the like, started with the command the desktop's agent registry names.
//!
//! - Handshake: `initialize` → `session/new` (the agent's session id).
//! - A turn is `session/prompt`, answered with its `stopReason` once it
//!   settles; `session/update` notifications stream the reply, thoughts, tool
//!   calls and plans meanwhile. There is no turn-started notification.
//! - Permission prompts are `session/request_permission` requests answered
//!   with one of the options the agent offered.
//! - One prompt at a time: messages sent during a turn run as the next turns.
//! - Conversations cannot be resumed (session/load is optional); reopening a
//!   session starts a new conversation.

use super::{find_program, Adapter, Line, Options, Output};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Command,
};

pub const PROTOCOL_VERSION: u64 = 1;

pub fn command(options: &Options) -> Command {
    let program = options.command.as_deref().unwrap_or_default();
    let path = Path::new(program);
    let program = if path.components().count() > 1 || path.is_absolute() {
        path.to_path_buf()
    } else {
        find_program(program, &[])
    };
    let mut command = Command::new(program);
    command.args(&options.args);
    command
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

pub struct Acp {
    cwd: PathBuf,
    next_id: u64,
    pending: HashMap<u64, String>,
    session: Option<String>,
    image_prompts: bool,
    prompt_in_flight: bool,
    busy_announced: bool,
    queued: VecDeque<Vec<Value>>,
    session_failed: bool,
    /// Synthetic call id → (request id, the options offered).
    approvals: HashMap<String, (Value, Vec<Value>)>,
    approval_seq: u64,
    tools: HashMap<String, Tool>,
}

impl Acp {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            next_id: 0,
            pending: HashMap::new(),
            session: None,
            image_prompts: false,
            prompt_in_flight: false,
            busy_announced: false,
            queued: VecDeque::new(),
            session_failed: false,
            approvals: HashMap::new(),
            approval_seq: 0,
            tools: HashMap::new(),
        }
    }

    fn request(&mut self, method: &str, params: Value) -> String {
        self.next_id += 1;
        self.pending.insert(self.next_id, method.to_string());
        json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params })
            .to_string()
    }

    fn prompt(&mut self, blocks: Vec<Value>) -> String {
        self.prompt_in_flight = true;
        self.busy_announced = false;
        let session = self.session.clone();
        self.request(
            "session/prompt",
            json!({ "sessionId": session, "prompt": blocks }),
        )
    }

    fn new_session(&mut self) -> String {
        let cwd = self.cwd.clone();
        self.request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
    }

    /// The turn settled: start the next queued prompt, or go ready.
    fn settle(&mut self) -> Output {
        self.prompt_in_flight = false;
        self.busy_announced = false;
        match self.queued.pop_front() {
            Some(next) if self.session.is_some() => Output {
                frames: vec![self.prompt(next)],
                events: vec![json!({ "type": "connecting" })],
            },
            _ => Output::events(vec![json!({ "type": "status", "message": "ready" })]),
        }
    }

    fn on_update(&mut self, update: &Value) -> Vec<Value> {
        match text(&update["sessionUpdate"]) {
            "agent_message_chunk" => match block_text(&update["content"]) {
                "" => vec![],
                chunk => vec![json!({ "type": "assistant_delta", "delta": chunk })],
            },
            "agent_thought_chunk" => match block_text(&update["content"]) {
                "" => vec![],
                chunk => vec![json!({ "type": "reasoning_delta", "delta": chunk })],
            },
            "tool_call" => {
                let id = text(&update["toolCallId"]).to_string();
                if id.is_empty() {
                    return vec![];
                }
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
            "plan" => {
                let plan: Vec<Value> = update["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|entry| json!({ "step": text(&entry["content"]), "status": match text(&entry["status"]) { "" => "pending", s => s } }))
                    .collect();
                vec![json!({ "type": "plan", "plan": plan })]
            }
            _ => vec![],
        }
    }

    fn on_response(&mut self, id: u64, result: &Value, error: &Value) -> Output {
        let Some(method) = self.pending.remove(&id) else {
            return Output::default();
        };
        if !error.is_null() {
            let message = match text(&error["message"]) {
                "" => format!("JSON-RPC error {}", error["code"]),
                message => message.to_string(),
            };
            let mut output = Output::events(vec![
                json!({ "type": "error", "message": format!("[acp] {message}") }),
            ]);
            if method == "session/prompt" {
                let settled = self.settle();
                output.events.extend(settled.events);
                output.frames.extend(settled.frames);
            }
            if method == "session/new" || method == "initialize" {
                if !self.queued.is_empty() {
                    self.queued.clear();
                    output.events.push(json!({ "type": "error", "message": "The agent could not open a session; the messages waiting for it were not sent" }));
                }
                self.session_failed = method == "session/new";
                output
                    .events
                    .push(json!({ "type": "status", "message": "ready" }));
            }
            return output;
        }
        match method.as_str() {
            "initialize" => {
                self.image_prompts =
                    result["agentCapabilities"]["promptCapabilities"]["image"] == true;
                Output {
                    events: vec![],
                    frames: vec![self.new_session()],
                }
            }
            "session/new" => {
                self.session = result["sessionId"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                let current = &result["models"]["currentModelId"];
                let model = result["models"]["availableModels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|m| &m["modelId"] == current)
                    .map(|m| text(&m["name"]).to_string())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| text(current).to_string());
                let mut events = vec![
                    json!({ "type": "startup", "model": model, "cwd": self.cwd, "session_id": "", "context_window": 0 }),
                ];
                if !model.is_empty() {
                    events.push(json!({ "type": "model_status", "provider": "acp", "model": model, "reasoning_effort": "", "reasoning_efforts": [], "context_window": 0, "context_limit": 0 }));
                }
                events.push(json!({ "type": "status", "message": "ready" }));
                let mut frames = Vec::new();
                if self.session.is_some() {
                    if let Some(next) = self.queued.pop_front() {
                        frames.push(self.prompt(next));
                        events.push(json!({ "type": "connecting" }));
                    }
                }
                Output { events, frames }
            }
            "session/prompt" => {
                let mut output = Output::default();
                match text(&result["stopReason"]) {
                    "refusal" => output.events.push(json!({ "type": "info", "message": "[acp] the agent refused this request" })),
                    reason @ ("max_tokens" | "max_turn_requests") => {
                        output.events.push(json!({ "type": "info", "message": format!("[acp] the turn stopped at a limit ({reason})") }))
                    }
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

    fn on_request(&mut self, id: &Value, method: &str, params: &Value) -> Output {
        if method != "session/request_permission" {
            return Output {
                frames: vec![json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unsupported by client: {method}") } }).to_string()],
                events: vec![json!({ "type": "info", "message": format!("[acp] unsupported request: {method}") })],
            };
        }
        self.approval_seq += 1;
        let call = format!("acp-approval-{}", self.approval_seq);
        let options = params["options"].as_array().cloned().unwrap_or_default();
        self.approvals.insert(call.clone(), (id.clone(), options));
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
}

impl Adapter for Acp {
    fn start(&mut self) -> Vec<String> {
        vec![self.request(
            "initialize",
            json!({ "protocolVersion": PROTOCOL_VERSION, "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false } }),
        )]
    }

    fn translate(&mut self, line: Line) -> Output {
        let frame = match line {
            Line::Stderr(line) => {
                let line = super::strip_ansi(&line);
                let line = line.trim();
                return if line.is_empty() {
                    Output::default()
                } else {
                    Output::events(vec![
                        json!({ "type": "info", "message": format!("[acp] {line}") }),
                    ])
                };
            }
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
                let mut blocks = vec![json!({ "type": "text", "text": op["content"] })];
                if self.image_prompts {
                    for image in op["images"].as_array().into_iter().flatten().map(text) {
                        blocks.push(json!({ "type": "image", "mimeType": image_mime(image), "data": "", "uri": format!("file://{image}") }));
                    }
                }
                if self.session.is_none() || self.prompt_in_flight {
                    self.queued.push_back(blocks);
                    if self.session.is_none() && self.session_failed {
                        self.session_failed = false;
                        vec![self.new_session()]
                    } else {
                        vec![]
                    }
                } else {
                    vec![self.prompt(blocks)]
                }
            }
            "approve" => {
                let call = text(&op["call_id"]);
                let Some((request, options)) = self.approvals.remove(call) else {
                    return Err(format!("no open approval {call}"));
                };
                let kinds: &[&str] = if op["decision"] == "deny" {
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
                vec![
                    json!({ "jsonrpc": "2.0", "id": request, "result": { "outcome": outcome } })
                        .to_string(),
                ]
            }
            "interrupt" => {
                let Some(session) = self.session.clone() else {
                    return Ok(Output::default());
                };
                let mut frames: Vec<String> = self
                    .approvals
                    .drain()
                    .map(|(_, (request, _))| json!({ "jsonrpc": "2.0", "id": request, "result": { "outcome": { "outcome": "cancelled" } } }).to_string())
                    .collect();
                self.queued.clear();
                frames.push(json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": session } }).to_string());
                frames
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

    fn restart_for(&self, _op: &Value) -> Option<Options> {
        None
    }

    fn conversation(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(acp: &mut Acp, value: Value) -> Output {
        acp.translate(Line::Frame(value))
    }

    fn sent(frames: &[String]) -> Vec<Value> {
        frames
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect()
    }

    #[test]
    fn prompts_queue_behind_the_handshake_and_the_running_turn() {
        let mut a = Acp::new(Path::new("/p"));
        assert_eq!(sent(&a.start())[0]["method"], "initialize");
        assert!(a
            .encode(&json!({ "op": "user_message", "content": "one" }))
            .unwrap()
            .frames
            .is_empty());
        let new = sent(&frame(&mut a, json!({ "id": 1, "result": {} })).frames);
        assert_eq!(new[0]["method"], "session/new");
        let opened = frame(&mut a, json!({ "id": 2, "result": { "sessionId": "s1" } }));
        assert_eq!(
            sent(&opened.frames)[0]["params"]["prompt"][0]["text"],
            "one"
        );
        assert!(a.busy());
        assert!(a
            .encode(&json!({ "op": "user_message", "content": "two" }))
            .unwrap()
            .frames
            .is_empty());

        let chunk = frame(
            &mut a,
            json!({ "method": "session/update", "params": { "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" } } } }),
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
        frame(
            &mut a,
            json!({ "id": 4, "result": { "stopReason": "end_turn" } }),
        );
        assert!(!a.busy());
    }

    #[test]
    fn permissions_pick_the_offered_option_and_tools_fill_their_cards() {
        let mut a = Acp::new(Path::new("/p"));
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

        let start = frame(
            &mut a,
            json!({ "method": "session/update", "params": { "update": { "sessionUpdate": "tool_call", "toolCallId": "t", "title": "ls", "kind": "execute" } } }),
        );
        assert_eq!(start.events[0]["type"], "tool_start");
        let done = frame(
            &mut a,
            json!({ "method": "session/update", "params": { "update": { "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed", "content": [{ "type": "content", "content": { "type": "text", "text": "a.rs" } }] } } }),
        );
        assert_eq!(done.events[0]["type"], "tool_output");
        assert!(text(&done.events[0]["output"]).contains("a.rs"));
    }
}
