//! Anthropic Messages API wire protocol: request-side conversion from
//! Responses-style input items, SSE stream parsing, and usage normalization
//! back to OpenAI subset semantics.

use crate::{read_sse_data, response_content_text, Usage, WireEvent};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

/// Version header required on Anthropic Messages requests.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Resolves the Messages endpoint from the configured base URL. A base that
/// already ends in `/v1` (any host) or targets the official Anthropic host
/// gets the plain `/v1/messages` path; other bases are treated as gateways
/// with an `/anthropic/v1` mount.
pub fn messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        return format!("{base}/messages");
    }
    if base.ends_with("/anthropic") || is_official_url(base) {
        return format!("{base}/v1/messages");
    }
    format!("{base}/anthropic/v1/messages")
}

/// True when the URL targets the official Anthropic API host.
pub fn is_official_url(url: &str) -> bool {
    url.strip_prefix("https://api.anthropic.com")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Maps a reasoning effort to an Anthropic extended-thinking token budget.
/// Returns None when reasoning should be disabled.
pub fn thinking_budget(effort: &str) -> Option<u64> {
    match effort {
        "low" => Some(4_000),
        "medium" => Some(10_000),
        "high" => Some(20_000),
        "xhigh" => Some(32_000),
        "max" => Some(64_000),
        _ => None,
    }
}

/// Converts Responses-style input items into Anthropic Messages format.
pub fn input_to_messages(input: &[Value], include_thinking: bool) -> Vec<Value> {
    let mut messages = Vec::new();
    // Calls whose arguments never parsed as JSON must not be replayed as normal
    // tool_use blocks; their tool_result is skipped too so pairs stay balanced.
    let mut skipped_call_ids: HashSet<String> = HashSet::new();
    for item in input {
        if item.get("role").and_then(Value::as_str) == Some("user") {
            let text = response_content_text(item, "input_text");
            if !text.is_empty() {
                push_content(
                    &mut messages,
                    "user",
                    json!({ "type": "text", "text": text }),
                );
            }
            for part in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if part.get("type").and_then(Value::as_str) == Some("input_image") {
                    if let Some(block) = image_block(part) {
                        push_content(&mut messages, "user", block);
                    }
                }
            }
            continue;
        }

        match item.get("type").and_then(Value::as_str).unwrap_or_default() {
            "thinking" => {
                let thinking = item
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let signature = item
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // Extended thinking requires the signature to be replayed verbatim, and
                // thinking blocks may only be sent when thinking is enabled this turn.
                if include_thinking && !signature.is_empty() {
                    push_content(
                        &mut messages,
                        "assistant",
                        json!({ "type": "thinking", "thinking": thinking, "signature": signature }),
                    );
                }
            }
            "redacted_thinking" => {
                let data = item.get("data").and_then(Value::as_str).unwrap_or_default();
                // Redacted thinking carries opaque data instead of a signature and
                // must be replayed verbatim or the API rejects the history.
                if include_thinking && !data.is_empty() {
                    push_content(
                        &mut messages,
                        "assistant",
                        json!({ "type": "redacted_thinking", "data": data }),
                    );
                }
            }
            "message" => {
                let text = response_content_text(item, "output_text");
                if !text.is_empty() {
                    push_content(
                        &mut messages,
                        "assistant",
                        json!({ "type": "text", "text": text }),
                    );
                }
            }
            "function_call" => {
                let id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                if id.is_empty() || name.is_empty() {
                    continue;
                }
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let input = if arguments.trim().is_empty() {
                    json!({})
                } else {
                    match serde_json::from_str::<Value>(arguments) {
                        Ok(input) => input,
                        Err(_) => {
                            skipped_call_ids.insert(id.to_string());
                            continue;
                        }
                    }
                };
                push_content(
                    &mut messages,
                    "assistant",
                    json!({ "type": "tool_use", "id": id, "name": name, "input": input }),
                );
            }
            "function_call_output" => {
                let id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id.is_empty() || skipped_call_ids.contains(id) {
                    continue;
                }
                let output = item
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut block =
                    json!({ "type": "tool_result", "tool_use_id": id, "content": output });
                if item.get("is_error").and_then(Value::as_bool) == Some(true) {
                    block["is_error"] = json!(true);
                }
                push_content(&mut messages, "user", block);
            }
            _ => {}
        }
    }
    messages
}

/// Converts an OpenAI-style `input_image` part (a `data:<mime>;base64,<data>`
/// URL) into an Anthropic image content block. Returns `None` if the URL is not
/// an inline base64 data URL.
fn image_block(part: &Value) -> Option<Value> {
    let url = part.get("image_url").and_then(Value::as_str)?;
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some(json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type,
            "data": data,
        },
    }))
}

