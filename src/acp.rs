//! `jucode acp` — Agent Client Protocol adapter over AgentCore.
//!
//! A thin, hand-rolled JSON-RPC 2.0 layer speaking newline-delimited JSON on
//! stdio (same dependency-light approach as the MCP client: serde_json plus
//! blocking I/O and one reader thread, no tokio, no protocol crate).
//!
//! What maps:
//! - `initialize`, `session/new`, `session/prompt`, `session/cancel`
//! - assistant / reasoning deltas → `session/update` message & thought chunks
//! - tool lifecycle → `tool_call` / `tool_call_update`
//! - plan updates → `plan`
//! - approval requests → `session/request_permission` round-trips
//!
//! Explicitly dropped (no ACP counterpart; requests get a JSON-RPC error,
//! events are silently skipped): `session/load` (loadSession: false),
//! session modes, client MCP server configs passed to `session/new`,
//! steer/queue status, hunk-subset approvals (whole-call allow/deny only),
//! conversation tree/resume/model pickers, and usage/context telemetry.

use jucode_agent_core::{AgentCore, AgentEvent, ApprovalMode};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
    sync::mpsc,
    thread,
    time::Duration,
};

pub const PROTOCOL_VERSION: u64 = 1;

/// JSON-RPC error codes used by the adapter.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

#[derive(Default)]
struct AcpState {
    session_counter: u64,
    active_session: Option<String>,
    /// JSON-RPC id of the in-flight `session/prompt`, answered when the turn
    /// settles (ready → end_turn, cancel → cancelled, error → JSON-RPC error).
    prompt_id: Option<Value>,
    prompt_cancelled: bool,
    /// Outstanding `session/request_permission` ids → approval call ids.
    permission_requests: HashMap<String, String>,
    permission_counter: u64,
}

pub fn run_acp(approval_mode: Option<ApprovalMode>) -> io::Result<i32> {
    let mut core = AgentCore::new()?;
    if let Some(mode) = approval_mode {
        let _ = core.set_approval_mode(mode);
    }
    let mut stdout = io::stdout();
    let mut state = AcpState::default();

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
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(message) => handle_message(&mut core, &mut state, &mut stdout, message)?,
                    Err(error) => write_line(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": null,
                            "error": {"code": -32700, "message": format!("parse error: {error}")},
                        }),
                    )?,
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
        }

        if state.prompt_id.is_some() {
            for event in core.poll_events() {
                handle_core_event(&mut core, &mut state, &mut stdout, event)?;
            }
        }
    }
}

fn handle_message(
    core: &mut AgentCore,
    state: &mut AcpState,
    stdout: &mut impl Write,
    message: Value,
) -> io::Result<()> {
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").cloned();
    match method {
        Some("initialize") => {
            if let Some(id) = id {
                write_line(stdout, &initialize_response(&id))?;
            }
        }
        Some("authenticate") => {
            if let Some(id) = id {
                write_error(
                    stdout,
                    &id,
                    INVALID_PARAMS,
                    "authentication is not required (authMethods is empty)",
                )?;
            }
        }
        Some("session/new") => {
            let Some(id) = id else { return Ok(()) };
            // The core starts with a fresh session; later calls roll to a new
            // one. Client-provided mcpServers are dropped (the agent manages
            // its own MCP config via ~/.jucode).
            if state.active_session.is_some() {
                let _ = core.handle_command("/new");
            }
            state.session_counter += 1;
            let session_id = format!("sess-{}", state.session_counter);
            state.active_session = Some(session_id.clone());
            write_line(
                stdout,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"sessionId": session_id},
                }),
            )?;
        }
        Some("session/prompt") => {
            let Some(id) = id else { return Ok(()) };
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if state.active_session.as_deref() != Some(session_id) {
                return write_error(
                    stdout,
                    &id,
                    INVALID_PARAMS,
                    &format!("unknown sessionId: {session_id}"),
                );
            }
            if state.prompt_id.is_some() {
                return write_error(
                    stdout,
                    &id,
                    INVALID_PARAMS,
                    "a prompt is already in flight for this session",
                );
            }
            let blocks = params
                .get("prompt")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let (text, images) = prompt_from_blocks(&blocks);
            if text.trim().is_empty() && images.is_empty() {
                return write_error(
                    stdout,
                    &id,
                    INVALID_PARAMS,
                    "prompt contains no usable content (only text and file:// image blocks are supported)",
                );
            }
            state.prompt_id = Some(id);
            state.prompt_cancelled = false;
            for event in core.submit_user_message_with_images(text, images) {
                handle_core_event(core, state, stdout, event)?;
            }
        }
        Some("session/cancel") => {
            // Notification: interrupt the turn and settle the in-flight
            // prompt with the "cancelled" stop reason, per spec.
            let _ = core.interrupt();
            state.prompt_cancelled = true;
            if let Some(prompt_id) = state.prompt_id.take() {
                write_line(stdout, &prompt_response(&prompt_id, "cancelled"))?;
            }
        }
        // Advertised as unsupported (loadSession: false) and explicitly
        // rejected here rather than half-implemented.
        Some("session/load") | Some("session/set_mode") | Some("session/set_model") => {
            if let Some(id) = id {
                write_error(
                    stdout,
                    &id,
                    METHOD_NOT_FOUND,
                    &format!(
                        "{} is not supported by jucode acp",
                        method.unwrap_or_default()
                    ),
                )?;
            }
        }
        Some(other) => {
            if let Some(id) = id {
                write_error(
                    stdout,
                    &id,
                    METHOD_NOT_FOUND,
                    &format!("unknown method: {other}"),
                )?;
            }
        }
        // No method: a response to one of our requests (permission).
        None => {
            let Some(id) = id.as_ref().and_then(Value::as_str) else {
                return Ok(());
            };
            let Some(call_id) = state.permission_requests.remove(id) else {
                return Ok(());
            };
            let (allow, always) = permission_outcome(&message);
            for event in core.approve(&call_id, allow, always, None) {
                handle_core_event(core, state, stdout, event)?;
            }
        }
    }
    Ok(())
}

