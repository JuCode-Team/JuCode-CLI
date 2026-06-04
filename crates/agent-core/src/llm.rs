use crate::{extensions::ExtensionRegistry, session::extract_response_text, tools};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env,
    io::{BufRead, BufReader},
    path::Path,
    sync::mpsc::{self, Sender},
    time::Duration,
};

const MAX_SUBAGENT_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    OpenAiResponses,
    AnthropicMessages,
}

pub struct OpenAiClient {
    api_key: String,
    pub model: String,
    reasoning_effort: String,
    system_prompt: String,
    extensions: ExtensionRegistry,
    base_url: String,
    max_output_tokens: u64,
    retry_attempts: usize,
    connect_timeout: Duration,
    read_timeout: Duration,
    allow_subagents: bool,
    provider_kind: ProviderKind,
    goal_tool_tx: Option<Sender<GoalToolRequest>>,
}

pub struct OpenAiClientConfig<'a> {
    pub model: String,
    pub reasoning_effort: String,
    pub system_prompt: String,
    pub extensions: ExtensionRegistry,
    pub base_url: String,
    pub max_output_tokens: u64,
    pub api_key: Option<&'a str>,
    pub api_key_env: &'a str,
    pub retry_attempts: usize,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub goal_tool_tx: Option<Sender<GoalToolRequest>>,
}

#[derive(Debug)]
pub struct GoalToolRequest {
    pub name: String,
    pub arguments: String,
    pub response_tx: Sender<ToolGoalResponse>,
}

#[derive(Debug)]
pub struct ToolGoalResponse {
    pub output: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// HTTP request is being sent; the connection is being established.
    CallStart,
    /// Response headers received; the model is now working (reasoning/answering).
    Connected,
    /// Streamed reasoning/thinking text (only for providers that return it).
    ReasoningDelta(String),
    Delta(String),
    Retrying {
        attempt: usize,
    },
    ResponseItem(Value),
    ToolStart {
        call_id: String,
        name: String,
    },
    ToolUpdate {
        call_id: String,
        name: String,
        output: String,
    },
    ToolOutput {
        call_id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
    },
}

