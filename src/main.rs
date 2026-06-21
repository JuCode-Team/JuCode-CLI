use jucode_agent_core::{AgentCore, AgentEvent};
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
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("--headless") {
        args.remove(0);
        let code = run_headless(args)?;
        std::process::exit(code);
    }
    if args.first().map(String::as_str) == Some("serve") {
        let code = run_serve()?;
        std::process::exit(code);
    }
    let mut core = AgentCore::new()?;
    core.start_update_check();
    TuiApp::new(Runtime(core)).run()
}

fn run_headless(args: Vec<String>) -> io::Result<i32> {
    let mut prompt = args.join(" ");
    if prompt.trim().is_empty() {
        io::stdin().read_to_string(&mut prompt)?;
    }
    let mut core = AgentCore::new()?;
    let mut stdout = io::stdout();
    let mut done = false;
    let mut stats = HeadlessStats::default();
    let started = Instant::now();
    for event in core.submit_user_message(prompt) {
        if matches!(event, AgentEvent::Error(_)) {
            done = true;
        }
        record_headless_event(&event, &mut stats);
        write_event(&mut stdout, event)?;
    }
    while !done {
        let events = core.poll_events();
        for event in events {
            if matches!(event, AgentEvent::Status(ref value) if value == "ready")
                || matches!(event, AgentEvent::Error(_))
            {
                done = true;
            }
            record_headless_event(&event, &mut stats);
            write_event(&mut stdout, event)?;
        }
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

/// Persistent bidirectional protocol mode for GUI/IDE front-ends.
///
/// Reads newline-delimited JSON commands on stdin and emits the engine's
/// `AgentEvent` stream as newline-delimited JSON on stdout (same schema as
/// `--headless`). Runs until stdin closes or a `shutdown`/`/quit` command.
fn run_serve() -> io::Result<i32> {
    let mut core = AgentCore::new()?;
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
            write_event(stdout, AgentEvent::Error(format!("invalid command: {error}")))?;
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
            let input = value.get("input").and_then(Value::as_str).unwrap_or_default();
            let (quit, events) = core.handle_command(input);
            for event in events {
                write_event(stdout, event)?;
            }
            return Ok(quit);
        }
        "steer" => core.steer(),
        "interrupt" => core.interrupt(),
        "shutdown" => return Ok(true),
        other => vec![AgentEvent::Error(format!("unknown op: {other}"))],
    };
    for event in events {
        write_event(stdout, event)?;
    }
    Ok(false)
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
        AgentEvent::TrustPrompt { .. } => "trust_prompt",
        AgentEvent::ModelView { .. } => "model_view",
        AgentEvent::CommandList(_) => "command_list",
        AgentEvent::Goal(_) => "goal",
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
            profile_dir,
            config_path,
            cwd,
            model,
            context_window,
        } => {
            json!({
                "type": "startup",
                "version": version,
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
            max_output_tokens,
            reasoning_efforts,
            state,
        } => json!({
            "type": "model_status",
            "provider": provider,
            "model": model,
            "reasoning_effort": reasoning_effort,
            "context_window": context_window,
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
                json!({ "command": command.command, "marker": command.marker })
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
}
