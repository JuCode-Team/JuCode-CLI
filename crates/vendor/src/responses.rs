//! OpenAI Responses API wire protocol: SSE stream parsing and input
//! sanitizing. Output items are already in the canonical Responses format, so
//! no conversion happens on the way out.

use crate::{normalized_arguments, read_sse_data, Usage, WireEvent};
use serde_json::Value;
use std::collections::BTreeMap;

/// Parses a Responses SSE stream, emitting deltas/items/usage as [`WireEvent`]s
/// and returning the completed output items. Errors if the stream ends before
/// `response.completed` (or `response.incomplete`).
pub fn read_sse_stream(
    reader: impl std::io::Read,
    mut emit: impl FnMut(WireEvent) -> Result<(), String>,
) -> Result<Vec<Value>, String> {
    let mut output_items = Vec::new();
    let mut completed = false;
    // Accumulates streamed function-call fields keyed by the stable item id.
    // The Responses API streams a tool call's name/call_id in
    // `response.output_item.added` and its arguments via
    // `response.function_call_arguments.delta`, then may deliver the final
    // `response.output_item.done` item with those fields blanked out —
    // expecting the consumer to have stitched the pieces together.
    let mut streamed_calls: BTreeMap<String, StreamedFunctionCall> = BTreeMap::new();

    read_sse_data(reader, |data| {
        if data == "[DONE]" {
            return Ok(true);
        }
        let done = handle_sse_data(data, &mut emit, &mut output_items, &mut streamed_calls)?;
        completed = completed || done;
        Ok(done)
    })?;

    if !completed {
        return Err("stream closed before response.completed".to_string());
    }

    Ok(output_items)
}

fn handle_sse_data(
    data: &str,
    emit: &mut impl FnMut(WireEvent) -> Result<(), String>,
    output_items: &mut Vec<Value>,
    streamed_calls: &mut BTreeMap<String, StreamedFunctionCall>,
) -> Result<bool, String> {
    let event = serde_json::from_str::<Value>(data).map_err(|error| error.to_string())?;
    match event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "response.output_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                emit(WireEvent::Delta(delta.to_string()))?;
            }
        }
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                emit(WireEvent::ReasoningDelta(delta.to_string()))?;
            }
        }
        // The tool call's name + call_id (and id) are announced here, before
        // the arguments stream. Some gateways then blank these out on the
        // terminal `output_item.done`, so capture them now keyed by item id.
        "response.output_item.added" => {
            if let Some(item) = event.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        let entry = streamed_calls.entry(id.to_string()).or_default();
                        entry.merge_fields(item);
                    }
                }
            }
        }
        // Tool-call arguments stream incrementally; stitch the fragments
        // together keyed by the owning item id. The delta event also carries
        // name/call_id, so capture those too in case `added` was missed.
        "response.function_call_arguments.delta" => {
            if let Some(item_id) = event.get("item_id").and_then(Value::as_str) {
                let entry = streamed_calls.entry(item_id.to_string()).or_default();
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    entry.arguments.push_str(delta);
                }
                entry.merge_fields(&event);
            }
        }
        // The terminal arguments event carries the full string when populated.
        "response.function_call_arguments.done" => {
            if let Some(item_id) = event.get("item_id").and_then(Value::as_str) {
                let entry = streamed_calls.entry(item_id.to_string()).or_default();
                if let Some(arguments) = event.get("arguments").and_then(Value::as_str) {
                    if !arguments.is_empty() {
                        entry.arguments = arguments.to_string();
                    }
                }
                entry.merge_fields(&event);
            }
        }
        "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                let item = backfill_function_call(item.clone(), streamed_calls);
                emit(WireEvent::ResponseItem(item.clone()))?;
                output_items.push(item);
            }
        }
        // `response.incomplete` is a normal terminal event (e.g. max_output_tokens
        // reached): accept the parsed output instead of failing the stream.
        "response.completed" | "response.incomplete" => {
            if let Some(response) = event.get("response") {
                if let Some(usage) = extract_usage(response) {
                    emit(WireEvent::Usage(usage))?;
                }
            }
            // A response that streamed no per-item events still carries its
            // full output on the terminal event.
            if output_items.is_empty() {
                if let Some(items) = event
                    .get("response")
                    .and_then(|response| response.get("output"))
                    .and_then(Value::as_array)
                {
                    for item in items {
                        let item = backfill_function_call(item.clone(), streamed_calls);
                        emit(WireEvent::ResponseItem(item.clone()))?;
                        output_items.push(item);
                    }
                }
            }
            return Ok(true);
        }
        "response.failed" | "error" => {
            return Err(event.to_string());
        }
        _ => {}
    }
    Ok(false)
}

