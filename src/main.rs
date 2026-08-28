use jucode_agent_core::{AgentCore, AgentEvent, ApprovalMode};
use jucode_tui::{TuiApp, TuiRuntime};
use serde_json::{json, Value};
use std::{
    env, io,
    io::{BufRead, Read, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Runtime(AgentCore);

#[derive(Default)]
struct HeadlessStats {
    status: String,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    context_tokens: u64,
    context_tokenizer: Option<String>,
    cost: f64,
    tool_calls: u64,
    subagent_events: u64,
    assistant_chars: usize,
    last_error: Option<String>,
    last_context_state: Option<String>,
    event_counts: std::collections::BTreeMap<String, u64>,
}

impl TuiRuntime for Runtime {
    fn startup_events(&self) -> Vec<AgentEvent> {
        self.0.startup_events()
    }

    fn model_status_event(&self) -> AgentEvent {
        self.0.model_status_event()
    }

    fn submit_user_message(&mut self, message: String) -> Vec<AgentEvent> {
        self.0.submit_user_message(message)
    }

    fn steer(&mut self) -> Vec<AgentEvent> {
        self.0.steer()
    }

    fn handle_command(&mut self, input: &str) -> (bool, Vec<AgentEvent>) {
        self.0.handle_command(input)
    }

    fn poll_events(&mut self) -> Vec<AgentEvent> {
        self.0.poll_events()
    }
}

fn main() -> io::Result<()> {
    jucode_agent_core::logging::init_global();
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    // Before anything that may touch the terminal: `--version` must work in
    // TTY-less contexts (CI release guard, the desktop's check_backend probe).
    if args.iter().any(|a| a == "--version" || a == "-V")
        || args.first().map(String::as_str) == Some("version")
    {
        println!("jucode {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let approval_mode = match take_approval_mode_flag(&mut args) {
        Ok(mode) => mode,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    if args.first().map(String::as_str) == Some("--headless") {
        args.remove(0);
        let code = run_headless(args, approval_mode)?;
        std::process::exit(code);
    }
    if args.first().map(String::as_str) == Some("serve") {
        let code = run_serve(approval_mode)?;
        std::process::exit(code);
    }
    if args.first().map(String::as_str) == Some("providers") {
        let list = jucode_agent_core::builtin_providers()
            .into_iter()
            .map(|(id, base_url, protocol)| {
                let models = jucode_agent_core::models_for_provider(&id)
                    .into_iter()
                    .map(|m| {
                        json!({
                            "name": m.name,
                            "context_window": m.context_window,
                            "max_output_tokens": m.max_output_tokens,
                            "reasoning_efforts": m.reasoning_efforts,
                        })
                    })
                    .collect::<Vec<_>>();
                json!({ "id": id, "base_url": base_url, "protocol": protocol, "models": models })
            })
            .collect::<Vec<_>>();
        println!("{}", json!(list));
        std::process::exit(0);
    }
    let mut core = AgentCore::new()?;
    if let Some(mode) = approval_mode {
        // Startup events (emitted by the TUI) will reflect the switched mode.
        let _ = core.set_approval_mode(mode);
    }
    core.start_update_check();
    TuiApp::new(Runtime(core)).run()
}

/// Extracts `--approval-mode <mode>` (or `--approval-mode=<mode>`) from `args`.
fn take_approval_mode_flag(args: &mut Vec<String>) -> Result<Option<ApprovalMode>, String> {
    let Some(index) = args
        .iter()
        .position(|arg| arg == "--approval-mode" || arg.starts_with("--approval-mode="))
    else {
        return Ok(None);
    };
    let arg = args.remove(index);
    let value = match arg.strip_prefix("--approval-mode=") {
        Some(value) => value.to_string(),
        None => {
            if index >= args.len() {
                return Err(
                    "--approval-mode requires a value: read-only, auto-edit, or full-auto"
                        .to_string(),
                );
            }
            args.remove(index)
        }
    };
    ApprovalMode::parse(&value).map(Some)
}

/// The approval mode a headless run uses. Headless reads no further stdin, so
/// approval prompts can never be answered interactively; instead of silently
/// running full-auto (the old behavior), headless defaults to the safest mode
/// and auto-denies gated tool calls. Loosen explicitly with
/// `--approval-mode auto-edit` or `--approval-mode full-auto`.
fn headless_approval_mode(flag: Option<ApprovalMode>) -> ApprovalMode {
    flag.unwrap_or(ApprovalMode::ReadOnly)
}

fn run_headless(args: Vec<String>, approval_mode: Option<ApprovalMode>) -> io::Result<i32> {
    let mut prompt = args.join(" ");
    if prompt.trim().is_empty() {
        io::stdin().read_to_string(&mut prompt)?;
    }
    let mut core = AgentCore::new()?;
    let mut stdout = io::stdout();
    for event in core.set_approval_mode(headless_approval_mode(approval_mode)) {
        write_event(&mut stdout, event)?;
    }
    let mut done = false;
    let mut stats = HeadlessStats::default();
    let started = Instant::now();
    let mut pending_denials = Vec::new();
    for event in core.submit_user_message(prompt) {
        if matches!(event, AgentEvent::Error(_)) {
            done = true;
        }
        if let AgentEvent::ApprovalRequest { call_id, name, .. } = &event {
            pending_denials.push((call_id.clone(), name.clone()));
        }
        record_headless_event(&event, &mut stats);
        write_event(&mut stdout, event)?;
    }
    auto_deny_approvals(&mut core, &mut stdout, &mut stats, &mut pending_denials)?;
    while !done {
        let events = core.poll_events();
        for event in events {
            if matches!(event, AgentEvent::Status(ref value) if value == "ready")
                || matches!(event, AgentEvent::Error(_))
            {
                done = true;
            }
            if let AgentEvent::ApprovalRequest { call_id, name, .. } = &event {
                pending_denials.push((call_id.clone(), name.clone()));
            }
            record_headless_event(&event, &mut stats);
            write_event(&mut stdout, event)?;
        }
        auto_deny_approvals(&mut core, &mut stdout, &mut stats, &mut pending_denials)?;
        thread::sleep(Duration::from_millis(50));
    }
    stats.status = if stats.last_error.is_some() {
        "error".to_string()
    } else {
        "ready".to_string()
    };
    write_json_value(
        &mut stdout,
        final_result_json(&stats, started.elapsed().as_millis() as u64),
    )?;
    Ok(if stats.last_error.is_some() { 1 } else { 0 })
}

/// Denies every approval request surfaced by a headless run: nobody can
/// answer them, so blocking would hang the turn. The model receives the
/// denial as the tool result and can adapt or finish.
fn auto_deny_approvals(
    core: &mut AgentCore,
    stdout: &mut impl Write,
    stats: &mut HeadlessStats,
    pending: &mut Vec<(String, String)>,
) -> io::Result<()> {
    for (call_id, name) in pending.drain(..) {
        let info = AgentEvent::Info(format!(
            "auto-denying {name} ({call_id}): approvals cannot be answered in --headless mode; rerun with --approval-mode auto-edit or full-auto to allow this class of tools"
        ));
        record_headless_event(&info, stats);
        write_event(stdout, info)?;
        for event in core.approve(&call_id, false, false, None) {
            record_headless_event(&event, stats);
            write_event(stdout, event)?;
        }
    }
    Ok(())
}

/// Persistent bidirectional protocol mode for GUI/IDE front-ends.
///
/// Reads newline-delimited JSON commands on stdin and emits the engine's
/// `AgentEvent` stream as newline-delimited JSON on stdout (same schema as
/// `--headless`). Runs until stdin closes or a `shutdown`/`/quit` command.
fn run_serve(approval_mode: Option<ApprovalMode>) -> io::Result<i32> {
    let mut core = AgentCore::new()?;
    if let Some(mode) = approval_mode {
        // Set before startup_events so the startup approval_mode event reflects it.
        let _ = core.set_approval_mode(mode);
    }
    core.start_update_check();
    let mut stdout = io::stdout();

    for event in core.startup_events() {
        write_event(&mut stdout, event)?;
    }
    // Seed dedup so the first poll loop doesn't immediately re-emit model_status.
    let mut last_status = Some(event_json(core.model_status_event()));

    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    loop {
        loop {
            match rx.try_recv() {
                Ok(line) => {
                    if handle_serve_line(&mut core, &mut stdout, &line)? {
                        return Ok(0);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(0),
            }
        }

        for event in core.poll_events() {
            write_event(&mut stdout, event)?;
        }

        let status = event_json(core.model_status_event());
        if last_status.as_ref() != Some(&status) {
            write_json_value(&mut stdout, status.clone())?;
            last_status = Some(status);
        }

        thread::sleep(Duration::from_millis(30));
    }
}

/// Dispatch one stdin command line. Returns `Ok(true)` to terminate serve mode.
fn handle_serve_line(
    core: &mut AgentCore,
    stdout: &mut impl Write,
    line: &str,
) -> io::Result<bool> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(false);
    }
    let value = match serde_json::from_str::<Value>(line) {
        Ok(value) => value,
        Err(error) => {
            jucode_agent_core::log_warn!(
                "serve",
                "failed to parse command line",
                error = error.to_string()
            );
            write_event(
                stdout,
                AgentEvent::Error(format!("invalid command: {error}")),
            )?;
            return Ok(false);
        }
    };
    let op = value.get("op").and_then(Value::as_str).unwrap_or_default();
    let events = match op {
        "user_message" => {
            let content = value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let images = value
                .get("images")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            core.submit_user_message_with_images(content.to_string(), images)
        }
        "command" => {
            let input = value
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (quit, events) = core.handle_command(input);
            for event in events {
                write_event(stdout, event)?;
            }
            return Ok(quit);
        }
        "steer" => core.steer(),
        "interrupt" => core.interrupt(),
        // Structured twin of the `/approve` text command (GUI convenience):
        // {"op":"approve","call_id":"...","decision":"allow|deny",
        //  "hunks":["f0h1"],"always":false} — routes to the same handler.
        "approve" => match parse_approve_op(&value) {
            Ok((call_id, allow, always, hunks)) => core.approve(&call_id, allow, always, hunks),
            Err(error) => vec![AgentEvent::Error(error)],
        },
        "set_approval_mode" => {
            let mode = value
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match ApprovalMode::parse(mode) {
                Ok(mode) => core.set_approval_mode(mode),
                Err(error) => vec![AgentEvent::Error(error)],
            }
        }
        "mcp_list" => vec![core.mcp_servers_event()],
        "mcp_set" => match value.get("server") {
            Some(server) => core.mcp_set(server),
            None => vec![AgentEvent::Error(
                "mcp_set requires a server object".to_string(),
            )],
        },
        "mcp_remove" => match value.get("name").and_then(Value::as_str) {
            Some(name) => core.mcp_remove(name),
            None => vec![AgentEvent::Error("mcp_remove requires name".to_string())],
        },
        "mcp_toggle" => {
            match (
                value.get("name").and_then(Value::as_str),
                value.get("enabled").and_then(Value::as_bool),
            ) {
                (Some(name), Some(enabled)) => core.mcp_toggle(name, enabled),
                _ => vec![AgentEvent::Error(
                    "mcp_toggle requires name and enabled".to_string(),
                )],
            }
        }
        "shutdown" => return Ok(true),
        other => {
            jucode_agent_core::log_warn!("serve", "unknown op", op = other);
            vec![AgentEvent::Error(format!("unknown op: {other}"))]
        }
    };
    for event in events {
        write_event(stdout, event)?;
    }
    Ok(false)
}

/// `(call_id, allow, always, hunks)` parsed from the serve `approve` op.
type ParsedApprove = (String, bool, bool, Option<Vec<String>>);

/// Parses the serve `approve` op into `(call_id, allow, always, hunks)`.
/// Combination rules (always vs hunks, unknown ids) are validated by
/// `AgentCore::approve` so text and structured paths behave identically.
fn parse_approve_op(value: &Value) -> Result<ParsedApprove, String> {
    let call_id = value
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "approve requires call_id".to_string())?;
    let allow = match value.get("decision").and_then(Value::as_str) {
        Some("allow") => true,
        Some("deny") => false,
        _ => return Err("approve requires decision: allow or deny".to_string()),
    };
    let always = value
        .get("always")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let hunks = match value.get("hunks") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            let mut ids = Vec::new();
            for item in items {
                match item.as_str().map(str::trim) {
                    Some(id) if !id.is_empty() => ids.push(id.to_string()),
                    _ => return Err("hunks must be an array of hunk id strings".to_string()),
                }
            }
            Some(ids)
        }
        Some(_) => return Err("hunks must be an array of hunk id strings".to_string()),
    };
    Ok((call_id.to_string(), allow, always, hunks))
}

fn write_event(stdout: &mut impl Write, event: AgentEvent) -> io::Result<()> {
    write_json_value(stdout, event_json(event))
}

fn write_json_value(stdout: &mut impl Write, value: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, &value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn record_headless_event(event: &AgentEvent, stats: &mut HeadlessStats) {
    let key = match event {
        AgentEvent::Startup { .. } => "startup",
        AgentEvent::ModelStatus { .. } => "model_status",
        AgentEvent::PendingMessages(_) => "pending_messages",
        AgentEvent::UserMessage(_) => "user_message",
        AgentEvent::FillInput(_) => "fill_input",
        AgentEvent::Connecting => "connecting",
        AgentEvent::CompactionStart => "compaction_start",
        AgentEvent::CompactionProgress { .. } => "compaction_progress",
        AgentEvent::CompactionEnd => "compaction_end",
        AgentEvent::CompactionFailed(_) => "compaction_failed",
        AgentEvent::ContextUsage {
            tokens,
            tokenizer,
            cost,
        } => {
            stats.context_tokens = *tokens;
            stats.context_tokenizer = Some(tokenizer.clone());
            stats.cost = *cost;
            "context_usage"
        }
        AgentEvent::ThinkingStart => "thinking_start",
        AgentEvent::ReasoningDelta(delta) => {
            stats.assistant_chars += delta.len();
            "reasoning_delta"
        }
        AgentEvent::AssistantStart => "assistant_start",
        AgentEvent::AssistantDelta(delta) => {
            stats.assistant_chars += delta.len();
            "assistant_delta"
        }
        AgentEvent::Retrying { .. } => "retrying",
        AgentEvent::ToolStart { .. } => {
            stats.tool_calls += 1;
            "tool_start"
        }
        AgentEvent::ToolUpdate { .. } => "tool_update",
        AgentEvent::ToolOutput { .. } => "tool_output",
        AgentEvent::SubagentLifecycle { .. } => {
            stats.subagent_events += 1;
            "subagent_lifecycle"
        }
        AgentEvent::Usage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            reasoning_tokens,
        } => {
            stats.input_tokens += input_tokens;
            stats.cached_input_tokens += cached_input_tokens;
            stats.output_tokens += output_tokens;
            stats.reasoning_tokens += reasoning_tokens;
            "usage"
        }
        AgentEvent::TreeView(_) => "tree_view",
        AgentEvent::ResumeView(_) => "resume_view",
        AgentEvent::CheckpointView(_) => "checkpoint_view",
        AgentEvent::McpServers { .. } => "mcp_servers",
        AgentEvent::ApprovalRequest { .. } => "approval_request",
        AgentEvent::ApprovalMode { .. } => "approval_mode",
        AgentEvent::TrustPrompt { .. } => "trust_prompt",
        AgentEvent::ModelView { .. } => "model_view",
        AgentEvent::CommandList(_) => "command_list",
        AgentEvent::Goal(_) => "goal",
        AgentEvent::Plan(_) => "plan",
        AgentEvent::Transcript(_) => "transcript",
        AgentEvent::Info(_) => "info",
        AgentEvent::Error(message) => {
            stats.last_error = Some(message.clone());
            "error"
        }
        AgentEvent::Status(message) => {
            stats.last_context_state = Some(message.clone());
            stats.status = message.clone();
            "status"
        }
    };
    *stats.event_counts.entry(key.to_string()).or_insert(0) += 1;
}

fn final_result_json(stats: &HeadlessStats, elapsed_ms: u64) -> Value {
    json!({
        "type": "final_result",
        "status": stats.status,
        "input_tokens": stats.input_tokens,
        "cached_input_tokens": stats.cached_input_tokens,
        "output_tokens": stats.output_tokens,
        "reasoning_tokens": stats.reasoning_tokens,
        "context_tokens": stats.context_tokens,
        "context_tokenizer": stats.context_tokenizer,
        "cost": stats.cost,
        "tool_calls": stats.tool_calls,
        "subagent_events": stats.subagent_events,
        "assistant_chars": stats.assistant_chars,
        "elapsed_ms": elapsed_ms,
        "last_error": stats.last_error,
        "last_context_state": stats.last_context_state,
        "event_counts": stats.event_counts,
    })
}

fn event_json(event: AgentEvent) -> Value {
    match event {
        AgentEvent::Startup {
            version,
            session_id,
            profile_dir,
            config_path,
            cwd,
            model,
            context_window,
        } => {
            json!({
                "type": "startup",
                "version": version,
                "session_id": session_id,
                "profile_dir": profile_dir,
                "config_path": config_path,
                "cwd": cwd,
                "model": model,
                "context_window": context_window
            })
        }
        AgentEvent::ModelStatus {
            provider,
            model,
            reasoning_effort,
            context_window,
            context_limit,
            max_output_tokens,
            reasoning_efforts,
            state,
        } => json!({
            "type": "model_status",
            "provider": provider,
            "model": model,
            "reasoning_effort": reasoning_effort,
            "context_window": context_window,
            "context_limit": context_limit,
            "max_output_tokens": max_output_tokens,
            "reasoning_efforts": reasoning_efforts,
            "state": state
        }),
        AgentEvent::PendingMessages(messages) => {
            json!({ "type": "pending_messages", "messages": messages })
        }
        AgentEvent::UserMessage(content) => json!({ "type": "user_message", "content": content }),
        AgentEvent::FillInput(content) => json!({ "type": "fill_input", "content": content }),
        AgentEvent::Connecting => json!({ "type": "connecting" }),
        AgentEvent::CompactionStart => json!({ "type": "compaction_start" }),
        AgentEvent::CompactionProgress { output_tokens } => {
            json!({ "type": "compaction_progress", "output_tokens": output_tokens })
        }
        AgentEvent::CompactionEnd => json!({ "type": "compaction_end" }),
        AgentEvent::CompactionFailed(error) => {
            json!({ "type": "compaction_failed", "error": error })
        }
        AgentEvent::ContextUsage {
            tokens,
            tokenizer,
            cost,
        } => {
            json!({ "type": "context_usage", "tokens": tokens, "tokenizer": tokenizer, "cost": cost })
        }
        AgentEvent::ThinkingStart => json!({ "type": "thinking_start" }),
        AgentEvent::ReasoningDelta(delta) => {
            json!({ "type": "reasoning_delta", "delta": delta })
        }
        AgentEvent::AssistantStart => json!({ "type": "assistant_start" }),
        AgentEvent::AssistantDelta(delta) => {
            json!({ "type": "assistant_delta", "delta": delta })
        }
        AgentEvent::Retrying { attempt } => json!({ "type": "retrying", "attempt": attempt }),
        AgentEvent::ToolStart { call_id, name } => {
            json!({ "type": "tool_start", "call_id": call_id, "name": name })
        }
        AgentEvent::ToolUpdate {
            call_id,
            name,
            output,
        } => {
            json!({ "type": "tool_update", "call_id": call_id, "name": name, "output": output })
        }
        AgentEvent::ToolOutput {
            call_id,
            name,
            output,
            is_error,
            ..
        } => json!({
            "type": "tool_output",
            "call_id": call_id,
            "name": name,
            "output": output,
            "is_error": is_error
        }),
        AgentEvent::SubagentLifecycle {
            path,
            status,
            message,
        } => json!({
            "type": "subagent_lifecycle",
            "path": path,
            "status": status,
            "message": message
        }),
        AgentEvent::Usage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            reasoning_tokens,
        } => {
            json!({ "type": "usage", "input_tokens": input_tokens, "cached_input_tokens": cached_input_tokens, "output_tokens": output_tokens, "reasoning_tokens": reasoning_tokens })
        }
        AgentEvent::TreeView(nodes) => json!({
            "type": "tree_view",
            "nodes": nodes.into_iter().map(|node| {
                json!({ "id": node.id, "parent_id": node.parent_id, "label": node.label, "active": node.active })
            }).collect::<Vec<_>>()
        }),
        AgentEvent::ResumeView(items) => json!({
            "type": "resume_view",
            "items": items.into_iter().map(|item| {
                json!({ "id": item.id, "label": item.label, "active": item.active })
            }).collect::<Vec<_>>()
        }),
        AgentEvent::CheckpointView(items) => json!({
            "type": "checkpoint_view",
            "items": items.into_iter().map(|item| {
                json!({ "id": item.id, "label": item.label, "detail": item.detail })
            }).collect::<Vec<_>>()
        }),
        AgentEvent::McpServers { servers } => json!({
            "type": "mcp_servers",
            "servers": servers.into_iter().map(|server| {
                let mut entry = json!({
                    "name": server.name,
                    "transport": server.transport,
                    "state": server.state,
                    "tools": server.tools.into_iter().map(|tool| {
                        json!({ "name": tool.name, "description": tool.description })
                    }).collect::<Vec<_>>(),
                });
                if let Some(error) = server.error {
                    entry["error"] = json!(error);
                }
                entry
            }).collect::<Vec<_>>()
        }),
        AgentEvent::ApprovalRequest {
            call_id,
            name,
            summary,
            subagent_id,
            hunks,
        } => json!({
            "type": "approval_request",
            "call_id": call_id,
            "name": name,
            "summary": summary,
            "subagent_id": subagent_id,
            // Edit tools: the selectable hunks of the pending change; answer
            // with the `approve` op (or /approve --hunks) to apply a subset.
            "hunks": hunks.map(|hunks| hunks.into_iter().map(|hunk| json!({
                "id": hunk.id,
                "file": hunk.file,
                "header": hunk.header,
                "lines": hunk.lines,
            })).collect::<Vec<_>>()),
        }),
        AgentEvent::ApprovalMode { mode } => json!({
            "type": "approval_mode",
            "mode": mode,
        }),
        AgentEvent::TrustPrompt { cwd, repo_root } => json!({
            "type": "trust_prompt",
            "cwd": cwd,
            "repo_root": repo_root,
        }),
        AgentEvent::ModelView {
            models,
            active_effort,
        } => json!({
            "type": "model_view",
            "active_effort": active_effort,
            "models": models.into_iter().map(|model| {
                json!({
                    "model": model.model,
                    "active": model.active,
                    "context_window": model.context_window,
                    "max_output_tokens": model.max_output_tokens,
                    "reasoning_efforts": model.reasoning_efforts
                })
            }).collect::<Vec<_>>()
        }),
        AgentEvent::CommandList(commands) => json!({
            "type": "command_list",
            "commands": commands.into_iter().map(|command| {
                json!({ "command": command.command, "marker": command.marker, "args": command.args, "description": command.description })
            }).collect::<Vec<_>>()
        }),
        AgentEvent::Goal(goal) => json!({
            "type": "goal",
            "goal": goal.map(|goal| json!({
                "objective": goal.objective,
                "status": goal.status,
                "token_budget": goal.token_budget,
                "tokens_used": goal.tokens_used,
                "time_used_seconds": goal.time_used_seconds,
                "created_at": goal.created_at,
                "updated_at": goal.updated_at,
            }))
        }),
        AgentEvent::Plan(items) => json!({
            "type": "plan",
            "plan": items.into_iter().map(|item| json!({
                "step": item.step,
                "status": item.status,
            })).collect::<Vec<_>>()
        }),
        AgentEvent::Transcript(items) => json!({
            "type": "transcript",
            "items": items.into_iter().map(|item| match item {
                jucode_agent_core::TranscriptItem::User(content) => json!({ "role": "user", "content": content }),
                jucode_agent_core::TranscriptItem::Assistant(content) => json!({ "role": "assistant", "content": content }),
                jucode_agent_core::TranscriptItem::Tool { name, output } => json!({ "role": "tool", "name": name, "output": output }),
                jucode_agent_core::TranscriptItem::Branch(label) => json!({ "role": "branch", "label": label }),
            }).collect::<Vec<_>>()
        }),
        AgentEvent::Info(message) => json!({ "type": "info", "message": message }),
        AgentEvent::Error(message) => json!({ "type": "error", "message": message }),
        AgentEvent::Status(message) => json!({ "type": "status", "message": message }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_result_contains_status_and_usage() {
        let mut stats = HeadlessStats {
            status: "ready".to_string(),
            ..Default::default()
        };
        stats.input_tokens = 12;
        stats.output_tokens = 8;
        stats.reasoning_tokens = 4;
        stats.context_tokens = 99;
        stats.tool_calls = 3;
        stats.subagent_events = 2;

        let value = final_result_json(&stats, 123);
        assert_eq!(value["type"], "final_result");
        assert_eq!(value["status"], "ready");
        assert_eq!(value["input_tokens"], 12);
        assert_eq!(value["cached_input_tokens"], 0);
        assert_eq!(value["context_tokens"], 99);
        assert_eq!(value["tool_calls"], 3);
        assert_eq!(value["elapsed_ms"], 123);
    }

    #[test]
    fn headless_defaults_to_read_only_and_honors_explicit_flag() {
        assert_eq!(headless_approval_mode(None), ApprovalMode::ReadOnly);
        assert_eq!(
            headless_approval_mode(Some(ApprovalMode::AutoEdit)),
            ApprovalMode::AutoEdit
        );
        assert_eq!(
            headless_approval_mode(Some(ApprovalMode::FullAuto)),
            ApprovalMode::FullAuto
        );
    }

    #[test]
    fn approve_op_round_trips_decision_always_and_hunks() {
        let op = json!({
            "op": "approve",
            "call_id": "call_9",
            "decision": "allow",
            "hunks": ["f0h1", "f1h2"],
            "always": false,
        });
        assert_eq!(
            parse_approve_op(&op).unwrap(),
            (
                "call_9".to_string(),
                true,
                false,
                Some(vec!["f0h1".to_string(), "f1h2".to_string()])
            )
        );

        let plain_deny = json!({ "op": "approve", "call_id": "call_9", "decision": "deny" });
        assert_eq!(
            parse_approve_op(&plain_deny).unwrap(),
            ("call_9".to_string(), false, false, None)
        );

        let always = json!({
            "op": "approve", "call_id": "call_9", "decision": "allow", "always": true,
            "hunks": null,
        });
        assert_eq!(
            parse_approve_op(&always).unwrap(),
            ("call_9".to_string(), true, true, None)
        );
    }

    #[test]
    fn approve_op_rejects_missing_fields_and_bad_hunks() {
        let missing_call = json!({ "op": "approve", "decision": "allow" });
        assert!(parse_approve_op(&missing_call)
            .unwrap_err()
            .contains("call_id"));

        let bad_decision = json!({ "op": "approve", "call_id": "c", "decision": "maybe" });
        assert!(parse_approve_op(&bad_decision)
            .unwrap_err()
            .contains("allow or deny"));

        let bad_hunks =
            json!({ "op": "approve", "call_id": "c", "decision": "allow", "hunks": [1] });
        assert!(parse_approve_op(&bad_hunks)
            .unwrap_err()
            .contains("array of hunk id strings"));

        let bad_hunks_type =
            json!({ "op": "approve", "call_id": "c", "decision": "allow", "hunks": "f0h1" });
        assert!(parse_approve_op(&bad_hunks_type)
            .unwrap_err()
            .contains("array"));
    }

    #[test]
    fn approval_request_event_serializes_hunks_for_the_wire() {
        let event = AgentEvent::ApprovalRequest {
            call_id: "call_7".to_string(),
            name: "apply_patch".to_string(),
            summary: "src/lib.rs".to_string(),
            subagent_id: None,
            hunks: Some(vec![jucode_agent_core::HunkView {
                id: "f0h1".to_string(),
                file: "src/lib.rs".to_string(),
                header: "@@ -1,3 +1,3 @@".to_string(),
                lines: vec![" a".to_string(), "-b".to_string(), "+B".to_string()],
            }]),
        };

        let value = event_json(event);
        assert_eq!(value["type"], "approval_request");
        assert_eq!(value["hunks"][0]["id"], "f0h1");
        assert_eq!(value["hunks"][0]["file"], "src/lib.rs");
        assert_eq!(value["hunks"][0]["header"], "@@ -1,3 +1,3 @@");
        assert_eq!(value["hunks"][0]["lines"][2], "+B");

        let plain = AgentEvent::ApprovalRequest {
            call_id: "call_8".to_string(),
            name: "bash".to_string(),
            summary: "ls".to_string(),
            subagent_id: None,
            hunks: None,
        };
        assert_eq!(event_json(plain)["hunks"], Value::Null);
    }
}
