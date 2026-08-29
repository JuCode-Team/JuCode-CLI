//! Vendor wire protocols and provider templates.
//!
//! This crate owns the hand-written LLM wire protocols so `agent-core` does
//! not accumulate ad-hoc HTTP clients:
//!
//! - [`responses`]: OpenAI Responses API (SSE) — used by the jucode gateway
//!   and official OpenAI.
//! - [`anthropic`]: Anthropic Messages API (SSE) — used by official Anthropic
//!   and Anthropic-compatible gateways (e.g. DeepSeek).
//! - [`chat`]: OpenAI Chat Completions API (SSE) — used by OpenAI-compatible
//!   servers such as Ollama and OpenRouter.
//!
//! Parsers read blocking SSE streams (no async runtime) and emit unified
//! [`WireEvent`]s plus Responses-style output items, which is the canonical
//! item format the agent session stores regardless of protocol. HTTP transport
//! (ureq), retries, and tool execution stay in `agent-core`.

use serde_json::Value;

pub mod anthropic;
pub mod chat;
pub mod providers;
pub mod responses;

pub use providers::{templates, ModelTemplate, ProviderTemplate};

/// Wire protocol spoken with a provider endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// OpenAI Responses API (`POST {base}/responses`).
    OpenAiResponses,
    /// Anthropic Messages API (`POST .../v1/messages`).
    AnthropicMessages,
    /// OpenAI Chat Completions API (`POST {base}/chat/completions`).
    OpenAiChatCompletions,
}

impl Protocol {
    /// Config wire name: "responses" | "anthropic" | "chat".
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAiResponses => "responses",
            Self::AnthropicMessages => "anthropic",
            Self::OpenAiChatCompletions => "chat",
        }
    }

    /// An explicit provider protocol wins; otherwise fall back to the model
    /// heuristic (claude-* → anthropic, else responses).
    pub fn resolve(protocol: &str, model: &str) -> Self {
        match protocol {
            "anthropic" => Self::AnthropicMessages,
            "responses" => Self::OpenAiResponses,
            "chat" => Self::OpenAiChatCompletions,
            _ => Self::from_model(model),
        }
    }

    fn from_model(model: &str) -> Self {
        if is_anthropic_model(model) {
            Self::AnthropicMessages
        } else {
            Self::OpenAiResponses
        }
    }
}

pub fn is_anthropic_model(model: &str) -> bool {
    model.starts_with("claude-")
}

/// Token usage in OpenAI subset semantics: `cached_input_tokens` is a subset
/// of `input_tokens`. Anthropic usage is normalized to this on parse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

/// Unified streaming event emitted by all protocol parsers. `agent-core` maps
/// these onto its own `StreamEvent` (which adds tool/retry lifecycle events).
#[derive(Debug, Clone)]
pub enum WireEvent {
    /// Streamed answer text.
    Delta(String),
    /// Streamed reasoning/thinking text (only for providers that return it).
    ReasoningDelta(String),
    /// A completed Responses-style output item (message, function_call, ...).
    ResponseItem(Value),
    /// Final token usage for the response.
    Usage(Usage),
}

/// Text of a response item's content parts, taking `preferred_type` and plain
/// "text" parts. Works on both input (`input_text`) and output (`output_text`)
/// items.
pub fn response_content_text(item: &Value, preferred_type: &str) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| {
            let part_type = part.get("type").and_then(Value::as_str).unwrap_or_default();
            if part_type == preferred_type || part_type == "text" {
                part.get("text").and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Tool-call arguments normalized so an empty string never reaches the JSON
/// parser: argument-less calls become `{}`.
pub(crate) fn normalized_arguments(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        "{}".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Splits an SSE byte stream into `data:` payloads and feeds each to
/// `handle`. `handle` returns Ok(true) to stop reading (terminal event seen);
/// whether the stream actually completed is tracked by the caller. A trailing
/// unterminated payload is still dispatched.
pub(crate) fn read_sse_data(
    reader: impl std::io::Read,
    mut handle: impl FnMut(&str) -> Result<bool, String>,
) -> Result<(), String> {
    let mut data_lines: Vec<String> = Vec::new();
    let reader = std::io::BufReader::new(reader);
    use std::io::BufRead;

    for line in reader.lines() {
        let line = line.map_err(|error| error.to_string())?;
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_string());
            continue;
        }
        if line.is_empty() && !data_lines.is_empty() {
            let data = data_lines.join("\n");
            data_lines.clear();
            if handle(&data)? {
                return Ok(());
            }
        }
    }

    if !data_lines.is_empty() {
        handle(&data_lines.join("\n"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_resolves_explicit_override_before_model_heuristic() {
        assert_eq!(
            Protocol::resolve("anthropic", "gpt-5.5"),
            Protocol::AnthropicMessages
        );
        assert_eq!(
            Protocol::resolve("responses", "claude-opus-4-8"),
            Protocol::OpenAiResponses
        );
        assert_eq!(
            Protocol::resolve("chat", "claude-opus-4-8"),
            Protocol::OpenAiChatCompletions
        );
        assert_eq!(
            Protocol::resolve("", "claude-opus-4-8"),
            Protocol::AnthropicMessages
        );
        assert_eq!(Protocol::resolve("", "gpt-5.5"), Protocol::OpenAiResponses);
    }

    #[test]
    fn protocol_wire_names_round_trip_through_resolve() {
        for protocol in [
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
            Protocol::OpenAiChatCompletions,
        ] {
            assert_eq!(Protocol::resolve(protocol.as_str(), "any-model"), protocol);
        }
    }
}