impl OpenAiClient {
    pub fn from_config(config: OpenAiClientConfig<'_>) -> Result<Self, String> {
        let api_key = match config.api_key {
            Some(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => env::var(config.api_key_env).map_err(|_| {
                format!(
                    "api_key is not set and {} is not set. Configure one before sending prompts.",
                    config.api_key_env
                )
            })?,
        };
        let provider_kind = ProviderKind::from_model(&config.model);
        Ok(Self {
            api_key,
            model: config.model,
            reasoning_effort: config.reasoning_effort,
            system_prompt: config.system_prompt,
            extensions: config.extensions,
            base_url: config.base_url,
            max_output_tokens: config.max_output_tokens,
            retry_attempts: config.retry_attempts,
            connect_timeout: config.connect_timeout,
            read_timeout: config.read_timeout,
            allow_subagents: true,
            provider_kind,
            goal_tool_tx: config.goal_tool_tx,
        })
    }

    pub fn run_turn_events(
        &self,
        mut input: Vec<Value>,
        cwd: &Path,
        mut emit: impl FnMut(StreamEvent) -> Result<(), String>,
    ) -> Result<(), String> {
        loop {
            emit(StreamEvent::CallStart)?;
            let output_items = match self.provider_kind {
                ProviderKind::OpenAiResponses => {
                    self.create_response_streaming(input.clone(), &mut emit)?
                }
                ProviderKind::AnthropicMessages => {
                    self.create_anthropic_message_streaming(input.clone(), &mut emit)?
                }
            };
            let mut function_calls = Vec::new();

            for item in &output_items {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    function_calls.push(item.clone());
                }
                input.push(item.clone());
            }

            if function_calls.is_empty() {
                return Ok(());
            }

            for call in function_calls {
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let call_id = call
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("tool call {name} is missing call_id"))?
                    .to_string();
                let arguments = call
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                emit(StreamEvent::ToolStart {
                    call_id: call_id.clone(),
                    name: name.clone(),
                })?;
                let result = if let Some(result) = self.run_goal_tool(&name, arguments) {
                    result
                } else if name == "spawn_subagent" && self.allow_subagents {
                    self.run_subagent(arguments, cwd)
                } else {
                    tools::run_tool_with_events(&name, arguments, cwd, |event| {
                        let tools::ToolExecutionEvent::Update(output) = event;
                        emit(StreamEvent::ToolUpdate {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            output,
                        })
                    })
                };
                let result = if result.is_error && result.output.contains("unknown tool") {
                    match self.extensions.run_tool(&name, arguments, cwd) {
                        Some((output, is_error)) => tools::ToolExecutionResult {
                            model_output: tools::project_model_output(&name, &output, cwd),
                            output,
                            is_error,
                        },
                        None => result,
                    }
                } else {
                    result
                };
                let output = result.output;
                let model_output = result.model_output;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": model_output
                }));
                emit(StreamEvent::ToolOutput {
                    call_id,
                    name,
                    output,
                    is_error: result.is_error,
                })?;
            }
        }
    }

    /// One-shot summarization used for context compaction. No tools, no thinking;
    /// returns the summary text. Errors (including empty output) let the caller fall
    /// back to sending the full context.
    pub fn summarize_with_progress(
        &self,
        conversation: &str,
        mut emit_output_tokens: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<String, String> {
        self.summarize_text(
            "You compress earlier conversation history so it can replace the raw turns while letting the work continue. Write a dense summary that preserves: the user's goals and explicit requests, decisions made, important facts and constraints discovered, files and tools touched with their outcomes, and any unfinished threads. Prefer tight prose or bullet points. Output only the summary.",
            &format!("Summarize this earlier conversation:\n\n{conversation}"),
            &mut emit_output_tokens,
        )
    }

    pub fn summarize_text(
        &self,
        system: &str,
        user: &str,
        mut emit_output_tokens: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<String, String> {
        let mut output_tokens = 0u64;
        let mut record_delta = |delta: &str| {
            output_tokens = output_tokens.saturating_add(estimate_text_tokens(delta));
            emit_output_tokens(output_tokens)
        };
        let summary = match self.provider_kind {
            ProviderKind::OpenAiResponses => {
                let body = json!({
                    "model": self.model,
                    "instructions": system,
                    "reasoning": { "effort": self.reasoning_effort },
                    "input": [{ "role": "user", "content": [{ "type": "input_text", "text": user }] }],
                    "stream": true
                });
                let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
                let response = self.send_with_retry(&url, &body, &mut |_| Ok(()))?;
                self.collect_text(response, false, &mut record_delta)?
            }
            ProviderKind::AnthropicMessages => {
                let body = json!({
                    "model": self.model,
                    "system": system,
                    "max_tokens": self.max_output_tokens.max(1),
                    "messages": [{ "role": "user", "content": [{ "type": "text", "text": user }] }],
                    "stream": true
                });
                let url = anthropic_messages_url(&self.base_url);
                let response = self.send_with_retry(&url, &body, &mut |_| Ok(()))?;
                self.collect_text(response, true, &mut record_delta)?
            }
        };
        let summary = summary.trim().to_string();
        if summary.is_empty() {
            return Err("summarization produced no output".to_string());
        }
        Ok(summary)
    }

    fn collect_text(
        &self,
        response: ureq::Response,
        anthropic: bool,
        emit_text: &mut impl FnMut(&str) -> Result<(), String>,
    ) -> Result<String, String> {
        let content_type = response
            .header("content-type")
            .unwrap_or_default()
            .to_string();
        let mut text = String::new();
        let mut accumulate = |event: StreamEvent| {
            if let StreamEvent::Delta(delta) = event {
                emit_text(&delta)?;
                text.push_str(&delta);
            }
            Ok(())
        };
        if content_type.contains("application/json") {
            let body = response.into_string().map_err(|error| error.to_string())?;
            let value = serde_json::from_str::<Value>(&body).map_err(|error| error.to_string())?;
            if anthropic {
                emit_anthropic_message(&value, &mut accumulate)?;
            } else if let Some(items) = value.get("output").and_then(Value::as_array) {
                for item in items {
                    let delta = extract_response_text(item);
                    if !delta.is_empty() {
                        emit_text(&delta)?;
                        text.push_str(&delta);
                    }
                }
            }
            return Ok(text);
        }
        if anthropic {
            read_anthropic_sse_output(response.into_reader(), accumulate)?;
        } else {
            read_sse_output(response.into_reader(), accumulate)?;
        }
        Ok(text)
    }

    fn create_response_streaming(
        &self,
        input: Vec<Value>,
        mut emit: impl FnMut(StreamEvent) -> Result<(), String>,
    ) -> Result<Vec<Value>, String> {
        // Request a reasoning summary so the thinking phase can stream content.
        // With effort "none" the model does not reason, so no summary is requested.
        let reasoning = if self.reasoning_effort == "none" {
            json!({ "effort": self.reasoning_effort })
        } else {
            json!({ "effort": self.reasoning_effort, "summary": "auto" })
        };
        let body = json!({
            "model": self.model,
            "instructions": self.system_prompt,
            "reasoning": reasoning,
            "input": sanitize_openai_input(input),
            "tools": self.tool_definitions(),
            "tool_choice": "auto",
            "stream": true
        });

        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let max_attempts = self.retry_attempts.saturating_add(1).max(1);
        for attempt in 1..=max_attempts {
            let response = self.send_with_retry(&url, &body, &mut emit)?;
            emit(StreamEvent::Connected)?;
            let content_type = response
                .header("content-type")
                .unwrap_or_default()
                .to_string();
            if !content_type.contains("text/event-stream")
                && !content_type.contains("application/json")
            {
                let body = response.into_string().map_err(|error| error.to_string())?;
                let snippet = truncate_error_body(&body);
                return Err(format!(
                    "OpenAI API returned non-JSON response from {url} (content-type: {content_type}). Check base_url; OpenAI-compatible endpoints usually end with /v1. Body starts: {snippet}"
                ));
            }
            if content_type.contains("application/json") {
                let body = response.into_string().map_err(|error| error.to_string())?;
                let value =
                    serde_json::from_str::<Value>(&body).map_err(|error| error.to_string())?;
                let output_items = value
                    .get("output")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for item in &output_items {
                    let text = extract_response_text(item);
                    if !text.is_empty() {
                        emit(StreamEvent::Delta(text))?;
                    }
                    emit(StreamEvent::ResponseItem(item.clone()))?;
                }
                if let Some((input_tokens, output_tokens, reasoning_tokens)) = extract_usage(&value)
                {
                    emit(StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                        reasoning_tokens,
                    })?;
                }
                return Ok(output_items);
            }
            // Stream live text but buffer session-mutating events so a mid-stream
            // failure can be retried (re-read from scratch) without duplicating
            // response items in the session.
            let mut buffered: Vec<StreamEvent> = Vec::new();
            let read = read_sse_output(response.into_reader(), |event| match event {
                StreamEvent::Delta(_) | StreamEvent::ReasoningDelta(_) => emit(event),
                other => {
                    buffered.push(other);
                    Ok(())
                }
            });
            match read {
                Ok(output_items) => {
                    for event in buffered {
                        emit(event)?;
                    }
                    return Ok(output_items);
                }
                Err(error) if attempt < max_attempts && is_stream_decode_error(&error) => {
                    emit(StreamEvent::Retrying {
                        attempt: attempt + 1,
                    })?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("streaming retry loop always returns")
    }

    fn create_anthropic_message_streaming(
        &self,
        input: Vec<Value>,
        mut emit: impl FnMut(StreamEvent) -> Result<(), String>,
    ) -> Result<Vec<Value>, String> {
        // Cap the thinking budget so it stays below max_tokens.
        let thinking_budget = anthropic_thinking_budget(&self.reasoning_effort)
            .map(|budget| budget.min(self.max_output_tokens.saturating_sub(1024).max(1024)));
        let mut body = json!({
            "model": self.model,
            "system": self.system_prompt,
            "max_tokens": self.max_output_tokens.max(1),
            "messages": responses_input_to_anthropic_messages(&input, thinking_budget.is_some()),
            "tools": self.anthropic_tool_definitions(),
            "tool_choice": { "type": "auto" },
            "stream": true
        });
        if let Some(budget) = thinking_budget {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }

        let url = anthropic_messages_url(&self.base_url);
        let max_attempts = self.retry_attempts.saturating_add(1).max(1);
        for attempt in 1..=max_attempts {
            let response = self.send_with_retry(&url, &body, &mut emit)?;
            emit(StreamEvent::Connected)?;
            let content_type = response
                .header("content-type")
                .unwrap_or_default()
                .to_string();
            if !content_type.contains("text/event-stream")
                && !content_type.contains("application/json")
            {
                let body = response.into_string().map_err(|error| error.to_string())?;
                let snippet = truncate_error_body(&body);
                return Err(format!(
                    "Anthropic API returned non-JSON response from {url} (content-type: {content_type}). Body starts: {snippet}"
                ));
            }
            if content_type.contains("application/json") {
                let body = response.into_string().map_err(|error| error.to_string())?;
                let value =
                    serde_json::from_str::<Value>(&body).map_err(|error| error.to_string())?;
                return emit_anthropic_message(&value, &mut emit);
            }
            let mut buffered: Vec<StreamEvent> = Vec::new();
            let read = read_anthropic_sse_output(response.into_reader(), |event| match event {
                StreamEvent::Delta(_) | StreamEvent::ReasoningDelta(_) => emit(event),
                other => {
                    buffered.push(other);
                    Ok(())
                }
            });
            match read {
                Ok(output_items) => {
                    for event in buffered {
                        emit(event)?;
                    }
                    return Ok(output_items);
                }
                Err(error) if attempt < max_attempts && is_stream_decode_error(&error) => {
                    emit(StreamEvent::Retrying {
                        attempt: attempt + 1,
                    })?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("streaming retry loop always returns")
    }

    /// Send the request with the configured timeouts, retrying on transport errors
    /// and 5xx responses. 4xx responses are returned immediately without retry.
    fn send_with_retry(
        &self,
        url: &str,
        body: &Value,
        emit: &mut impl FnMut(StreamEvent) -> Result<(), String>,
    ) -> Result<ureq::Response, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(self.connect_timeout)
            .timeout_read(self.read_timeout)
            .build();
        let max_attempts = self.retry_attempts.saturating_add(1).max(1);
        for attempt in 1..=max_attempts {
            let result = agent
                .post(url)
                .set("Authorization", &format!("Bearer {}", self.api_key))
                .set("Accept", "text/event-stream")
                .set("Content-Type", "application/json")
                .send_json(body.clone());
            match result {
                Ok(response) => return Ok(response),
                Err(error) if attempt < max_attempts && is_retryable_error(&error) => {
                    emit(StreamEvent::Retrying {
                        attempt: attempt + 1,
                    })?;
                }
                Err(error) => return handle_response_error(error),
            }
        }
        unreachable!("retry loop always returns a response or error")
    }

    fn tool_definitions(&self) -> Vec<Value> {
        let mut definitions = tools::definitions();
        if self.allow_subagents {
            definitions.push(subagent_definition());
        }
        definitions.extend(self.extensions.definitions());
        if self.goal_tool_tx.is_some() {
            definitions.extend(goal_tool_definitions());
        }
        definitions
    }

    fn anthropic_tool_definitions(&self) -> Vec<Value> {
        self.tool_definitions()
            .into_iter()
            .filter_map(|definition| {
                let name = definition.get("name")?.clone();
                let mut tool = json!({
                    "name": name,
                    "input_schema": definition.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                });
                if let Some(description) = definition.get("description") {
                    tool["description"] = description.clone();
                }
                Some(tool)
            })
            .collect()
    }

    fn run_subagent(&self, arguments: &str, cwd: &Path) -> tools::ToolExecutionResult {
        let args = serde_json::from_str::<Value>(arguments)
            .unwrap_or_else(|error| json!({ "error": format!("invalid JSON arguments: {error}") }));
        let Some(task) = args
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            let output =
                json!({ "success": false, "error": "spawn_subagent requires task" }).to_string();
            return tools::ToolExecutionResult {
                model_output: output.clone(),
                output,
                is_error: true,
            };
        };
        let model = args
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&self.model);
        let system = args
            .get("system")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_SUBAGENT_SYSTEM);

        let child = OpenAiClient {
            api_key: self.api_key.clone(),
            model: model.to_string(),
            reasoning_effort: self.reasoning_effort.clone(),
            system_prompt: system.to_string(),
            extensions: self.extensions.clone(),
            base_url: self.base_url.clone(),
            max_output_tokens: self.max_output_tokens,
            retry_attempts: self.retry_attempts,
            connect_timeout: self.connect_timeout,
            read_timeout: self.read_timeout,
            allow_subagents: false,
            provider_kind: ProviderKind::from_model(model),
            goal_tool_tx: None,
        };
        let input = vec![json!({
            "role": "user",
            "content": [{ "type": "input_text", "text": task }]
        })];
        let mut output_text = String::new();
        let mut tool_iters = 0u64;
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let result = child.run_turn_events(input, cwd, |event| {
            match event {
                StreamEvent::Delta(delta) => output_text.push_str(&delta),
                StreamEvent::ToolOutput { .. } => tool_iters += 1,
                StreamEvent::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    ..
                } => {
                    input_tokens += input;
                    output_tokens += output;
                }
                _ => {}
            }
            Ok(())
        });

        let value = match result {
            Ok(()) => json!({
                "success": true,
                "output": truncate_subagent_output(&output_text),
                "tool_iters": tool_iters,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "model": model,
            }),
            Err(error) => json!({
                "success": false,
                "error": error,
                "partial_output": truncate_subagent_output(&output_text),
                "tool_iters": tool_iters,
                "model": model,
            }),
        };
        let output = value.to_string();
        tools::ToolExecutionResult {
            model_output: output.clone(),
            is_error: value.get("success").and_then(Value::as_bool) != Some(true),
            output,
        }
    }

    fn run_goal_tool(&self, name: &str, arguments: &str) -> Option<tools::ToolExecutionResult> {
        if !matches!(name, "get_goal" | "create_goal" | "update_goal") {
            return None;
        }
        let Some(tx) = &self.goal_tool_tx else {
            return None;
        };
        let (response_tx, response_rx) = mpsc::channel();
        if tx
            .send(GoalToolRequest {
                name: name.to_string(),
                arguments: arguments.to_string(),
                response_tx,
            })
            .is_err()
        {
            let output = json!({ "error": "goal tool handler is unavailable" }).to_string();
            return Some(tools::ToolExecutionResult {
                output: output.clone(),
                model_output: output,
                is_error: true,
            });
        }
        let response = response_rx.recv().unwrap_or_else(|error| ToolGoalResponse {
            output: json!({ "error": error.to_string() }).to_string(),
            is_error: true,
        });
        Some(tools::ToolExecutionResult {
            model_output: response.output.clone(),
            output: response.output,
            is_error: response.is_error,
        })
    }
}

impl ProviderKind {
    fn from_model(model: &str) -> Self {
        if is_anthropic_model(model) {
            Self::AnthropicMessages
        } else {
            Self::OpenAiResponses
        }
    }
}

const DEFAULT_SUBAGENT_SYSTEM: &str = "You are a lightweight JuCode subagent. Work on the single task given by the parent agent. Use tools when helpful. Return one concise, self-contained answer with the evidence the parent needs. Do not ask follow-up questions or spawn another subagent.";

fn subagent_definition() -> Value {
    json!({
        "type": "function",
        "name": "spawn_subagent",
        "description": "Spawn one isolated lightweight subagent for an independent coding subtask. Prefer direct tools for small checks; use this for parallel research or when many reads/searches would pollute the main context. The parent sees only the final distilled answer.",
        "parameters": {
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Self-contained task. The subagent only sees this task plus its system prompt."
                },
                "system": {
                    "type": "string",
                    "description": "Optional short system override. Keep it focused; omit for the default generic coding subagent."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override. Defaults to the parent model."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }
    })
}

fn goal_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": "get_goal",
            "description": "Get the current goal for this session, including status, token budget, token usage, and elapsed time.",
            "parameters": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "create_goal",
            "description": "Create a goal only when explicitly requested. Fails if a goal already exists.",
            "parameters": {
                "type": "object",
                "properties": {
                    "objective": { "type": "string", "description": "Concrete objective to pursue." },
                    "token_budget": { "type": "number", "description": "Optional positive token budget." }
                },
                "required": ["objective"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "update_goal",
            "description": "Mark the existing goal complete or blocked. Do not use this for pause, resume, budget-limited, or usage-limited status changes.",
            "parameters": {
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["complete", "blocked"],
                        "description": "Set complete only when all required work is done; set blocked only when progress genuinely cannot continue."
                    }
                },
                "required": ["status"],
                "additionalProperties": false
            }
        }),
    ]
}