/// Fields of a tool call accumulated across Responses streaming events, keyed
/// by the stable item id. Some gateways announce a call's name/call_id in
/// `response.output_item.added` and stream its arguments separately, then send
/// the terminal `response.output_item.done` item with those fields blanked —
/// so we stitch them back together.
#[derive(Default)]
struct StreamedFunctionCall {
    name: String,
    call_id: String,
    arguments: String,
}

impl StreamedFunctionCall {
    /// Fills any not-yet-known name/call_id from an event/item, without
    /// overwriting a value already captured with a non-empty one.
    fn merge_fields(&mut self, source: &Value) {
        if self.name.is_empty() {
            if let Some(name) = source.get("name").and_then(Value::as_str) {
                if !name.is_empty() {
                    self.name = name.to_string();
                }
            }
        }
        if self.call_id.is_empty() {
            if let Some(call_id) = source.get("call_id").and_then(Value::as_str) {
                if !call_id.is_empty() {
                    self.call_id = call_id.to_string();
                }
            }
        }
    }
}

/// Backfills a streamed `function_call` item's name/call_id/arguments from the
/// fragments captured during streaming. Gateways translating Anthropic tool
/// calls to the Responses protocol can deliver the terminal item with these
/// fields empty, which previously surfaced as "unknown tool:" and
/// "EOF while parsing a value". Falls back to `{}` for genuinely argument-less
/// calls so an empty string never reaches the JSON parser.
fn backfill_function_call(
    mut item: Value,
    streamed_calls: &BTreeMap<String, StreamedFunctionCall>,
) -> Value {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return item;
    }
    let streamed = item
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| streamed_calls.get(id));

    let is_blank = |map: &serde_json::Map<String, Value>, key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .map(|value| value.trim().is_empty())
            .unwrap_or(true)
    };

    if let Value::Object(map) = &mut item {
        if is_blank(map, "name") {
            if let Some(name) = streamed
                .map(|call| call.name.as_str())
                .filter(|n| !n.is_empty())
            {
                map.insert("name".to_string(), Value::String(name.to_string()));
            }
        }
        if is_blank(map, "call_id") {
            if let Some(call_id) = streamed
                .map(|call| call.call_id.as_str())
                .filter(|c| !c.is_empty())
            {
                map.insert("call_id".to_string(), Value::String(call_id.to_string()));
            }
        }
        if is_blank(map, "arguments") {
            let accumulated = streamed.map(|call| call.arguments.as_str()).unwrap_or("");
            map.insert(
                "arguments".to_string(),
                Value::String(normalized_arguments(accumulated)),
            );
        }
    }
    item
}

/// Reads usage counters from a complete response object (streamed terminal
/// event or non-streaming JSON body).
pub fn extract_usage(value: &Value) -> Option<Usage> {
    let usage = value.get("usage")?;
    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64)?;
    let output_tokens = usage.get("output_tokens").and_then(Value::as_u64)?;
    let cached_input_tokens = usage
        .get("cached_input_tokens")
        .or_else(|| usage.get("cached_prompt_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| {
            usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    let reasoning_tokens = usage
        .get("output_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Usage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_tokens,
    })
}

/// Drops items the Responses API cannot accept as input and strips
/// service-owned metadata.
pub fn sanitize_input(input: Vec<Value>) -> Vec<Value> {
    input.into_iter().filter_map(sanitize_input_item).collect()
}