fn handle_core_event(
    core: &mut AgentCore,
    state: &mut AcpState,
    stdout: &mut impl Write,
    event: AgentEvent,
) -> io::Result<()> {
    match &event {
        AgentEvent::ApprovalRequest {
            call_id,
            name,
            summary,
            ..
        } => {
            let Some(session_id) = state.active_session.clone() else {
                // No session to route the request to: deny instead of hanging.
                let _ = core.approve(call_id, false, false, None);
                return Ok(());
            };
            state.permission_counter += 1;
            let request_id = format!("perm-{}", state.permission_counter);
            state
                .permission_requests
                .insert(request_id.clone(), call_id.clone());
            write_line(
                stdout,
                &permission_request(&request_id, &session_id, call_id, name, summary),
            )?;
        }
        AgentEvent::Error(message) => {
            if let Some(prompt_id) = state.prompt_id.take() {
                write_error(stdout, &prompt_id, INTERNAL_ERROR, message)?;
            }
        }
        AgentEvent::Status(status) if status == "ready" => {
            if let Some(prompt_id) = state.prompt_id.take() {
                let stop_reason = if state.prompt_cancelled {
                    "cancelled"
                } else {
                    "end_turn"
                };
                write_line(stdout, &prompt_response(&prompt_id, stop_reason))?;
            }
        }
        _ => {
            if let Some(session_id) = state.active_session.as_deref() {
                if let Some(update) = session_update_for_event(session_id, &event) {
                    write_line(stdout, &update)?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn initialize_response(id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "agentCapabilities": {
                "loadSession": false,
                "promptCapabilities": {
                    "image": true,
                    "audio": false,
                    "embeddedContext": false,
                },
            },
            "authMethods": [],
        },
    })
}

fn prompt_response(id: &Value, stop_reason: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"stopReason": stop_reason},
    })
}

fn permission_request(
    request_id: &str,
    session_id: &str,
    call_id: &str,
    name: &str,
    summary: &str,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": call_id,
                "title": format!("{name}: {summary}"),
                "kind": tool_kind(name),
                "status": "pending",
            },
            "options": [
                {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "allow_always", "name": format!("Allow {name} for this session"), "kind": "allow_always"},
                {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
            ],
        },
    })
}

/// `(allow, always)` from a client's `session/request_permission` response.
/// Anything other than an explicit allow selection (cancelled outcome, error
/// response, malformed payload) denies.
pub(crate) fn permission_outcome(response: &Value) -> (bool, bool) {
    let Some(outcome) = response
        .get("result")
        .and_then(|result| result.get("outcome"))
    else {
        return (false, false);
    };
    if outcome.get("outcome").and_then(Value::as_str) != Some("selected") {
        return (false, false);
    }
    match outcome.get("optionId").and_then(Value::as_str) {
        Some("allow") => (true, false),
        Some("allow_always") => (true, true),
        _ => (false, false),
    }
}