fn truncate_subagent_output(value: &str) -> String {
    if value.len() <= MAX_SUBAGENT_OUTPUT_BYTES {
        return value.to_string();
    }
    let mut end = MAX_SUBAGENT_OUTPUT_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[...subagent output truncated {} bytes...]",
        &value[..end],
        value.len().saturating_sub(end)
    )
}

fn handle_response_error<T>(error: ureq::Error) -> Result<T, String> {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response
                .into_string()
                .unwrap_or_else(|_| "<failed to read error body>".to_string());
            Err(format!("LLM API returned HTTP {code}: {body}"))
        }
        error => Err(error.to_string()),
    }
}

fn estimate_text_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let chars = text.chars().count() as u64;
    u64::max(1, chars.div_ceil(4))
}

fn anthropic_messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/anthropic") {
        return format!("{base}/v1/messages");
    }
    if let Some(root) = base.strip_suffix("/v1") {
        format!("{root}/anthropic/v1/messages")
    } else {
        format!("{base}/anthropic/v1/messages")
    }
}

fn is_anthropic_model(model: &str) -> bool {
    model.starts_with("claude-")
}

fn responses_input_to_anthropic_messages(input: &[Value], include_thinking: bool) -> Vec<Value> {
    let mut messages = Vec::new();
    for item in input {
        if item.get("role").and_then(Value::as_str) == Some("user") {
            let text = response_content_text(item, "input_text");
            if !text.is_empty() {
                push_anthropic_content(
                    &mut messages,
                    "user",
                    json!({ "type": "text", "text": text }),
                );
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
                    push_anthropic_content(
                        &mut messages,
                        "assistant",
                        json!({ "type": "thinking", "thinking": thinking, "signature": signature }),
                    );
                }
            }
            "message" => {
                let text = response_content_text(item, "output_text");
                if !text.is_empty() {
                    push_anthropic_content(
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
                let input = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                    .unwrap_or_else(|| json!({}));
                push_anthropic_content(
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
                if id.is_empty() {
                    continue;
                }
                let output = item
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                push_anthropic_content(
                    &mut messages,
                    "user",
                    json!({ "type": "tool_result", "tool_use_id": id, "content": output }),
                );
            }
            _ => {}
        }
    }
    messages
}

fn push_anthropic_content(messages: &mut Vec<Value>, role: &str, block: Value) {
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

fn response_content_text(item: &Value, preferred_type: &str) -> String {
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

/// Retry on transport errors (timeouts, dropped connections, DNS) and on 5xx
/// responses. 4xx responses are client errors and are never retried.
fn is_retryable_error(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Status(code, _) => *code >= 500,
        ureq::Error::Transport(_) => true,
    }
}

/// True for transport-level failures that occur while reading the streamed body
/// (e.g. a dropped/garbled chunked connection). Re-sending the request is safe and
/// usually succeeds; data errors (bad JSON, `response.failed`) won't match.
fn is_stream_decode_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "decoding chunk",
        "while decoding",
        "timed out",
        "timeout",
        "connection reset",
        "connection closed",
        "connection aborted",
        "broken pipe",
        "unexpected eof",
        "eof while",
        "io error",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn sanitize_openai_input(input: Vec<Value>) -> Vec<Value> {
    // Anthropic-only "thinking" items are not valid OpenAI Responses input.
    input
        .into_iter()
        .filter(|item| item.get("type").and_then(Value::as_str) != Some("thinking"))
        .collect()
}

/// Maps a reasoning effort to an Anthropic extended-thinking token budget.
/// Returns None when reasoning should be disabled.
fn anthropic_thinking_budget(effort: &str) -> Option<u64> {
    match effort {
        "low" => Some(4_000),
        "medium" => Some(10_000),
        "high" => Some(20_000),
        "xhigh" => Some(32_000),
        _ => None,
    }
}

fn read_sse_output(
    reader: impl std::io::Read,
    mut emit: impl FnMut(StreamEvent) -> Result<(), String>,
) -> Result<Vec<Value>, String> {
    let mut output_items = Vec::new();
    let mut data_lines = Vec::new();
    let reader = BufReader::new(reader);

    for line in reader.lines() {
        let line = line.map_err(|error| error.to_string())?;
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_string());
            continue;
        }

        if line.is_empty() && !data_lines.is_empty() {
            let data = data_lines.join("\n");
            data_lines.clear();
            if data == "[DONE]" {
                break;
            }
            if handle_sse_data(&data, &mut emit, &mut output_items)? {
                break;
            }
        }
    }

    if !data_lines.is_empty() {
        let data = data_lines.join("\n");
        if data != "[DONE]" {
            let _ = handle_sse_data(&data, &mut emit, &mut output_items)?;
        }
    }

    Ok(output_items)
}

fn handle_sse_data(
    data: &str,
    emit: &mut impl FnMut(StreamEvent) -> Result<(), String>,
    output_items: &mut Vec<Value>,
) -> Result<bool, String> {
    let event = serde_json::from_str::<Value>(data).map_err(|error| error.to_string())?;
    match event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "response.output_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                emit(StreamEvent::Delta(delta.to_string()))?;
            }
        }
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                emit(StreamEvent::ReasoningDelta(delta.to_string()))?;
            }
        }
        "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                emit(StreamEvent::ResponseItem(item.clone()))?;
                output_items.push(item.clone());
            }
        }
        "response.completed" if output_items.is_empty() => {
            if let Some(response) = event.get("response") {
                if let Some((input_tokens, output_tokens, reasoning_tokens)) =
                    extract_usage(response)
                {
                    emit(StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                        reasoning_tokens,
                    })?;
                }
            }
            if let Some(items) = event
                .get("response")
                .and_then(|response| response.get("output"))
                .and_then(Value::as_array)
            {
                for item in items {
                    emit(StreamEvent::ResponseItem(item.clone()))?;
                }
                output_items.extend(items.iter().cloned());
            }
            return Ok(true);
        }
        "response.completed" => {
            if let Some(response) = event.get("response") {
                if let Some((input_tokens, output_tokens, reasoning_tokens)) =
                    extract_usage(response)
                {
                    emit(StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                        reasoning_tokens,
                    })?;
                }
            }
            return Ok(true);
        }
        "response.failed" => {
            return Err(event.to_string());
        }
        _ => {}
    }
    Ok(false)
}

