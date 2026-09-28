//! The JSON wire format shared by `jucode serve` and the daemon: engine
//! events serialize to `{"type": ...}` objects and client ops arrive as
//! `{"op": ...}` objects. See `docs/serve-protocol.md`.

use crate::{event::AgentEvent, AgentCore, ApprovalMode};
use serde_json::{json, Value};

/// Wire protocol version, announced in the `hello` event. Clients refuse to
/// talk to a different version instead of guessing at field meanings.
pub const PROTOCOL_VERSION: u64 = 2;

/// First frame on every connection: the protocol version and the engine
/// build, so a client can refuse a mismatch before reading anything else.
pub fn hello_json(version: &str) -> Value {
    json!({ "type": "hello", "protocol": PROTOCOL_VERSION, "version": version })
}

/// An engine event on the wire, tagged with the session it belongs to so
/// one connection can carry several sessions.
pub fn session_event_json(session: &str, event: AgentEvent) -> Value {
    let mut value = event_json(event);
    value["session"] = json!(session);
    value
}

/// Applies one parsed client op to the engine. Returns `(quit, events)`;
/// `quit` is set by `shutdown` and `/quit`.
pub fn apply_op(core: &mut AgentCore, value: &Value) -> (bool, Vec<AgentEvent>) {
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
            return core.handle_command(input);
        }
        "steer" => core.steer(),
        "interrupt" => core.interrupt(),
        // Structured twin of the `/approve` text command (GUI convenience):
        // {"op":"approve","call_id":"...","decision":"allow|deny",
        //  "hunks":["f0h1"],"always":false} — routes to the same handler.
        "approve" => match parse_approve_op(value) {
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
        "set_attended" => match value.get("attended").and_then(Value::as_bool) {
            Some(attended) => core.set_attended(attended),
            None => vec![AgentEvent::Error(
                "set_attended requires attended: true or false".to_string(),
            )],
        },
        // The action id travels as `action`: `id` is a client's request id.
        "decide_action" => match (
            value.get("action").and_then(Value::as_str),
            value.get("decision").and_then(Value::as_str),
        ) {
            (Some(id), Some("allow")) => core.decide_action(id, true),
            (Some(id), Some("deny")) => core.decide_action(id, false),
            _ => vec![AgentEvent::Error(
                "decide_action requires action and decision: allow or deny".to_string(),
            )],
        },
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
        "shutdown" => return (true, Vec::new()),
        other => {
            crate::log_warn!("serve", "unknown op", op = other);
            vec![AgentEvent::Error(format!("unknown op: {other}"))]
        }
    };
    (false, events)
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

pub fn event_json(event: AgentEvent) -> Value {
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
        AgentEvent::LoginPicker(providers) => json!({
            "type": "login_picker",
            "providers": providers.into_iter().map(|p| json!({
                "id": p.id,
                "label": p.label,
                "detail": p.detail,
                "active": p.active,
                "wants_key": p.wants_key
            })).collect::<Vec<_>>()
        }),
        AgentEvent::LoginPastePrompt { provider } => json!({
            "type": "login_paste_prompt",
            "provider": provider
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
                crate::TranscriptItem::User(content) => json!({ "role": "user", "content": content }),
                crate::TranscriptItem::Assistant(content) => json!({ "role": "assistant", "content": content }),
                crate::TranscriptItem::Tool { name, output } => json!({ "role": "tool", "name": name, "output": output }),
                crate::TranscriptItem::Branch(label) => json!({ "role": "branch", "label": label }),
            }).collect::<Vec<_>>()
        }),
        AgentEvent::Info(message) => json!({ "type": "info", "message": message }),
        AgentEvent::Error(message) => json!({ "type": "error", "message": message }),
        AgentEvent::Status(message) => json!({ "type": "status", "message": message }),
        AgentEvent::ActionDeferred(action) => {
            let mut value = action.to_json();
            value["type"] = json!("action_deferred");
            value
        }
        AgentEvent::ActionDecided {
            id,
            allow,
            output,
            is_error,
        } => json!({
            "type": "action_decided",
            "id": id,
            "decision": if allow { "allow" } else { "deny" },
            "output": output,
            "is_error": is_error,
        }),
        AgentEvent::Attended(attended) => json!({ "type": "attended", "attended": attended }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            hunks: Some(vec![crate::HunkView {
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
