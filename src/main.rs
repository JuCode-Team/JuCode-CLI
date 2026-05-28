use jucode_agent_core::{AgentCore, AgentEvent};
use jucode_tui::{TuiApp, TuiRuntime};
use serde_json::{json, Value};
use std::{
    env, io,
    io::{Read, Write},
    thread,
    time::Duration,
};

struct Runtime(AgentCore);

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
        return run_headless(args);
    }
    TuiApp::new(Runtime(AgentCore::new()?)).run()
}

fn run_headless(args: Vec<String>) -> io::Result<()> {
    let mut prompt = args.join(" ");
    if prompt.trim().is_empty() {
        io::stdin().read_to_string(&mut prompt)?;
    }
    let mut core = AgentCore::new()?;
    let mut stdout = io::stdout();
    let mut done = false;
    for event in core.submit_user_message(prompt) {
        if matches!(event, AgentEvent::Error(_)) {
            done = true;
        }
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
            write_event(&mut stdout, event)?;
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn write_event(stdout: &mut impl Write, event: AgentEvent) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, &event_json(event))?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn event_json(event: AgentEvent) -> Value {
    match event {
        AgentEvent::Startup {
            version,
            profile_dir,
            config_path,
        } => {
            json!({ "type": "startup", "version": version, "profile_dir": profile_dir, "config_path": config_path })
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
        AgentEvent::ThinkingStart => json!({ "type": "thinking_start" }),
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
        } => json!({
            "type": "tool_output",
            "call_id": call_id,
            "name": name,
            "output": output,
            "is_error": is_error
        }),
        AgentEvent::Usage {
            input_tokens,
            output_tokens,
        } => {
            json!({ "type": "usage", "input_tokens": input_tokens, "output_tokens": output_tokens })
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