#[derive(Default)]
struct AnthropicStreamState {
    output_items: Vec<Value>,
    blocks: BTreeMap<u64, AnthropicBlock>,
    input_tokens: u64,
    output_tokens: u64,
}

struct AnthropicBlock {
    kind: String,
    id: String,
    name: String,
    text: String,
    arguments: String,
    signature: String,
}

fn read_anthropic_sse_output(
    reader: impl std::io::Read,
    mut emit: impl FnMut(StreamEvent) -> Result<(), String>,
) -> Result<Vec<Value>, String> {
    let mut state = AnthropicStreamState::default();
    let mut data_lines = Vec::new();
    let reader = BufReader::new(reader);

    for line in reader.lines() {
        let line = line.map_err(|error| error.to_string())?;
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_string());
            continue;
        }

        if line.is_empty() && !data_lines.is_empty() {
            let data = data_lines.join("\n");
            data_lines.clear();
            if handle_anthropic_sse_data(&data, &mut emit, &mut state)? {
                break;
            }
        }
    }

    if !data_lines.is_empty() {
        let data = data_lines.join("\n");
        let _ = handle_anthropic_sse_data(&data, &mut emit, &mut state)?;
    }

    Ok(state.output_items)
}

fn handle_anthropic_sse_data(
    data: &str,
    emit: &mut impl FnMut(StreamEvent) -> Result<(), String>,
    state: &mut AnthropicStreamState,
) -> Result<bool, String> {
    let event = serde_json::from_str::<Value>(data).map_err(|error| error.to_string())?;
    match event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "message_start" => {
            state.input_tokens = event
                .get("message")
                .and_then(|message| message.get("usage"))
                .and_then(|usage| usage.get("input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        "content_block_start" => {
            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
            let block = event.get("content_block").unwrap_or(&Value::Null);
            state.blocks.insert(
                index,
                AnthropicBlock {
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
                            emit(StreamEvent::Delta(text.to_string()))?;
                        }
                    }
                    "thinking_delta" => {
                        if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                            block.text.push_str(thinking);
                            emit(StreamEvent::ReasoningDelta(thinking.to_string()))?;
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
                if let Some(item) = anthropic_block_to_response_item(block) {
                    emit(StreamEvent::ResponseItem(item.clone()))?;
                    state.output_items.push(item);
                }
            }
        }
        "message_delta" => {
            state.output_tokens = event
                .get("usage")
                .and_then(|usage| usage.get("output_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(state.output_tokens);
        }
        "message_stop" => {
            emit(StreamEvent::Usage {
                input_tokens: state.input_tokens,
                output_tokens: state.output_tokens,
                reasoning_tokens: 0,
            })?;
            return Ok(true);
        }
        "error" => return Err(event.to_string()),
        _ => {}
    }
    Ok(false)
}

fn emit_anthropic_message(
    message: &Value,
    emit: &mut impl FnMut(StreamEvent) -> Result<(), String>,
) -> Result<Vec<Value>, String> {
    let mut output_items = Vec::new();
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(item) = anthropic_content_block_to_response_item(block) {
            let text = extract_response_text(&item);
            if !text.is_empty() {
                emit(StreamEvent::Delta(text))?;
            }
            emit(StreamEvent::ResponseItem(item.clone()))?;
            output_items.push(item);
        }
    }
    if let Some(usage) = message.get("usage") {
        emit(StreamEvent::Usage {
            input_tokens: usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            reasoning_tokens: 0,
        })?;
    }
    Ok(output_items)
}

fn anthropic_block_to_response_item(block: AnthropicBlock) -> Option<Value> {
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
        "tool_use" => Some(json!({
            "type": "function_call",
            "call_id": block.id,
            "name": block.name,
            "arguments": normalized_arguments(&block.arguments)
        })),
        _ => None,
    }
}

fn anthropic_content_block_to_response_item(block: &Value) -> Option<Value> {
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
        "tool_use" => Some(json!({
            "type": "function_call",
            "call_id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
            "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
            "arguments": block.get("input").map(Value::to_string).unwrap_or_else(|| "{}".to_string())
        })),
        _ => None,
    }
}

