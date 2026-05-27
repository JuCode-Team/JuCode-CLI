use crate::{session::extract_response_text, tools};
use serde_json::{json, Value};
use std::{
    env,
    error::Error as StdError,
    io::{self, BufRead, BufReader},
    path::Path,
    time::Duration,
};

const SYSTEM_PROMPT: &str = "You are JuCode, a lightweight coding agent. Use tools when you need filesystem or shell access. Keep responses concise and factual.";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct OpenAiClient {
    api_key: String,
    pub model: String,
    base_url: String,
    retry_attempts: usize,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    CallStart,
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
    },
}

impl OpenAiClient {
    pub fn from_config(
        model: String,
        base_url: String,
        api_key: Option<&str>,
        api_key_env: &str,
        retry_attempts: usize,
    ) -> Result<Self, String> {
        let api_key = match api_key {
            Some(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => env::var(api_key_env).map_err(|_| {
                format!("api_key is not set and {api_key_env} is not set. Configure one before sending prompts.")
            })?,
        };
        Ok(Self {
            api_key,
            model,
            base_url,
            retry_attempts,
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
            let output_items = self.create_response_streaming(input.clone(), &mut emit)?;
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
                let result = tools::run_tool_with_events(&name, arguments, cwd, |event| {
                    let tools::ToolExecutionEvent::Update(output) = event;
                    emit(StreamEvent::ToolUpdate {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        output,
                    })
                });
                let output = result.output;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output
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

    fn create_response_streaming(
        &self,
        input: Vec<Value>,
        mut emit: impl FnMut(StreamEvent) -> Result<(), String>,
    ) -> Result<Vec<Value>, String> {
        let body = json!({
            "model": self.model,
            "instructions": SYSTEM_PROMPT,
            "input": input,
            "tools": tools::definitions(),
            "tool_choice": "auto",
            "stream": true
        });

        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(CONNECT_TIMEOUT)
            .build();

        let mut response = None;
        let max_attempts = self.retry_attempts.saturating_add(1).max(1);
        for attempt in 1..=max_attempts {
            let result = agent
                .post(&url)
                .set("Authorization", &format!("Bearer {}", self.api_key))
                .set("Accept", "text/event-stream")
                .set("Content-Type", "application/json")
                .send_json(body.clone());

            match result {
                Ok(value) => {
                    response = Some(value);
                    break;
                }
                Err(error) if attempt < max_attempts && is_timeout_error(&error) => {
                    emit(StreamEvent::Retrying {
                        attempt: attempt + 1,
                    })?;
                }
                Err(error) => return handle_response_error(error),
            }
        }

        let response = response.expect("response must be set or returned as error");
        let content_type = response
            .header("content-type")
            .unwrap_or_default()
            .to_string();
        if !content_type.contains("text/event-stream") && !content_type.contains("application/json")
        {
            let body = response.into_string().map_err(|error| error.to_string())?;
            let snippet = truncate_error_body(&body);
            return Err(format!(
                "OpenAI API returned non-JSON response from {url} (content-type: {content_type}). Check base_url; OpenAI-compatible endpoints usually end with /v1. Body starts: {snippet}"
            ));
        }
        if content_type.contains("application/json") {
            let body = response.into_string().map_err(|error| error.to_string())?;
            let value = serde_json::from_str::<Value>(&body).map_err(|error| error.to_string())?;
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
            if let Some((input_tokens, output_tokens)) = extract_usage(&value) {
                emit(StreamEvent::Usage {
                    input_tokens,
                    output_tokens,
                })?;
            }
            return Ok(output_items);
        }
        read_sse_output(response.into_reader(), emit)
    }
}

fn handle_response_error<T>(error: ureq::Error) -> Result<T, String> {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response
                .into_string()
                .unwrap_or_else(|_| "<failed to read error body>".to_string());
            Err(format!("OpenAI API returned HTTP {code}: {body}"))
        }
        error => Err(error.to_string()),
    }
}

fn is_timeout_error(error: &ureq::Error) -> bool {
    if error.to_string().to_ascii_lowercase().contains("timed out") {
        return true;
    }
    let ureq::Error::Transport(error) = error else {
        return false;
    };
    error.kind() == ureq::ErrorKind::Io
        && error
            .source()
            .and_then(|source| source.downcast_ref::<io::Error>())
            .is_some_and(|error| error.kind() == io::ErrorKind::TimedOut)
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
        "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                emit(StreamEvent::ResponseItem(item.clone()))?;
                output_items.push(item.clone());
            }
        }
        "response.completed" if output_items.is_empty() => {
            if let Some(response) = event.get("response") {
                if let Some((input_tokens, output_tokens)) = extract_usage(response) {
                    emit(StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
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
                if let Some((input_tokens, output_tokens)) = extract_usage(response) {
                    emit(StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
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

fn extract_usage(value: &Value) -> Option<(u64, u64)> {
    let usage = value.get("usage")?;
    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64)?;
    let output_tokens = usage.get("output_tokens").and_then(Value::as_u64)?;
    Some((input_tokens, output_tokens))
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
