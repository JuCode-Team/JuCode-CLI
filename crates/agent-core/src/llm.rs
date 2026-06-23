use crate::{
    extensions::ExtensionRegistry,
    hooks::Hooks,
    session::extract_response_text,
    subagents::{
        SubagentManager, SubagentRunResult, SubagentSpawn, MAX_LIVE_SUBAGENTS, MAX_SUBAGENT_DEPTH,
    },
    tools,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    env,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Sender},
        Arc, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_SUBAGENT_OUTPUT_BYTES: usize = 16 * 1024;
const DEFAULT_SUBAGENT_TIMEOUT_SECS: u64 = 180;
const DEFAULT_SUBAGENT_MAX_TOOL_CALLS: u64 = 12;
const DEFAULT_SUBAGENT_MAX_OUTPUT_TOKENS: u64 = 4096;
const RETRY_BACKOFF_BASE_MS: u64 = 250;
const RETRY_BACKOFF_MAX_MS: u64 = 4_000;
const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
const MAX_EMPTY_RESPONSE_CONTINUATIONS: usize = 2;
const EMPTY_RESPONSE_REMINDER: &str = "<runtime_reminder>\nYou have not produced visible progress yet. Continue the user's implementation task now: inspect only what is needed, make the required file changes, run a focused verification when possible, and do not end after exploration alone.\n</runtime_reminder>";

#[derive(Debug, Clone, Copy, Default)]
struct UsageTokens {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    OpenAiResponses,
    AnthropicMessages,
}

pub struct OpenAiClient {
    api_key: String,
    pub model: String,
    reasoning_effort: String,
    /// Supported reasoning-effort tiers per model name (low→high), used to default
    /// a spawned subagent to the cheapest tier of its model.
    model_reasoning_efforts: Vec<(String, Vec<String>)>,
    system_prompt: String,
    prompt_cache_key: String,
    turn_state: Arc<OnceLock<String>>,
    extensions: ExtensionRegistry,
    base_url: String,
    max_output_tokens: u64,
    retry_attempts: usize,
    connect_timeout: Duration,
    read_timeout: Duration,
    allow_subagents: bool,
    max_tool_calls: Option<u64>,
    deadline: Option<Instant>,
    provider_kind: ProviderKind,
    goal_tool_tx: Option<Sender<GoalToolRequest>>,
    approval_tx: Option<Sender<ApprovalRequest>>,
    subagent_manager: Option<SubagentManager>,
    agent_path: String,
    agent_depth: u64,
    hooks: Hooks,
}

pub struct OpenAiClientConfig<'a> {
    pub model: String,
    pub protocol: String,
    pub reasoning_effort: String,
    /// Supported reasoning-effort tiers per model name (low→high). Pass an empty
    /// vec for clients that never spawn subagents.
    pub model_reasoning_efforts: Vec<(String, Vec<String>)>,
    pub system_prompt: String,
    pub prompt_cache_key: String,
    pub extensions: ExtensionRegistry,
    pub base_url: String,
    pub max_output_tokens: u64,
    pub api_key: Option<&'a str>,
    pub api_key_env: &'a str,
    pub retry_attempts: usize,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub goal_tool_tx: Option<Sender<GoalToolRequest>>,
    pub approval_tx: Option<Sender<ApprovalRequest>>,
    pub subagent_manager: Option<SubagentManager>,
    pub hooks: Hooks,
}

#[derive(Debug)]
pub struct GoalToolRequest {
    pub name: String,
    pub arguments: String,
    pub response_tx: Sender<ToolGoalResponse>,
}

/// A gated tool call awaiting the user's allow/deny decision. The worker thread
/// blocks on `response_rx` until the core forwards the client's decision.
#[derive(Debug)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub name: String,
    pub summary: String,
    pub response_tx: Sender<bool>,
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
        model_output: String,
        is_error: bool,
    },
    Usage {
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
    },
}

#[derive(Clone, Debug)]
struct ToolCallRequest {
    call_id: String,
    name: String,
    arguments: String,
}

struct ToolCallResult {
    request: ToolCallRequest,
    result: tools::ToolExecutionResult,
}

enum ParallelToolMessage {
    Update {
        call_id: String,
        name: String,
        output: String,
    },
    Done {
        index: usize,
        request: ToolCallRequest,
        result: tools::ToolExecutionResult,
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
        let provider_kind = ProviderKind::resolve(&config.protocol, &config.model);
        Ok(Self {
            api_key,
            model: config.model,
            reasoning_effort: config.reasoning_effort,
            model_reasoning_efforts: config.model_reasoning_efforts,
            system_prompt: config.system_prompt,
            prompt_cache_key: config.prompt_cache_key,
            turn_state: Arc::new(OnceLock::new()),
            extensions: config.extensions,
            base_url: config.base_url,
            max_output_tokens: config.max_output_tokens,
            retry_attempts: config.retry_attempts,
            connect_timeout: config.connect_timeout,
            read_timeout: config.read_timeout,
            allow_subagents: true,
            max_tool_calls: None,
            deadline: None,
            provider_kind,
            goal_tool_tx: config.goal_tool_tx,
            approval_tx: config.approval_tx,
            subagent_manager: config.subagent_manager,
            agent_path: "/root".to_string(),
            agent_depth: 0,
            hooks: config.hooks,
        })
    }

    pub fn run_turn_events(
        &self,
        mut input: Vec<Value>,
        cwd: &Path,
        mut emit: impl FnMut(StreamEvent) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut tool_calls_executed = 0u64;
        let mut empty_response_continuations = 0usize;
        loop {
            if self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                return Err("subagent timed out".to_string());
            }
            self.append_queued_subagent_messages(&mut input);
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
                if should_continue_after_empty_response(&output_items)
                    && empty_response_continuations < MAX_EMPTY_RESPONSE_CONTINUATIONS
                {
                    empty_response_continuations += 1;
                    input.push(runtime_reminder_item());
                    continue;
                }
                return Ok(());
            }
            empty_response_continuations = 0;
            let pending_call_ids = function_calls
                .iter()
                .filter_map(|call| call.get("call_id").and_then(Value::as_str))
                .map(str::to_string)
                .collect::<HashSet<_>>();

