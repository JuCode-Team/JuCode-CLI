use crate::{
    config::ApprovalMode,
    extensions::ExtensionRegistry,
    hooks::Hooks,
    hunks::{self, HunkView},
    mcp::McpManager,
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
const ANTHROPIC_VERSION: &str = "2023-06-01";
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
    mcp: McpManager,
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
    /// Which tool classes this client gates on the approval channel. Captured
    /// at client construction (turn start / subagent spawn) and never changed
    /// mid-run; loosening a running turn happens core-side instead.
    approval_mode: ApprovalMode,
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
    pub mcp: McpManager,
    pub base_url: String,
    pub max_output_tokens: u64,
    pub api_key: Option<&'a str>,
    pub api_key_env: &'a str,
    pub retry_attempts: usize,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub goal_tool_tx: Option<Sender<GoalToolRequest>>,
    pub approval_tx: Option<Sender<ApprovalRequest>>,
    pub approval_mode: ApprovalMode,
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
    /// Path of the subagent that issued the call; None for the main agent.
    pub subagent_id: Option<String>,
    /// Hunk breakdown for edit tools, computed before anything is applied so
    /// the client can approve a subset. None means whole-call decisions only.
    pub hunks: Option<Vec<HunkView>>,
    pub response_tx: Sender<ApprovalDecision>,
}

/// The user's answer to an [`ApprovalRequest`].
#[derive(Debug, Clone)]
pub struct ApprovalDecision {
    pub allow: bool,
    /// With `allow`, Some(ids) applies only the listed hunks of an edit tool
    /// call; None approves the whole call.
    pub approved_hunks: Option<Vec<String>>,
}

impl ApprovalDecision {
    pub fn allow_all() -> Self {
        Self {
            allow: true,
            approved_hunks: None,
        }
    }

