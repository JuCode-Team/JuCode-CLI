//! Codex through `codex app-server`: JSON-RPC 2.0 over stdio lines, the v2
//! thread/turn surface (verified against codex-cli 0.144–0.158).
//!
//! - Handshake: `initialize` → `initialized` → `thread/start` (or
//!   `thread/resume`, whose answer carries the history) and `model/list`.
//!   The thread id is the conversation id.
//! - A turn is `turn/start`; notifications `turn/started`, `item/started`,
//!   `item/*/delta`, `item/completed`, `thread/tokenUsage/updated` and
//!   `turn/completed` describe it.
//! - Approvals are server→client requests
//!   (`item/commandExecution/requestApproval`,
//!   `item/fileChange/requestApproval`) answered with `{decision}`.
//! - Approval mode and the model picked with `/model` apply as overrides on
//!   every later `turn/start`; there is no thread-level setter.

use super::{home, resolve, Adapter, Line, Options, Output};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::Command,
};

pub fn command() -> Command {
    let mut command = Command::new(resolve("codex", "CODEX_BIN", &[]));
    command.arg("app-server");
    command
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// Client approval mode (jucode or Desktop names) → the Desktop engine mode
/// Codex supports: `read-only`, `auto-edit` or `full-auto`.
fn engine_mode(mode: &str) -> &'static str {
    match mode {
        "auto-edit" | "auto" => "auto-edit",
        "full-auto" | "full-access" => "full-auto",
        _ => "read-only",
    }
}

/// The approval policy and sandbox policy of an engine mode.
fn policy(mode: &str) -> (&'static str, Value) {
    match mode {
        "auto-edit" => (
            "on-request",
            json!({ "type": "workspaceWrite", "writableRoots": [], "networkAccess": false, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false }),
        ),
        "full-auto" => ("never", json!({ "type": "dangerFullAccess" })),
        _ => (
            "on-request",
            json!({ "type": "readOnly", "networkAccess": false }),
        ),
    }
}

/// `thread/start` takes the sandbox as a mode string.
fn sandbox_mode(sandbox: &Value) -> &'static str {
    match text(&sandbox["type"]) {
        "dangerFullAccess" => "danger-full-access",
        "workspaceWrite" => "workspace-write",
        _ => "read-only",
    }
}

fn error_event(message: &str, info: &Value) -> Value {
    let lower = message.to_lowercase();
    let unauthorized = info == "unauthorized"
        || info
            .as_object()
            .is_some_and(|map| map.values().any(|v| v["httpStatusCode"] == 401))
        || lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
        || (lower.contains("token") && (lower.contains("invalid") || lower.contains("expired")));
    let hint = if unauthorized {
        " (Codex needs to sign in again: run `codex login` in a terminal.)"
    } else {
        ""
    };
    json!({ "type": "error", "message": format!("{message}{hint}") })
}

fn file_change_output(changes: &[Value], error: Option<&str>) -> String {
    let paths: Vec<&str> = changes
        .iter()
        .map(|c| text(&c["path"]))
        .filter(|p| !p.is_empty())
        .collect();
    let diff = changes
        .iter()
        .map(|c| text(&c["diff"]))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out =
        json!({ "path": paths.first().copied().unwrap_or_default(), "paths": paths, "diff": diff });
    if let Some(error) = error {
        out["error"] = json!(error);
    }
    out.to_string()
}

fn mcp_body(item: &Value) -> String {
    let body = [
        &item["error"]["message"],
        &item["result"]["structuredContent"],
        &item["result"]["content"],
    ]
    .into_iter()
    .find(|v| !v.is_null())
    .cloned()
    .unwrap_or(Value::Null);
    match body {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

fn command_output(item: &Value, command: &str, streamed: &str) -> Value {
    let mut out = json!({
        "command": match text(&item["command"]) { "" => command, c => c },
        "stdout": item["aggregatedOutput"].as_str().unwrap_or(streamed),
    });
    if let Some(code) = item["exitCode"].as_i64() {
        out["exit_code"] = json!(code);
    }
    out
}

/// A resumed thread's history (`thread.turns[].items`) as transcript items.
fn transcript(turns: &Value) -> Vec<Value> {
    let mut rows = Vec::new();
    for item in turns
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|turn| turn["items"].as_array().into_iter().flatten())
    {
        match text(&item["type"]) {
            "userMessage" => {
                let content = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|c| c["type"] == "text")
                    .map(|c| text(&c["text"]))
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !content.is_empty() {
                    rows.push(json!({ "role": "user", "content": content }));
                }
            }
            "agentMessage" if !text(&item["text"]).is_empty() => {
                rows.push(json!({ "role": "assistant", "content": item["text"] }));
            }
            "commandExecution" => rows.push(json!({ "role": "tool", "name": "bash", "output": command_output(item, "", "").to_string() })),
            "fileChange" => rows.push(json!({
                "role": "tool", "name": "apply_patch",
                "output": file_change_output(item["changes"].as_array().map(Vec::as_slice).unwrap_or_default(), None),
            })),
            "mcpToolCall" => rows.push(json!({
                "role": "tool", "name": format!("{}.{}", text(&item["server"]), text(&item["tool"])), "output": mcp_body(item),
            })),
            "webSearch" => rows.push(json!({ "role": "tool", "name": "web_search", "output": json!({ "query": item["query"] }).to_string() })),
            _ => {}
        }
    }
    rows
}