            let mut tool_requests = Vec::new();
            for call in function_calls {
                if self
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    return Err("subagent timed out".to_string());
                }
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
                    .unwrap_or("{}")
                    .to_string();
                tool_requests.push(ToolCallRequest {
                    call_id,
                    name,
                    arguments,
                });
            }
            if let Some(max_tool_calls) = self.max_tool_calls {
                let requested = u64::try_from(tool_requests.len()).unwrap_or(u64::MAX);
                if tool_calls_executed.saturating_add(requested) > max_tool_calls {
                    return Err(format!("subagent exceeded tool budget ({max_tool_calls})"));
                }
            }
            tool_calls_executed = tool_calls_executed
                .saturating_add(u64::try_from(tool_requests.len()).unwrap_or(u64::MAX));

            // Fire pre_tool_use hooks up front: a hook may block a tool, in which
            // case it is never executed and the model receives the block reason.
            let mut blocked_results = Vec::new();
            let mut allowed_requests = Vec::new();
            for request in tool_requests {
                if let Some(reason) = self.hooks.pre_tool(&request.name, &request.arguments, cwd) {
                    emit(StreamEvent::ToolStart {
                        call_id: request.call_id.clone(),
                        name: request.name.clone(),
                    })?;
                    let result = hook_blocked_result(&reason);
                    emit_tool_output(&request, &result, &mut emit)?;
                    blocked_results.push(ToolCallResult { request, result });
                } else {
                    allowed_requests.push(request);
                }
            }

            // Gate side-effecting tools on a user decision before any execution,
            // so the prompt happens one at a time even when calls run in parallel.
            let mut approved_requests = Vec::new();
            for request in allowed_requests {
                if self.needs_approval(&request.name) && !self.request_approval(&request) {
                    emit(StreamEvent::ToolStart {
                        call_id: request.call_id.clone(),
                        name: request.name.clone(),
                    })?;
                    let result = approval_denied_result();
                    emit_tool_output(&request, &result, &mut emit)?;
                    blocked_results.push(ToolCallResult { request, result });
                } else {
                    approved_requests.push(request);
                }
            }
            let allowed_requests = approved_requests;

            let mut tool_results = if should_run_parallel_tools(&allowed_requests) {
                for request in &allowed_requests {
                    emit(StreamEvent::ToolStart {
                        call_id: request.call_id.clone(),
                        name: request.name.clone(),
                    })?;
                }
                run_parallel_builtin_tools(&allowed_requests, cwd, &mut emit)?
            } else {
                let mut results = Vec::new();
                for request in allowed_requests {
                    emit(StreamEvent::ToolStart {
                        call_id: request.call_id.clone(),
                        name: request.name.clone(),
                    })?;
                    let result =
                        self.run_tool_call(&request, cwd, &input, &pending_call_ids, &mut emit);
                    emit_tool_output(&request, &result, &mut emit)?;
                    results.push(ToolCallResult { request, result });
                }
                results
            };

            for tool_result in &tool_results {
                self.hooks
                    .post_tool(&tool_result.request.name, &tool_result.result.output, cwd);
            }
            tool_results.append(&mut blocked_results);

            for tool_result in tool_results {
                let image_item = tools::image_content_item(&tool_result.result.output);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_result.request.call_id,
                    "output": tool_result.result.model_output
                }));
                if let Some(image_item) = image_item {
                    input.push(image_item);
                }
            }
        }
    }

    fn max_attempts(&self) -> usize {
        env::var("JUCODE_RETRY_ATTEMPTS")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(self.retry_attempts)
            .saturating_add(1)
            .max(1)
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
            "prompt_cache_key": self.prompt_cache_key,
            "reasoning": reasoning,
            "input": sanitize_openai_input(input),
            "tools": self.tool_definitions(),
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "store": false,
            "stream": true
        });

        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let max_attempts = self.max_attempts();
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
                if let Some(usage) = extract_usage(&value) {
                    emit(StreamEvent::Usage {
                        input_tokens: usage.input_tokens,
                        cached_input_tokens: usage.cached_input_tokens,
                        output_tokens: usage.output_tokens,
                        reasoning_tokens: usage.reasoning_tokens,
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
                Err(error) if attempt < max_attempts && is_retryable_stream_error(&error) => {
                    emit(StreamEvent::Retrying {
                        attempt: attempt + 1,
                    })?;
                    std::thread::sleep(retry_backoff(attempt));
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
        let max_attempts = self.max_attempts();
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
                Err(error) if attempt < max_attempts && is_retryable_stream_error(&error) => {
                    emit(StreamEvent::Retrying {
                        attempt: attempt + 1,
                    })?;
                    std::thread::sleep(retry_backoff(attempt));
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
        let max_attempts = self.max_attempts();
        for attempt in 1..=max_attempts {
            let mut request = agent
                .post(url)
                .set("Authorization", &format!("Bearer {}", self.api_key))
                .set("Accept", "text/event-stream")
                .set("Content-Type", "application/json")
                .set("session-id", &self.prompt_cache_key)
                .set("thread-id", &self.prompt_cache_key)
                .set("x-client-request-id", &self.prompt_cache_key);
            if let Some(turn_state) = self.turn_state.get() {
                request = request.set(X_CODEX_TURN_STATE_HEADER, turn_state);
            }
            if cache_debug_enabled() {
                eprintln!(
                    "[jucode-cache] send provider={:?} turn_state={}",
                    self.provider_kind,
                    self.turn_state.get().is_some()
                );
            }
            let result = request.send_json(body.clone());
            match result {
                Ok(response) => {
                    let saw_turn_state = capture_turn_state(&response, &self.turn_state);
                    if cache_debug_enabled() {
                        eprintln!(
                            "[jucode-cache] response turn_state_header={} turn_state_stored={}",
                            saw_turn_state,
                            self.turn_state.get().is_some()
                        );
                    }
                    return Ok(response);
                }
                Err(error) if attempt < max_attempts && is_retryable_error(&error) => {
                    emit(StreamEvent::Retrying {
                        attempt: attempt + 1,
                    })?;
                    std::thread::sleep(retry_backoff(attempt));
                }
                Err(error) => return handle_response_error(error),
            }
        }
        unreachable!("retry loop always returns a response or error")
    }

    fn tool_definitions(&self) -> Vec<Value> {
        let mut definitions = tools::definitions();
        if self.allow_subagents && self.subagent_manager.is_some() {
            definitions.extend(subagent_definitions());
        }
        definitions.extend(self.extensions.definitions());
        if self.goal_tool_tx.is_some() {
            definitions.extend(goal_tool_definitions());
            definitions.push(plan_tool_definition());
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

    fn run_subagent_tool(
        &self,
        name: &str,
        arguments: &str,
        cwd: &Path,
        input: &[Value],
        pending_call_ids: &HashSet<String>,
    ) -> Option<tools::ToolExecutionResult> {
        if !matches!(
            name,
            "spawn_agent" | "wait_agent" | "list_agents" | "send_message" | "close_agent"
        ) {
            return None;
        }
        let result = match name {
            "spawn_agent" => self.spawn_agent(arguments, cwd, input, pending_call_ids),
            "wait_agent" => self.wait_agent(arguments),
            "list_agents" => self.list_agents(arguments),
            "send_message" => self.send_message(arguments),
            "close_agent" => self.close_agent(arguments),
            _ => unreachable!(),
        };
        Some(match result {
            Ok(value) => json_tool_result(value, false),
            Err(error) => json_tool_result(json!({ "error": error }), true),
        })
    }

    fn run_tool_call(
        &self,
        request: &ToolCallRequest,
        cwd: &Path,
        input: &[Value],
        pending_call_ids: &HashSet<String>,
        emit: &mut impl FnMut(StreamEvent) -> Result<(), String>,
    ) -> tools::ToolExecutionResult {
        let result = if let Some(result) = self.run_goal_tool(&request.name, &request.arguments) {
            result
        } else if let Some(result) = self.run_subagent_tool(
            &request.name,
            &request.arguments,
            cwd,
            input,
            pending_call_ids,
        ) {
            result
        } else {
            tools::run_tool_with_events(&request.name, &request.arguments, cwd, |event| {
                let tools::ToolExecutionEvent::Update(output) = event;
                emit(StreamEvent::ToolUpdate {
                    call_id: request.call_id.clone(),
                    name: request.name.clone(),
                    output,
                })
            })
        };
        if result.is_error && result.output.contains("unknown tool") {
            match self
                .extensions
                .run_tool(&request.name, &request.arguments, cwd)
            {
                Some((output, is_error)) => tools::ToolExecutionResult {
                    model_output: tools::project_model_output(&request.name, &output, cwd),
                    output,
                    is_error,
                },
                None => result,
            }
        } else {
            result
        }
    }

    fn spawn_agent(
        &self,
        arguments: &str,
        cwd: &Path,
        input: &[Value],
        pending_call_ids: &HashSet<String>,
    ) -> Result<Value, String> {
        let manager = self
            .subagent_manager
            .clone()
            .ok_or_else(|| "subagent manager is unavailable".to_string())?;
        if !self.allow_subagents {
            return Err("agent depth limit reached. Solve the task yourself.".to_string());
        }
        let args = serde_json::from_str::<Value>(arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let task_name = required_str(&args, "task_name")?;
        let message = required_str(&args, "message")?;
        let model = args
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&self.model)
            .to_string();
        let reasoning_effort = match args
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(explicit) => explicit.to_string(),
            // Default a subagent to the cheapest supported tier of its model
            // (subagents don't share the parent prompt cache, so this is free
            // savings). Fall back to the parent effort when tiers are unknown.
            None => self
                .model_reasoning_efforts
                .iter()
                .find(|(name, _)| name == &model)
                .and_then(|(_, efforts)| efforts.first().cloned())
                .unwrap_or_else(|| self.reasoning_effort.clone()),
        };
        let max_tool_calls = args
            .get("max_tool_calls")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_SUBAGENT_MAX_TOOL_CALLS)
            .clamp(1, DEFAULT_SUBAGENT_MAX_TOOL_CALLS);
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_SECS)
            .clamp(10, DEFAULT_SUBAGENT_TIMEOUT_SECS);
        let max_output_tokens = args
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_SUBAGENT_MAX_OUTPUT_TOKENS)
            .clamp(
                512,
                self.max_output_tokens
                    .min(DEFAULT_SUBAGENT_MAX_OUTPUT_TOKENS)
                    .max(512),
            );
        let fork_turns = args
            .get("fork_turns")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("1")
            .to_string();
        validate_fork_turns(&fork_turns)?;
        let child_depth = self.agent_depth.saturating_add(1);
        let slot = manager.reserve_spawn(SubagentSpawn {
            parent_path: self.agent_path.clone(),
            task_name: task_name.to_string(),
            message: message.to_string(),
            model: model.clone(),
            reasoning_effort: reasoning_effort.clone(),
            depth: child_depth,
        })?;
        let child_input =
            build_subagent_input(input, pending_call_ids, &fork_turns, &slot.path, message)?;
        let started = Instant::now();
        let child_manager = manager.clone();
        let child_path = slot.path.clone();
        let child_cwd = PathBuf::from(cwd);
        let child = OpenAiClient {
            api_key: self.api_key.clone(),
            model: model.clone(),
            reasoning_effort,
            model_reasoning_efforts: self.model_reasoning_efforts.clone(),
            system_prompt: subagent_system_prompt(&self.system_prompt, &child_path),
            prompt_cache_key: self.prompt_cache_key.clone(),
            turn_state: Arc::clone(&self.turn_state),
            extensions: self.extensions.clone(),
            base_url: self.base_url.clone(),
            max_output_tokens,
            retry_attempts: self.retry_attempts,
            connect_timeout: self.connect_timeout,
            read_timeout: self.read_timeout.min(Duration::from_secs(timeout_secs)),
            allow_subagents: child_depth < MAX_SUBAGENT_DEPTH,
            max_tool_calls: Some(max_tool_calls),
            deadline: Some(started + Duration::from_secs(timeout_secs)),
            provider_kind: self.provider_kind,
            goal_tool_tx: None,
            approval_tx: None,
            subagent_manager: Some(manager.clone()),
            agent_path: child_path.clone(),
            agent_depth: child_depth,
            hooks: self.hooks.clone(),
        };

        std::thread::spawn(move || {
            child_manager.mark_running(&child_path);
            let mut output_text = String::new();
            let mut tool_iters = 0u64;
            let mut tools_used: Vec<String> = Vec::new();
            let mut input_tokens = 0u64;
            let mut cached_input_tokens = 0u64;
            let mut output_tokens = 0u64;
            let result = child.run_turn_events(child_input, &child_cwd, |event| {
                if slot
                    .interrupt_flag
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return Err("interrupted".to_string());
                }
                match event {
                    StreamEvent::Delta(delta) => output_text.push_str(&delta),
                    StreamEvent::ToolStart { name, .. } => {
                        tool_iters += 1;
                        tools_used.push(name);
                    }
                    StreamEvent::Usage {
                        input_tokens: input,
                        cached_input_tokens: cached,
                        output_tokens: output,
                        ..
                    } => {
                        input_tokens += input;
                        cached_input_tokens += cached;
                        output_tokens += output;
                    }
                    _ => {}
                }
                Ok(())
            });
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let run_result = SubagentRunResult {
                summary: truncate_subagent_output(&output_text),
                partial_output: truncate_subagent_output(&output_text),
                tool_calls: tool_iters,
                tools_used,
                input_tokens,
                cached_input_tokens,
                output_tokens,
                elapsed_ms,
                model,
            };
            match result {
                Ok(()) => child_manager.finish_ok(&child_path, run_result),
                Err(error) => child_manager.finish_err(&child_path, error, run_result),
            }
        });

        Ok(json!({
            "task_name": task_name,
            "path": slot.path,
            "status": "running",
        }))
    }

    fn wait_agent(&self, arguments: &str) -> Result<Value, String> {
        let manager = self
            .subagent_manager
            .clone()
            .ok_or_else(|| "subagent manager is unavailable".to_string())?;
        let args = serde_json::from_str::<Value>(arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let targets = args
            .get("targets")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000)
            .clamp(100, 30_000);
        manager.wait_agents(&self.agent_path, targets, timeout_ms)
    }

    fn list_agents(&self, arguments: &str) -> Result<Value, String> {
        let manager = self
            .subagent_manager
            .clone()
            .ok_or_else(|| "subagent manager is unavailable".to_string())?;
        let args = serde_json::from_str::<Value>(arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        Ok(manager.list_agents(
            &self.agent_path,
            args.get("path_prefix").and_then(Value::as_str),
        ))
    }

    fn send_message(&self, arguments: &str) -> Result<Value, String> {
        let manager = self
            .subagent_manager
            .clone()
            .ok_or_else(|| "subagent manager is unavailable".to_string())?;
        let args = serde_json::from_str::<Value>(arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let target = required_str(&args, "target")?;
        let message = required_str(&args, "message")?;
        manager.send_message(&self.agent_path, target, message)
    }

    fn close_agent(&self, arguments: &str) -> Result<Value, String> {
        let manager = self
            .subagent_manager
            .clone()
            .ok_or_else(|| "subagent manager is unavailable".to_string())?;
        let args = serde_json::from_str::<Value>(arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let target = required_str(&args, "target")?;
        manager.close_agent(&self.agent_path, target)
    }

    fn append_queued_subagent_messages(&self, input: &mut Vec<Value>) {
        let Some(manager) = &self.subagent_manager else {
            return;
        };
        for message in manager.drain_messages(&self.agent_path) {
            input.push(json!({
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": format!("<subagent_message>\n{message}\n</subagent_message>")
                }]
            }));
        }
    }

    fn run_goal_tool(&self, name: &str, arguments: &str) -> Option<tools::ToolExecutionResult> {
        if !matches!(
            name,
            "get_goal" | "create_goal" | "update_goal" | "update_plan"
        ) {
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

    /// Tools whose side effects warrant a user decision before they run:
    /// shell execution and file mutations. Only gated when an approval handler
    /// is wired (interactive serve / TUI), never for subagents.
    fn needs_approval(&self, name: &str) -> bool {
        self.approval_tx.is_some()
            && matches!(
                name,
                "bash"
                    | "execute"
                    | "exec_command"
                    | "shell_command"
                    | "write"
                    | "edit"
                    | "str_replace"
                    | "hashline_edit"
                    | "apply_patch"
            )
    }

    /// Blocks until the core forwards the user's decision. A dropped channel
    /// (interrupt / no handler) is treated as a denial so the worker unblocks.
    fn request_approval(&self, request: &ToolCallRequest) -> bool {
        let Some(tx) = &self.approval_tx else {
            return true;
        };
        let (response_tx, response_rx) = mpsc::channel();
        if tx
            .send(ApprovalRequest {
                call_id: request.call_id.clone(),
                name: request.name.clone(),
                summary: approval_summary(&request.name, &request.arguments),
                response_tx,
            })
            .is_err()
        {
            return false;
        }
        response_rx.recv().unwrap_or(false)
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

    /// An explicit provider protocol wins; otherwise fall back to the model heuristic.
    fn resolve(protocol: &str, model: &str) -> Self {
        match protocol {
            "anthropic" => Self::AnthropicMessages,
            "responses" => Self::OpenAiResponses,
            _ => Self::from_model(model),
        }
    }
}

fn subagent_definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": "spawn_agent",
            "description": format!("Start a lightweight background subagent for an independent bounded task. The agent shares the current cwd and tools, inherits the system prompt and skills, and returns immediately. Keep at most {MAX_LIVE_SUBAGENTS} live agents; nesting is capped at depth {MAX_SUBAGENT_DEPTH}."),
            "parameters": {
                "type": "object",
                "properties": {
                    "task_name": {
                        "type": "string",
                        "description": "Stable lowercase identifier for this child under the current agent path. Use lowercase letters, digits, and underscores."
                    },
                    "message": {
                        "type": "string",
                        "description": "Self-contained task for the subagent."
                    },
                    "fork_turns": {
                        "type": "string",
                        "description": "Context to fork into the subagent: 1 (default), all, none, or a positive integer string for the last N user turns."
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model override. Defaults to the parent model."
                    },
                    "reasoning_effort": {
                        "type": "string",
                        "description": "Optional reasoning effort override. Defaults to the parent effort."
                    },
                    "max_tool_calls": {
                        "type": "number",
                        "description": "Optional tool-call budget. Defaults to 12 and is capped at 12."
                    },
                    "timeout_secs": {
                        "type": "number",
                        "description": "Optional wall-clock timeout. Defaults to 180 seconds and is capped at 180."
                    },
                    "max_output_tokens": {
                        "type": "number",
                        "description": "Optional output token cap. Defaults to 4096 and is capped at 4096."
                    }
                },
                "required": ["task_name", "message"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "wait_agent",
            "description": "Wait for one or more subagents to finish and return their current status/results. Without targets, returns when any agent finishes or there are no live agents.",
            "parameters": {
                "type": "object",
                "properties": {
                    "targets": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional agent paths or child names to wait for."
                    },
                    "timeout_ms": {
                        "type": "number",
                        "description": "Optional wait timeout in milliseconds. Defaults to 30000 and is capped at 30000."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "list_agents",
            "description": "List known subagents and their statuses for this active turn.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path_prefix": {
                        "type": "string",
                        "description": "Optional absolute path or child-name prefix filter."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "send_message",
            "description": "Queue a short message for a running subagent. The subagent receives it before its next model call.",
            "parameters": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Agent path or child name." },
                    "message": { "type": "string", "description": "Message to deliver." }
                },
                "required": ["target", "message"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "close_agent",
            "description": "Interrupt and close a running subagent.",
            "parameters": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Agent path or child name." }
                },
                "required": ["target"],
                "additionalProperties": false
            }
        }),
    ]
}

fn json_tool_result(value: Value, is_error: bool) -> tools::ToolExecutionResult {
    let output = value.to_string();
    tools::ToolExecutionResult {
        model_output: output.clone(),
        output,
        is_error,
    }
}

fn is_parallel_safe_tool(name: &str) -> bool {
    // Only read-only inspection tools may fan out. Shell tools are excluded: a
    // model can batch several mutating commands that share one cwd, and running
    // those concurrently races (and bypasses the sequential approval path).
    matches!(name, "read" | "ls" | "ripgrep" | "outline")
}

fn should_run_parallel_tools(requests: &[ToolCallRequest]) -> bool {
    requests.len() > 1
        && requests
            .iter()
            .all(|request| is_parallel_safe_tool(&request.name))
}

fn run_parallel_builtin_tools(
    requests: &[ToolCallRequest],
    cwd: &Path,
    emit: &mut impl FnMut(StreamEvent) -> Result<(), String>,
) -> Result<Vec<ToolCallResult>, String> {
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();

    for (index, request) in requests.iter().cloned().enumerate() {
        let tx = tx.clone();
        let cwd = cwd.to_path_buf();
        handles.push(thread::spawn(move || {
            let result = tools::run_tool_with_events(&request.name, &request.arguments, &cwd, {
                let tx = tx.clone();
                let call_id = request.call_id.clone();
                let name = request.name.clone();
                move |event| {
                    let tools::ToolExecutionEvent::Update(output) = event;
                    tx.send(ParallelToolMessage::Update {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        output,
                    })
                    .map_err(|error| error.to_string())
                }
            });
            let _ = tx.send(ParallelToolMessage::Done {
                index,
                request,
                result,
            });
        }));
    }
    drop(tx);

    let mut completed = Vec::new();
    while completed.len() < requests.len() {
        match rx.recv() {
            Ok(ParallelToolMessage::Update {
                call_id,
                name,
                output,
            }) => emit(StreamEvent::ToolUpdate {
                call_id,
                name,
                output,
            })?,
            Ok(ParallelToolMessage::Done {
                index,
                request,
                result,
            }) => completed.push((index, ToolCallResult { request, result })),
            Err(error) => {
                for handle in handles {
                    let _ = handle.join();
                }
                return Err(format!("parallel tool worker failed: {error}"));
            }
        }
    }

    for handle in handles {
        handle
            .join()
            .map_err(|_| "parallel tool worker panicked".to_string())?;
    }

    completed.sort_by_key(|(index, _)| *index);
    let results = completed
        .into_iter()
        .map(|(_, result)| result)
        .collect::<Vec<_>>();
    for result in &results {
        emit_tool_output(&result.request, &result.result, emit)?;
    }
    Ok(results)
}

fn hook_blocked_result(reason: &str) -> tools::ToolExecutionResult {
    let output = json!({
        "error": format!("blocked by pre_tool_use hook: {reason}")
    })
    .to_string();
    tools::ToolExecutionResult {
        model_output: output.clone(),
        output,
        is_error: true,
    }
}

fn approval_denied_result() -> tools::ToolExecutionResult {
    let output = json!({
        "error": "denied by user: the user declined to run this tool call. Do not retry it; ask how to proceed or try a different approach."
    })
    .to_string();
    tools::ToolExecutionResult {
        model_output: output.clone(),
        output,
        is_error: true,
    }
}

/// A short human-readable description of a gated tool call for the approval UI.
fn approval_summary(name: &str, arguments: &str) -> String {
    let args = serde_json::from_str::<Value>(arguments).unwrap_or(Value::Null);
    let field = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    match name {
        "bash" | "execute" | "exec_command" | "shell_command" => {
            field("command").or_else(|| field("cmd")).unwrap_or_default()
        }
        _ => field("path").unwrap_or_default(),
    }
}

fn emit_tool_output(
    request: &ToolCallRequest,
    result: &tools::ToolExecutionResult,
    emit: &mut impl FnMut(StreamEvent) -> Result<(), String>,
) -> Result<(), String> {
    emit(StreamEvent::ToolOutput {
        call_id: request.call_id.clone(),
        name: request.name.clone(),
        output: result.output.clone(),
        model_output: result.model_output.clone(),
        is_error: result.is_error,
    })
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{key} is required"))?;
    Ok(value)
}

fn subagent_system_prompt(parent_system: &str, path: &str) -> String {
    format!(
        "{parent_system}\n\n<subagent_context>\nYou are JuCode subagent {path}. Work only on the task delegated by the parent. Keep work bounded: inspect only what is needed, avoid broad refactors, and stop when you have enough evidence. Return a concise self-contained answer with Summary, Evidence, Files/commands checked, and Risks or unknowns. Do not ask follow-up questions unless the task is impossible without missing information.\n</subagent_context>"
    )
}

fn build_subagent_input(
    input: &[Value],
    pending_call_ids: &HashSet<String>,
    fork_turns: &str,
    child_path: &str,
    message: &str,
) -> Result<Vec<Value>, String> {
    let mut forked = match fork_turns {
        "none" => Vec::new(),
        "all" => filter_pending_subagent_items(input, pending_call_ids),
        other => {
            let turns = other.parse::<usize>().map_err(|_| {
                "fork_turns must be \"all\", \"none\", or a positive integer string".to_string()
            })?;
            if turns == 0 {
                return Err(
                    "fork_turns must be \"all\", \"none\", or a positive integer string"
                        .to_string(),
                );
            }
            let filtered = filter_pending_subagent_items(input, pending_call_ids);
            let mut seen = 0usize;
            let mut start = 0usize;
            for (index, item) in filtered.iter().enumerate().rev() {
                if item.get("role").and_then(Value::as_str) == Some("user") {
                    seen += 1;
                    if seen == turns {
                        start = index;
                        break;
                    }
                }
            }
            filtered[start..].to_vec()
        }
    };
    forked.push(json!({
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!("<subagent_task path=\"{child_path}\">\n{message}\n</subagent_task>")
        }]
    }));
    Ok(forked)
}

fn validate_fork_turns(fork_turns: &str) -> Result<(), String> {
    if fork_turns == "all" || fork_turns == "none" {
        return Ok(());
    }
    if fork_turns.parse::<usize>().is_ok_and(|turns| turns > 0) {
        Ok(())
    } else {
        Err("fork_turns must be \"all\", \"none\", or a positive integer string".to_string())
    }
}

fn filter_pending_subagent_items(
    input: &[Value],
    pending_call_ids: &HashSet<String>,
) -> Vec<Value> {
    input
        .iter()
        .filter(|item| {
            if !matches!(
                item.get("type").and_then(Value::as_str),
                Some("function_call" | "function_call_output")
            ) {
                return true;
            }
            item.get("call_id")
                .and_then(Value::as_str)
                .is_none_or(|call_id| !pending_call_ids.contains(call_id))
        })
        .cloned()
        .collect()
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

fn plan_tool_definition() -> Value {
    json!({
        "type": "function",
        "name": "update_plan",
        "description": "Maintain a short, visible task plan for multi-step work. Call it at the start to lay out the steps, and again whenever the plan changes — mark exactly one step in_progress and flip finished steps to completed. Keep steps concise (a handful of words). Skip it for trivial single-step tasks.",
        "parameters": {
            "type": "object",
            "properties": {
                "plan": {
                    "type": "array",
                    "description": "The full ordered list of steps; replaces the previous plan.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": { "type": "string", "description": "Short description of the step." },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                        },
                        "required": ["step", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["plan"],
            "additionalProperties": false
        }
    })
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
            for part in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if part.get("type").and_then(Value::as_str) == Some("input_image") {
                    if let Some(block) = anthropic_image_block(part) {
                        push_anthropic_content(&mut messages, "user", block);
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

/// Converts an OpenAI-style `input_image` part (a `data:<mime>;base64,<data>`
/// URL) into an Anthropic image content block. Returns `None` if the URL is not
/// an inline base64 data URL.
fn anthropic_image_block(part: &Value) -> Option<Value> {
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

fn should_continue_after_empty_response(output_items: &[Value]) -> bool {
    output_items.iter().all(|item| {
        item.get("type").and_then(Value::as_str) != Some("function_call")
            && extract_response_text(item).trim().is_empty()
    })
}

fn runtime_reminder_item() -> Value {
    json!({
        "role": "user",
        "content": [{ "type": "input_text", "text": EMPTY_RESPONSE_REMINDER }]
    })
}

/// Retry on transport errors (timeouts, dropped connections, DNS) and on 5xx
/// responses. 4xx responses are client errors and are never retried.
fn is_retryable_error(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Status(code, _) => *code >= 500,
        ureq::Error::Transport(_) => true,
    }
}

fn retry_backoff(attempt: usize) -> Duration {
    let multiplier = 1u64 << attempt.saturating_sub(1).min(4);
    Duration::from_millis((RETRY_BACKOFF_BASE_MS * multiplier).min(RETRY_BACKOFF_MAX_MS))
}

fn cache_debug_enabled() -> bool {
    env::var("JUCODE_CACHE_DEBUG")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

fn capture_turn_state(response: &ureq::Response, turn_state: &OnceLock<String>) -> bool {
    if let Some(value) = response
        .header(X_CODEX_TURN_STATE_HEADER)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let _ = turn_state.set(value.to_string());
        true
    } else {
        false
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
        "peer closed connection",
        "broken pipe",
        "tls close_notify",
        "stream closed before response.completed",
        "unexpected end of file",
        "unexpected eof",
        "unexpected-eof",
        "eof while",
        "io error",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn is_retryable_stream_error(message: &str) -> bool {
    is_stream_decode_error(message) || is_retryable_response_failed(message)
}

fn is_retryable_response_failed(message: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(message) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("response.failed") {
        return false;
    }
    let error = value
        .get("response")
        .and_then(|response| response.get("error"));
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let detail = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    code == "upstream_error" || detail.contains("upstream request failed")
}

fn sanitize_openai_input(input: Vec<Value>) -> Vec<Value> {
    input
        .into_iter()
        .filter_map(sanitize_openai_input_item)
        .collect()
}

fn sanitize_openai_input_item(mut item: Value) -> Option<Value> {
    // Anthropic-only "thinking" items are not valid OpenAI Responses input.
    if item.get("type").and_then(Value::as_str) == Some("thinking") {
        return None;
    }
    // Response item ids are service-owned metadata. Replaying them makes the
    // request prefix less stable and differs from Codex, which does not
    // serialize ids back into Responses input.
    if let Value::Object(map) = &mut item {
        map.remove("id");
    }
    Some(item)
}

/// Maps a reasoning effort to an Anthropic extended-thinking token budget.
/// Returns None when reasoning should be disabled.
fn anthropic_thinking_budget(effort: &str) -> Option<u64> {
    match effort {
        "low" => Some(4_000),
        "medium" => Some(10_000),
        "high" => Some(20_000),
        "xhigh" => Some(32_000),
        "max" => Some(64_000),
        _ => None,
    }
}

fn read_sse_output(
    reader: impl std::io::Read,
    mut emit: impl FnMut(StreamEvent) -> Result<(), String>,
) -> Result<Vec<Value>, String> {
    let mut output_items = Vec::new();
    let mut data_lines = Vec::new();
    let mut completed = false;
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
                completed = true;
                break;
            }
        }
    }

    if !data_lines.is_empty() {
        let data = data_lines.join("\n");
        if data != "[DONE]" {
            completed = handle_sse_data(&data, &mut emit, &mut output_items)?;
        }
    }

    if !completed {
        return Err("stream closed before response.completed".to_string());
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
                if let Some(usage) = extract_usage(response) {
                    emit(StreamEvent::Usage {
                        input_tokens: usage.input_tokens,
                        cached_input_tokens: usage.cached_input_tokens,
                        output_tokens: usage.output_tokens,
                        reasoning_tokens: usage.reasoning_tokens,
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
                if let Some(usage) = extract_usage(response) {
                    emit(StreamEvent::Usage {
                        input_tokens: usage.input_tokens,
                        cached_input_tokens: usage.cached_input_tokens,
                        output_tokens: usage.output_tokens,
                        reasoning_tokens: usage.reasoning_tokens,
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
    cached_input_tokens: u64,
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
            let usage = event
                .get("message")
                .and_then(|message| message.get("usage"));
            state.input_tokens = usage
                .and_then(|usage| usage.get("input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            state.cached_input_tokens = usage
                .and_then(|usage| usage.get("cache_read_input_tokens"))
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
                cached_input_tokens: state.cached_input_tokens,
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
            cached_input_tokens: usage
                .get("cache_read_input_tokens")
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

fn extract_usage(value: &Value) -> Option<UsageTokens> {
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
    Some(UsageTokens {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_tokens,
    })
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
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

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
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":2},\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":4}}}}\n\n",
        );
        let mut reasoning = String::new();
        let mut cached_input_tokens = 0;
        let mut reasoning_tokens = 0;
        let _ = read_sse_output(sse.as_bytes(), |event| {
            match event {
                StreamEvent::ReasoningDelta(delta) => reasoning.push_str(&delta),
                StreamEvent::Usage {
                    cached_input_tokens: cached,
                    reasoning_tokens: tokens,
                    ..
                } => {
                    cached_input_tokens = cached;
                    reasoning_tokens = tokens;
                }
                _ => {}
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(reasoning, "thinking...");
        assert_eq!(cached_input_tokens, 2);
        assert_eq!(reasoning_tokens, 4);
    }

    #[test]
    fn openai_input_sanitizer_removes_service_ids_and_anthropic_thinking() {
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

        let sanitized = sanitize_openai_input(input);

        assert_eq!(sanitized.len(), 2);
        assert!(sanitized[0].get("id").is_none());
        assert_eq!(sanitized[0]["type"], "reasoning");
        assert_eq!(sanitized[0]["encrypted_content"], "enc");
        assert!(sanitized[1].get("id").is_none());
        assert_eq!(sanitized[1]["call_id"], "call_1");
    }

    #[test]
    fn empty_non_tool_response_requests_runtime_reminder() {
        assert!(should_continue_after_empty_response(&[]));
        assert!(should_continue_after_empty_response(&[json!({
            "type": "reasoning",
            "summary": []
        })]));
        assert!(!should_continue_after_empty_response(&[json!({
            "type": "message",
            "content": [{ "type": "output_text", "text": "done" }]
        })]));
        assert!(!should_continue_after_empty_response(&[json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "ls",
            "arguments": "{}"
        })]));
    }

    #[test]
    fn runtime_reminder_is_user_context_item() {
        let item = runtime_reminder_item();

        assert_eq!(item["role"], "user");
        assert!(item["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("do not end after exploration alone"));
    }

    #[test]
    fn parallel_policy_requires_multiple_safe_builtin_tools() {
        let read = ToolCallRequest {
            call_id: "read_1".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        };
        let rg = ToolCallRequest {
            call_id: "rg_1".to_string(),
            name: "ripgrep".to_string(),
            arguments: "{}".to_string(),
        };
        let write = ToolCallRequest {
            call_id: "write_1".to_string(),
            name: "write".to_string(),
            arguments: "{}".to_string(),
        };
        let subagent = ToolCallRequest {
            call_id: "agent_1".to_string(),
            name: "spawn_agent".to_string(),
            arguments: "{}".to_string(),
        };
        let bash = ToolCallRequest {
            call_id: "bash_1".to_string(),
            name: "exec_command".to_string(),
            arguments: "{}".to_string(),
        };

        assert!(should_run_parallel_tools(&[read.clone(), rg]));
        assert!(!should_run_parallel_tools(&[read.clone()]));
        assert!(!should_run_parallel_tools(&[read.clone(), write]));
        assert!(!should_run_parallel_tools(&[read.clone(), subagent]));
        // Shell tools are not parallel-safe: a batch of commands serializes.
        assert!(!should_run_parallel_tools(&[bash.clone(), bash.clone()]));
        assert!(!should_run_parallel_tools(&[read, bash]));
    }

    // Exercises the executor directly: it can run any tools concurrently and
    // preserves submission order. The routing policy (is_parallel_safe_tool)
    // decides what actually reaches it — shell is no longer routed here.
    #[test]
    fn parallel_builtin_tools_run_concurrently_and_preserve_output_order() {
        let dir = test_dir("parallel-tools");
        fs::create_dir_all(&dir).unwrap();
        let (wait_command, touch_command) = if cfg!(windows) {
            (
                "while (-not (Test-Path ready)) { Start-Sleep -Milliseconds 50 }; Write-Output first",
                "Start-Sleep -Milliseconds 100; New-Item -ItemType File ready | Out-Null; Write-Output second",
            )
        } else {
            (
                "while [ ! -f ready ]; do sleep 0.05; done; echo first",
                "sleep 0.1; touch ready; echo second",
            )
        };
        let requests = vec![
            ToolCallRequest {
                call_id: "first".to_string(),
                name: "exec_command".to_string(),
                arguments: json!({ "cmd": wait_command, "timeout": 3 }).to_string(),
            },
            ToolCallRequest {
                call_id: "second".to_string(),
                name: "exec_command".to_string(),
                arguments: json!({ "cmd": touch_command, "timeout": 3 }).to_string(),
            },
        ];
        let mut events = Vec::new();

        let results = run_parallel_builtin_tools(&requests, &dir, &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

        assert_eq!(results[0].request.call_id, "first");
        assert_eq!(results[1].request.call_id, "second");
        assert!(!results[0].result.is_error, "{}", results[0].result.output);
        assert!(!results[1].result.is_error, "{}", results[1].result.output);
        assert!(results[0].result.output.contains("first"));
        assert!(results[1].result.output.contains("second"));
        let output_call_ids = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolOutput { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(output_call_ids, ["first", "second"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stream_decode_errors_are_retryable_but_data_errors_are_not() {
        assert!(is_stream_decode_error("Error while decoding chunks"));
        assert!(is_stream_decode_error("connection reset by peer"));
        assert!(is_stream_decode_error("the operation timed out"));
        assert!(is_stream_decode_error(
            "peer closed connection without sending TLS close_notify"
        ));
        assert!(is_stream_decode_error(
            "https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof"
        ));
        assert!(is_stream_decode_error(
            "tls connection init failed: unexpected end of file"
        ));
        assert!(is_stream_decode_error(
            "stream closed before response.completed"
        ));
        assert!(is_retryable_stream_error(
            r#"{"type":"response.failed","response":{"error":{"code":"upstream_error","message":"Upstream request failed"}}}"#
        ));
        assert!(!is_stream_decode_error(
            "{\"type\":\"response.failed\",\"response\":{}}"
        ));
        assert!(!is_retryable_stream_error(
            r#"{"type":"response.failed","response":{"error":{"code":"invalid_request_error","message":"bad request"}}}"#
        ));
        assert!(!is_stream_decode_error("expected value at line 1 column 1"));
    }

    #[test]
    fn captures_codex_turn_state_once() {
        let turn_state = OnceLock::new();
        let response: ureq::Response = "HTTP/1.1 200 OK\r\n\
             x-codex-turn-state: sticky-1\r\n\
             \r\n"
            .parse()
            .unwrap();

        capture_turn_state(&response, &turn_state);

        assert_eq!(turn_state.get().map(String::as_str), Some("sticky-1"));

        let response: ureq::Response = "HTTP/1.1 200 OK\r\n\
             x-codex-turn-state: sticky-2\r\n\
             \r\n"
            .parse()
            .unwrap();

        capture_turn_state(&response, &turn_state);

        assert_eq!(turn_state.get().map(String::as_str), Some("sticky-1"));
    }

    #[test]
    fn openai_stream_without_completed_is_an_error() {
        let error = read_sse_output(
            "data: {\"type\":\"response.created\"}\n\n".as_bytes(),
            |_| Ok(()),
        )
        .expect_err("stream without response.completed should fail");

        assert!(error.contains("stream closed before response.completed"));
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
    fn retry_backoff_increases_and_caps() {
        assert_eq!(retry_backoff(1), Duration::from_millis(250));
        assert_eq!(retry_backoff(2), Duration::from_millis(500));
        assert_eq!(retry_backoff(3), Duration::from_millis(1000));
        assert_eq!(retry_backoff(5), Duration::from_millis(4000));
        assert_eq!(retry_backoff(99), Duration::from_millis(4000));
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

    fn test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("jucode-llm-test-{name}-{nanos}"))
    }

    #[test]
    fn builds_subagent_input_from_last_user_turn_by_default() {
        let input = vec![
            json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": "old task" }]
            }),
            json!({ "type": "message", "content": [{ "type": "output_text", "text": "old answer" }] }),
            json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": "recent task" }]
            }),
            json!({ "type": "function_call", "call_id": "pending_1", "name": "spawn_agent", "arguments": "{}" }),
            json!({ "type": "function_call_output", "call_id": "done_1", "output": "done output" }),
        ];
        let pending_call_ids = HashSet::from(["pending_1".to_string()]);

        let forked =
            build_subagent_input(&input, &pending_call_ids, "1", "/root/child", "inspect").unwrap();

        assert_eq!(forked.len(), 3);
        assert_eq!(
            response_content_text(&forked[0], "input_text"),
            "recent task"
        );
        assert_eq!(forked[1]["call_id"], "done_1");
        assert!(response_content_text(&forked[2], "input_text").contains("inspect"));
        assert!(!forked.iter().any(|item| item["call_id"] == "pending_1"));
    }

    #[test]
    fn builds_subagent_input_for_all_and_none_modes() {
        let input = vec![
            json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": "first" }]
            }),
            json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": "second" }]
            }),
        ];
        let pending_call_ids = HashSet::new();

        let all = build_subagent_input(&input, &pending_call_ids, "all", "/root/child", "inspect")
            .unwrap();
        let none =
            build_subagent_input(&input, &pending_call_ids, "none", "/root/child", "inspect")
                .unwrap();

        assert_eq!(all.len(), 3);
        assert_eq!(response_content_text(&all[0], "input_text"), "first");
        assert_eq!(response_content_text(&all[1], "input_text"), "second");
        assert_eq!(none.len(), 1);
        assert!(response_content_text(&none[0], "input_text").contains("inspect"));
    }
}