    pub fn deny() -> Self {
        Self {
            allow: false,
            approved_hunks: None,
        }
    }
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

/// Aggregates a subagent's streamed events into the run result. A `Retrying`
/// event means the in-flight response will replay its deltas from scratch, so
/// text accumulated since the last `CallStart` is discarded to avoid
/// duplicating it in the output returned to the parent model.
#[derive(Default)]
struct SubagentTurnStats {
    output_text: String,
    /// Byte offset in `output_text` where the in-flight response began.
    response_start: usize,
    tool_calls: u64,
    tools_used: Vec<String>,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
}

impl SubagentTurnStats {
    fn record(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::CallStart => self.response_start = self.output_text.len(),
            StreamEvent::Retrying { .. } => self.output_text.truncate(self.response_start),
            StreamEvent::Delta(delta) => self.output_text.push_str(&delta),
            StreamEvent::ToolStart { name, .. } => {
                self.tool_calls += 1;
                self.tools_used.push(name);
            }
            StreamEvent::Usage {
                input_tokens,
                cached_input_tokens,
                output_tokens,
                ..
            } => {
                self.input_tokens += input_tokens;
                self.cached_input_tokens += cached_input_tokens;
                self.output_tokens += output_tokens;
            }
            _ => {}
        }
    }
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
            mcp: config.mcp,
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
            approval_mode: config.approval_mode,
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
            self.append_queued_subagent_messages(&mut input, &mut emit)?;
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
                    inject_input_item(&mut input, runtime_reminder_item(), &mut emit)?;
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
            // A partial (hunk-subset) approval rewrites the call's arguments to
            // the approved hunks and records what was rejected for the result.
            let mut approved_requests = Vec::new();
            let mut partial_approvals: BTreeMap<String, (Vec<String>, Vec<String>)> =
                BTreeMap::new();
            for mut request in allowed_requests {
                if !self.needs_approval(&request.name) {
                    approved_requests.push(request);
                    continue;
                }
                let decision = self.request_approval(&request, cwd);
                if !decision.allow {
                    emit(StreamEvent::ToolStart {
                        call_id: request.call_id.clone(),
                        name: request.name.clone(),
                    })?;
                    let result = approval_denied_result();
                    emit_tool_output(&request, &result, &mut emit)?;
                    blocked_results.push(ToolCallResult { request, result });
                    continue;
                }
                match decision.approved_hunks {
                    None => approved_requests.push(request),
                    Some(approved) => {
                        match hunks::filter_edit_call(&request.name, &request.arguments, &approved)
                        {
                            Ok(filtered) => {
                                request.arguments = filtered.arguments;
                                partial_approvals.insert(
                                    request.call_id.clone(),
                                    (filtered.applied, filtered.rejected),
                                );
                                approved_requests.push(request);
                            }
                            Err(error) => {
                                emit(StreamEvent::ToolStart {
                                    call_id: request.call_id.clone(),
                                    name: request.name.clone(),
                                })?;
                                let result = hunk_selection_failed_result(&error);
                                emit_tool_output(&request, &result, &mut emit)?;
                                blocked_results.push(ToolCallResult { request, result });
                            }
                        }
                    }
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
                    let mut result =
                        self.run_tool_call(&request, cwd, &input, &pending_call_ids, &mut emit);
                    // Edit tools are never parallel-safe, so a partially
                    // approved call always lands here; tell the model exactly
                    // which hunks were applied and which the user rejected.
                    if let Some((applied, rejected)) = partial_approvals.remove(&request.call_id) {
                        result.output =
                            hunks::merge_selective_summary(&result.output, &applied, &rejected);
                        result.model_output = hunks::merge_selective_summary(
                            &result.model_output,
                            &applied,
                            &rejected,
                        );
                    }
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

            push_tool_result_items(&mut input, tool_results);
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
                let body = self.openai_summarize_body(system, user);
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

    /// Body for the one-shot summarization call on the OpenAI Responses path.
    /// Matches the main path's `store: false` and output cap.
    fn openai_summarize_body(&self, system: &str, user: &str) -> Value {
        json!({
            "model": self.model,
            "instructions": system,
            "reasoning": { "effort": self.reasoning_effort },
            "max_output_tokens": self.max_output_tokens.max(1),
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": user }] }],
            "store": false,
            "stream": true
        })
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

    /// Body for the main streaming call on the OpenAI Responses path.
    /// `include` requests encrypted reasoning so multi-turn tool calls can
    /// replay it under `store: false`.
    fn openai_responses_body(&self, input: Vec<Value>) -> Value {
        // Request a reasoning summary so the thinking phase can stream content.
        // With effort "none" the model does not reason, so no summary is requested.
        let reasoning = if self.reasoning_effort == "none" {
            json!({ "effort": self.reasoning_effort })
        } else {
            json!({ "effort": self.reasoning_effort, "summary": "auto" })
        };
        json!({
            "model": self.model,
            "instructions": self.system_prompt,
            "prompt_cache_key": self.prompt_cache_key,
            "reasoning": reasoning,
            "input": sanitize_openai_input(input),
            "tools": self.tool_definitions(),
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "max_output_tokens": self.max_output_tokens.max(1),
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "stream": true
        })
    }

    fn create_response_streaming(
        &self,
        input: Vec<Value>,
        mut emit: impl FnMut(StreamEvent) -> Result<(), String>,
    ) -> Result<Vec<Value>, String> {
        let body = self.openai_responses_body(input);
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let max_attempts = self.max_attempts();
        for attempt in 1..=max_attempts {
            // Single send per attempt: this loop owns all retries, so send and
            // stream failures cannot multiply into nested retry rounds.
            let response = match self.send_once(&url, &body) {
                Ok(response) => response,
                Err(error) if attempt < max_attempts && is_retryable_error(&error) => {
                    crate::log_warn!(
                        "llm",
                        "retrying request",
                        attempt = attempt + 1,
                        error = error.to_string()
                    );
                    emit(StreamEvent::Retrying {
                        attempt: attempt + 1,
                    })?;
                    std::thread::sleep(retry_backoff(attempt));
                    continue;
                }
                Err(error) => return handle_response_error(*error),
            };
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
                    crate::log_warn!(
                        "llm",
                        "retrying after stream error",
                        attempt = attempt + 1,
                        error = error.clone()
                    );
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
            // Single send per attempt: this loop owns all retries, so send and
            // stream failures cannot multiply into nested retry rounds.
            let response = match self.send_once(&url, &body) {
                Ok(response) => response,
                Err(error) if attempt < max_attempts && is_retryable_error(&error) => {
                    crate::log_warn!(
                        "llm",
                        "retrying request",
                        attempt = attempt + 1,
                        error = error.to_string()
                    );
                    emit(StreamEvent::Retrying {
                        attempt: attempt + 1,
                    })?;
                    std::thread::sleep(retry_backoff(attempt));
                    continue;
                }
                Err(error) => return handle_response_error(*error),
            };
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
                    crate::log_warn!(
                        "llm",
                        "retrying after stream error",
                        attempt = attempt + 1,
                        error = error.clone()
                    );
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

    /// Builds and sends the request once with the configured timeouts. Retry
    /// policy belongs to the caller: streaming paths own their retry loop,
    /// `send_with_retry` wraps this for one-shot calls.
    fn send_once(&self, url: &str, body: &Value) -> Result<ureq::Response, Box<ureq::Error>> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(self.connect_timeout)
            .timeout_read(self.read_timeout)
            .build();
        let mut request = agent
            .post(url)
            .set("Accept", "text/event-stream")
            .set("Content-Type", "application/json")
            .set("session-id", &self.prompt_cache_key)
            .set("thread-id", &self.prompt_cache_key)
            .set("x-client-request-id", &self.prompt_cache_key);
        // The official Anthropic API authenticates with x-api-key; gateways keep
        // the Bearer scheme. anthropic-version is required on Anthropic requests.
        if is_official_anthropic_url(url) {
            request = request.set("x-api-key", &self.api_key);
        } else {
            request = request.set("Authorization", &format!("Bearer {}", self.api_key));
        }
        if self.provider_kind == ProviderKind::AnthropicMessages {
            request = request.set("anthropic-version", ANTHROPIC_VERSION);
        }
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
        let response = request.send_json(body.clone()).map_err(Box::new)?;
        let saw_turn_state = capture_turn_state(&response, &self.turn_state);
        if cache_debug_enabled() {
            eprintln!(
                "[jucode-cache] response turn_state_header={} turn_state_stored={}",
                saw_turn_state,
                self.turn_state.get().is_some()
            );
        }
        Ok(response)
    }

    /// Send the request with the configured timeouts, retrying on transport
    /// errors, 429, and 5xx responses. Other 4xx responses are returned
    /// immediately without retry.
    fn send_with_retry(
        &self,
        url: &str,
        body: &Value,
        emit: &mut impl FnMut(StreamEvent) -> Result<(), String>,
    ) -> Result<ureq::Response, String> {
        let max_attempts = self.max_attempts();
        for attempt in 1..=max_attempts {
            match self.send_once(url, body) {
                Ok(response) => return Ok(response),
                Err(error) if attempt < max_attempts && is_retryable_error(&error) => {
                    crate::log_warn!(
                        "llm",
                        "retrying request",
                        attempt = attempt + 1,
                        error = error.to_string()
                    );
                    emit(StreamEvent::Retrying {
                        attempt: attempt + 1,
                    })?;
                    std::thread::sleep(retry_backoff(attempt));
                }
                Err(error) => return handle_response_error(*error),
            }
        }
        unreachable!("retry loop always returns a response or error")
    }

    fn tool_definitions(&self) -> Vec<Value> {
        let mut definitions = tools::definitions();
        if std::env::var("JUCODE_DESKTOP").is_ok() {
            definitions.push(tools::browser_open_definition());
        }
        if self.allow_subagents && self.subagent_manager.is_some() {
            definitions.extend(subagent_definitions());
        }
        definitions.extend(self.extensions.definitions());
        definitions.extend(self.mcp.definitions());
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
        } else if request.name.starts_with("mcp__") {
            match self.mcp.run_tool(&request.name, &request.arguments) {
                Some((output, is_error)) => tools::ToolExecutionResult {
                    model_output: tools::project_model_output(&request.name, &output, cwd),
                    output,
                    is_error,
                },
                None => json_tool_result(
                    json!({ "error": format!("unknown MCP tool: {}", request.name) }),
                    true,
                ),
            }
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
                    .clamp(512, DEFAULT_SUBAGENT_MAX_OUTPUT_TOKENS),
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
            mcp: self.mcp.clone(),
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
            // The child shares the parent's approval channel and inherits the
            // parent's mode as of spawn time; a later /approvals switch does
            // not retarget live subagents (their requests can still be
            // auto-approved core-side if the live mode is looser).
            approval_tx: self.approval_tx.clone(),
            approval_mode: self.approval_mode,
            subagent_manager: Some(manager.clone()),
            agent_path: child_path.clone(),
            agent_depth: child_depth,
            hooks: self.hooks.clone(),
        };

        crate::log_info!(
            "subagent",
            "spawned",
            task = task_name,
            model = model.clone(),
            path = child_path.clone()
        );
        std::thread::spawn(move || {
            child_manager.mark_running(&child_path);
            let mut stats = SubagentTurnStats::default();
            let result = child.run_turn_events(child_input, &child_cwd, |event| {
                if slot
                    .interrupt_flag
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return Err("interrupted".to_string());
                }
                stats.record(event);
                Ok(())
            });
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let run_result = SubagentRunResult {
                summary: truncate_subagent_output(&stats.output_text),
                partial_output: truncate_subagent_output(&stats.output_text),
                tool_calls: stats.tool_calls,
                tools_used: stats.tools_used,
                input_tokens: stats.input_tokens,
                cached_input_tokens: stats.cached_input_tokens,
                output_tokens: stats.output_tokens,
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
        let result = manager.close_agent(&self.agent_path, target);
        if result.is_ok() {
            crate::log_info!("subagent", "closed", target = target);
        }
        result
    }

    fn append_queued_subagent_messages(
        &self,
        input: &mut Vec<Value>,
        emit: &mut impl FnMut(StreamEvent) -> Result<(), String>,
    ) -> Result<(), String> {
        let Some(manager) = &self.subagent_manager else {
            return Ok(());
        };
        for message in manager.drain_messages(&self.agent_path) {
            let item = json!({
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": format!("<subagent_message>\n{message}\n</subagent_message>")
                }]
            });
            inject_input_item(input, item, emit)?;
        }
        Ok(())
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

    /// Tools whose side effects warrant a user decision before they run under
    /// this client's approval mode. Only gated when an approval handler is
    /// wired (interactive serve / TUI); the class-per-mode policy lives in
    /// `ApprovalMode::requires_approval`. Requests that do go out may still be
    /// auto-approved core-side by the session allowlist or a looser live mode.
    fn needs_approval(&self, name: &str) -> bool {
        if self.approval_tx.is_none() {
            return false;
        }
        // MCP tools consult the server's readOnlyHint annotation; a tool
        // without a cached hint falls through to the conservative name check.
        if let Some(read_only_hint) = self.mcp.tool_read_only_hint(name) {
            return self.approval_mode.requires_approval_for_mcp(read_only_hint);
        }
        self.approval_mode.requires_approval(name)
    }

    /// Blocks until the core forwards the user's decision. A dropped channel
    /// (interrupt / no handler) is treated as a denial so the worker unblocks.
    /// Edit tool calls carry their hunk breakdown so the decision may approve
    /// only a subset of the change.
    fn request_approval(&self, request: &ToolCallRequest, cwd: &Path) -> ApprovalDecision {
        let Some(tx) = &self.approval_tx else {
            return ApprovalDecision::allow_all();
        };
        let (response_tx, response_rx) = mpsc::channel();
        if tx
            .send(ApprovalRequest {
                call_id: request.call_id.clone(),
                name: request.name.clone(),
                summary: approval_summary(&request.name, &request.arguments),
                subagent_id: (self.agent_depth > 0).then(|| self.agent_path.clone()),
                hunks: hunks::plan_edit_hunks(&request.name, &request.arguments, cwd),
                response_tx,
            })
            .is_err()
        {
            return ApprovalDecision::deny();
        }
        response_rx
            .recv()
            .unwrap_or_else(|_| ApprovalDecision::deny())
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

/// The user's hunk selection could not be applied to the call (e.g. the patch
/// no longer splits); nothing was executed.
fn hunk_selection_failed_result(error: &str) -> tools::ToolExecutionResult {
    let output = json!({
        "error": format!("selective approval failed: {error}. The call was not executed; ask the user how to proceed or retry with a smaller change.")
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
        "bash" | "execute" | "exec_command" | "shell_command" => field("command")
            .or_else(|| field("cmd"))
            .unwrap_or_default(),
        "write_stdin" => field("text").or_else(|| field("chars")).unwrap_or_default(),
        _ => field("path").unwrap_or_default(),
    }
}

/// Appends a batch of tool results to the model input: every
/// function_call_output first, then any extracted image items. Anthropic
/// requires all tool_result blocks to lead the next user message, so an image
/// must never sit between two outputs. Failed calls carry `is_error` so the
/// Anthropic conversion can surface it; the OpenAI sanitizer strips it.
fn push_tool_result_items(input: &mut Vec<Value>, tool_results: Vec<ToolCallResult>) {
    let mut image_items = Vec::new();
    for tool_result in tool_results {
        if let Some(image_item) = tools::image_content_item(&tool_result.result.output) {
            image_items.push(image_item);
        }
        let mut item = json!({
            "type": "function_call_output",
            "call_id": tool_result.request.call_id,
            "output": tool_result.result.model_output
        });
        if tool_result.result.is_error {
            item["is_error"] = json!(true);
        }
        input.push(item);
    }
    input.append(&mut image_items);
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
            crate::log_error!("llm", "http request failed", status = code);
            Err(format!("LLM API returned HTTP {code}: {body}"))
        }
        error => {
            crate::log_error!("llm", "http request failed", error = error.to_string());
            Err(error.to_string())
        }
    }
}

fn estimate_text_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let chars = text.chars().count() as u64;
    u64::max(1, chars.div_ceil(4))
}

/// Resolves the Messages endpoint from the configured base URL. A base that
/// already ends in `/v1` (any host) or targets the official Anthropic host
/// gets the plain `/v1/messages` path; other bases are treated as gateways
/// with an `/anthropic/v1` mount.
fn anthropic_messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        return format!("{base}/messages");
    }
    if base.ends_with("/anthropic") || is_official_anthropic_url(base) {
        return format!("{base}/v1/messages");
    }
    format!("{base}/anthropic/v1/messages")
}

/// True when the URL targets the official Anthropic API host.
fn is_official_anthropic_url(url: &str) -> bool {
    url.strip_prefix("https://api.anthropic.com")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn is_anthropic_model(model: &str) -> bool {
    model.starts_with("claude-")
}

fn responses_input_to_anthropic_messages(input: &[Value], include_thinking: bool) -> Vec<Value> {
    let mut messages = Vec::new();
    // Calls whose arguments never parsed as JSON must not be replayed as normal
    // tool_use blocks; their tool_result is skipped too so pairs stay balanced.
    let mut skipped_call_ids: HashSet<String> = HashSet::new();
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
            "redacted_thinking" => {
                let data = item.get("data").and_then(Value::as_str).unwrap_or_default();
                // Redacted thinking carries opaque data instead of a signature and
                // must be replayed verbatim or the API rejects the history.
                if include_thinking && !data.is_empty() {
                    push_anthropic_content(
                        &mut messages,
                        "assistant",
                        json!({ "type": "redacted_thinking", "data": data }),
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
                push_anthropic_content(&mut messages, "user", block);
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

/// Injects a runtime item (reminder, queued subagent message) into the model
/// input and mirrors it to the session via `emit`, so next-turn projection
/// matches what this turn actually sent. Injections happen outside the
/// stream-retry loops in `create_*_streaming`, so each one is emitted exactly
/// once even when the surrounding request is retried.
fn inject_input_item(
    input: &mut Vec<Value>,
    item: Value,
    emit: &mut impl FnMut(StreamEvent) -> Result<(), String>,
) -> Result<(), String> {
    emit(StreamEvent::ResponseItem(item.clone()))?;
    input.push(item);
    Ok(())
}

/// Retry on transport errors (timeouts, dropped connections, DNS), 429 rate
/// limits, and 5xx responses. Other 4xx responses are client errors and are
/// never retried.
fn is_retryable_error(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Status(code, _) => *code == 429 || *code >= 500,
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
        "stream closed before message_stop",
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
    is_stream_decode_error(message)
        || is_retryable_response_failed(message)
        || is_retryable_anthropic_error(message)
}

/// Anthropic in-stream `error` events for transient conditions are safe to
/// re-send; other error types (invalid_request, authentication, ...) are not.
fn is_retryable_anthropic_error(message: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(message) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("error") {
        return false;
    }
    let kind = value
        .get("error")
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    matches!(kind, "overloaded_error" | "api_error")
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
    // Accumulates streamed function-call fields keyed by the stable item id.
    // The Responses API streams a tool call's name/call_id in
    // `response.output_item.added` and its arguments via
    // `response.function_call_arguments.delta`, then may deliver the final
    // `response.output_item.done` item with those fields blanked out —
    // expecting the consumer to have stitched the pieces together.
    let mut streamed_calls: BTreeMap<String, StreamedFunctionCall> = BTreeMap::new();
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
            if handle_sse_data(&data, &mut emit, &mut output_items, &mut streamed_calls)? {
                completed = true;
                break;
            }
        }
    }

    if !data_lines.is_empty() {
        let data = data_lines.join("\n");
        if data != "[DONE]" {
            completed = handle_sse_data(&data, &mut emit, &mut output_items, &mut streamed_calls)?;
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
                emit(StreamEvent::Delta(delta.to_string()))?;
            }
        }
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                emit(StreamEvent::ReasoningDelta(delta.to_string()))?;
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
                emit(StreamEvent::ResponseItem(item.clone()))?;
                output_items.push(item);
            }
        }
        // `response.incomplete` is a normal terminal event (e.g. max_output_tokens
        // reached): accept the parsed output instead of failing the stream.
        "response.completed" | "response.incomplete" if output_items.is_empty() => {
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
                    let item = backfill_function_call(item.clone(), streamed_calls);
                    emit(StreamEvent::ResponseItem(item.clone()))?;
                    output_items.push(item);
                }
            }
            return Ok(true);
        }
        "response.completed" | "response.incomplete" => {
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
        "response.failed" | "error" => {
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
    stop_reason: String,
    /// Name of a tool_use block whose accumulated arguments never parsed as
    /// JSON (typically a max_tokens truncation). Turns the stream into an
    /// error at message_stop instead of executing a broken call.
    invalid_tool_use: Option<String>,
}

struct AnthropicBlock {
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

fn read_anthropic_sse_output(
    reader: impl std::io::Read,
    mut emit: impl FnMut(StreamEvent) -> Result<(), String>,
) -> Result<Vec<Value>, String> {
    let mut state = AnthropicStreamState::default();
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
            if handle_anthropic_sse_data(&data, &mut emit, &mut state)? {
                completed = true;
                break;
            }
        }
    }

    if !completed && !data_lines.is_empty() {
        let data = data_lines.join("\n");
        completed = handle_anthropic_sse_data(&data, &mut emit, &mut state)?;
    }

    if !completed {
        return Err("stream closed before message_stop".to_string());
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
            let (input_tokens, cached_input_tokens) = normalize_anthropic_usage(usage);
            state.input_tokens = input_tokens;
            state.cached_input_tokens = cached_input_tokens;
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
                if block.kind == "tool_use"
                    && serde_json::from_str::<Value>(&anthropic_tool_use_arguments(&block)).is_err()
                {
                    state.invalid_tool_use = Some(block.name);
                } else if let Some(item) = anthropic_block_to_response_item(block) {
                    emit(StreamEvent::ResponseItem(item.clone()))?;
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
        let (input_tokens, cached_input_tokens) = normalize_anthropic_usage(Some(usage));
        emit(StreamEvent::Usage {
            input_tokens,
            cached_input_tokens,
            output_tokens: usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            reasoning_tokens: 0,
        })?;
    }
    Ok(output_items)
}

/// Normalizes Anthropic usage to OpenAI semantics: OpenAI's `cached_tokens` is
/// a subset of `input_tokens`, while Anthropic's `input_tokens` excludes both
/// `cache_read_input_tokens` and `cache_creation_input_tokens` (disjoint
/// counts). Downstream consumers subtract `cached_input_tokens` from
/// `input_tokens`, so report total = input + cache_read + cache_creation and
/// cached = cache_read. Cache-creation tokens are priced as regular input.
fn normalize_anthropic_usage(usage: Option<&Value>) -> (u64, u64) {
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
        // Redacted thinking must be preserved verbatim (via its opaque data)
        // or replaying the history is rejected by the API.
        "redacted_thinking" if !block.data.is_empty() => Some(json!({
            "type": "redacted_thinking",
            "data": block.data
        })),
        "tool_use" => {
            let arguments = anthropic_tool_use_arguments(&block);
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
fn anthropic_tool_use_arguments(block: &AnthropicBlock) -> String {
    let trimmed = block.arguments.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    match &block.input {
        Value::Object(map) if !map.is_empty() => block.input.to_string(),
        _ => "{}".to_string(),
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

fn normalized_arguments(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        "{}".to_string()
    } else {
        trimmed.to_string()
    }
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
    fn openai_streamed_tool_call_backfills_blanked_done_item() {
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
        let items = read_sse_output(sse.as_bytes(), |_| Ok(())).unwrap();
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
    fn openai_argumentless_tool_call_defaults_to_empty_object() {
        // A genuinely argument-less call with no deltas must still yield "{}"
        // rather than an empty string that fails JSON parsing.
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_2\",\"call_id\":\"call_2\",\"name\":\"list_agents\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let items = read_sse_output(sse.as_bytes(), |_| Ok(())).unwrap();
        let call = items
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("function_call item present");
        assert_eq!(call["arguments"], "{}");
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
    fn injected_input_items_are_emitted_for_session_persistence() {
        // Runtime injections (reminders, queued subagent messages) must reach
        // the session via emit, or the next turn's projection would miss an
        // item the model already saw this turn.
        let mut input = vec![json!({ "role": "user", "content": [] })];
        let mut emitted = Vec::new();
        inject_input_item(&mut input, runtime_reminder_item(), &mut |event| {
            emitted.push(event);
            Ok(())
        })
        .unwrap();

        assert_eq!(input.len(), 2);
        assert_eq!(emitted.len(), 1);
        match &emitted[0] {
            StreamEvent::ResponseItem(item) => assert_eq!(item, &input[1]),
            other => panic!("expected ResponseItem, got {other:?}"),
        }
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
        assert!(!should_run_parallel_tools(std::slice::from_ref(&read)));
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
    fn anthropic_stream_without_message_stop_is_a_retryable_error() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        );
        let error = read_anthropic_sse_output(sse.as_bytes(), |_| Ok(()))
            .expect_err("truncated stream should fail");

        assert!(error.contains("stream closed before message_stop"));
        assert!(is_retryable_stream_error(&error));
    }

    #[test]
    fn openai_incomplete_response_accepts_parsed_output() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"partial answer\"}]}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":3,\"output_tokens\":7}}}\n\n",
        );
        let mut usage_seen = false;
        let items = read_sse_output(sse.as_bytes(), |event| {
            if matches!(event, StreamEvent::Usage { .. }) {
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
    fn openai_stream_error_event_is_a_non_retryable_error() {
        let sse =
            "data: {\"type\":\"error\",\"code\":\"invalid_api_key\",\"message\":\"bad key\"}\n\n";
        let error = read_sse_output(sse.as_bytes(), |_| Ok(()))
            .expect_err("in-stream error event should fail");

        assert!(error.contains("invalid_api_key"));
        assert!(!is_retryable_stream_error(&error));
    }

    #[test]
    fn anthropic_truncated_tool_use_errors_instead_of_executing() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"write\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.txt\\\",\\\"content\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":9}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut items_emitted = 0;
        let error = read_anthropic_sse_output(sse.as_bytes(), |event| {
            if matches!(event, StreamEvent::ResponseItem(_)) {
                items_emitted += 1;
            }
            Ok(())
        })
        .expect_err("truncated tool_use should fail");

        assert!(error.contains("write"));
        assert!(error.contains("max_tokens"));
        assert_eq!(items_emitted, 0);
        assert!(!is_retryable_stream_error(&error));
    }

    #[test]
    fn anthropic_tool_use_input_from_block_start_used_without_deltas() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"read\",\"input\":{\"path\":\"Cargo.toml\"}}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let items = read_anthropic_sse_output(sse.as_bytes(), |_| Ok(())).unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        let parsed: Value = serde_json::from_str(items[0]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["path"], "Cargo.toml");
    }

    #[test]
    fn anthropic_redacted_thinking_is_preserved_and_replayed() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque-bytes\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let items = read_anthropic_sse_output(sse.as_bytes(), |_| Ok(())).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "redacted_thinking");
        assert_eq!(items[0]["data"], "opaque-bytes");

        // Non-streaming path preserves the block too.
        let block = json!({ "type": "redacted_thinking", "data": "opaque-bytes" });
        let item = anthropic_content_block_to_response_item(&block).unwrap();
        assert_eq!(item["type"], "redacted_thinking");

        // Projection back to Anthropic messages replays it verbatim.
        let messages = responses_input_to_anthropic_messages(&items, true);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"][0]["type"], "redacted_thinking");
        assert_eq!(messages[0]["content"][0]["data"], "opaque-bytes");

        // The OpenAI path must not see Anthropic-only items.
        assert!(sanitize_openai_input(items).is_empty());
    }

    #[test]
    fn anthropic_transient_stream_errors_are_retryable() {
        assert!(is_retryable_stream_error(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
        ));
        assert!(is_retryable_stream_error(
            r#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#
        ));
        assert!(!is_retryable_stream_error(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#
        ));
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

        let messages = responses_input_to_anthropic_messages(&input, false);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(messages[0]["content"][0]["id"], "call_ok");
        assert_eq!(messages[0]["content"][0]["input"], json!({}));
    }

    #[test]
    fn retry_policy_retries_429_and_5xx_but_not_other_4xx() {
        assert!(!is_retryable_error(&ureq::Error::Status(
            400,
            ureq::Response::new(400, "Bad Request", "").unwrap()
        )));
        assert!(is_retryable_error(&ureq::Error::Status(
            429,
            ureq::Response::new(429, "Too Many Requests", "").unwrap()
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
    fn anthropic_usage_is_normalized_to_openai_subset_semantics() {
        // Anthropic reports input, cache_read and cache_creation disjointly;
        // normalized: input = 10 + 90 + 20 = 120, cached = 90.
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":90,\"cache_creation_input_tokens\":20}}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut usage = None;
        read_anthropic_sse_output(sse.as_bytes(), |event| {
            if let StreamEvent::Usage {
                input_tokens,
                cached_input_tokens,
                ..
            } = event
            {
                usage = Some((input_tokens, cached_input_tokens));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(usage, Some((120, 90)));

        // The non-streaming path normalizes identically.
        let message = json!({
            "content": [],
            "usage": {
                "input_tokens": 10,
                "cache_read_input_tokens": 90,
                "cache_creation_input_tokens": 20,
                "output_tokens": 5
            }
        });
        let mut usage = None;
        emit_anthropic_message(&message, &mut |event| {
            if let StreamEvent::Usage {
                input_tokens,
                cached_input_tokens,
                ..
            } = event
            {
                usage = Some((input_tokens, cached_input_tokens));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(usage, Some((120, 90)));
    }

    #[test]
    fn builds_anthropic_messages_url_for_gateway_and_official_bases() {
        assert_eq!(
            anthropic_messages_url("https://api.jucode.cn"),
            "https://api.jucode.cn/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.jucode.cn/anthropic"),
            "https://api.jucode.cn/anthropic/v1/messages"
        );
        // A base already ending in /v1 exposes the endpoint directly.
        assert_eq!(
            anthropic_messages_url("https://api.jucode.cn/v1"),
            "https://api.jucode.cn/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn official_anthropic_host_is_detected_exactly() {
        assert!(is_official_anthropic_url("https://api.anthropic.com"));
        assert!(is_official_anthropic_url(
            "https://api.anthropic.com/v1/messages"
        ));
        assert!(!is_official_anthropic_url(
            "https://api.jucode.cn/anthropic/v1/messages"
        ));
        assert!(!is_official_anthropic_url(
            "https://api.anthropic.com.evil.example/v1/messages"
        ));
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

    fn test_client() -> OpenAiClient {
        OpenAiClient::from_config(OpenAiClientConfig {
            model: "test-model".to_string(),
            protocol: "responses".to_string(),
            reasoning_effort: "medium".to_string(),
            model_reasoning_efforts: Vec::new(),
            system_prompt: "system".to_string(),
            prompt_cache_key: "cache-key".to_string(),
            extensions: ExtensionRegistry::load(&[], Path::new("."), Path::new(".")),
            mcp: McpManager::default(),
            base_url: "https://api.jucode.cn/v1".to_string(),
            max_output_tokens: 2048,
            api_key: Some("test-key"),
            api_key_env: "JUCODE_TEST_API_KEY",
            retry_attempts: 1,
            connect_timeout: Duration::from_secs(1),
            read_timeout: Duration::from_secs(1),
            goal_tool_tx: None,
            approval_tx: None,
            approval_mode: ApprovalMode::default(),
            subagent_manager: None,
            hooks: Hooks::default(),
        })
        .unwrap()
    }

    fn approval_test_client(
        mode: ApprovalMode,
        approval_tx: Option<Sender<ApprovalRequest>>,
    ) -> OpenAiClient {
        let mut client = test_client();
        client.approval_mode = mode;
        client.approval_tx = approval_tx;
        client
    }

    #[test]
    fn needs_approval_follows_mode_per_tool_class() {
        let (tx, _rx) = mpsc::channel();
        let cases = [
            (ApprovalMode::ReadOnly, "bash", true),
            (ApprovalMode::ReadOnly, "write_stdin", true),
            (ApprovalMode::ReadOnly, "write", true),
            (ApprovalMode::ReadOnly, "apply_patch", true),
            (ApprovalMode::ReadOnly, "read", false),
            (ApprovalMode::AutoEdit, "bash", true),
            (ApprovalMode::AutoEdit, "str_replace", false),
            (ApprovalMode::AutoEdit, "hashline_edit", false),
            (ApprovalMode::FullAuto, "bash", false),
            (ApprovalMode::FullAuto, "write", false),
        ];
        for (mode, tool, expected) in cases {
            let client = approval_test_client(mode, Some(tx.clone()));
            assert_eq!(
                client.needs_approval(tool),
                expected,
                "{tool} under {}",
                mode.as_str()
            );
        }
    }

    #[test]
    fn needs_approval_is_disabled_without_a_handler() {
        let client = approval_test_client(ApprovalMode::ReadOnly, None);
        assert!(!client.needs_approval("bash"));
    }

    #[test]
    fn mcp_tools_gate_by_read_only_hint() {
        let (tx, _rx) = mpsc::channel();
        let tools = json!([
            { "name": "lookup", "inputSchema": { "type": "object" },
              "annotations": { "readOnlyHint": true } },
            { "name": "mutate", "inputSchema": { "type": "object" } }
        ]);
        let cases = [
            (ApprovalMode::ReadOnly, "mcp__srv__lookup", true),
            (ApprovalMode::ReadOnly, "mcp__srv__mutate", true),
            (ApprovalMode::AutoEdit, "mcp__srv__lookup", false),
            (ApprovalMode::AutoEdit, "mcp__srv__mutate", true),
            (ApprovalMode::FullAuto, "mcp__srv__lookup", false),
            (ApprovalMode::FullAuto, "mcp__srv__mutate", false),
            // Unknown MCP names have no hint and gate conservatively.
            (ApprovalMode::AutoEdit, "mcp__other__tool", true),
        ];
        for (mode, tool, expected) in cases {
            let mut client = approval_test_client(mode, Some(tx.clone()));
            client.mcp =
                crate::mcp::test_support::manager_with_tools("srv", tools.clone(), json!({}));
            assert_eq!(
                client.needs_approval(tool),
                expected,
                "{tool} under {}",
                mode.as_str()
            );
        }
    }

    #[test]
    fn subagent_approval_request_carries_subagent_id_and_blocks_for_answer() {
        let (tx, rx) = mpsc::channel();
        let mut client = approval_test_client(ApprovalMode::ReadOnly, Some(tx));
        client.agent_path = "/root/worker".to_string();
        client.agent_depth = 1;

        let responder = thread::spawn(move || {
            let request: ApprovalRequest = rx.recv().unwrap();
            assert_eq!(request.name, "bash");
            assert_eq!(request.subagent_id.as_deref(), Some("/root/worker"));
            assert_eq!(request.summary, "rm -rf build");
            assert!(request.hunks.is_none(), "non-edit tools carry no hunks");
            request
                .response_tx
                .send(ApprovalDecision::allow_all())
                .unwrap();
        });

        let decision = client.request_approval(
            &ToolCallRequest {
                call_id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments: json!({ "command": "rm -rf build" }).to_string(),
            },
            Path::new("."),
        );
        assert!(decision.allow);
        assert!(decision.approved_hunks.is_none());
        responder.join().unwrap();
    }

    #[test]
    fn main_agent_approval_request_has_no_subagent_id_and_denies_on_drop() {
        let (tx, rx) = mpsc::channel();
        let client = approval_test_client(ApprovalMode::ReadOnly, Some(tx));

        let responder = thread::spawn(move || {
            let request: ApprovalRequest = rx.recv().unwrap();
            assert_eq!(request.subagent_id, None);
            // Dropping the responder (interrupt / handler gone) must deny.
            drop(request);
        });

        let decision = client.request_approval(
            &ToolCallRequest {
                call_id: "call_2".to_string(),
                name: "write".to_string(),
                arguments: json!({ "path": "a.txt" }).to_string(),
            },
            Path::new("."),
        );
        assert!(!decision.allow);
        responder.join().unwrap();
    }

    #[test]
    fn edit_tool_approval_request_carries_hunks_and_returns_the_subset() {
        let dir = std::env::temp_dir().join(format!(
            "jucode-llm-approval-hunks-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "old\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let client = approval_test_client(ApprovalMode::ReadOnly, Some(tx));

        let responder = thread::spawn(move || {
            let request: ApprovalRequest = rx.recv().unwrap();
            let hunks = request.hunks.as_ref().expect("write must carry hunks");
            assert_eq!(hunks.len(), 1, "write is a single all-or-nothing hunk");
            assert_eq!(hunks[0].id, "f0h1");
            request
                .response_tx
                .send(ApprovalDecision {
                    allow: true,
                    approved_hunks: Some(vec!["f0h1".to_string()]),
                })
                .unwrap();
        });

        let decision = client.request_approval(
            &ToolCallRequest {
                call_id: "call_3".to_string(),
                name: "write".to_string(),
                arguments: json!({ "path": "a.txt", "content": "new\n" }).to_string(),
            },
            &dir,
        );
        assert!(decision.allow);
        assert_eq!(decision.approved_hunks, Some(vec!["f0h1".to_string()]));
        responder.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn openai_responses_body_sets_output_cap_store_and_reasoning_include() {
        let body = test_client().openai_responses_body(Vec::new());

        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn openai_summarize_body_sets_store_false_and_output_cap() {
        let body = test_client().openai_summarize_body("sys", "user");

        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], 2048);
    }

    #[test]
    fn retrying_event_discards_replayed_subagent_deltas() {
        let mut stats = SubagentTurnStats::default();
        stats.record(StreamEvent::CallStart);
        stats.record(StreamEvent::Delta("first ".to_string()));
        stats.record(StreamEvent::CallStart);
        stats.record(StreamEvent::Delta("dup".to_string()));
        stats.record(StreamEvent::Retrying { attempt: 2 });
        stats.record(StreamEvent::Delta("second".to_string()));

        assert_eq!(stats.output_text, "first second");
    }

    #[test]
    fn tool_result_images_follow_all_function_call_outputs() {
        let image_output =
            json!({ "kind": "image", "mime": "image/png", "base64": "aGk=" }).to_string();
        let results = vec![
            ToolCallResult {
                request: ToolCallRequest {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                },
                result: tools::ToolExecutionResult {
                    output: image_output,
                    model_output: "image attached".to_string(),
                    is_error: false,
                },
            },
            ToolCallResult {
                request: ToolCallRequest {
                    call_id: "call_2".to_string(),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                },
                result: tools::ToolExecutionResult {
                    output: "text".to_string(),
                    model_output: "text".to_string(),
                    is_error: true,
                },
            },
        ];
        let mut input = Vec::new();

        push_tool_result_items(&mut input, results);

        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0].get("is_error"), None);
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_2");
        assert_eq!(input[1]["is_error"], true);
        assert_eq!(input[2]["content"][0]["type"], "input_image");
    }

    #[test]
    fn error_function_call_output_maps_to_anthropic_is_error() {
        let input = vec![
            json!({ "type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{}" }),
            json!({ "type": "function_call_output", "call_id": "call_1", "output": "boom", "is_error": true }),
        ];

        let messages = responses_input_to_anthropic_messages(&input, false);

        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["is_error"], true);
    }

    #[test]
    fn sanitizer_strips_is_error_annotation_for_openai() {
        let sanitized = sanitize_openai_input(vec![json!({
            "type": "function_call_output",
            "call_id": "call_1",
            "output": "boom",
            "is_error": true
        })]);

        assert_eq!(sanitized.len(), 1);
        assert!(sanitized[0].get("is_error").is_none());
        assert_eq!(sanitized[0]["output"], "boom");
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
