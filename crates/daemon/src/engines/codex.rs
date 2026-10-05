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
//!   every later `turn/start`; there is no thread-level setter. Plan mode is
//!   the experimental `collaborationMode`, auto mode the `auto_review`
//!   approvals reviewer.
//! - A message sent mid-turn waits here (`pending_messages`) and starts the
//!   next turn, or joins the running one with `turn/steer`.
//! - Subagents are threads of their own whose notifications share this
//!   connection (their `threadId` differs): they feed the agent trace, never
//!   the conversation. `thread/turns/list` reads one back.
//! - A rewind is `thread/revert` to before a user message's turn.

use super::{home, resolve, Adapter, Line, Options, Output};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::Command,
};

pub fn command(options: &Options) -> Command {
    let program = match &options.bin {
        Some(bin) => PathBuf::from(bin),
        None => resolve("codex", "CODEX_BIN", &[]),
    };
    let mut command = Command::new(program);
    command.arg("app-server");
    command
}

/// Codex's own TUI resuming thread `id`, in the session's approval mode and
/// model (for the GUI ⇄ TUI handoff).
pub fn tui(id: &str, options: &Options) -> Command {
    let program = match &options.bin {
        Some(bin) => PathBuf::from(bin),
        None => resolve("codex", "CODEX_BIN", &[]),
    };
    let mut command = Command::new(program);
    command.args(["resume", id]);
    let mode = engine_mode(options.approval_mode.as_deref().unwrap_or_default());
    if mode == "full-auto" {
        command.arg("--dangerously-bypass-approvals-and-sandbox");
    } else {
        let (approval, sandbox) = policy(mode);
        command.args(["-a", approval, "-s", sandbox_mode(&sandbox)]);
    }
    if let Some(model) = &options.model {
        command.args(["-m", model]);
    }
    command
}

/// The provider a gateway session configures (see `use_gateway`).
const GATEWAY_PROVIDER: &str = "jucode_gateway";

/// The env var a gateway session reads its key from.
const GATEWAY_KEY_ENV: &str = "JUCODE_GATEWAY_TOKEN";

/// This session talks to the JuCode gateway through the daemon's local
/// gateway (`base`, see crate::gateway): config overrides for this process
/// alone, with the local `key` (never the JuCode token) in its environment.
pub fn use_gateway(command: &mut Command, base: &str, key: &str) -> Result<(), String> {
    command
        .args(["-c", &format!("model_provider=\"{GATEWAY_PROVIDER}\""), "-c"])
        .arg(format!(
            "model_providers.{GATEWAY_PROVIDER}={{name=\"JuCode\",base_url=\"{base}/v1\",env_key=\"{GATEWAY_KEY_ENV}\",wire_api=\"responses\"}}"
        ))
        .env(GATEWAY_KEY_ENV, key);
    Ok(())
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `mcp_servers` from `mcpServerStatus/list`: Codex's own servers, which can
/// be reconnected and signed in to here but are switched in its config.
fn mcp_servers(data: &Value) -> Value {
    let servers: Vec<Value> = data
        .as_array()
        .into_iter()
        .flatten()
        .map(|server| {
            let status = text(&server["runtimeStatus"]);
            let state = match status {
                "connected" => "connected",
                "failed" | "cancelled" | "authenticationRequired" => "failed",
                "disabled" => "disabled",
                _ => "connecting",
            };
            let tools: Vec<Value> = server["tools"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, tool)| json!({ "name": name, "description": tool["description"] }))
                .collect();
            let mut view = json!({
                "name": server["name"],
                "transport": if server["httpOrigin"].is_string() { "http" } else { "stdio" },
                "state": state,
                "tools": tools,
                "can_toggle": false,
                "needs_auth": status == "authenticationRequired",
            });
            if status == "authenticationRequired" {
                view["error"] = json!("needs sign-in");
            }
            view
        })
        .collect();
    json!({ "type": "mcp_servers", "servers": servers })
}

/// Client approval mode (jucode or Desktop names) → the Desktop engine mode
/// Codex supports: `read-only`, `auto-edit` or `full-auto`.
fn engine_mode(mode: &str) -> &'static str {
    match mode {
        "auto-edit" => "auto-edit",
        "auto" => "auto",
        "plan" => "plan",
        "full-auto" | "full-access" => "full-auto",
        _ => "read-only",
    }
}