/// Extracts `(text, image_paths)` from ACP prompt content blocks. Text blocks
/// concatenate; image blocks referencing a local `file://` uri (or a bare
/// path) attach as images. Unsupported blocks (base64 images, audio,
/// embedded resources) are dropped with a marker so the model knows.
pub(crate) fn prompt_from_blocks(blocks: &[Value]) -> (String, Vec<String>) {
    let mut parts = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            Some("image") => {
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or_default();
                let path = uri.strip_prefix("file://").unwrap_or(uri);
                if !path.is_empty() && std::path::Path::new(path).exists() {
                    images.push(path.to_string());
                } else {
                    parts.push("[unsupported image block dropped]".to_string());
                }
            }
            Some("resource_link") => {
                if let Some(uri) = block.get("uri").and_then(Value::as_str) {
                    let path = uri.strip_prefix("file://").unwrap_or(uri);
                    parts.push(format!("@{path}"));
                }
            }
            Some(other) => parts.push(format!("[unsupported {other} block dropped]")),
            None => {}
        }
    }
    (parts.join("\n\n"), images)
}

/// Maps one engine event to an ACP `session/update` notification. Events with
/// no ACP counterpart return `None` and are dropped.
pub(crate) fn session_update_for_event(session_id: &str, event: &AgentEvent) -> Option<Value> {
    let update = match event {
        AgentEvent::AssistantDelta(delta) => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": delta},
        }),
        AgentEvent::ReasoningDelta(delta) => json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": delta},
        }),
        AgentEvent::ToolStart { call_id, name } => json!({
            "sessionUpdate": "tool_call",
            "toolCallId": call_id,
            "title": name,
            "kind": tool_kind(name),
            "status": "pending",
        }),
        AgentEvent::ToolUpdate {
            call_id, output, ..
        } => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call_id,
            "status": "in_progress",
            "content": [tool_content(output)],
        }),
        AgentEvent::ToolOutput {
            call_id,
            output,
            is_error,
            ..
        } => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call_id,
            "status": if *is_error { "failed" } else { "completed" },
            "content": [tool_content(output)],
        }),
        AgentEvent::Plan(items) => json!({
            "sessionUpdate": "plan",
            "entries": items.iter().map(|item| json!({
                "content": item.step,
                "priority": "medium",
                "status": plan_status(&item.status),
            })).collect::<Vec<_>>(),
        }),
        // Everything else (queue/steer status, pickers, usage telemetry, MCP
        // state, transcripts, subagent lifecycle) has no ACP counterpart.
        _ => return None,
    };
    Some(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": session_id, "update": update},
    }))
}

fn tool_content(output: &str) -> Value {
    json!({
        "type": "content",
        "content": {"type": "text", "text": output},
    })
}

fn plan_status(status: &str) -> &'static str {
    match status {
        "completed" => "completed",
        "in_progress" => "in_progress",
        _ => "pending",
    }
}

/// ACP `ToolKind` for a jucode tool name.
pub(crate) fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "grep" | "glob" | "ls" => "read",
        "write" | "edit" | "str_replace" | "hashline_edit" | "apply_patch" => "edit",
        "bash" | "write_stdin" => "execute",
        "web_fetch" => "fetch",
        "plan" | "goal" => "think",
        _ => "other",
    }
}

fn write_error(stdout: &mut impl Write, id: &Value, code: i64, message: &str) -> io::Result<()> {
    write_line(
        stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }),
    )
}

