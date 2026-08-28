//! OpenAI Chat Completions API wire protocol: request-side conversion from
//! Responses-style input items and SSE chunk parsing back into Responses-style
//! output items.
//!
//! This is the protocol spoken by OpenAI-compatible servers such as Ollama and
//! OpenRouter. Official OpenAI is driven over the Responses protocol instead
//! (richer reasoning support), so this path deliberately uses the widely
//! compatible `max_tokens` field rather than `max_completion_tokens`.

use crate::{normalized_arguments, read_sse_data, response_content_text, Usage, WireEvent};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Chat Completions endpoint for a configured base URL.
pub fn completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// Parameters for one streaming Chat Completions request.
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub system_prompt: &'a str,
    /// Conversation as Responses-style input items.
    pub input: &'a [Value],
    /// Responses-style tool definitions; converted to the nested chat format.
    pub tools: &'a [Value],
    pub max_output_tokens: u64,
    /// "" or "none" omits the field entirely.
    pub reasoning_effort: &'a str,
}

/// Builds a streaming Chat Completions request body.
pub fn request_body(request: &ChatRequest<'_>) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": messages_from_input(request.system_prompt, request.input),
        "max_tokens": request.max_output_tokens.max(1),
        "stream": true,
        "stream_options": { "include_usage": true }
    });
    let tools = tools_from_definitions(request.tools);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
    }
    if !matches!(request.reasoning_effort, "" | "none") {
        body["reasoning_effort"] = json!(request.reasoning_effort);
    }
    body
}

/// Converts Responses-style input items into Chat Completions messages.
/// Reasoning/thinking items are not replayable over this protocol and are
/// dropped.
pub fn messages_from_input(system_prompt: &str, input: &[Value]) -> Vec<Value> {
    let mut messages = Vec::new();
    if !system_prompt.is_empty() {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    for item in input {
        if item.get("role").and_then(Value::as_str) == Some("user") {
            messages.push(user_message(item));
            continue;
        }
        match item.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message" => {
                let text = response_content_text(item, "output_text");
                if !text.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": text }));
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
                let call = json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": normalized_arguments(arguments) }
                });
                push_tool_call(&mut messages, call);
            }
            "function_call_output" => {
                let id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id.is_empty() {
                    continue;
                }
                let output = item
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": output
                }));
            }
            _ => {}
        }
    }
    messages
}

/// A user item becomes a plain string message, or a content-part array when it
/// carries images (`input_image` parts map to `image_url` parts).
fn user_message(item: &Value) -> Value {
    let mut parts = Vec::new();
    let text = response_content_text(item, "input_text");
    if !text.is_empty() {
        parts.push(json!({ "type": "text", "text": text }));
    }
    for part in item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if part.get("type").and_then(Value::as_str) == Some("input_image") {
            if let Some(url) = part.get("image_url").and_then(Value::as_str) {
                parts.push(json!({ "type": "image_url", "image_url": { "url": url } }));
            }
        }
    }
    if parts.len() == 1 && parts[0]["type"] == "text" {
        return json!({ "role": "user", "content": text });
    }
    json!({ "role": "user", "content": parts })
}

/// Parallel calls arrive as consecutive `function_call` items; the chat format
/// wants them merged into a single assistant message's `tool_calls` array.
fn push_tool_call(messages: &mut Vec<Value>, call: Value) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(calls) = last.get_mut("tool_calls").and_then(Value::as_array_mut) {
                calls.push(call);
                return;
            }
        }
    }
    messages.push(json!({ "role": "assistant", "content": Value::Null, "tool_calls": [call] }));
}

/// Converts Responses-style flat tool definitions into the nested chat format.
pub fn tools_from_definitions(definitions: &[Value]) -> Vec<Value> {
    definitions
        .iter()
        .filter_map(|definition| {
            let name = definition.get("name")?.as_str()?;
            let mut function = json!({
                "name": name,
                "parameters": definition
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object" })),
            });
            if let Some(description) = definition.get("description") {
                function["description"] = description.clone();
            }
            Some(json!({ "type": "function", "function": function }))
        })
        .collect()
}

#[derive(Default)]
struct StreamState {
    text: String,
    tool_calls: BTreeMap<u64, ToolCall>,
    usage: Option<Usage>,
    finish_reason: String,
}

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Parses a Chat Completions SSE stream, emitting deltas as they arrive and
/// the assembled items/usage at the end. Errors if the stream ends before any
/// `finish_reason`, or if a tool call's stitched arguments are not valid JSON
/// (typically a length truncation).
pub fn read_sse_stream(
    reader: impl std::io::Read,
    mut emit: impl FnMut(WireEvent) -> Result<(), String>,
) -> Result<Vec<Value>, String> {
    let mut state = StreamState::default();

    read_sse_data(reader, |data| {
        if data == "[DONE]" {
            return Ok(true);
        }
        handle_sse_data(data, &mut emit, &mut state)?;
        Ok(false)
    })?;

    if state.finish_reason.is_empty() {
        return Err("stream closed before finish_reason".to_string());
    }

    let items = state_to_items(state, &mut emit)?;
    Ok(items)
}