fn sanitize_input_item(mut item: Value) -> Option<Value> {
    // Anthropic-only thinking items are not valid OpenAI Responses input.
    if matches!(
        item.get("type").and_then(Value::as_str),
        Some("thinking") | Some("redacted_thinking")
    ) {
        return None;
    }
    // Response item ids are service-owned metadata. Replaying them makes the
    // request prefix less stable and differs from Codex, which does not
    // serialize ids back into Responses input. is_error is a local annotation
    // for the Anthropic conversion; the Responses API rejects unknown fields.
    if let Value::Object(map) = &mut item {
        map.remove("id");
        map.remove("is_error");
    }
    Some(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reasoning_summary_delta_emits_reasoning_event() {
        let sse = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking...\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":2},\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":4}}}}\n\n",
        );
        let mut reasoning = String::new();
        let mut usage = Usage::default();
        let _ = read_sse_stream(sse.as_bytes(), |event| {
            match event {
                WireEvent::ReasoningDelta(delta) => reasoning.push_str(&delta),
                WireEvent::Usage(seen) => usage = seen,
                _ => {}
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(reasoning, "thinking...");
        assert_eq!(usage.cached_input_tokens, 2);
        assert_eq!(usage.reasoning_tokens, 4);
    }

    #[test]
    fn streamed_tool_call_backfills_blanked_done_item() {
        // Matches the JuCode gateway's Anthropic→Responses translation: the
        // name + call_id arrive in `output_item.added`, the arguments stream
        // via `function_call_arguments.delta`, and the terminal
        // `output_item.done` blanks name/call_id/arguments — leaving only the
        // stable `id`. All three must be stitched back in.
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"tooluse_X\",\"name\":\"read\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"call_id\":\"tooluse_X\",\"name\":\"read\",\"delta\":\"{\\\"path\\\": \\\"src/store.ts\\\"}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"item_1\",\"arguments\":\"\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"\",\"name\":\"\",\"arguments\":\"\",\"status\":\"completed\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let items = read_sse_stream(sse.as_bytes(), |_| Ok(())).unwrap();
        let call = items
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("function_call item present");
        assert_eq!(call["name"], "read");
        assert_eq!(call["call_id"], "tooluse_X");
        let parsed: Value =
            serde_json::from_str(call["arguments"].as_str().unwrap()).expect("arguments parse");
        assert_eq!(parsed["path"], "src/store.ts");
    }

    #[test]
    fn argumentless_tool_call_defaults_to_empty_object() {
        // A genuinely argument-less call with no deltas must still yield "{}"
        // rather than an empty string that fails JSON parsing.
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_2\",\"call_id\":\"call_2\",\"name\":\"list_agents\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let items = read_sse_stream(sse.as_bytes(), |_| Ok(())).unwrap();
        let call = items
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("function_call item present");
        assert_eq!(call["arguments"], "{}");
    }

    #[test]
    fn incomplete_response_accepts_parsed_output() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"partial answer\"}]}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":3,\"output_tokens\":7}}}\n\n",
        );
        let mut usage_seen = false;
        let items = read_sse_stream(sse.as_bytes(), |event| {
            if matches!(event, WireEvent::Usage(_)) {
                usage_seen = true;
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert!(usage_seen);
    }

    #[test]
    fn completed_event_carries_output_when_nothing_streamed() {
        let sse = "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"whole answer\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n";
        let items = read_sse_stream(sse.as_bytes(), |_| Ok(())).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["content"][0]["text"], "whole answer");
    }

    #[test]
    fn stream_without_completed_is_an_error() {
        let error = read_sse_stream(
            "data: {\"type\":\"response.created\"}\n\n".as_bytes(),
            |_| Ok(()),
        )
        .expect_err("stream without response.completed should fail");

        assert!(error.contains("stream closed before response.completed"));
    }

    #[test]
    fn stream_error_event_surfaces_the_payload() {
        let sse =
            "data: {\"type\":\"error\",\"code\":\"invalid_api_key\",\"message\":\"bad key\"}\n\n";
        let error = read_sse_stream(sse.as_bytes(), |_| Ok(()))
            .expect_err("in-stream error event should fail");

        assert!(error.contains("invalid_api_key"));
    }

    #[test]
    fn input_sanitizer_removes_service_ids_and_anthropic_thinking() {
        let input = vec![
            json!({
                "type": "reasoning",
                "id": "rs_123",
                "summary": [],
                "encrypted_content": "enc"
            }),
            json!({
                "type": "function_call",
                "id": "fc_123",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{}"
            }),
            json!({ "type": "thinking", "thinking": "anthropic-only", "signature": "sig" }),
        ];

        let sanitized = sanitize_input(input);

        assert_eq!(sanitized.len(), 2);
        assert!(sanitized[0].get("id").is_none());
        assert_eq!(sanitized[0]["type"], "reasoning");
        assert_eq!(sanitized[0]["encrypted_content"], "enc");
        assert!(sanitized[1].get("id").is_none());
        assert_eq!(sanitized[1]["call_id"], "call_1");
    }

    #[test]
    fn sanitizer_strips_is_error_annotation() {
        let sanitized = sanitize_input(vec![json!({
            "type": "function_call_output",
            "call_id": "call_1",
            "output": "boom",
            "is_error": true
        })]);

        assert_eq!(sanitized.len(), 1);
        assert!(sanitized[0].get("is_error").is_none());
        assert_eq!(sanitized[0]["output"], "boom");
    }
}