fn push_content(messages: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                content.push(block);
                return;
            }
        }
    }
    messages.push(json!({ "role": role, "content": [block] }));
}

#[derive(Default)]
struct StreamState {
    output_items: Vec<Value>,
    blocks: BTreeMap<u64, Block>,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    stop_reason: String,
    /// Name of a tool_use block whose accumulated arguments never parsed as
    /// JSON (typically a max_tokens truncation). Turns the stream into an
    /// error at message_stop instead of executing a broken call.
    invalid_tool_use: Option<String>,
}

struct Block {
    kind: String,
    id: String,
    name: String,
    text: String,
    arguments: String,
    signature: String,
    /// Full tool_use input from `content_block_start`; compatible gateways may
    /// send it complete without any `input_json_delta` events.
    input: Value,
    /// Opaque payload of a `redacted_thinking` block.
    data: String,
}

/// Parses an Anthropic Messages SSE stream, emitting deltas/items/usage as
/// [`WireEvent`]s and returning Responses-style output items. Errors if the
/// stream ends before `message_stop`.
pub fn read_sse_stream(
    reader: impl std::io::Read,
    mut emit: impl FnMut(WireEvent) -> Result<(), String>,
) -> Result<Vec<Value>, String> {
    let mut state = StreamState::default();
    let mut completed = false;

    read_sse_data(reader, |data| {
        let done = handle_sse_data(data, &mut emit, &mut state)?;
        completed = completed || done;
        Ok(done)
    })?;

    if !completed {
        return Err("stream closed before message_stop".to_string());
    }

    Ok(state.output_items)
}

fn handle_sse_data(
    data: &str,
    emit: &mut impl FnMut(WireEvent) -> Result<(), String>,
    state: &mut StreamState,
) -> Result<bool, String> {
    let event = serde_json::from_str::<Value>(data).map_err(|error| error.to_string())?;
    match event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "message_start" => {
            let usage = event
                .get("message")
                .and_then(|message| message.get("usage"));
            let (input_tokens, cached_input_tokens) = normalize_usage(usage);
            state.input_tokens = input_tokens;
            state.cached_input_tokens = cached_input_tokens;
        }
        "content_block_start" => {
            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
            let block = event.get("content_block").unwrap_or(&Value::Null);
            state.blocks.insert(
                index,
                Block {
                    kind: block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    text: block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments: String::new(),
                    signature: block
                        .get("signature")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: block.get("input").cloned().unwrap_or(Value::Null),
                    data: block
                        .get("data")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
            );
        }
        "content_block_delta" => {
            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
            let delta = event.get("delta").unwrap_or(&Value::Null);
            if let Some(block) = state.blocks.get_mut(&index) {
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            block.text.push_str(text);
                            emit(WireEvent::Delta(text.to_string()))?;
                        }
                    }
                    "thinking_delta" => {
                        if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                            block.text.push_str(thinking);
                            emit(WireEvent::ReasoningDelta(thinking.to_string()))?;
                        }
                    }
                    "signature_delta" => {
                        if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                            block.signature.push_str(signature);
                        }
                    }
                    "input_json_delta" => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            block.arguments.push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
        }
        "content_block_stop" => {
            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
            if let Some(block) = state.blocks.remove(&index) {
                if block.kind == "tool_use"
                    && serde_json::from_str::<Value>(&tool_use_arguments(&block)).is_err()
                {
                    state.invalid_tool_use = Some(block.name);
                } else if let Some(item) = block_to_response_item(block) {
                    emit(WireEvent::ResponseItem(item.clone()))?;
                    state.output_items.push(item);
                }
            }
        }
        "message_delta" => {
            if let Some(stop_reason) = event
                .get("delta")
                .and_then(|delta| delta.get("stop_reason"))
                .and_then(Value::as_str)
            {
                state.stop_reason = stop_reason.to_string();
            }
            state.output_tokens = event
                .get("usage")
                .and_then(|usage| usage.get("output_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(state.output_tokens);
        }
        "message_stop" => {
            if let Some(name) = state.invalid_tool_use.take() {
                let stop_reason = if state.stop_reason.is_empty() {
                    "unknown"
                } else {
                    state.stop_reason.as_str()
                };
                return Err(format!(
                    "tool_use '{name}' arguments are not valid JSON (stop_reason: {stop_reason}); dropping truncated tool call"
                ));
            }
            emit(WireEvent::Usage(Usage {
                input_tokens: state.input_tokens,
                cached_input_tokens: state.cached_input_tokens,
                output_tokens: state.output_tokens,
                reasoning_tokens: 0,
            }))?;
            return Ok(true);
        }
        "error" => return Err(event.to_string()),
        _ => {}
    }
    Ok(false)
}