fn handle_sse_data(
    data: &str,
    emit: &mut impl FnMut(WireEvent) -> Result<(), String>,
    state: &mut StreamState,
) -> Result<(), String> {
    let event = serde_json::from_str::<Value>(data).map_err(|error| error.to_string())?;
    if event.get("error").is_some() {
        return Err(event.to_string());
    }
    // The final usage chunk (stream_options.include_usage) has empty choices.
    if let Some(usage) = event.get("usage").filter(|usage| !usage.is_null()) {
        state.usage = Some(usage_from_value(usage));
    }
    let Some(choice) = event
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(());
    };
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        state.finish_reason = reason.to_string();
    }
    let Some(delta) = choice.get("delta") else {
        return Ok(());
    };
    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            state.text.push_str(content);
            emit(WireEvent::Delta(content.to_string()))?;
        }
    }
    // DeepSeek-style `reasoning_content` and OpenRouter-style `reasoning`
    // carry streamed thinking text.
    if let Some(reasoning) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(Value::as_str)
    {
        if !reasoning.is_empty() {
            emit(WireEvent::ReasoningDelta(reasoning.to_string()))?;
        }
    }
    for call in delta
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let entry = state.tool_calls.entry(index).or_default();
        if entry.id.is_empty() {
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                entry.id = id.to_string();
            }
        }
        if let Some(function) = call.get("function") {
            if entry.name.is_empty() {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    entry.name = name.to_string();
                }
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                entry.arguments.push_str(arguments);
            }
        }
    }
    Ok(())
}

fn state_to_items(
    state: StreamState,
    emit: &mut impl FnMut(WireEvent) -> Result<(), String>,
) -> Result<Vec<Value>, String> {
    let mut items = Vec::new();
    if !state.text.is_empty() {
        items.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": state.text }]
        }));
    }
    for (_, call) in state.tool_calls {
        let arguments = normalized_arguments(&call.arguments);
        if serde_json::from_str::<Value>(&arguments).is_err() {
            return Err(format!(
                "tool call '{}' arguments are not valid JSON (finish_reason: {}); dropping truncated tool call",
                call.name, state.finish_reason
            ));
        }
        items.push(json!({
            "type": "function_call",
            "call_id": call.id,
            "name": call.name,
            "arguments": arguments
        }));
    }
    for item in &items {
        emit(WireEvent::ResponseItem(item.clone()))?;
    }
    if let Some(usage) = state.usage {
        emit(WireEvent::Usage(usage))?;
    }
    Ok(items)
}

/// Converts a complete (non-streaming) chat completion body into
/// Responses-style output items plus usage.
pub fn completion_to_items(value: &Value) -> (Vec<Value>, Option<Usage>) {
    let mut items = Vec::new();
    let message = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"));
    if let Some(message) = message {
        if let Some(content) = message.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                items.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": content }]
                }));
            }
        }
        for call in message
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let function = call.get("function").unwrap_or(&Value::Null);
            items.push(json!({
                "type": "function_call",
                "call_id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                "name": function.get("name").and_then(Value::as_str).unwrap_or_default(),
                "arguments": normalized_arguments(
                    function.get("arguments").and_then(Value::as_str).unwrap_or_default()
                )
            }));
        }
    }
    let usage = value.get("usage").map(usage_from_value);
    (items, usage)
}