struct Item {
    name: String,
    command: String,
    changes: Vec<Value>,
    streamed: usize,
    output: String,
}

impl Item {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            command: String::new(),
            changes: Vec::new(),
            streamed: 0,
            output: String::new(),
        }
    }
}

pub struct Codex {
    cwd: PathBuf,
    mode: &'static str,
    next_id: u64,
    /// Our outstanding requests: id → (method, tag).
    pending: HashMap<u64, (String, String)>,
    thread: Option<String>,
    resume: Option<String>,
    active_turn: Option<String>,
    /// A turn is starting or running.
    busy: bool,
    /// Input sent before the thread opened.
    queued: Vec<Value>,
    open_params: Value,
    /// Synthetic call id → the server request id awaiting our answer.
    approvals: HashMap<String, Value>,
    approval_seq: u64,
    items: HashMap<String, Item>,
    model: String,
    provider: String,
    effort: String,
    previous_total: (u64, u64),
    context_window: u64,
    catalog: Vec<Value>,
    pending_pick: Option<(String, Option<String>)>,
    override_model: Option<String>,
    override_effort: Option<String>,
    saw_compaction_item: bool,
}

impl Codex {
    pub fn new(cwd: &Path, options: &Options) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            mode: engine_mode(options.approval_mode.as_deref().unwrap_or_default()),
            next_id: 0,
            pending: HashMap::new(),
            thread: None,
            resume: options.resume.clone(),
            active_turn: None,
            busy: false,
            queued: Vec::new(),
            open_params: Value::Null,
            approvals: HashMap::new(),
            approval_seq: 0,
            items: HashMap::new(),
            model: String::new(),
            provider: String::new(),
            effort: String::new(),
            previous_total: (0, 0),
            context_window: 0,
            catalog: Vec::new(),
            pending_pick: None,
            override_model: options.model.clone(),
            override_effort: None,
            saw_compaction_item: false,
        }
    }

    fn request(&mut self, method: &str, params: Value, tag: &str) -> String {
        self.next_id += 1;
        self.pending
            .insert(self.next_id, (method.to_string(), tag.to_string()));
        json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params })
            .to_string()
    }

    fn turn_start(&mut self, input: Vec<Value>) -> String {
        let (approval, sandbox) = policy(self.mode);
        let mut params = json!({
            "threadId": self.thread,
            "input": input,
            "approvalPolicy": approval,
            "sandboxPolicy": sandbox,
        });
        if let Some(model) = &self.override_model {
            params["model"] = json!(model);
        }
        if let Some(effort) = &self.override_effort {
            params["effort"] = json!(effort);
        }
        self.busy = true;
        self.request("turn/start", params, "")
    }

    fn efforts(&self, model: &str) -> Vec<Value> {
        self.catalog
            .iter()
            .find(|m| m["model"] == model || m["id"] == model)
            .and_then(|m| m["supportedReasoningEfforts"].as_array())
            .map(|efforts| {
                efforts
                    .iter()
                    .map(|e| e["reasoningEffort"].clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn model_status(&self) -> Value {
        json!({
            "type": "model_status",
            "provider": self.provider,
            "model": self.model,
            "reasoning_effort": self.effort,
            "reasoning_efforts": self.efforts(&self.model),
            "context_window": self.context_window,
            "context_limit": 0,
            "state": if self.busy { "streaming" } else { "ready" },
        })
    }

    fn command_list() -> Value {
        let commands: Vec<Value> = [("/model", ""), ("/resume", ""), ("/compact", ""), ("/goal", "Set or show the thread goal (/goal <objective>, /goal clear)")]
            .iter()
            .map(|(command, description)| json!({ "command": command, "marker": null, "args": "", "description": description }))
            .collect();
        json!({ "type": "command_list", "commands": commands })
    }

    fn goal_event(goal: &Value) -> Value {
        if !goal.is_object() {
            return json!({ "type": "goal", "goal": null });
        }
        let status = match text(&goal["status"]) {
            "usageLimited" | "budgetLimited" => "blocked",
            status => status,
        };
        json!({ "type": "goal", "goal": {
            "objective": goal["objective"].as_str().unwrap_or_default(),
            "status": status,
            "token_budget": goal["tokenBudget"],
            "tokens_used": goal["tokensUsed"].as_u64().unwrap_or(0),
            "time_used_seconds": goal["timeUsedSeconds"].as_u64().unwrap_or(0),
        } })
    }

    fn model_view(&self) -> Value {
        let mut rows: Vec<Value> = self
            .catalog
            .iter()
            .map(|m| {
                let active = m["model"] == self.model.as_str();
                json!({
                    "model": m["model"], "active": active,
                    "context_window": if active { self.context_window } else { 0 },
                    "max_output_tokens": 0,
                    "reasoning_efforts": self.efforts(text(&m["model"])),
                })
            })
            .collect();
        if !self.model.is_empty() && !rows.iter().any(|r| r["active"] == true) {
            rows.insert(0, json!({ "model": self.model, "active": true, "context_window": self.context_window, "max_output_tokens": 0, "reasoning_efforts": self.efforts(&self.model) }));
        }
        json!({ "type": "model_view", "models": rows, "active_effort": self.effort })
    }

    fn thread_opened(&mut self, result: &Value, resumed: bool) -> Output {
        self.thread = result["thread"]["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        self.model = text(&result["model"]).to_string();
        self.provider = text(&result["modelProvider"]).to_string();
        self.effort = text(&result["reasoningEffort"]).to_string();
        self.items.clear();
        self.previous_total = (0, 0);
        let mut events = Vec::new();
        if resumed {
            let rows = transcript(&result["thread"]["turns"]);
            if !rows.is_empty() {
                events.push(json!({ "type": "transcript", "items": rows }));
            }
        }
        events.extend([
            json!({ "type": "startup", "model": self.model, "cwd": result["cwd"], "session_id": self.thread, "context_window": self.context_window }),
            self.model_status(),
            Self::command_list(),
            json!({ "type": "approval_mode", "mode": self.mode }),
            json!({ "type": "status", "message": "ready" }),
        ]);
        let mut frames = Vec::new();
        if self.thread.is_some() && !self.queued.is_empty() {
            let input = std::mem::take(&mut self.queued);
            frames.push(self.turn_start(input));
            events.push(json!({ "type": "connecting" }));
        }
        Output { events, frames }
    }

    fn on_response(&mut self, id: u64, result: &Value, error: &Value) -> Output {
        let Some((method, tag)) = self.pending.remove(&id) else {
            return Output::default();
        };
        if !error.is_null() {
            let message = match text(&error["message"]) {
                "" => format!("JSON-RPC error {}", error["code"]),
                message => message.to_string(),
            };
            if method == "thread/compact/start" {
                return Output::events(vec![
                    json!({ "type": "compaction_failed", "error": message }),
                ]);
            }
            if method == "thread/resume" && self.open_params.is_object() {
                let params = self.open_params.clone();
                let frame = self.request("thread/start", params, "");
                return Output {
                    events: vec![
                        json!({ "type": "resume_failed" }),
                        error_event(&message, &Value::Null),
                    ],
                    frames: vec![frame],
                };
            }
            let mut events = vec![error_event(&message, &Value::Null)];
            if method == "thread/start" && !self.queued.is_empty() {
                self.queued.clear();
                events.push(error_event(
                    "Codex could not open a thread; the messages waiting for it were not sent",
                    &Value::Null,
                ));
            }
            if matches!(
                method.as_str(),
                "thread/start" | "thread/resume" | "turn/start"
            ) {
                self.busy = false;
                events.push(json!({ "type": "status", "message": "ready" }));
            }
            return Output::events(events);
        }
        match method.as_str() {
            "initialize" => {
                let (approval, sandbox) = policy(self.mode);
                let open = json!({ "cwd": self.cwd, "approvalPolicy": approval, "sandbox": sandbox_mode(&sandbox) });
                self.open_params = open.clone();
                let mut frames =
                    vec![json!({ "jsonrpc": "2.0", "method": "initialized" }).to_string()];
                frames.push(match self.resume.clone() {
                    Some(thread) => {
                        let mut params = open;
                        params["threadId"] = json!(thread);
                        self.request("thread/resume", params, "")
                    }
                    None => self.request("thread/start", open, ""),
                });
                frames.push(self.request("model/list", json!({}), ""));
                Output {
                    events: vec![],
                    frames,
                }
            }
            "thread/start" => self.thread_opened(result, false),
            "thread/resume" => self.thread_opened(result, true),
            "model/list" => {
                self.catalog = result["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| m["hidden"] != true)
                    .cloned()
                    .collect();
                if tag == "view" {
                    return Output::events(vec![self.model_view()]);
                }
                if tag == "apply" {
                    if let Some((model, effort)) = self.pending_pick.take() {
                        let entry = self
                            .catalog
                            .iter()
                            .find(|m| m["model"] == model.as_str() || m["id"] == model.as_str());
                        let model = entry
                            .map(|m| text(&m["model"]).to_string())
                            .unwrap_or(model);
                        let effort = effort.or_else(|| {
                            entry
                                .and_then(|m| m["defaultReasoningEffort"].as_str())
                                .map(str::to_string)
                        });
                        self.model = model.clone();
                        self.effort = effort.clone().unwrap_or_default();
                        self.override_model = Some(model);
                        self.override_effort = effort;
                        return Output::events(vec![self.model_status()]);
                    }
                }
                Output::events(if self.model.is_empty() {
                    vec![]
                } else {
                    vec![self.model_status()]
                })
            }
            "thread/list" => {
                let items: Vec<Value> = result["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|thread| {
                        let id = text(&thread["id"]);
                        let label = [text(&thread["name"]), text(&thread["preview"])].into_iter().find(|l| !l.is_empty()).map(str::to_string).unwrap_or_else(|| id.chars().take(8).collect());
                        json!({ "id": id, "label": label, "detail": "", "active": Some(id) == self.thread.as_deref() })
                    })
                    .collect();
                Output::events(vec![json!({ "type": "resume_view", "items": items })])
            }
            "thread/goal/get" => Output::events(vec![Self::goal_event(&result["goal"])]),
            "turn/start" => {
                if let Some(turn) = result["turn"]["id"].as_str() {
                    self.active_turn = Some(turn.to_string());
                }
                Output::default()
            }
            _ => Output::default(),
        }
    }

    fn on_server_request(&mut self, id: &Value, method: &str, params: &Value) -> Output {
        let approval = matches!(
            method,
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
        );
        // Full access never prompts; a turn started before the switch still may.
        if approval && self.mode == "full-auto" {
            return Output {
                events: vec![],
                frames: vec![
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "decision": "accept" } })
                        .to_string(),
                ],
            };
        }
        if !approval {
            return Output {
                frames: vec![json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unsupported by client: {method}") } }).to_string()],
                events: vec![json!({ "type": "info", "message": format!("[codex] unsupported request: {method}") })],
            };
        }
        self.approval_seq += 1;
        let call = format!("approval-{}", self.approval_seq);
        self.approvals.insert(call.clone(), id.clone());
        let item = self.items.get(text(&params["itemId"]));
        let (name, summary) = if method == "item/commandExecution/requestApproval" {
            let command = match text(&params["command"]) {
                "" => item.map(|i| i.command.clone()).unwrap_or_default(),
                command => command.to_string(),
            };
            let summary = match text(&params["reason"]) {
                "" => command,
                reason => format!("{command}\n{reason}"),
            };
            ("bash", summary)
        } else {
            let summary = item
                .map(|i| {
                    i.changes
                        .iter()
                        .map(|c| {
                            format!(
                                "{} {}\n{}",
                                match text(&c["kind"]["type"]) {
                                    "" => "edit",
                                    k => k,
                                },
                                text(&c["path"]),
                                text(&c["diff"])
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| text(&params["reason"]).to_string());
            ("apply_patch", summary)
        };
        Output::events(vec![
            json!({ "type": "approval_request", "call_id": call, "name": name, "summary": summary, "subagent_id": null, "hunks": null }),
        ])
    }

    fn item_started(&mut self, item: &Value) -> Vec<Value> {
        let id = text(&item["id"]).to_string();
        match text(&item["type"]) {
            "agentMessage" => {
                self.items.insert(id, Item::new("assistant"));
                vec![json!({ "type": "assistant_start" })]
            }
            "reasoning" => {
                self.items.insert(id, Item::new("reasoning"));
                vec![]
            }
            "commandExecution" => {
                let mut meta = Item::new("bash");
                meta.command = text(&item["command"]).to_string();
                let update = json!({ "command": meta.command }).to_string();
                self.items.insert(id.clone(), meta);
                vec![
                    json!({ "type": "tool_start", "call_id": id, "name": "bash" }),
                    json!({ "type": "tool_update", "call_id": id, "output": update }),
                ]
            }
            "fileChange" => {
                let mut meta = Item::new("apply_patch");
                meta.changes = item["changes"].as_array().cloned().unwrap_or_default();
                let update = file_change_output(&meta.changes, None);
                self.items.insert(id.clone(), meta);
                vec![
                    json!({ "type": "tool_start", "call_id": id, "name": "apply_patch" }),
                    json!({ "type": "tool_update", "call_id": id, "output": update }),
                ]
            }
            "mcpToolCall" => {
                let name = format!("{}.{}", text(&item["server"]), text(&item["tool"]));
                self.items.insert(id.clone(), Item::new(&name));
                vec![json!({ "type": "tool_start", "call_id": id, "name": name })]
            }
            "dynamicToolCall" => {
                let name = match text(&item["tool"]) {
                    "" => "tool",
                    t => t,
                }
                .to_string();
                self.items.insert(id.clone(), Item::new(&name));
                vec![json!({ "type": "tool_start", "call_id": id, "name": name })]
            }
            "webSearch" => {
                self.items.insert(id.clone(), Item::new("web_search"));
                vec![
                    json!({ "type": "tool_start", "call_id": id, "name": "web_search" }),
                    json!({ "type": "tool_update", "call_id": id, "output": json!({ "query": item["query"] }).to_string() }),
                ]
            }
            "contextCompaction" => {
                self.saw_compaction_item = true;
                vec![json!({ "type": "compaction_start" })]
            }
            _ => vec![],
        }
    }

    fn item_completed(&mut self, item: &Value) -> Vec<Value> {
        let id = text(&item["id"]).to_string();
        let meta = self.items.remove(&id);
        let status = text(&item["status"]);
        match text(&item["type"]) {
            "agentMessage" => {
                let full = text(&item["text"]);
                let seen = meta.map_or(0, |m| m.streamed);
                match full.get(seen..) {
                    Some(tail) if !tail.is_empty() => {
                        vec![json!({ "type": "assistant_delta", "delta": tail })]
                    }
                    _ => vec![],
                }
            }
            "reasoning" => {
                if meta.is_some_and(|m| m.streamed > 0) {
                    return vec![];
                }
                let summary = item["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(text)
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if summary.is_empty() {
                    vec![]
                } else {
                    vec![json!({ "type": "reasoning_delta", "delta": summary })]
                }
            }
            "commandExecution" => {
                let (command, streamed) = meta.map(|m| (m.command, m.output)).unwrap_or_default();
                let mut output = command_output(item, &command, &streamed);
                if status == "declined" {
                    output["error"] = json!("declined");
                }
                vec![
                    json!({ "type": "tool_output", "call_id": id, "name": "bash", "output": output.to_string(), "is_error": status != "completed" }),
                ]
            }
            "fileChange" => {
                let changes = item["changes"]
                    .as_array()
                    .cloned()
                    .or_else(|| meta.map(|m| m.changes))
                    .unwrap_or_default();
                let error = (status != "completed").then_some(status);
                vec![
                    json!({ "type": "tool_output", "call_id": id, "name": "apply_patch", "output": file_change_output(&changes, error), "is_error": status != "completed" }),
                ]
            }
            "mcpToolCall" => {
                let failed = !item["error"].is_null() || status == "failed";
                let name = meta.map(|m| m.name).unwrap_or_else(|| {
                    format!("{}.{}", text(&item["server"]), text(&item["tool"]))
                });
                vec![
                    json!({ "type": "tool_output", "call_id": id, "name": name, "output": mcp_body(item), "is_error": failed }),
                ]
            }
            "dynamicToolCall" => {
                let failed = item["success"] == false || status == "failed";
                let name = meta
                    .map(|m| m.name)
                    .unwrap_or_else(|| match text(&item["tool"]) {
                        "" => "tool".into(),
                        t => t.into(),
                    });
                vec![
                    json!({ "type": "tool_output", "call_id": id, "name": name, "output": item["contentItems"].to_string(), "is_error": failed }),
                ]
            }
            "webSearch" => vec![
                json!({ "type": "tool_output", "call_id": id, "name": "web_search", "output": json!({ "query": item["query"] }).to_string(), "is_error": false }),
            ],
            "contextCompaction" => vec![json!({ "type": "compaction_end" })],
            _ => vec![],
        }
    }

    fn on_notification(&mut self, method: &str, params: &Value) -> Vec<Value> {
        match method {
            "turn/started" => {
                self.busy = true;
                if let Some(turn) = params["turn"]["id"].as_str() {
                    self.active_turn = Some(turn.to_string());
                }
                vec![json!({ "type": "connecting" })]
            }
            "turn/completed" => {
                self.active_turn = None;
                self.busy = false;
                let turn = &params["turn"];
                let mut events = Vec::new();
                if turn["status"] == "failed" && turn["error"].is_object() {
                    events.push(error_event(
                        text(&turn["error"]["message"]),
                        &turn["error"]["codexErrorInfo"],
                    ));
                }
                events.push(json!({ "type": "status", "message": "ready" }));
                events
            }
            "item/started" => self.item_started(&params["item"]),
            "item/completed" => self.item_completed(&params["item"]),
            "item/agentMessage/delta" => {
                let delta = text(&params["delta"]);
                if let Some(meta) = self.items.get_mut(text(&params["itemId"])) {
                    meta.streamed += delta.len();
                }
                vec![json!({ "type": "assistant_delta", "delta": delta })]
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                let delta = text(&params["delta"]);
                if let Some(meta) = self.items.get_mut(text(&params["itemId"])) {
                    meta.streamed += delta.len();
                }
                vec![json!({ "type": "reasoning_delta", "delta": delta })]
            }
            "item/reasoning/summaryPartAdded" => {
                match self.items.get_mut(text(&params["itemId"])) {
                    Some(meta) if meta.streamed > 0 => {
                        meta.streamed += 2;
                        vec![json!({ "type": "reasoning_delta", "delta": "\n\n" })]
                    }
                    _ => vec![],
                }
            }
            "item/commandExecution/outputDelta" => {
                let id = text(&params["itemId"]).to_string();
                let Some(meta) = self.items.get_mut(&id) else {
                    return vec![];
                };
                meta.output.push_str(text(&params["delta"]));
                let update = json!({ "command": meta.command, "stdout": meta.output }).to_string();
                vec![json!({ "type": "tool_update", "call_id": id, "output": update })]
            }
            "thread/tokenUsage/updated" => {
                let usage = &params["tokenUsage"];
                if !usage.is_object() {
                    return vec![];
                }
                let mut events = Vec::new();
                if let Some(window) = usage["modelContextWindow"]
                    .as_u64()
                    .filter(|w| *w > 0 && *w != self.context_window)
                {
                    self.context_window = window;
                    events.push(self.model_status());
                }
                let input = usage["total"]["inputTokens"].as_u64().unwrap_or(0);
                let output = usage["total"]["outputTokens"].as_u64().unwrap_or(0);
                events.push(json!({
                    "type": "usage",
                    "input_tokens": input.saturating_sub(self.previous_total.0),
                    "output_tokens": output.saturating_sub(self.previous_total.1),
                }));
                events.push(json!({ "type": "context_usage", "tokens": usage["last"]["totalTokens"].as_u64().unwrap_or(0) }));
                self.previous_total = (input, output);
                events
            }
            "turn/plan/updated" => vec![
                json!({ "type": "plan", "plan": params["plan"].as_array().cloned().unwrap_or_default() }),
            ],
            "error" => {
                let message = text(&params["error"]["message"]);
                if message.is_empty() {
                    vec![]
                } else if params["willRetry"] == true {
                    vec![json!({ "type": "info", "message": format!("[codex] {message}") })]
                } else {
                    vec![error_event(message, &params["error"]["codexErrorInfo"])]
                }
            }
            "guardianWarning" if !text(&params["message"]).is_empty() => {
                vec![
                    json!({ "type": "info", "message": format!("[codex] {}", text(&params["message"])) }),
                ]
            }
            "thread/compacted" if !self.saw_compaction_item => {
                vec![json!({ "type": "compaction_end" })]
            }
            "model/rerouted" => {
                let to = [
                    text(&params["toModel"]),
                    text(&params["model"]),
                    text(&params["to"]),
                ]
                .into_iter()
                .find(|t| !t.is_empty())
                .unwrap_or_default();
                let from = [text(&params["fromModel"]), text(&params["from"])]
                    .into_iter()
                    .find(|t| !t.is_empty())
                    .unwrap_or_default();
                let change = if !from.is_empty() && !to.is_empty() {
                    format!("{from} → {to}")
                } else {
                    format!("{to}{from}")
                };
                let description = [change.as_str(), text(&params["reason"])]
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                if description.is_empty() {
                    vec![]
                } else {
                    vec![
                        json!({ "type": "info", "message": format!("[codex] model rerouted: {description}") }),
                    ]
                }
            }
            "thread/goal/updated" if params["goal"].is_object() => {
                vec![Self::goal_event(&params["goal"])]
            }
            "thread/goal/cleared" => vec![Self::goal_event(&Value::Null)],
            "serverRequest/resolved" => {
                self.approvals
                    .retain(|_, request| *request != params["requestId"]);
                vec![]
            }
            _ => vec![],
        }
    }
}

impl Adapter for Codex {
    fn start(&mut self) -> Vec<String> {
        vec![self.request(
            "initialize",
            json!({ "clientInfo": { "name": "jucode-daemon", "title": "JuCode", "version": env!("CARGO_PKG_VERSION") }, "capabilities": null }),
            "",
        )]
    }

    fn translate(&mut self, line: Line) -> Output {
        let frame = match line {
            Line::Stderr(line) => {
                let line = super::strip_ansi(&line);
                let line = line.trim();
                if line.is_empty() || super::log_level(line).is_some() {
                    return Output::default();
                }
                return Output::events(vec![
                    json!({ "type": "info", "message": format!("[codex] {line}") }),
                ]);
            }
            Line::Frame(frame) => frame,
        };
        let id = &frame["id"];
        let has_id = id.is_u64() || id.is_string();
        match frame["method"].as_str() {
            Some(method) if has_id => self.on_server_request(id, method, &frame["params"]),
            Some(method) => Output::events(self.on_notification(method, &frame["params"])),
            None => match id.as_u64() {
                Some(id) => self.on_response(id, &frame["result"], &frame["error"]),
                None => Output::default(),
            },
        }
    }

    fn encode(&mut self, op: &Value) -> Result<Output, String> {
        let frames = match text(&op["op"]) {
            "user_message" => {
                let mut input =
                    vec![json!({ "type": "text", "text": op["content"], "text_elements": [] })];
                for image in op["images"].as_array().into_iter().flatten() {
                    input.push(json!({ "type": "localImage", "path": image }));
                }
                if self.thread.is_none() {
                    self.queued.extend(input);
                    return Ok(Output::default());
                }
                vec![self.turn_start(input)]
            }
            "approve" => {
                let call = text(&op["call_id"]);
                let Some(request) = self.approvals.remove(call) else {
                    return Err(format!("no open approval {call}"));
                };
                let decision = if op["decision"] == "deny" {
                    "decline"
                } else if op["always"] == true {
                    "acceptForSession"
                } else {
                    "accept"
                };
                vec![
                    json!({ "jsonrpc": "2.0", "id": request, "result": { "decision": decision } })
                        .to_string(),
                ]
            }
            "interrupt" => match (self.thread.clone(), self.active_turn.clone()) {
                (Some(thread), Some(turn)) => vec![self.request(
                    "turn/interrupt",
                    json!({ "threadId": thread, "turnId": turn }),
                    "",
                )],
                _ => vec![],
            },
            "set_approval_mode" => {
                self.mode = engine_mode(text(&op["mode"]));
                return Ok(Output::events(vec![
                    json!({ "type": "approval_mode", "mode": self.mode }),
                ]));
            }
            "command" => {
                let input = text(&op["input"]).trim();
                let (command, arg) = match input.split_once(' ') {
                    Some((command, arg)) => (command, arg.trim()),
                    None => (input, ""),
                };
                let thread = self.thread.clone();
                match (command, thread) {
                    ("/model", _) if arg.is_empty() => {
                        vec![self.request("model/list", json!({}), "view")]
                    }
                    ("/model", _) => {
                        let mut parts = arg.split_whitespace();
                        let model = parts.next().unwrap_or_default().to_string();
                        self.pending_pick = Some((model, parts.next().map(str::to_string)));
                        vec![self.request("model/list", json!({}), "apply")]
                    }
                    ("/resume", _) if arg.is_empty() => {
                        let cwd = self.cwd.clone();
                        vec![self.request("thread/list", json!({ "cwd": cwd, "limit": 50 }), "")]
                    }
                    ("/compact", Some(thread)) => vec![self.request(
                        "thread/compact/start",
                        json!({ "threadId": thread }),
                        "",
                    )],
                    ("/goal", Some(thread)) => {
                        let (method, params) = match arg {
                            "" => ("thread/goal/get", json!({ "threadId": thread })),
                            "clear" => ("thread/goal/clear", json!({ "threadId": thread })),
                            "pause" => (
                                "thread/goal/set",
                                json!({ "threadId": thread, "status": "paused" }),
                            ),
                            "resume" => (
                                "thread/goal/set",
                                json!({ "threadId": thread, "status": "active" }),
                            ),
                            objective => (
                                "thread/goal/set",
                                json!({ "threadId": thread, "objective": objective }),
                            ),
                        };
                        vec![self.request(method, params, "")]
                    }
                    ("/rewind", Some(thread)) => match arg.parse::<u64>() {
                        Ok(turns) if turns > 0 => vec![self.request(
                            "thread/rollback",
                            json!({ "threadId": thread, "numTurns": turns }),
                            "",
                        )],
                        _ => vec![],
                    },
                    ("/compact" | "/goal" | "/rewind", None) => vec![],
                    (command, _) => {
                        return Err(format!("{command} is not available in a Codex session"))
                    }
                }
            }
            "shutdown" => vec![],
            other => return Err(format!("Codex sessions do not support {other}")),
        };
        Ok(Output {
            events: Vec::new(),
            frames,
        })
    }

    fn busy(&self) -> bool {
        self.busy
    }

    fn restart_for(&self, _op: &Value) -> Option<Options> {
        None
    }

    fn conversation(&self) -> Option<String> {
        self.thread.clone()
    }
}

// --- saved threads ---

fn sessions_dir(home: &Path) -> PathBuf {
    home.join(".codex").join("sessions")
}

/// Rollout files, newest first (their names start with the start time).
fn rollouts(home: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![(sessions_dir(home), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() && depth < 3 {
                dirs.push((path, depth + 1));
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                files.push(path);
            }
        }
    }
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    files.truncate(2000);
    files
}

fn lines(path: &Path, limit: u64) -> impl Iterator<Item = Value> {
    fs::File::open(path)
        .ok()
        .map(|file| BufReader::new(file.take(limit)))
        .into_iter()
        .flat_map(|reader| reader.lines().map_while(Result::ok))
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
}

/// A user message Codex wrote itself (instructions, environment context).
fn is_injected(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with('<') || text.starts_with("# AGENTS.md")
}

/// Threads Codex saved for `cwd`, newest first: (id, title, updated ms).
pub fn saved(cwd: &Path) -> Vec<(String, String, u64)> {
    saved_in(&home(), cwd)
}

fn saved_in(home: &Path, cwd: &Path) -> Vec<(String, String, u64)> {
    let real = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut found = Vec::new();
    for path in rollouts(home) {
        let Some(meta) = lines(&path, 64 * 1024)
            .next()
            .filter(|first| first["type"] == "session_meta")
        else {
            continue;
        };
        let saved_cwd = PathBuf::from(text(&meta["payload"]["cwd"]));
        if saved_cwd != real && saved_cwd != cwd {
            continue;
        }
        let id = text(&meta["payload"]["id"]).to_string();
        if id.is_empty() {
            continue;
        }
        let title = lines(&path, 512 * 1024)
            .filter(|row| row["type"] == "response_item" && row["payload"]["role"] == "user")
            .flat_map(|row| {
                row["payload"]["content"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .map(|block| text(&block["text"]).to_string())
            .find(|text| !text.trim().is_empty() && !is_injected(text))
            .map(|text| {
                text.lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(80)
                    .collect()
            })
            .unwrap_or_default();
        let updated = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as u64);
        found.push((id, title, updated));
        if found.len() == 50 {
            break;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(codex: &mut Codex, value: Value) -> Output {
        codex.translate(Line::Frame(value))
    }

    fn types(events: &[Value]) -> Vec<&str> {
        events.iter().map(|e| text(&e["type"])).collect()
    }

    fn sent(frames: &[String]) -> Vec<Value> {
        frames
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect()
    }

    fn opened() -> Codex {
        let mut c = Codex::new(
            Path::new("/p"),
            &Options {
                approval_mode: Some("auto-edit".into()),
                ..Options::default()
            },
        );
        let init = sent(&c.start());
        assert_eq!(init[0]["method"], "initialize");
        let next = sent(&frame(&mut c, json!({ "id": 1, "result": {} })).frames);
        assert_eq!(next[0]["method"], "initialized");
        assert_eq!(next[1]["method"], "thread/start");
        assert_eq!(next[1]["params"]["sandbox"], "workspace-write");
        let open = frame(
            &mut c,
            json!({ "id": 2, "result": { "thread": { "id": "th-1" }, "model": "gpt-5", "modelProvider": "openai" } }),
        );
        assert_eq!(
            types(&open.events),
            [
                "startup",
                "model_status",
                "command_list",
                "approval_mode",
                "status"
            ]
        );
        c
    }

    #[test]
    fn a_turn_streams_and_a_command_approval_round_trips() {
        let mut c = opened();
        assert_eq!(c.conversation().as_deref(), Some("th-1"));
        let turn = sent(
            &c.encode(&json!({ "op": "user_message", "content": "hi" }))
                .unwrap()
                .frames,
        );
        assert_eq!(turn[0]["method"], "turn/start");
        assert_eq!(turn[0]["params"]["approvalPolicy"], "on-request");
        assert!(c.busy());

        frame(
            &mut c,
            json!({ "method": "turn/started", "params": { "turn": { "id": "t1" } } }),
        );
        assert_eq!(types(&frame(&mut c, json!({ "method": "item/started", "params": { "item": { "id": "m1", "type": "agentMessage" } } })).events), ["assistant_start"]);
        frame(
            &mut c,
            json!({ "method": "item/agentMessage/delta", "params": { "itemId": "m1", "delta": "Hel" } }),
        );
        let done = frame(
            &mut c,
            json!({ "method": "item/completed", "params": { "item": { "id": "m1", "type": "agentMessage", "text": "Hello" } } }),
        );
        assert_eq!(done.events[0]["delta"], "lo");

        frame(
            &mut c,
            json!({ "method": "item/started", "params": { "item": { "id": "c1", "type": "commandExecution", "command": "ls" } } }),
        );
        let ask = frame(
            &mut c,
            json!({ "id": 77, "method": "item/commandExecution/requestApproval", "params": { "itemId": "c1" } }),
        );
        assert_eq!(ask.events[0]["summary"], "ls");
        let answer = sent(&c.encode(&json!({ "op": "approve", "call_id": ask.events[0]["call_id"], "decision": "allow", "always": true })).unwrap().frames);
        assert_eq!(
            answer[0],
            json!({ "jsonrpc": "2.0", "id": 77, "result": { "decision": "acceptForSession" } })
        );
        let output = frame(
            &mut c,
            json!({ "method": "item/completed", "params": { "item": { "id": "c1", "type": "commandExecution", "command": "ls", "aggregatedOutput": "a\n", "exitCode": 0, "status": "completed" } } }),
        );
        assert_eq!(output.events[0]["is_error"], false);

        let end = frame(
            &mut c,
            json!({ "method": "turn/completed", "params": { "turn": { "id": "t1", "status": "completed" } } }),
        );
        assert_eq!(types(&end.events), ["status"]);
        assert!(!c.busy());
    }

    #[test]
    fn a_resume_replays_history_and_falls_back_to_a_new_thread() {
        let mut c = Codex::new(
            Path::new("/p"),
            &Options {
                resume: Some("th-9".into()),
                ..Options::default()
            },
        );
        c.start();
        let next = sent(&frame(&mut c, json!({ "id": 1, "result": {} })).frames);
        assert_eq!(next[1]["method"], "thread/resume");
        assert_eq!(next[1]["params"]["threadId"], "th-9");
        let resumed = frame(
            &mut c,
            json!({ "id": 2, "result": { "thread": { "id": "th-9", "turns": [{ "items": [
            { "type": "userMessage", "content": [{ "type": "text", "text": "q" }] },
            { "type": "agentMessage", "text": "a" },
        ] }] } } }),
        );
        assert_eq!(
            resumed.events[0]["items"],
            json!([{ "role": "user", "content": "q" }, { "role": "assistant", "content": "a" }])
        );

        let mut c = Codex::new(
            Path::new("/p"),
            &Options {
                resume: Some("gone".into()),
                ..Options::default()
            },
        );
        c.start();
        frame(&mut c, json!({ "id": 1, "result": {} }));
        let failed = frame(
            &mut c,
            json!({ "id": 2, "error": { "code": -1, "message": "no rollout" } }),
        );
        assert_eq!(types(&failed.events)[0], "resume_failed");
        assert_eq!(sent(&failed.frames)[0]["method"], "thread/start");
    }

    #[test]
    fn modes_and_models_apply_to_later_turns() {
        let mut c = opened();
        let mode = c
            .encode(&json!({ "op": "set_approval_mode", "mode": "full-access" }))
            .unwrap();
        assert_eq!(mode.events[0]["mode"], "full-auto");
        // Full access answers approvals itself.
        let auto = frame(
            &mut c,
            json!({ "id": 5, "method": "item/fileChange/requestApproval", "params": {} }),
        );
        assert!(auto.events.is_empty());
        assert_eq!(sent(&auto.frames)[0]["result"]["decision"], "accept");

        let pick = sent(
            &c.encode(&json!({ "op": "command", "input": "/model gpt-5-mini low" }))
                .unwrap()
                .frames,
        );
        let id = pick[0]["id"].clone();
        frame(
            &mut c,
            json!({ "id": id, "result": { "data": [{ "model": "gpt-5-mini", "supportedReasoningEfforts": [{ "reasoningEffort": "low" }] }] } }),
        );
        let turn = sent(
            &c.encode(&json!({ "op": "user_message", "content": "x" }))
                .unwrap()
                .frames,
        );
        assert_eq!(turn[0]["params"]["model"], "gpt-5-mini");
        assert_eq!(turn[0]["params"]["effort"], "low");
        assert_eq!(
            turn[0]["params"]["sandboxPolicy"]["type"],
            "dangerFullAccess"
        );
        assert!(c
            .encode(&json!({ "op": "command", "input": "/resume th-2" }))
            .is_err());
    }

    #[test]
    fn saved_threads_are_found_by_directory() {
        let home = std::env::temp_dir().join(format!("codex-home-{}", std::process::id()));
        let day = home.join(".codex/sessions/2026/09/30");
        fs::create_dir_all(&day).unwrap();
        let rows = |id: &str, cwd: &str, message: &str| {
            [
                json!({ "type": "session_meta", "payload": { "id": id, "cwd": cwd } }),
                json!({ "type": "response_item", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "# AGENTS.md instructions\n..." }] } }),
                json!({ "type": "response_item", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": message }] } }),
            ]
            .iter()
            .map(|row| row.to_string() + "\n")
            .collect::<String>()
        };
        fs::write(
            day.join("rollout-2026-09-30T10-00-00-a.jsonl"),
            rows("a", "/work/app", "fix login\nmore"),
        )
        .unwrap();
        fs::write(
            day.join("rollout-2026-09-30T11-00-00-b.jsonl"),
            rows("b", "/work/other", "x"),
        )
        .unwrap();
        let found = saved_in(&home, Path::new("/work/app"));
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].0.as_str(), found[0].1.as_str()),
            ("a", "fix login")
        );
    }
}