/// Normalizes Anthropic usage to OpenAI semantics: OpenAI's `cached_tokens` is
/// a subset of `input_tokens`, while Anthropic's `input_tokens` excludes both
/// `cache_read_input_tokens` and `cache_creation_input_tokens` (disjoint
/// counts). Downstream consumers subtract `cached_input_tokens` from
/// `input_tokens`, so report total = input + cache_read + cache_creation and
/// cached = cache_read. Cache-creation tokens are priced as regular input.
pub fn normalize_usage(usage: Option<&Value>) -> (u64, u64) {
    let field = |key: &str| {
        usage
            .and_then(|usage| usage.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let cache_read = field("cache_read_input_tokens");
    let total_input = field("input_tokens") + cache_read + field("cache_creation_input_tokens");
    (total_input, cache_read)
}

fn block_to_response_item(block: Block) -> Option<Value> {
    match block.kind.as_str() {
        "text" => Some(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": block.text }]
        })),
        // A thinking block can only be replayed to Anthropic with its signature,
        // so drop it when the signature is missing rather than break later turns.
        "thinking" if !block.signature.is_empty() => Some(json!({
            "type": "thinking",
            "thinking": block.text,
            "signature": block.signature
        })),
        // Redacted thinking must be preserved verbatim (via its opaque data)
        // or replaying the history is rejected by the API.
        "redacted_thinking" if !block.data.is_empty() => Some(json!({
            "type": "redacted_thinking",
            "data": block.data
        })),
        "tool_use" => {
            let arguments = tool_use_arguments(&block);
            Some(json!({
                "type": "function_call",
                "call_id": block.id,
                "name": block.name,
                "arguments": arguments
            }))
        }
        _ => None,
    }
}

/// Resolves a streamed tool_use block's arguments: streamed `input_json_delta`
/// fragments win; otherwise a complete non-empty `input` object delivered in
/// `content_block_start` (some gateways skip deltas); otherwise `{}`.
fn tool_use_arguments(block: &Block) -> String {
    let trimmed = block.arguments.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    match &block.input {
        Value::Object(map) if !map.is_empty() => block.input.to_string(),
        _ => "{}".to_string(),
    }
}