/// The approval policy and sandbox policy of an engine mode.
fn policy(mode: &str) -> (&'static str, Value) {
    match mode {
        // Auto: workspace writes, with a reviewer subagent deciding approvals.
        "auto-edit" | "auto" => (
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
        " (Codex needs to sign in again: send /login here, or run `codex login` in a terminal.)"
    } else {
        ""
    };
    json!({ "type": "error", "message": format!("{message}{hint}") })
}

/// A ChatGPT plan's usage from a rate-limit snapshot: its primary and
/// secondary windows (`usedPercent` 0-100, `resetsAt` unix seconds).
fn plan_usage(snapshot: &Value) -> Option<Value> {
    let windows: Vec<Value> = ["primary", "secondary"]
        .iter()
        .filter_map(|key| {
            let window = &snapshot[*key];
            let used = window["usedPercent"].as_f64()?;
            Some(super::plan_window(
                key,
                used,
                &window["resetsAt"],
                window["windowDurationMins"].as_u64(),
            ))
        })
        .collect();
    let plan = snapshot["planType"].as_str();
    (!windows.is_empty()).then(|| json!({ "type": "plan_usage", "plan": plan, "windows": windows }))
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
                let (content, images) = user_input(&item["content"]);
                if !content.is_empty() || !images.is_empty() {
                    let mut row = json!({ "role": "user", "content": content });
                    if !images.is_empty() {
                        row["images"] = json!(images);
                    }
                    rows.push(row);
                }
            }
            "agentMessage" | "plan" if !text(&item["text"]).is_empty() => {
                rows.push(json!({ "role": "assistant", "content": item["text"] }));
            }
            "reasoning" => {
                let summary = item["summary"].as_array().into_iter().flatten().map(text).collect::<Vec<_>>().join("\n\n");
                if !summary.is_empty() {
                    rows.push(json!({ "role": "reasoning", "content": summary }));
                }
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

/// A user message's text and its images (local paths).
fn user_input(content: &Value) -> (String, Vec<String>) {
    let blocks = content.as_array().map(Vec::as_slice).unwrap_or_default();
    let text_part = blocks
        .iter()
        .filter(|c| c["type"] == "text")
        .map(|c| text(&c["text"]))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let images = blocks
        .iter()
        .filter(|c| c["type"] == "localImage")
        .map(|c| text(&c["path"]).to_string())
        .filter(|p| !p.is_empty())
        .collect();
    (text_part, images)
}

/// A subagent's short name from its path (`/root/pong` → `pong`).
fn agent_label(path: &str, thread: &str) -> String {
    match path.rsplit('/').find(|part| !part.is_empty()) {
        Some(name) if name != "root" => name.to_string(),
        _ => thread.chars().take(8).collect(),
    }
}

/// Codex's approval-request summary of the permissions it asks for.
fn permissions_summary(params: &Value) -> String {
    let wanted = &params["permissions"];
    let mut parts = Vec::new();
    if wanted["network"]["enabled"] == true {
        parts.push("network access".to_string());
    }
    for (key, label) in [("read", "read"), ("write", "write")] {
        let paths: Vec<&str> = wanted["fileSystem"][key]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect();
        if !paths.is_empty() {
            parts.push(format!("{label}: {}", paths.join(", ")));
        }
    }
    let reason = text(&params["reason"]);
    [parts.join("\n"), reason.to_string()]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
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
    /// Talks to the JuCode gateway: a resumed thread must too, whichever
    /// provider it was written with (`thread/resume` would use that one).
    gateway: bool,
    active_turn: Option<String>,
    /// A turn is starting or running.
    busy: bool,
    /// Input sent before the thread opened.
    queued: Vec<Value>,
    /// A `thread/rollback` is on its way: input sent meanwhile waits for it,
    /// or the new turn could start on the history before the rollback.
    rolling_back: bool,
    after_rollback: Vec<Value>,
    open_params: Value,
    /// Synthetic call id → the server request awaiting our answer: its id,
    /// method and params.
    approvals: HashMap<String, (Value, String, Value)>,
    approval_seq: u64,
    items: HashMap<String, Item>,
    model: String,
    provider: String,
    effort: String,
    /// Thread totals at the last usage update: input, cached input, output,
    /// reasoning. None after a resume until the first update: the thread's
    /// totals then include every earlier turn.
    previous_total: Option<(u64, u64, u64, u64)>,
    context_window: u64,
    catalog: Vec<Value>,
    pending_pick: Option<(String, Option<String>)>,
    override_model: Option<String>,
    override_effort: Option<String>,
    saw_compaction_item: bool,
    /// Enabled skills from `skills/list`: name → (path, description).
    skills: Vec<(String, String, String)>,
    /// The turn of each user message, oldest first: `/rewind N` reverts to
    /// before the Nth from the end.
    user_turns: Vec<String>,
    /// Messages sent mid-turn: (text, input), started in order once the turn
    /// ends, or steered into it.
    waiting: Vec<(String, Vec<Value>)>,
    /// Messages a `turn/steer` carries, until it is answered.
    steering: Vec<(String, Vec<Value>)>,
    /// Subagents (their threads), as agent trace entries.
    agents: Vec<Value>,
    /// Plan mode was sent on the last turn: leaving it is sent once too.
    plan_sent: bool,
    /// The service tier asked for (`priority` is fast mode); None: the
    /// model's default.
    service_tier: Option<String>,
    /// Reasoning summaries are shown.
    thinking: bool,
    /// Warnings already shown (Codex repeats a config warning per thread).
    warned: std::collections::HashSet<String>,
    /// A client looked at the MCP servers: their changes are sent on.
    mcp_watched: bool,
    mcp_relist: bool,
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
            gateway: options.gateway == Some(true),
            active_turn: None,
            busy: false,
            queued: Vec::new(),
            rolling_back: false,
            after_rollback: Vec::new(),
            open_params: Value::Null,
            approvals: HashMap::new(),
            approval_seq: 0,
            items: HashMap::new(),
            model: String::new(),
            provider: String::new(),
            effort: String::new(),
            previous_total: Some((0, 0, 0, 0)),
            context_window: 0,
            catalog: Vec::new(),
            pending_pick: None,
            override_model: options.model.clone(),
            override_effort: None,
            saw_compaction_item: false,
            skills: Vec::new(),
            user_turns: Vec::new(),
            waiting: Vec::new(),
            steering: Vec::new(),
            agents: Vec::new(),
            plan_sent: false,
            service_tier: options.fast.then(|| "priority".to_string()),
            thinking: options.thinking.unwrap_or(true),
            warned: Default::default(),
            mcp_watched: false,
            mcp_relist: false,
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
        params["approvalsReviewer"] = json!(if self.mode == "auto" {
            "auto_review"
        } else {
            "user"
        });
        params["summary"] = json!(if self.thinking { "auto" } else { "none" });
        if let Some(tier) = &self.service_tier {
            params["serviceTier"] = json!(tier);
        }
        // Plan mode, and the turn after it, name the collaboration mode.
        let plan = self.mode == "plan";
        if plan || self.plan_sent {
            let effort = (!self.effort.is_empty()).then(|| self.effort.clone());
            params["collaborationMode"] = json!({
                "mode": if plan { "plan" } else { "default" },
                "settings": { "model": self.model, "reasoning_effort": effort, "developer_instructions": null },
            });
            self.plan_sent = plan;
        }
        self.busy = true;
        self.request("turn/start", params, "")
    }

    /// The fast service tier: asked for, else the model's default.
    fn fast(&self) -> bool {
        let model_default = self
            .catalog
            .iter()
            .find(|m| m["model"] == self.model.as_str())
            .and_then(|m| m["defaultServiceTier"].as_str());
        self.service_tier.as_deref().or(model_default) == Some("priority")
    }

    fn fast_available(&self) -> bool {
        self.catalog
            .iter()
            .find(|m| m["model"] == self.model.as_str())
            .and_then(|m| m["serviceTiers"].as_array())
            .is_some_and(|tiers| tiers.iter().any(|t| t["id"] == "priority"))
    }

    fn pending_event(&self) -> Value {
        let texts: Vec<&str> = self.waiting.iter().map(|(text, _)| text.as_str()).collect();
        json!({ "type": "pending_messages", "messages": texts })
    }

    fn agent_runs(&self) -> Value {
        json!({ "type": "agent_runs", "workflows": [], "agents": self.agents })
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
            "fast": self.fast(),
            "fast_available": self.fast_available(),
            "thinking_summaries": self.thinking,
            "state": if self.busy { "streaming" } else { "ready" },
        })
    }

    /// The commands a Codex session runs through the app-server (its other
    /// slash commands belong to its TUI), then its skills.
    fn command_list(&self) -> Value {
        let builtin = [
            ("/model", "", "Choose the model and reasoning effort"),
            ("/resume", "", "Resume an earlier thread"),
            (
                "/compact",
                "",
                "Summarize the conversation to free up context",
            ),
            (
                "/review",
                "[instructions]",
                "Review uncommitted changes, or what the instructions ask for",
            ),
            (
                "/goal",
                "[objective | clear | pause | resume]",
                "Set or show the thread goal",
            ),
            ("/login", "", "Sign Codex in to ChatGPT with a code"),
        ];
        let mut commands: Vec<Value> = builtin
            .iter()
            .map(|(command, args, description)| json!({ "command": command, "marker": null, "args": args, "description": description }))
            .collect();
        for (name, _, description) in &self.skills {
            commands.push(json!({ "command": format!("/{name}"), "marker": "SKILL", "args": "", "description": description }));
        }
        json!({ "type": "command_list", "commands": commands })
    }

    /// The skill a `/name` command names: (name, path).
    fn skill(&self, command: &str) -> Option<(String, String)> {
        let name = command.strip_prefix('/')?;
        self.skills
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(n, path, _)| (n.clone(), path.clone()))
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
            rows.insert(0, json!({ "model": self.model, "active": true, "context_window": self.context_window, "max_output_tokens": 0, "reasoning_efforts": self.efforts(&self.model), "listed": false }));
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
        self.previous_total = if resumed { None } else { Some((0, 0, 0, 0)) };
        let mut events = Vec::new();
        if resumed {
            let turns = &result["thread"]["turns"];
            let rows = transcript(turns);
            if !rows.is_empty() {
                events.push(json!({ "type": "transcript", "items": rows }));
            }
            self.user_turns.clear();
            self.agents.clear();
            for turn in turns.as_array().into_iter().flatten() {
                for item in turn["items"].as_array().into_iter().flatten() {
                    match text(&item["type"]) {
                        "userMessage" => self.user_turns.push(text(&turn["id"]).to_string()),
                        "subAgentActivity" => {
                            self.agent_activity(item);
                        }
                        _ => {}
                    }
                }
            }
            if !self.agents.is_empty() {
                events.push(self.agent_runs());
            }
        }
        events.extend([
            json!({ "type": "startup", "model": self.model, "cwd": result["cwd"], "session_id": self.thread, "context_window": self.context_window }),
            self.model_status(),
            self.command_list(),
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
        if method == "turn/steer" {
            let steered = std::mem::take(&mut self.steering);
            if error.is_null() {
                return Output::default();
            }
            // Not steerable (a review, a compaction, the turn just ended):
            // they wait for the next turn after all.
            self.waiting.splice(0..0, steered);
            return Output::events(vec![
                json!({ "type": "info", "message": format!("[codex] {}", text(&error["message"])) }),
                self.pending_event(),
            ]);
        }
        if method == "thread/turns/list" {
            let rows = if error.is_null() {
                transcript(&result["data"])
            } else {
                Vec::new()
            };
            let mut event =
                json!({ "type": "subagent_transcript", "agent_id": tag, "items": rows });
            if !error.is_null() {
                event["error"] = json!(text(&error["message"]));
            }
            return Output::events(vec![event]);
        }
        if method == "mcpServerStatus/list" {
            if !error.is_null() {
                return Output::events(vec![
                    json!({ "type": "info", "message": format!("[codex] {}", text(&error["message"])) }),
                ]);
            }
            return Output::events(vec![mcp_servers(&result["data"])]);
        }
        if method == "mcpServer/oauth/login" {
            return Output::events(vec![if error.is_null() {
                json!({ "type": "mcp_login", "name": tag, "url": result["authorizationUrl"] })
            } else {
                json!({ "type": "info", "message": format!("[codex] {tag}: {}", text(&error["message"])) })
            }]);
        }
        if method == "account/login/start" {
            return Output::events(vec![if error.is_null() {
                json!({ "type": "info", "message": format!("[codex] Sign in: open {} and enter the code {}", text(&result["verificationUrl"]), text(&result["userCode"])) })
            } else {
                json!({ "type": "info", "message": format!("[codex] sign-in failed: {}", text(&error["message"])) })
            }]);
        }
        if method == "thread/name/set" || method == "config/mcpServer/reload" {
            return if error.is_null() {
                Output::default()
            } else {
                Output::events(vec![
                    json!({ "type": "info", "message": format!("[codex] {}", text(&error["message"])) }),
                ])
            };
        }
        if method == "thread/revert" {
            self.rolling_back = false;
            let input = std::mem::take(&mut self.after_rollback);
            if !error.is_null() {
                let mut events = vec![error_event(
                    &format!("Codex could not rewind: {}", text(&error["message"])),
                    &Value::Null,
                )];
                // Its turns are still there.
                self.user_turns
                    .extend(tag.split(',').filter(|t| !t.is_empty()).map(str::to_string));
                // Sent on the history it was meant to replace, it would repeat a turn.
                if !input.is_empty() {
                    events.push(error_event(
                        "the message waiting for the rewind was not sent",
                        &Value::Null,
                    ));
                }
                return Output::events(events);
            }
            return Output {
                events: Vec::new(),
                frames: if input.is_empty() {
                    Vec::new()
                } else {
                    vec![self.turn_start(input)]
                },
            };
        }
        if !error.is_null() {
            let message = match text(&error["message"]) {
                "" => format!("JSON-RPC error {}", error["code"]),
                message => message.to_string(),
            };
            // An account without a ChatGPT plan has no limits to read.
            if method == "account/rateLimits/read" {
                return Output::default();
            }
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
                        if self.gateway {
                            params["modelProvider"] = json!(GATEWAY_PROVIDER);
                        }
                        self.request("thread/resume", params, "")
                    }
                    None => self.request("thread/start", open, ""),
                });
                frames.push(self.request("model/list", json!({}), ""));
                let cwd = self.cwd.clone();
                frames.push(self.request("skills/list", json!({ "cwds": [cwd] }), ""));
                // The ChatGPT plan's limits (a gateway session has none).
                if !self.gateway {
                    frames.push(self.request("account/rateLimits/read", json!({}), ""));
                }
                Output {
                    events: vec![],
                    frames,
                }
            }
            "thread/start" => self.thread_opened(result, false),
            "thread/resume" => self.thread_opened(result, true),
            "account/rateLimits/read" => {
                Output::events(plan_usage(&result["rateLimits"]).into_iter().collect())
            }
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
            "skills/list" => {
                self.skills = result["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|entry| entry["skills"].as_array().into_iter().flatten())
                    .filter(|skill| skill["enabled"] != false && !text(&skill["name"]).is_empty())
                    .map(|skill| {
                        let description = [
                            text(&skill["interface"]["shortDescription"]),
                            text(&skill["shortDescription"]),
                            text(&skill["description"]),
                        ]
                        .into_iter()
                        .find(|d| !d.is_empty())
                        .unwrap_or_default();
                        (
                            text(&skill["name"]).to_string(),
                            text(&skill["path"]).to_string(),
                            description.to_string(),
                        )
                    })
                    .collect();
                Output::events(vec![self.command_list()])
            }
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
        let answer =
            |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
        let full = self.mode == "full-auto";
        // Full access never prompts; a turn started before the switch still may.
        match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" if full => {
                return Output {
                    events: vec![],
                    frames: vec![answer(json!({ "decision": "accept" }))],
                };
            }
            "item/permissions/requestApproval" if full => {
                return Output {
                    events: vec![],
                    frames: vec![answer(
                        json!({ "permissions": params["permissions"], "scope": "session" }),
                    )],
                };
            }
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
            | "mcpServer/elicitation/request" => {}
            _ => {
                return Output {
                    frames: vec![json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unsupported by client: {method}") } }).to_string()],
                    events: vec![json!({ "type": "info", "message": format!("[codex] unsupported request: {method}") })],
                };
            }
        }
        self.approval_seq += 1;
        let call = format!("approval-{}", self.approval_seq);
        self.approvals.insert(
            call.clone(),
            (id.clone(), method.to_string(), params.clone()),
        );
        // A subagent's request names it.
        let subagent = self
            .agents
            .iter()
            .find(|a| a["id"] == params["threadId"])
            .map(|a| a["label"].clone())
            .unwrap_or(Value::Null);
        let mut event = json!({ "type": "approval_request", "call_id": call, "subagent_id": subagent, "hunks": null });
        let item = self.items.get(text(&params["itemId"]));
        match method {
            "item/commandExecution/requestApproval" => {
                let command = match text(&params["command"]) {
                    "" => item.map(|i| i.command.clone()).unwrap_or_default(),
                    command => command.to_string(),
                };
                event["name"] = json!("bash");
                event["summary"] = json!(match text(&params["reason"]) {
                    "" => command,
                    reason => format!("{command}\n{reason}"),
                });
                // Codex can keep a rule for commands like this one.
                if params["proposedExecpolicyAmendment"].is_array() {
                    event["scopes"] = json!(["session", "rule"]);
                }
            }
            "item/fileChange/requestApproval" => {
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
                event["name"] = json!("apply_patch");
                event["summary"] = json!(summary);
            }
            "item/permissions/requestApproval" => {
                event["name"] = json!("permissions");
                event["summary"] = json!(permissions_summary(params));
            }
            "item/tool/requestUserInput" => {
                let questions: Vec<Value> = params["questions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|q| {
                        json!({
                            "question": q["question"],
                            "header": q["header"],
                            "options": q["options"].as_array().cloned().unwrap_or_default(),
                            "multiSelect": false,
                        })
                    })
                    .collect();
                event["name"] = json!("ask_question");
                event["summary"] = json!("");
                event["questions"] = json!(questions);
            }
            _ => {
                let server = text(&params["serverName"]);
                event["name"] = json!("mcp_elicitation");
                event["summary"] = json!(format!("{server}: {}", text(&params["message"])));
                event["url"] = params["url"].clone();
                event["questions"] =
                    super::claude::elicitation_questions(&params["requestedSchema"]);
            }
        }
        Output::events(vec![event])
    }

    /// A subagent's activity on the parent thread: started, interacted with,
    /// interrupted or done. Returns whether its entry changed.
    fn agent_activity(&mut self, item: &Value) -> bool {
        let thread = text(&item["agentThreadId"]).to_string();
        if thread.is_empty() {
            return false;
        }
        let status = match text(&item["kind"]) {
            "completed" => "completed",
            "interrupted" => "stopped",
            _ => "running",
        };
        match self.agents.iter_mut().find(|a| a["id"] == thread.as_str()) {
            Some(agent) => {
                if agent["status"] != "failed" {
                    agent["status"] = json!(status);
                }
            }
            None => self.agents.push(json!({
                "id": thread,
                "label": agent_label(text(&item["agentPath"]), &thread),
                "type": "subagent",
                "tool_use_id": item["id"],
                "status": status,
                "started_at": now_ms(),
                "tokens": 0,
                "tool_calls": 0,
            })),
        }
        true
    }

    /// A subagent thread's own notification: its progress in the agent
    /// trace and the subagent strip, never in this conversation.
    fn child_notification(&mut self, thread: &str, method: &str, params: &Value) -> Vec<Value> {
        let Some(agent) = self.agents.iter_mut().find(|a| a["id"] == thread) else {
            return vec![];
        };
        match method {
            "turn/started" => agent["status"] = json!("running"),
            "turn/completed" => {
                let turn = &params["turn"];
                agent["status"] = json!(if turn["status"] == "failed" {
                    "failed"
                } else {
                    "completed"
                });
                if let Some(ms) = turn["durationMs"].as_u64() {
                    agent["duration_ms"] = json!(agent["duration_ms"].as_u64().unwrap_or(0) + ms);
                }
            }
            "thread/tokenUsage/updated" => {
                agent["tokens"] = params["tokenUsage"]["total"]["totalTokens"].clone();
            }
            "item/completed" => {
                let item = &params["item"];
                match text(&item["type"]) {
                    "agentMessage" => {
                        let summary: String = text(&item["text"]).chars().take(200).collect();
                        agent["result"] = json!(summary);
                    }
                    "commandExecution" | "fileChange" | "mcpToolCall" | "dynamicToolCall"
                    | "webSearch" => {
                        agent["tool_calls"] = json!(agent["tool_calls"].as_u64().unwrap_or(0) + 1);
                    }
                    _ => return vec![],
                }
            }
            "item/started" => {
                let item = &params["item"];
                let message = match text(&item["type"]) {
                    "commandExecution" => text(&item["command"]).to_string(),
                    "fileChange" => "apply_patch".to_string(),
                    "mcpToolCall" => format!("{}.{}", text(&item["server"]), text(&item["tool"])),
                    _ => return vec![],
                };
                agent["summary"] = json!(message.chars().take(120).collect::<String>());
            }
            _ => return vec![],
        }
        let lifecycle = json!({
            "type": "subagent_lifecycle",
            "path": agent["id"],
            "label": agent["label"],
            "status": agent["status"],
            "message": agent["summary"].as_str().unwrap_or_default(),
        });
        vec![lifecycle, self.agent_runs()]
    }

    fn item_started(&mut self, item: &Value, turn: &str) -> Vec<Value> {
        let id = text(&item["id"]).to_string();
        match text(&item["type"]) {
            // Every client sees the turn's input, not only the one that sent it.
            "userMessage" => {
                if !turn.is_empty() {
                    self.user_turns.push(turn.to_string());
                }
                let (content, images) = user_input(&item["content"]);
                if content.is_empty() && images.is_empty() {
                    vec![]
                } else {
                    let mut event = json!({ "type": "user_message", "content": content });
                    if !images.is_empty() {
                        event["images"] = json!(images);
                    }
                    vec![event]
                }
            }
            // Plan mode's plan, streamed like a reply.
            "agentMessage" | "plan" => {
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
            // A subagent starting: its card, and its entry in the trace.
            "subAgentActivity" => {
                let started = item["kind"] == "started";
                self.agent_activity(item);
                let mut events = Vec::new();
                if started {
                    let label = agent_label(text(&item["agentPath"]), text(&item["agentThreadId"]));
                    events.push(
                        json!({ "type": "tool_start", "call_id": id, "name": "spawn_agent" }),
                    );
                    events.push(json!({ "type": "tool_update", "call_id": id, "output": json!({ "description": label }).to_string() }));
                    events.push(json!({ "type": "subagent_lifecycle", "path": item["agentThreadId"], "label": label, "status": "running", "message": "" }));
                } else if let Some(agent) = self
                    .agents
                    .iter()
                    .find(|a| a["id"] == item["agentThreadId"])
                {
                    events.push(json!({ "type": "subagent_lifecycle", "path": agent["id"], "label": agent["label"], "status": agent["status"], "message": "" }));
                }
                events.push(self.agent_runs());
                events
            }
            "collabAgentToolCall" => {
                let tool = text(&item["tool"]).to_string();
                for receiver in item["receiverThreadIds"].as_array().into_iter().flatten() {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a["id"] == *receiver) {
                        if !item["prompt"].is_null() {
                            agent["prompt"] = item["prompt"].clone();
                        }
                        if !item["model"].is_null() {
                            agent["model"] = item["model"].clone();
                        }
                    }
                }
                let name = format!("agent_{tool}");
                self.items.insert(id.clone(), Item::new(&name));
                let description = match text(&item["prompt"]) {
                    "" => self.receivers(item),
                    prompt => prompt.to_string(),
                };
                vec![
                    json!({ "type": "tool_start", "call_id": id, "name": name }),
                    json!({ "type": "tool_update", "call_id": id, "output": json!({ "description": description }).to_string() }),
                ]
            }
            _ => vec![],
        }
    }

    /// The subagents a collab call names, by label.
    fn receivers(&self, item: &Value) -> String {
        item["receiverThreadIds"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|id| {
                self.agents
                    .iter()
                    .find(|a| a["id"] == *id)
                    .map(|a| text(&a["label"]).to_string())
                    .unwrap_or_else(|| text(id).chars().take(8).collect())
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn item_completed(&mut self, item: &Value) -> Vec<Value> {
        let id = text(&item["id"]).to_string();
        let meta = self.items.remove(&id);
        let status = text(&item["status"]);
        match text(&item["type"]) {
            "agentMessage" | "plan" => {
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
            "subAgentActivity" if item["kind"] == "started" => {
                let label = agent_label(text(&item["agentPath"]), text(&item["agentThreadId"]));
                vec![
                    json!({ "type": "tool_output", "call_id": id, "name": "spawn_agent", "output": json!({ "description": label }).to_string(), "is_error": false }),
                ]
            }
            "collabAgentToolCall" => {
                let name = meta
                    .map(|m| m.name)
                    .unwrap_or_else(|| format!("agent_{}", text(&item["tool"])));
                let states: Vec<String> = item["agentsStates"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(thread, state)| {
                        let label = self
                            .agents
                            .iter()
                            .find(|a| a["id"] == thread.as_str())
                            .map(|a| text(&a["label"]).to_string())
                            .unwrap_or_else(|| thread.chars().take(8).collect());
                        format!("{label}: {}", text(&state["status"]))
                    })
                    .collect();
                let description = match states.is_empty() {
                    true => self.receivers(item),
                    false => states.join("\n"),
                };
                vec![
                    json!({ "type": "tool_output", "call_id": id, "name": name, "output": json!({ "description": description }).to_string(), "is_error": status == "failed" }),
                ]
            }
            // A review's findings arrive whole when the review ends.
            "exitedReviewMode" if !text(&item["review"]).is_empty() => vec![
                json!({ "type": "assistant_start" }),
                json!({ "type": "assistant_delta", "delta": text(&item["review"]) }),
            ],
            _ => vec![],
        }
    }

    fn on_notification(&mut self, method: &str, params: &Value) -> Vec<Value> {
        // Another thread's notification (a subagent's) is never this
        // conversation's: not its text, its usage or its turn ending.
        if let (Some(thread), Some(own)) = (params["threadId"].as_str(), self.thread.as_deref()) {
            if thread != own {
                return self.child_notification(thread, method, params);
            }
        }
        match method {
            "account/rateLimits/updated" => plan_usage(&params["rateLimits"]).into_iter().collect(),
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
            "item/started" => self.item_started(&params["item"], text(&params["turnId"])),
            "item/completed" => self.item_completed(&params["item"]),
            "item/agentMessage/delta" | "item/plan/delta" => {
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
                let counts = |part: &Value| {
                    let n = |key: &str| part[key].as_u64().unwrap_or(0);
                    (
                        n("inputTokens"),
                        n("cachedInputTokens"),
                        n("outputTokens"),
                        n("reasoningOutputTokens"),
                    )
                };
                let total = counts(&usage["total"]);
                // Since the last update; right after a resume only the last
                // request is new.
                let spent = match self.previous_total {
                    Some(previous) => (
                        total.0.saturating_sub(previous.0),
                        total.1.saturating_sub(previous.1),
                        total.2.saturating_sub(previous.2),
                        total.3.saturating_sub(previous.3),
                    ),
                    None => counts(&usage["last"]),
                };
                events.push(json!({
                    "type": "usage",
                    "input_tokens": spent.0,
                    "cached_input_tokens": spent.1,
                    "output_tokens": spent.2,
                    "reasoning_tokens": spent.3,
                }));
                events.push(json!({ "type": "context_usage", "tokens": usage["last"]["totalTokens"].as_u64().unwrap_or(0) }));
                self.previous_total = Some(total);
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
                    // "Reconnecting... 2/5"
                    let (attempt, max) = message
                        .rsplit(' ')
                        .next()
                        .and_then(|n| n.split_once('/'))
                        .and_then(|(a, m)| Some((a.parse::<u64>().ok()?, m.parse::<u64>().ok()?)))
                        .unwrap_or((0, 0));
                    let reason = match text(&params["error"]["additionalDetails"]) {
                        "" => message,
                        details => details,
                    };
                    vec![
                        json!({ "type": "retrying", "attempt": attempt, "max_attempts": max, "delay_ms": 0, "reason": reason }),
                    ]
                } else {
                    vec![error_event(message, &params["error"]["codexErrorInfo"])]
                }
            }
            // Warnings the user should see (a config typo, a deprecation),
            // each once: Codex repeats them per thread.
            "warning" | "guardianWarning" | "configWarning" | "deprecationNotice" => {
                let message = [text(&params["message"]), text(&params["summary"])]
                    .into_iter()
                    .find(|m| !m.is_empty())
                    .unwrap_or_default();
                let full = match text(&params["details"]) {
                    "" => message.to_string(),
                    details => format!("{message}\n{details}"),
                };
                if full.is_empty() || !self.warned.insert(message.to_string()) {
                    return vec![];
                }
                vec![json!({ "type": "info", "message": format!("[codex] {full}") })]
            }
            "thread/name/updated" => match params["threadName"].as_str() {
                Some(name) if !name.trim().is_empty() => {
                    vec![json!({ "type": "session_title", "title": name })]
                }
                _ => vec![],
            },
            "item/fileChange/patchUpdated" => {
                let id = text(&params["itemId"]).to_string();
                let Some(meta) = self.items.get_mut(&id) else {
                    return vec![];
                };
                meta.changes = params["changes"].as_array().cloned().unwrap_or_default();
                let update = file_change_output(&meta.changes, None);
                vec![json!({ "type": "tool_update", "call_id": id, "output": update })]
            }
            "item/mcpToolCall/progress" if !text(&params["message"]).is_empty() => {
                vec![
                    json!({ "type": "tool_update", "call_id": params["itemId"], "output": params["message"] }),
                ]
            }
            "mcpServer/startupStatus/updated" => {
                self.mcp_relist = self.mcp_watched;
                vec![]
            }
            "mcpServer/oauthLogin/completed" => {
                self.mcp_relist = self.mcp_watched;
                let name = text(&params["name"]);
                vec![
                    json!({ "type": "info", "message": if params["success"] == true {
                    format!("[codex] {name}: signed in")
                } else {
                    format!("[codex] {name}: sign-in failed: {}", text(&params["error"]))
                } }),
                ]
            }
            "account/login/completed" => vec![
                json!({ "type": "info", "message": if params["success"] == true {
                "[codex] signed in".to_string()
            } else {
                format!("[codex] sign-in failed: {}", text(&params["error"]))
            } }),
            ],
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
                .unwrap_or_default()
                .to_string();
                let from = [text(&params["fromModel"]), text(&params["from"])]
                    .into_iter()
                    .find(|t| !t.is_empty())
                    .unwrap_or_default();
                if to.is_empty() {
                    return vec![];
                }
                let mut events = vec![
                    json!({ "type": "model_fallback", "from": from, "to": to, "reason": text(&params["reason"]) }),
                ];
                if to != self.model {
                    self.model = to;
                    events.push(self.model_status());
                }
                events
            }
            "thread/goal/updated" if params["goal"].is_object() => {
                vec![Self::goal_event(&params["goal"])]
            }
            "thread/goal/cleared" => vec![Self::goal_event(&Value::Null)],
            "serverRequest/resolved" => {
                self.approvals
                    .retain(|_, (request, _, _)| *request != params["requestId"]);
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
            json!({ "clientInfo": { "name": "jucode-daemon", "title": "JuCode", "version": env!("CARGO_PKG_VERSION") }, "capabilities": { "experimentalApi": true } }),
            "",
        )]
    }

    fn translate(&mut self, line: Line) -> Output {
        let frame = match line {
            // Diagnostics only; kept for an exit message (Session::note_stderr).
            Line::Stderr(_) => return Output::default(),
            Line::Frame(frame) => frame,
        };
        let id = &frame["id"];
        let has_id = id.is_u64() || id.is_string();
        let mut out = match frame["method"].as_str() {
            Some(method) if has_id => self.on_server_request(id, method, &frame["params"]),
            Some(method) => Output::events(self.on_notification(method, &frame["params"])),
            None => match id.as_u64() {
                Some(id) => self.on_response(id, &frame["result"], &frame["error"]),
                None => Output::default(),
            },
        };
        // The turn ended: the next message that waited for it starts.
        if !self.busy && self.thread.is_some() && !self.rolling_back && !self.waiting.is_empty() {
            let (_, input) = self.waiting.remove(0);
            out.frames.push(self.turn_start(input));
            out.events.push(self.pending_event());
        }
        if std::mem::take(&mut self.mcp_relist) {
            let thread = self.thread.clone();
            out.frames.push(self.request(
                "mcpServerStatus/list",
                json!({ "detail": "toolsAndAuthOnly", "threadId": thread }),
                "",
            ));
        }
        out
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
                if self.rolling_back {
                    self.after_rollback.extend(input);
                    return Ok(Output::default());
                }
                // Mid-turn it waits for the turn to end, or to be steered in.
                if self.busy {
                    self.waiting.push((text(&op["content"]).to_string(), input));
                    return Ok(Output::events(vec![self.pending_event()]));
                }
                vec![self.turn_start(input)]
            }
            "steer" => {
                let (Some(thread), Some(turn)) = (self.thread.clone(), self.active_turn.clone())
                else {
                    return Ok(Output::default());
                };
                if self.waiting.is_empty() {
                    return Ok(Output::default());
                }
                self.steering = std::mem::take(&mut self.waiting);
                let input: Vec<Value> = self
                    .steering
                    .iter()
                    .flat_map(|(_, input)| input.clone())
                    .collect();
                let frame = self.request(
                    "turn/steer",
                    json!({ "threadId": thread, "expectedTurnId": turn, "input": input }),
                    "",
                );
                return Ok(Output {
                    events: vec![self.pending_event()],
                    frames: vec![frame],
                });
            }
            "rename" => match self.thread.clone() {
                Some(thread) => vec![self.request(
                    "thread/name/set",
                    json!({ "threadId": thread, "name": op["title"] }),
                    "",
                )],
                None => vec![],
            },
            "agent_runs" => return Ok(Output::events(vec![self.agent_runs()])),
            "subagent_transcript" => {
                let id = text(&op["agent_id"]).to_string();
                vec![self.request(
                    "thread/turns/list",
                    json!({ "threadId": id, "itemsView": "full", "sortDirection": "asc", "limit": 200 }),
                    &id,
                )]
            }
            "mcp_list" => {
                self.mcp_watched = true;
                let thread = self.thread.clone();
                vec![self.request(
                    "mcpServerStatus/list",
                    json!({ "detail": "toolsAndAuthOnly", "threadId": thread }),
                    "",
                )]
            }
            "mcp_reconnect" => {
                let thread = self.thread.clone();
                vec![
                    self.request("config/mcpServer/reload", json!({}), ""),
                    self.request(
                        "mcpServerStatus/list",
                        json!({ "detail": "toolsAndAuthOnly", "threadId": thread }),
                        "",
                    ),
                ]
            }
            "mcp_login" => {
                let name = text(&op["name"]).to_string();
                let thread = self.thread.clone();
                vec![self.request(
                    "mcpServer/oauth/login",
                    json!({ "name": name, "threadId": thread }),
                    &name,
                )]
            }
            "approve" => {
                let call = text(&op["call_id"]);
                let Some((request, method, params)) = self.approvals.remove(call) else {
                    return Err(format!("no open approval {call}"));
                };
                let deny = op["decision"] == "deny";
                let always = op["always"] == true;
                let result = match method.as_str() {
                    "item/permissions/requestApproval" => json!({
                        "permissions": if deny { json!({}) } else { params["permissions"].clone() },
                        "scope": if always { "session" } else { "turn" },
                    }),
                    "item/tool/requestUserInput" => {
                        let mut answers = serde_json::Map::new();
                        if !deny {
                            for q in params["questions"].as_array().into_iter().flatten() {
                                if let Some(answer) = op["answers"][text(&q["question"])].as_str() {
                                    answers.insert(
                                        text(&q["id"]).to_string(),
                                        json!({ "answers": [answer] }),
                                    );
                                }
                            }
                        }
                        json!({ "answers": answers })
                    }
                    "mcpServer/elicitation/request" => {
                        if deny {
                            json!({ "action": "decline", "content": null, "_meta": null })
                        } else {
                            let content = super::claude::elicitation_content(
                                &params["requestedSchema"],
                                &op["answers"],
                            );
                            json!({ "action": "accept", "content": content, "_meta": null })
                        }
                    }
                    _ => {
                        let amendment = &params["proposedExecpolicyAmendment"];
                        let decision = if deny {
                            json!("decline")
                        } else if always && op["always_scope"] == "rule" && amendment.is_array() {
                            json!({ "acceptWithExecpolicyAmendment": { "execpolicy_amendment": amendment } })
                        } else if always {
                            json!("acceptForSession")
                        } else {
                            json!("accept")
                        };
                        json!({ "decision": decision })
                    }
                };
                vec![json!({ "jsonrpc": "2.0", "id": request, "result": result }).to_string()]
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
                    // The Nth user message from the end: back to before its turn.
                    ("/rewind", Some(thread)) => match arg.parse::<usize>() {
                        Ok(n) if n > 0 && n <= self.user_turns.len() => {
                            let at = self.user_turns.len() - n;
                            let dropped = self.user_turns.split_off(at);
                            let before = dropped[0].clone();
                            // Messages of one turn (a steered one) go together.
                            while self.user_turns.last() == Some(&before) {
                                self.user_turns.pop();
                            }
                            self.rolling_back = true;
                            vec![self.request(
                                "thread/revert",
                                json!({ "threadId": thread, "beforeTurnId": before }),
                                &dropped.join(","),
                            )]
                        }
                        _ => return Err(format!("Codex cannot rewind {arg} messages here")),
                    },
                    ("/fast" | "/thinking", _) => {
                        let on = match arg {
                            "" if command == "/fast" => !self.fast(),
                            "" => !self.thinking,
                            "on" => true,
                            "off" => false,
                            other => return Err(format!("{command} takes on or off, not {other}")),
                        };
                        if command == "/fast" {
                            self.service_tier =
                                Some(if on { "priority" } else { "default" }.to_string());
                        } else {
                            self.thinking = on;
                        }
                        return Ok(Output::events(vec![self.model_status()]));
                    }
                    ("/login", _) => vec![self.request(
                        "account/login/start",
                        json!({ "type": "chatgptDeviceCode" }),
                        "",
                    )],
                    ("/review", Some(thread)) => {
                        let target = if arg.is_empty() {
                            json!({ "type": "uncommittedChanges" })
                        } else {
                            json!({ "type": "custom", "instructions": arg })
                        };
                        self.busy = true;
                        let frame = self.request(
                            "review/start",
                            json!({ "threadId": thread, "target": target }),
                            "",
                        );
                        return Ok(Output {
                            events: vec![json!({ "type": "user_message", "content": input })],
                            frames: vec![frame],
                        });
                    }
                    ("/compact" | "/goal" | "/rewind" | "/review", None) => vec![],
                    (command, _) if self.skill(command).is_some() => {
                        let (name, path) = self.skill(command).unwrap_or_default();
                        let text = if arg.is_empty() {
                            format!("${name}")
                        } else {
                            format!("${name} {arg}")
                        };
                        let input = vec![
                            json!({ "type": "skill", "name": name, "path": path }),
                            json!({ "type": "text", "text": text, "text_elements": [] }),
                        ];
                        if self.thread.is_none() {
                            self.queued.extend(input);
                            return Ok(Output::default());
                        }
                        vec![self.turn_start(input)]
                    }
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

    /// Codex takes the approval policy with each turn.
    fn mode_applies_live(&self) -> bool {
        false
    }

    fn restart_for(&self, _op: &Value) -> Option<Options> {
        None
    }

    fn keep(&self, options: Options) -> Options {
        Options {
            fast: self.fast(),
            thinking: Some(self.thinking),
            ..options
        }
    }

    fn conversation(&self) -> Option<String> {
        self.thread.clone()
    }

    fn approval_mode(&self) -> Option<String> {
        Some(self.mode.to_string())
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
    fn a_message_sent_during_a_rewind_waits_for_it() {
        let mut c = opened();
        for (turn, text) in [("t1", "one"), ("t2", "two")] {
            frame(
                &mut c,
                json!({ "method": "item/started", "params": { "threadId": "th-1", "turnId": turn,
                "item": { "type": "userMessage", "id": format!("u-{turn}"), "content": [{ "type": "text", "text": text }] } } }),
            );
        }
        assert!(c
            .encode(&json!({ "op": "command", "input": "/rewind 3" }))
            .is_err());
        let revert = sent(
            &c.encode(&json!({ "op": "command", "input": "/rewind 1" }))
                .unwrap()
                .frames,
        );
        assert_eq!(revert[0]["method"], "thread/revert");
        assert_eq!(revert[0]["params"]["beforeTurnId"], "t2");
        let held = c
            .encode(&json!({ "op": "user_message", "content": "again" }))
            .unwrap();
        assert!(held.frames.is_empty());
        let id = revert[0]["id"].clone();
        let done = sent(&frame(&mut c, json!({ "id": id, "result": {} })).frames);
        assert_eq!(done[0]["method"], "turn/start");
        assert_eq!(done[0]["params"]["input"][0]["text"], "again");
        // The next rewind goes before the first message.
        let revert = sent(
            &c.encode(&json!({ "op": "command", "input": "/rewind 1" }))
                .unwrap()
                .frames,
        );
        assert_eq!(revert[0]["params"]["beforeTurnId"], "t1");
    }

    fn busy(c: &mut Codex) {
        let started = c
            .encode(&json!({ "op": "user_message", "content": "go" }))
            .unwrap();
        assert_eq!(sent(&started.frames)[0]["method"], "turn/start");
        frame(
            c,
            json!({ "method": "turn/started", "params": { "threadId": "th-1", "turn": { "id": "turn-1" } } }),
        );
    }

    #[test]
    fn a_message_mid_turn_waits_or_steers_in() {
        let mut c = opened();
        busy(&mut c);
        let queued = c
            .encode(&json!({ "op": "user_message", "content": "also this" }))
            .unwrap();
        assert!(queued.frames.is_empty());
        assert_eq!(
            queued.events[0],
            json!({ "type": "pending_messages", "messages": ["also this"] })
        );
        let steer = c.encode(&json!({ "op": "steer" })).unwrap();
        let frames = sent(&steer.frames);
        assert_eq!(frames[0]["method"], "turn/steer");
        assert_eq!(frames[0]["params"]["expectedTurnId"], "turn-1");
        assert_eq!(frames[0]["params"]["input"][0]["text"], "also this");
        // Not steerable after all: it waits for the next turn.
        let failed = frame(
            &mut c,
            json!({ "id": frames[0]["id"], "error": { "code": -32600, "message": "turn is not steerable" } }),
        );
        assert!(failed
            .events
            .iter()
            .any(|e| e["type"] == "pending_messages" && e["messages"] == json!(["also this"])));
        let ended = frame(
            &mut c,
            json!({ "method": "turn/completed", "params": { "threadId": "th-1", "turn": { "id": "turn-1", "status": "completed" } } }),
        );
        let next = sent(&ended.frames);
        assert_eq!(next[0]["method"], "turn/start");
        assert_eq!(next[0]["params"]["input"][0]["text"], "also this");
    }

    #[test]
    fn a_subagents_thread_stays_out_of_the_conversation() {
        let mut c = opened();
        busy(&mut c);
        let spawned = frame(&mut c, json!({ "method": "item/started", "params": { "threadId": "th-1", "turnId": "turn-1",
            "item": { "type": "subAgentActivity", "id": "call-1", "kind": "started", "agentThreadId": "child", "agentPath": "/root/pong" } } })).events;
        assert!(spawned
            .iter()
            .any(|e| e["type"] == "tool_start" && e["name"] == "spawn_agent"));
        assert!(spawned
            .iter()
            .any(|e| e["type"] == "subagent_lifecycle" && e["label"] == "pong"));
        let runs = spawned.iter().find(|e| e["type"] == "agent_runs").unwrap();
        assert_eq!(runs["agents"][0]["tool_use_id"], "call-1");
        // The child's text, usage and turn end are not this turn's.
        let delta = frame(&mut c, json!({ "method": "item/agentMessage/delta", "params": { "threadId": "child", "itemId": "m", "delta": "PONG" } })).events;
        assert!(delta.is_empty());
        frame(
            &mut c,
            json!({ "method": "thread/tokenUsage/updated", "params": { "threadId": "child", "tokenUsage": { "total": { "totalTokens": 900 }, "last": {} } } }),
        );
        let done = frame(&mut c, json!({ "method": "turn/completed", "params": { "threadId": "child", "turn": { "id": "c1", "status": "completed", "durationMs": 3814 } } })).events;
        assert!(c.busy());
        let runs = done.iter().find(|e| e["type"] == "agent_runs").unwrap();
        assert_eq!(runs["agents"][0]["tokens"], 900);
        assert_eq!(runs["agents"][0]["status"], "completed");
        assert_eq!(runs["agents"][0]["duration_ms"], 3814);
        // Its conversation is read back from its thread.
        let ask = sent(
            &c.encode(&json!({ "op": "subagent_transcript", "agent_id": "child" }))
                .unwrap()
                .frames,
        );
        assert_eq!(ask[0]["method"], "thread/turns/list");
        assert_eq!(ask[0]["params"]["itemsView"], "full");
        let read = frame(
            &mut c,
            json!({ "id": ask[0]["id"], "result": { "data": [{ "id": "c1", "items": [
            { "type": "userMessage", "content": [{ "type": "text", "text": "reply PONG" }] },
            { "type": "agentMessage", "text": "PONG" }
        ] }] } }),
        )
        .events;
        assert_eq!(read[0]["type"], "subagent_transcript");
        assert_eq!(read[0]["agent_id"], "child");
        assert_eq!(read[0]["items"][1]["content"], "PONG");
    }

    fn request(c: &mut Codex, method: &str, params: Value) -> Value {
        let out = frame(c, json!({ "id": 77, "method": method, "params": params }));
        out.events
            .into_iter()
            .find(|e| e["type"] == "approval_request")
            .unwrap()
    }

    fn answer(c: &mut Codex, op: Value) -> Value {
        let frames = c.encode(&op).unwrap().frames;
        sent(&frames)[0]["result"].clone()
    }

    #[test]
    fn codex_asks_for_permissions_input_and_mcp_forms() {
        let mut c = opened();
        let card = request(
            &mut c,
            "item/permissions/requestApproval",
            json!({ "threadId": "th-1", "itemId": "i", "reason": "fetch deps",
            "permissions": { "network": { "enabled": true }, "fileSystem": null } }),
        );
        assert_eq!(card["name"], "permissions");
        assert!(card["summary"].as_str().unwrap().contains("network access"));
        let result = answer(
            &mut c,
            json!({ "op": "approve", "call_id": card["call_id"], "decision": "allow", "always": true }),
        );
        assert_eq!(
            result,
            json!({ "permissions": { "network": { "enabled": true }, "fileSystem": null }, "scope": "session" })
        );

        let card = request(
            &mut c,
            "item/tool/requestUserInput",
            json!({ "threadId": "th-1", "itemId": "i", "questions": [
            { "id": "q1", "header": "Purpose", "question": "What is it for?", "isOther": true, "isSecret": false, "options": [{ "label": "Docs", "description": "" }] }
        ] }),
        );
        assert_eq!(card["questions"][0]["options"][0]["label"], "Docs");
        let result = answer(
            &mut c,
            json!({ "op": "approve", "call_id": card["call_id"], "decision": "allow", "answers": { "What is it for?": "Docs" } }),
        );
        assert_eq!(
            result,
            json!({ "answers": { "q1": { "answers": ["Docs"] } } })
        );

        let card = request(
            &mut c,
            "mcpServer/elicitation/request",
            json!({ "threadId": "th-1", "serverName": "jira", "mode": "form", "message": "Pick",
            "requestedSchema": { "type": "object", "properties": { "n": { "type": "integer" } } } }),
        );
        assert_eq!(card["name"], "mcp_elicitation");
        let result = answer(
            &mut c,
            json!({ "op": "approve", "call_id": card["call_id"], "decision": "allow", "answers": { "n": "3" } }),
        );
        assert_eq!(
            result,
            json!({ "action": "accept", "content": { "n": 3 }, "_meta": null })
        );

        let card = request(
            &mut c,
            "item/commandExecution/requestApproval",
            json!({ "threadId": "th-1", "itemId": "i", "command": "npm test",
            "proposedExecpolicyAmendment": ["npm", "test"] }),
        );
        assert_eq!(card["scopes"], json!(["session", "rule"]));
        let result = answer(
            &mut c,
            json!({ "op": "approve", "call_id": card["call_id"], "decision": "allow", "always": true, "always_scope": "rule" }),
        );
        assert_eq!(
            result,
            json!({ "decision": { "acceptWithExecpolicyAmendment": { "execpolicy_amendment": ["npm", "test"] } } })
        );
    }

    #[test]
    fn notices_titles_and_session_switches() {
        let mut c = opened();
        let warn = |c: &mut Codex| {
            frame(c, json!({ "method": "configWarning", "params": { "summary": "bad key", "details": null } })).events
        };
        assert_eq!(warn(&mut c)[0]["message"], "[codex] bad key");
        assert!(warn(&mut c).is_empty());
        let retry = frame(
            &mut c,
            json!({ "method": "error", "params": { "threadId": "th-1", "willRetry": true,
            "error": { "message": "Reconnecting... 2/5", "additionalDetails": "HTTP 502" } } }),
        )
        .events;
        assert_eq!(
            retry[0],
            json!({ "type": "retrying", "attempt": 2, "max_attempts": 5, "delay_ms": 0, "reason": "HTTP 502" })
        );
        let fallback = frame(&mut c, json!({ "method": "model/rerouted", "params": { "threadId": "th-1", "fromModel": "gpt-5", "toModel": "gpt-5-mini", "reason": "capacity" } })).events;
        assert_eq!(fallback[0]["type"], "model_fallback");
        let title = frame(&mut c, json!({ "method": "thread/name/updated", "params": { "threadId": "th-1", "threadName": "Fix login" } })).events;
        assert_eq!(
            title[0],
            json!({ "type": "session_title", "title": "Fix login" })
        );
        let rename = sent(
            &c.encode(&json!({ "op": "rename", "title": "New" }))
                .unwrap()
                .frames,
        );
        assert_eq!(rename[0]["method"], "thread/name/set");

        let status = c
            .encode(&json!({ "op": "command", "input": "/fast on" }))
            .unwrap()
            .events;
        assert_eq!(status[0]["fast"], true);
        c.encode(&json!({ "op": "command", "input": "/thinking off" }))
            .unwrap();
        c.encode(&json!({ "op": "set_approval_mode", "mode": "plan" }))
            .unwrap();
        let turn = sent(
            &c.encode(&json!({ "op": "user_message", "content": "plan it" }))
                .unwrap()
                .frames,
        );
        assert_eq!(turn[0]["params"]["serviceTier"], "priority");
        assert_eq!(turn[0]["params"]["summary"], "none");
        assert_eq!(turn[0]["params"]["collaborationMode"]["mode"], "plan");
        assert_eq!(turn[0]["params"]["sandboxPolicy"]["type"], "readOnly");
        frame(
            &mut c,
            json!({ "method": "turn/completed", "params": { "threadId": "th-1", "turn": { "id": "x", "status": "completed" } } }),
        );
        c.encode(&json!({ "op": "set_approval_mode", "mode": "auto" }))
            .unwrap();
        let turn = sent(
            &c.encode(&json!({ "op": "user_message", "content": "do it" }))
                .unwrap()
                .frames,
        );
        assert_eq!(turn[0]["params"]["collaborationMode"]["mode"], "default");
        assert_eq!(turn[0]["params"]["approvalsReviewer"], "auto_review");
        let kept = c.keep(Options::default());
        assert!(kept.fast);
        assert_eq!(kept.thinking, Some(false));
    }

    #[test]
    fn mcp_servers_list_reload_and_sign_in() {
        let mut c = opened();
        let list = sent(&c.encode(&json!({ "op": "mcp_list" })).unwrap().frames);
        assert_eq!(list[0]["params"]["detail"], "toolsAndAuthOnly");
        let view = frame(&mut c, json!({ "id": list[0]["id"], "result": { "data": [
            { "name": "gh", "runtimeStatus": "authenticationRequired", "httpOrigin": "https://x", "tools": {} },
            { "name": "fs", "runtimeStatus": "connected", "httpOrigin": null, "tools": { "read": { "description": "Read" } } }
        ] } })).events;
        let servers = view[0]["servers"].as_array().unwrap();
        assert_eq!(
            (
                servers[0]["state"].as_str(),
                servers[0]["needs_auth"].as_bool()
            ),
            (Some("failed"), Some(true))
        );
        assert_eq!(servers[1]["tools"][0]["name"], "read");
        assert_eq!(servers[1]["can_toggle"], false);
        let login = sent(
            &c.encode(&json!({ "op": "mcp_login", "name": "gh" }))
                .unwrap()
                .frames,
        );
        let url = frame(
            &mut c,
            json!({ "id": login[0]["id"], "result": { "authorizationUrl": "https://auth" } }),
        )
        .events;
        assert_eq!(
            url[0],
            json!({ "type": "mcp_login", "name": "gh", "url": "https://auth" })
        );
        // A server that changes is listed again for the client watching.
        let relist = frame(
            &mut c,
            json!({ "method": "mcpServer/startupStatus/updated", "params": {} }),
        );
        assert_eq!(sent(&relist.frames)[0]["method"], "mcpServerStatus/list");
    }

    #[test]
    fn the_gateway_goes_to_this_process_only() {
        let mut command = std::process::Command::new("codex");
        use_gateway(&mut command, "http://127.0.0.1:7788/gw", "tok").unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[1], "model_provider=\"jucode_gateway\"");
        assert!(args[3].contains("base_url=\"http://127.0.0.1:7788/gw/v1\""));
        assert!(!args.concat().contains("tok\""));
        let env: Vec<_> = command.get_envs().collect();
        assert_eq!(
            env,
            [(
                std::ffi::OsStr::new("JUCODE_GATEWAY_TOKEN"),
                Some(std::ffi::OsStr::new("tok"))
            )]
        );
        assert_eq!(
            Options::from_json(&json!({ "jucode_gateway": true })).gateway,
            Some(true)
        );
        assert_eq!(Options::from_json(&json!({})).gateway, None);
    }

    #[test]
    fn usage_reports_per_update_deltas_with_cached_input() {
        let mut c = opened();
        let update = |c: &mut Codex, input: u64, cached: u64, output: u64| {
            let params = json!({ "tokenUsage": {
                "total": { "inputTokens": input, "cachedInputTokens": cached, "outputTokens": output },
                "last": { "totalTokens": input + output }
            } });
            c.on_notification("thread/tokenUsage/updated", &params)
                .into_iter()
                .find(|e| e["type"] == "usage")
                .unwrap()
        };
        let first = update(&mut c, 1000, 0, 10);
        assert_eq!(first["cached_input_tokens"], 0);
        let second = update(&mut c, 2500, 900, 30);
        assert_eq!(second["input_tokens"], 1500);
        assert_eq!(second["cached_input_tokens"], 900);
        assert_eq!(second["output_tokens"], 20);
    }

    #[test]
    fn a_resumed_thread_counts_only_new_requests() {
        let mut c = opened();
        let _ = c.thread_opened(
            &json!({ "thread": { "id": "th", "turns": [] }, "model": "gpt-5.5" }),
            true,
        );
        let update = |c: &mut Codex, total: u64, last: u64| {
            let params = json!({ "tokenUsage": {
                "total": { "inputTokens": total, "outputTokens": total / 10 },
                "last": { "inputTokens": last, "outputTokens": last / 10, "totalTokens": last }
            } });
            c.on_notification("thread/tokenUsage/updated", &params)
                .into_iter()
                .find(|e| e["type"] == "usage")
                .unwrap()
        };
        // The thread already holds 500k tokens from before the restart.
        let first = update(&mut c, 500_000, 2_000);
        assert_eq!(first["input_tokens"], 2_000);
        let second = update(&mut c, 503_000, 3_000);
        assert_eq!(second["input_tokens"], 3_000);
    }

    #[test]
    fn skills_list_as_commands_and_review_and_skills_run() {
        let mut c = opened();
        let listed = frame(
            &mut c,
            json!({ "id": 4, "result": { "data": [{ "cwd": "/p", "errors": [], "skills": [
                { "name": "lint", "path": "/s/lint/SKILL.md", "description": "Long text", "shortDescription": "Run the linters", "enabled": true },
                { "name": "off", "path": "/s/off/SKILL.md", "description": "x", "enabled": false }
            ] }] } }),
        );
        let commands = listed.events[0]["commands"].as_array().unwrap();
        let names: Vec<&str> = commands
            .iter()
            .map(|c| c["command"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["/model", "/resume", "/compact", "/review", "/goal", "/login", "/lint"]
        );
        assert_eq!(commands[6]["description"], "Run the linters");
        assert_eq!(commands[6]["marker"], "SKILL");

        let skill = c
            .encode(&json!({ "op": "command", "input": "/lint src" }))
            .unwrap();
        let turn = &sent(&skill.frames)[0];
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(
            turn["params"]["input"][0],
            json!({ "type": "skill", "name": "lint", "path": "/s/lint/SKILL.md" })
        );
        assert_eq!(turn["params"]["input"][1]["text"], "$lint src");

        let review = c
            .encode(&json!({ "op": "command", "input": "/review" }))
            .unwrap();
        assert_eq!(
            review.events[0],
            json!({ "type": "user_message", "content": "/review" })
        );
        let request = &sent(&review.frames)[0];
        assert_eq!(request["method"], "review/start");
        assert_eq!(request["params"]["target"]["type"], "uncommittedChanges");
        let custom = c
            .encode(&json!({ "op": "command", "input": "/review check the SQL" }))
            .unwrap();
        assert_eq!(
            sent(&custom.frames)[0]["params"]["target"],
            json!({ "type": "custom", "instructions": "check the SQL" })
        );
        let done = frame(
            &mut c,
            json!({ "method": "item/completed", "params": { "item": { "id": "r1", "type": "exitedReviewMode", "review": "No issues" } } }),
        );
        assert_eq!(done.events[1]["delta"], "No issues");

        assert!(c
            .encode(&json!({ "op": "command", "input": "/nope" }))
            .is_err());
        let echo = frame(
            &mut c,
            json!({ "method": "item/started", "params": { "item": { "id": "u1", "type": "userMessage", "content": [{ "type": "text", "text": "hi" }] } } }),
        );
        assert_eq!(
            echo.events,
            vec![json!({ "type": "user_message", "content": "hi" })]
        );
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
    fn the_chatgpt_plan_usage_is_read_and_followed() {
        let mut c = Codex::new(Path::new("/p"), &Options::default());
        c.start();
        let next = sent(&frame(&mut c, json!({ "id": 1, "result": {} })).frames);
        let read = next
            .iter()
            .find(|f| f["method"] == "account/rateLimits/read")
            .expect("asked for the limits");
        let snapshot = json!({ "planType": "plus",
            "primary": { "usedPercent": 37, "windowDurationMins": 300, "resetsAt": 1790800000 },
            "secondary": { "usedPercent": 12, "windowDurationMins": 10080, "resetsAt": 1791300000 } });
        let out = frame(
            &mut c,
            json!({ "id": read["id"], "result": { "rateLimits": snapshot } }),
        );
        assert_eq!(types(&out.events), ["plan_usage"]);
        assert_eq!(out.events[0]["plan"], "plus");
        assert_eq!(
            out.events[0]["windows"][0],
            json!({ "key": "primary", "used": 37.0, "resets_at": 1_790_800_000_000u64, "minutes": 300 })
        );
        let out = frame(
            &mut c,
            json!({ "method": "account/rateLimits/updated", "params": { "rateLimits": { "primary": { "usedPercent": 40 } } } }),
        );
        assert_eq!(out.events[0]["windows"][0]["used"], 40.0);
        assert_eq!(out.events[0]["windows"][0]["resets_at"], Value::Null);
    }

    #[test]
    fn an_account_without_a_plan_reads_no_limits_quietly() {
        let mut c = Codex::new(Path::new("/p"), &Options::default());
        c.start();
        let next = sent(&frame(&mut c, json!({ "id": 1, "result": {} })).frames);
        let read = next
            .iter()
            .find(|f| f["method"] == "account/rateLimits/read")
            .unwrap();
        let out = frame(
            &mut c,
            json!({ "id": read["id"], "error": { "code": -32600, "message": "not signed in with ChatGPT" } }),
        );
        assert!(out.events.is_empty());
        // A gateway session does not ask.
        let mut g = Codex::new(
            Path::new("/p"),
            &Options {
                gateway: Some(true),
                ..Options::default()
            },
        );
        g.start();
        let next = sent(&frame(&mut g, json!({ "id": 1, "result": {} })).frames);
        assert!(next
            .iter()
            .all(|f| f["method"] != "account/rateLimits/read"));
    }

    #[test]
    fn a_gateway_resume_stays_on_the_gateway() {
        let mut c = Codex::new(
            Path::new("/p"),
            &Options {
                resume: Some("th-9".into()),
                gateway: Some(true),
                ..Options::default()
            },
        );
        c.start();
        let next = sent(&frame(&mut c, json!({ "id": 1, "result": {} })).frames);
        assert_eq!(next[1]["method"], "thread/resume");
        assert_eq!(next[1]["params"]["modelProvider"], GATEWAY_PROVIDER);
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
        assert!(
            next[1]["params"].get("modelProvider").is_none(),
            "keeps the thread's provider"
        );
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