fn normalized_arguments(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        "{}".to_string()
    } else {
        trimmed.to_string()
    }
}

fn extract_usage(value: &Value) -> Option<(u64, u64, u64)> {
    let usage = value.get("usage")?;
    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64)?;
    let output_tokens = usage.get("output_tokens").and_then(Value::as_u64)?;
    let reasoning_tokens = usage
        .get("output_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some((input_tokens, output_tokens, reasoning_tokens))
}

fn truncate_error_body(body: &str) -> String {
    let mut chars = body.chars();
    let snippet = chars.by_ref().take(180).collect::<String>();
    if chars.next().is_some() {
        format!("{snippet}...")
    } else {
        snippet
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

        let messages = responses_input_to_anthropic_messages(&input, false);

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

        let with_thinking = responses_input_to_anthropic_messages(&input, true);
        assert_eq!(with_thinking[0]["content"][0]["type"], "thinking");
        assert_eq!(with_thinking[0]["content"][0]["signature"], "sig-1");
        assert_eq!(with_thinking[0]["content"][1]["type"], "tool_use");

        let without_thinking = responses_input_to_anthropic_messages(&input, false);
        assert_eq!(without_thinking[0]["content"][0]["type"], "tool_use");
    }

    #[test]
    fn parses_anthropic_thinking_stream_into_reasoning_and_item() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"step one\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-xyz\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut reasoning = String::new();
        let items = read_anthropic_sse_output(sse.as_bytes(), |event| {
            if let StreamEvent::ReasoningDelta(delta) = event {
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
    fn openai_reasoning_summary_delta_emits_reasoning_event() {
        let sse = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking...\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":4}}}}\n\n",
        );
        let mut reasoning = String::new();
        let mut reasoning_tokens = 0;
        let _ = read_sse_output(sse.as_bytes(), |event| {
            match event {
                StreamEvent::ReasoningDelta(delta) => reasoning.push_str(&delta),
                StreamEvent::Usage {
                    reasoning_tokens: tokens,
                    ..
                } => reasoning_tokens = tokens,
                _ => {}
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(reasoning, "thinking...");
        assert_eq!(reasoning_tokens, 4);
    }

    #[test]
    fn stream_decode_errors_are_retryable_but_data_errors_are_not() {
        assert!(is_stream_decode_error("Error while decoding chunks"));
        assert!(is_stream_decode_error("connection reset by peer"));
        assert!(is_stream_decode_error("the operation timed out"));
        assert!(!is_stream_decode_error(
            "{\"type\":\"response.failed\",\"response\":{}}"
        ));
        assert!(!is_stream_decode_error("expected value at line 1 column 1"));
    }

    #[test]
    fn retry_policy_skips_4xx_and_allows_5xx() {
        assert!(!is_retryable_error(&ureq::Error::Status(
            400,
            ureq::Response::new(400, "Bad Request", "").unwrap()
        )));
        assert!(is_retryable_error(&ureq::Error::Status(
            500,
            ureq::Response::new(500, "Server Error", "").unwrap()
        )));
    }

    #[test]
    fn parses_anthropic_tool_stream_as_response_items() {
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
        let items = read_anthropic_sse_output(sse.as_bytes(), |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["arguments"], "{\"path\":\"Cargo.toml\"}");
        assert!(matches!(events.last(), Some(StreamEvent::Usage { .. })));
    }

    #[test]
    fn builds_anthropic_messages_url_from_openai_or_anthropic_base() {
        assert_eq!(
            anthropic_messages_url("https://api.jucode.cn/v1"),
            "https://api.jucode.cn/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.jucode.cn/anthropic"),
            "https://api.jucode.cn/anthropic/v1/messages"
        );
    }
}