/// Converts one content block of a complete (non-streaming) Anthropic message
/// into a Responses-style output item.
pub fn content_block_to_response_item(block: &Value) -> Option<Value> {
    match block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "text" => Some(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": block.get("text").and_then(Value::as_str).unwrap_or_default() }]
        })),
        "thinking" => {
            let signature = block
                .get("signature")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if signature.is_empty() {
                None
            } else {
                Some(json!({
                    "type": "thinking",
                    "thinking": block.get("thinking").and_then(Value::as_str).unwrap_or_default(),
                    "signature": signature
                }))
            }
        }
        "redacted_thinking" => {
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if data.is_empty() {
                None
            } else {
                Some(json!({ "type": "redacted_thinking", "data": data }))
            }
        }
        "tool_use" => Some(json!({
            "type": "function_call",
            "call_id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
            "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
            "arguments": block.get("input").map(Value::to_string).unwrap_or_else(|| "{}".to_string())
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_responses_context_to_anthropic_messages() {
        let input = vec![
            json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": "inspect" }]
            }),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{\"path\":\"Cargo.toml\"}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "ok"
            }),
        ];

        let messages = input_to_messages(&input, false);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["input"]["path"], "Cargo.toml");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn replays_thinking_blocks_only_when_enabled() {
        let input = vec![
            json!({ "type": "thinking", "thinking": "reasoning", "signature": "sig-1" }),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{}"
            }),
        ];

        let with_thinking = input_to_messages(&input, true);
        assert_eq!(with_thinking[0]["content"][0]["type"], "thinking");
        assert_eq!(with_thinking[0]["content"][0]["signature"], "sig-1");
        assert_eq!(with_thinking[0]["content"][1]["type"], "tool_use");

        let without_thinking = input_to_messages(&input, false);
        assert_eq!(without_thinking[0]["content"][0]["type"], "tool_use");
    }

    #[test]
    fn invalid_function_call_arguments_skip_tool_use_and_matching_result() {
        let input = vec![
            json!({
                "type": "function_call",
                "call_id": "call_bad",
                "name": "write",
                "arguments": "{\"path\":\"a.txt\",\"content"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_bad",
                "output": "error"
            }),
            json!({
                "type": "function_call",
                "call_id": "call_ok",
                "name": "read",
                "arguments": ""
            }),
        ];

        let messages = input_to_messages(&input, false);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(messages[0]["content"][0]["id"], "call_ok");
        assert_eq!(messages[0]["content"][0]["input"], json!({}));
    }

    #[test]
    fn error_function_call_output_maps_to_is_error() {
        let input = vec![
            json!({ "type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{}" }),
            json!({ "type": "function_call_output", "call_id": "call_1", "output": "boom", "is_error": true }),
        ];

        let messages = input_to_messages(&input, false);

        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["is_error"], true);
    }

    #[test]
    fn parses_thinking_stream_into_reasoning_and_item() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"step one\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-xyz\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut reasoning = String::new();
        let items = read_sse_stream(sse.as_bytes(), |event| {
            if let WireEvent::ReasoningDelta(delta) = event {
                reasoning.push_str(&delta);
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(reasoning, "step one");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "thinking");
        assert_eq!(items[0]["signature"], "sig-xyz");
    }

    #[test]
    fn parses_tool_stream_as_response_items() {
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"read\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Cargo.toml\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut events = Vec::new();
        let items = read_sse_stream(sse.as_bytes(), |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["arguments"], "{\"path\":\"Cargo.toml\"}");
        assert!(matches!(events.last(), Some(WireEvent::Usage(_))));
    }

    #[test]
    fn truncated_tool_use_errors_instead_of_executing() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"write\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.txt\\\",\\\"content\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":9}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut items_emitted = 0;
        let error = read_sse_stream(sse.as_bytes(), |event| {
            if matches!(event, WireEvent::ResponseItem(_)) {
                items_emitted += 1;
            }
            Ok(())
        })
        .expect_err("truncated tool_use should fail");

        assert!(error.contains("write"));
        assert!(error.contains("max_tokens"));
        assert_eq!(items_emitted, 0);
    }

    #[test]
    fn tool_use_input_from_block_start_used_without_deltas() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"read\",\"input\":{\"path\":\"Cargo.toml\"}}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let items = read_sse_stream(sse.as_bytes(), |_| Ok(())).unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        let parsed: Value = serde_json::from_str(items[0]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["path"], "Cargo.toml");
    }

    #[test]
    fn redacted_thinking_is_preserved_and_replayed() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque-bytes\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let items = read_sse_stream(sse.as_bytes(), |_| Ok(())).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "redacted_thinking");
        assert_eq!(items[0]["data"], "opaque-bytes");

        // Non-streaming path preserves the block too.
        let block = json!({ "type": "redacted_thinking", "data": "opaque-bytes" });
        let item = content_block_to_response_item(&block).unwrap();
        assert_eq!(item["type"], "redacted_thinking");

        // Projection back to Anthropic messages replays it verbatim.
        let messages = input_to_messages(&items, true);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"][0]["type"], "redacted_thinking");
        assert_eq!(messages[0]["content"][0]["data"], "opaque-bytes");

        // The OpenAI path must not see Anthropic-only items.
        assert!(crate::responses::sanitize_input(items).is_empty());
    }

    #[test]
    fn usage_is_normalized_to_openai_subset_semantics() {
        // Anthropic reports input, cache_read and cache_creation disjointly;
        // normalized: input = 10 + 90 + 20 = 120, cached = 90.
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":90,\"cache_creation_input_tokens\":20}}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut usage = None;
        read_sse_stream(sse.as_bytes(), |event| {
            if let WireEvent::Usage(seen) = event {
                usage = Some((seen.input_tokens, seen.cached_input_tokens));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(usage, Some((120, 90)));

        // The non-streaming normalizer behaves identically.
        let raw = json!({
            "input_tokens": 10,
            "cache_read_input_tokens": 90,
            "cache_creation_input_tokens": 20,
            "output_tokens": 5
        });
        assert_eq!(normalize_usage(Some(&raw)), (120, 90));
    }

    #[test]
    fn builds_messages_url_for_gateway_and_official_bases() {
        assert_eq!(
            messages_url("https://api.jucode.cn"),
            "https://api.jucode.cn/anthropic/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.jucode.cn/anthropic"),
            "https://api.jucode.cn/anthropic/v1/messages"
        );
        // A base already ending in /v1 exposes the endpoint directly.
        assert_eq!(
            messages_url("https://api.jucode.cn/v1"),
            "https://api.jucode.cn/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn official_anthropic_host_is_detected_exactly() {
        assert!(is_official_url("https://api.anthropic.com"));
        assert!(is_official_url("https://api.anthropic.com/v1/messages"));
        assert!(!is_official_url(
            "https://api.jucode.cn/anthropic/v1/messages"
        ));
        assert!(!is_official_url(
            "https://api.anthropic.com.evil.example/v1/messages"
        ));
    }

    #[test]
    fn thinking_budget_scales_with_effort() {
        assert_eq!(thinking_budget("none"), None);
        assert_eq!(thinking_budget(""), None);
        assert_eq!(thinking_budget("low"), Some(4_000));
        assert_eq!(thinking_budget("max"), Some(64_000));
    }
}