fn write_line(stdout: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_advertises_v1_without_load_session() {
        let response = initialize_response(&json!(1));
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], 1);
        assert_eq!(
            response["result"]["agentCapabilities"]["loadSession"],
            false
        );
        assert_eq!(
            response["result"]["agentCapabilities"]["promptCapabilities"]["image"],
            true
        );
        assert_eq!(response["result"]["authMethods"], json!([]));
    }

    #[test]
    fn assistant_and_reasoning_deltas_map_to_chunks() {
        let message =
            session_update_for_event("sess-1", &AgentEvent::AssistantDelta("hello".to_string()))
                .unwrap();
        assert_eq!(message["method"], "session/update");
        assert_eq!(message["params"]["sessionId"], "sess-1");
        assert_eq!(
            message["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(message["params"]["update"]["content"]["text"], "hello");

        let thought =
            session_update_for_event("sess-1", &AgentEvent::ReasoningDelta("hmm".to_string()))
                .unwrap();
        assert_eq!(
            thought["params"]["update"]["sessionUpdate"],
            "agent_thought_chunk"
        );
    }

    #[test]
    fn tool_lifecycle_maps_to_tool_call_updates() {
        let start = session_update_for_event(
            "sess-1",
            &AgentEvent::ToolStart {
                call_id: "call_1".to_string(),
                name: "bash".to_string(),
            },
        )
        .unwrap();
        assert_eq!(start["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(start["params"]["update"]["toolCallId"], "call_1");
        assert_eq!(start["params"]["update"]["kind"], "execute");
        assert_eq!(start["params"]["update"]["status"], "pending");

        let done = session_update_for_event(
            "sess-1",
            &AgentEvent::ToolOutput {
                call_id: "call_1".to_string(),
                name: "bash".to_string(),
                output: "ok".to_string(),
                is_error: false,
            },
        )
        .unwrap();
        assert_eq!(
            done["params"]["update"]["sessionUpdate"],
            "tool_call_update"
        );
        assert_eq!(done["params"]["update"]["status"], "completed");
        assert_eq!(
            done["params"]["update"]["content"][0]["content"]["text"],
            "ok"
        );
    }

    #[test]
    fn unmappable_events_are_dropped() {
        for event in [
            AgentEvent::Connecting,
            AgentEvent::ThinkingStart,
            AgentEvent::PendingMessages(vec!["queued".to_string()]),
            AgentEvent::Status("thinking".to_string()),
        ] {
            assert!(session_update_for_event("sess-1", &event).is_none());
        }
    }

    #[test]
    fn prompt_blocks_extract_text_mentions_and_file_images() {
        let dir = std::env::temp_dir().join(format!(
            "jucode-acp-img-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("shot.png");
        std::fs::write(&image, b"png").unwrap();

        let blocks = vec![
            json!({"type": "text", "text": "fix the bug"}),
            json!({"type": "resource_link", "uri": "file:///repo/src/lib.rs"}),
            json!({"type": "image", "uri": format!("file://{}", image.display())}),
            json!({"type": "image", "data": "aGk=", "mimeType": "image/png"}),
            json!({"type": "audio", "data": "aGk="}),
        ];
        let (text, images) = prompt_from_blocks(&blocks);
        assert!(text.starts_with("fix the bug"));
        assert!(text.contains("@/repo/src/lib.rs"));
        assert!(text.contains("[unsupported image block dropped]"));
        assert!(text.contains("[unsupported audio block dropped]"));
        assert_eq!(images, vec![image.display().to_string()]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn permission_outcomes_map_to_approve_arguments() {
        let allow = json!({"id": "perm-1", "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}});
        assert_eq!(permission_outcome(&allow), (true, false));

        let always = json!({"id": "perm-1", "result": {"outcome": {"outcome": "selected", "optionId": "allow_always"}}});
        assert_eq!(permission_outcome(&always), (true, true));

        let deny = json!({"id": "perm-1", "result": {"outcome": {"outcome": "selected", "optionId": "deny"}}});
        assert_eq!(permission_outcome(&deny), (false, false));

        let cancelled = json!({"id": "perm-1", "result": {"outcome": {"outcome": "cancelled"}}});
        assert_eq!(permission_outcome(&cancelled), (false, false));

        let error = json!({"id": "perm-1", "error": {"code": -32603, "message": "boom"}});
        assert_eq!(permission_outcome(&error), (false, false));
    }

    #[test]
    fn plan_maps_to_acp_plan_entries() {
        let update = session_update_for_event(
            "sess-1",
            &AgentEvent::Plan(vec![jucode_agent_core::PlanItem {
                step: "write tests".to_string(),
                status: "in_progress".to_string(),
            }]),
        )
        .unwrap();
        assert_eq!(update["params"]["update"]["sessionUpdate"], "plan");
        assert_eq!(
            update["params"]["update"]["entries"][0]["content"],
            "write tests"
        );
        assert_eq!(
            update["params"]["update"]["entries"][0]["status"],
            "in_progress"
        );
    }

    #[test]
    fn tool_kinds_cover_the_builtin_tools() {
        assert_eq!(tool_kind("read"), "read");
        assert_eq!(tool_kind("apply_patch"), "edit");
        assert_eq!(tool_kind("bash"), "execute");
        assert_eq!(tool_kind("web_fetch"), "fetch");
        assert_eq!(tool_kind("spawn_agent"), "other");
    }
}