fn usage_from_value(usage: &Value) -> Usage {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: field("prompt_tokens"),
        cached_input_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: field("completion_tokens"),
        reasoning_tokens: usage
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_deltas_stream_and_assemble_into_a_message_item() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut deltas = String::new();
        let items = read_sse_stream(sse.as_bytes(), |event| {
            if let WireEvent::Delta(delta) = event {
                deltas.push_str(&delta);
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(deltas, "Hello");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn tool_call_chunks_stitch_into_a_function_call_item() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"pa\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"Cargo.toml\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut emitted_items = Vec::new();
        let items = read_sse_stream(sse.as_bytes(), |event| {
            if let WireEvent::ResponseItem(item) = event {
                emitted_items.push(item);
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_abc");
        assert_eq!(items[0]["name"], "read");
        let parsed: Value = serde_json::from_str(items[0]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["path"], "Cargo.toml");
        assert_eq!(emitted_items, items);
    }

    #[test]
    fn parallel_tool_calls_keep_index_order() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"ls\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let items = read_sse_stream(sse.as_bytes(), |_| Ok(())).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["call_id"], "call_a");
        assert_eq!(items[1]["call_id"], "call_b");
    }

    #[test]
    fn usage_chunk_maps_cached_and_reasoning_details() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":6},\"completion_tokens_details\":{\"reasoning_tokens\":3}}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut usage = None;
        read_sse_stream(sse.as_bytes(), |event| {
            if let WireEvent::Usage(seen) = event {
                usage = Some(seen);
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(
            usage,
            Some(Usage {
                input_tokens: 11,
                cached_input_tokens: 6,
                output_tokens: 7,
                reasoning_tokens: 3,
            })
        );
    }

    #[test]
    fn reasoning_content_delta_emits_reasoning_event() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hmm \"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"indeed\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut reasoning = String::new();
        let items = read_sse_stream(sse.as_bytes(), |event| {
            if let WireEvent::ReasoningDelta(delta) = event {
                reasoning.push_str(&delta);
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(reasoning, "hmm indeed");
        // Reasoning is not replayable over chat completions: only the answer
        // becomes an output item.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
    }

    #[test]
    fn stream_without_finish_reason_is_an_error() {
        let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
        let error =
            read_sse_stream(sse.as_bytes(), |_| Ok(())).expect_err("truncated stream should fail");
        assert!(error.contains("stream closed before finish_reason"));
    }

    #[test]
    fn truncated_tool_arguments_error_instead_of_executing() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\",\\\"content\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let error = read_sse_stream(sse.as_bytes(), |_| Ok(()))
            .expect_err("truncated tool call should fail");
        assert!(error.contains("write"));
        assert!(error.contains("length"));
    }

    #[test]
    fn in_stream_error_payload_fails_the_stream() {
        let sse = "data: {\"error\":{\"message\":\"model not found\",\"code\":404}}\n\n";
        let error = read_sse_stream(sse.as_bytes(), |_| Ok(())).expect_err("error payload");
        assert!(error.contains("model not found"));
    }

    #[test]
    fn converts_responses_context_to_chat_messages() {
        let input = vec![
            json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": "inspect" }]
            }),
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "looking" }]
            }),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{\"path\":\"Cargo.toml\"}"
            }),
            json!({
                "type": "function_call",
                "call_id": "call_2",
                "name": "ls",
                "arguments": ""
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "ok"
            }),
            json!({ "type": "reasoning", "encrypted_content": "enc" }),
            json!({ "type": "thinking", "thinking": "t", "signature": "s" }),
        ];

        let messages = messages_from_input("be brief", &input);

        // Reasoning/thinking items are dropped; everything else maps 1:1
        // except parallel function_calls, which merge into one message.
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be brief");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "inspect");
        assert_eq!(messages[2]["content"], "looking");
        let calls = messages[3]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[1]["function"]["arguments"], "{}");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "call_1");
    }

    #[test]
    fn tool_results_become_tool_role_messages() {
        let input = vec![
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "file contents"
            }),
        ];
        let messages = messages_from_input("", &input);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_1");
        assert_eq!(messages[1]["content"], "file contents");
    }

    #[test]
    fn user_images_map_to_image_url_parts() {
        let input = vec![json!({
            "role": "user",
            "content": [
                { "type": "input_text", "text": "what is this" },
                { "type": "input_image", "image_url": "data:image/png;base64,aGk=" }
            ]
        })];
        let messages = messages_from_input("", &input);

        let parts = messages[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,aGk=");
    }

    #[test]
    fn tool_definitions_convert_to_nested_chat_format() {
        let definitions = vec![
            json!({
                "type": "function",
                "name": "read",
                "description": "Read a file",
                "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
            }),
            json!({ "type": "function" }),
        ];
        let tools = tools_from_definitions(&definitions);

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "read");
        assert_eq!(tools[0]["function"]["description"], "Read a file");
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn request_body_sets_stream_usage_cap_and_optional_reasoning() {
        let tools = vec![json!({ "type": "function", "name": "read" })];
        let request = ChatRequest {
            model: "qwen3-coder:30b",
            system_prompt: "sys",
            input: &[],
            tools: &tools,
            max_output_tokens: 2048,
            reasoning_effort: "none",
        };
        let body = request_body(&request);

        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["tool_choice"], "auto");
        assert!(body.get("reasoning_effort").is_none());

        let request = ChatRequest {
            reasoning_effort: "high",
            tools: &[],
            ..request
        };
        let body = request_body(&request);
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn non_streaming_completion_converts_to_items_and_usage() {
        let completion = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hello",
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": { "name": "read", "arguments": "{\"path\":\"x\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2 }
        });
        let (items, usage) = completion_to_items(&completion);

        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call_9");
        assert_eq!(
            usage,
            Some(Usage {
                input_tokens: 5,
                cached_input_tokens: 0,
                output_tokens: 2,
                reasoning_tokens: 0,
            })
        );
    }

    #[test]
    fn completions_url_joins_base_without_double_slash() {
        assert_eq!(
            completions_url("http://127.0.0.1:11434/v1"),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
        assert_eq!(
            completions_url("https://openrouter.ai/api/v1/"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
    }
}
