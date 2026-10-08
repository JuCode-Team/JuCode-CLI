//! What a running subagent is doing, kept for the front-ends: a bounded
//! transcript of its messages and tool calls, its latest action as one line,
//! and its usage. The desktop's agent trace reads it through `agent_runs`
//! (every agent, one row each) and `subagent_transcript` (one agent's work).

use crate::llm::StreamEvent;
use serde_json::{json, Value};

/// At most this many transcript items per agent; older ones are dropped
/// behind one "earlier steps trimmed" marker.
const MAX_ITEMS: usize = 300;
/// At most this much text per agent across its items.
const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Per tool call: arguments and output are cut to this.
const MAX_TOOL_FIELD_BYTES: usize = 4 * 1024;
/// Per reasoning item.
const MAX_REASONING_BYTES: usize = 2 * 1024;
const MAX_ACTIVITY_CHARS: usize = 120;

#[derive(Debug, Clone, PartialEq)]
enum Item {
    Task(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        call_id: String,
        name: String,
        input: String,
        output: String,
        running: bool,
        is_error: bool,
    },
    Note(String),
}

impl Item {
    fn bytes(&self) -> usize {
        match self {
            Item::Task(text) | Item::Assistant(text) | Item::Reasoning(text) | Item::Note(text) => {
                text.len()
            }
            Item::Tool {
                input,
                output,
                name,
                ..
            } => input.len() + output.len() + name.len(),
        }
    }

    fn json(&self) -> Value {
        match self {
            Item::Task(text) => json!({ "role": "user", "content": text }),
            Item::Assistant(text) => json!({ "role": "assistant", "content": text }),
            Item::Reasoning(text) => json!({ "role": "reasoning", "content": text }),
            Item::Note(text) => json!({ "role": "user", "content": text }),
            Item::Tool {
                call_id,
                name,
                input,
                output,
                running,
                is_error,
            } => json!({
                "role": "tool",
                "call_id": call_id,
                "name": name,
                "input": input,
                "output": output,
                "running": running,
                "is_error": is_error,
            }),
        }
    }
}

/// One subagent's live record of work.
#[derive(Debug, Clone, Default)]
pub(crate) struct SubagentTrace {
    items: Vec<Item>,
    bytes: usize,
    trimmed: bool,
    /// The assistant text of the response in flight (merged into one item).
    streaming: Option<usize>,
    /// Reasoning of the response in flight.
    reasoning: Option<usize>,
    activity: String,
    tool_calls: u64,
    tokens: u64,
    /// Bumped on every change, so a front-end refresh can skip idle agents.
    pub(crate) revision: u64,
}

impl SubagentTrace {
    pub(crate) fn new(task: &str) -> Self {
        let mut trace = Self::default();
        trace.push(Item::Task(cut(task, MAX_TEXT_BYTES / 4)));
        trace.activity = "Starting".to_string();
        trace
    }

    pub(crate) fn activity(&self) -> &str {
        &self.activity
    }

    pub(crate) fn tool_calls(&self) -> u64 {
        self.tool_calls
    }

    pub(crate) fn tokens(&self) -> u64 {
        self.tokens
    }

    /// Records one event of the subagent's turn.
    pub(crate) fn record(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::CallStart => {
                self.streaming = None;
                self.reasoning = None;
            }
            StreamEvent::Connected => {
                if self.activity.is_empty() || self.activity == "Starting" {
                    self.set_activity("Thinking".to_string());
                }
            }
            StreamEvent::ReasoningDelta(delta) => {
                match self.reasoning {
                    Some(index) => self.append(index, delta, MAX_REASONING_BYTES),
                    None => {
                        self.push(Item::Reasoning(cut(delta, MAX_REASONING_BYTES)));
                        self.reasoning = Some(self.items.len() - 1);
                    }
                }
                self.set_activity("Thinking".to_string());
            }
            StreamEvent::Delta(delta) => {
                match self.streaming {
                    Some(index) => self.append(index, delta, MAX_TEXT_BYTES / 4),
                    None => {
                        self.push(Item::Assistant(delta.clone()));
                        self.streaming = Some(self.items.len() - 1);
                    }
                }
                self.set_activity("Writing".to_string());
            }
            StreamEvent::Retrying {
                attempt,
                max_attempts,
                reason,
                ..
            } => {
                // A retried response restarts its text.
                if let Some(index) = self.streaming.take() {
                    self.remove(index);
                }
                self.reasoning = None;
                self.set_activity(format!("Retrying {attempt}/{max_attempts}: {reason}"));
            }
            StreamEvent::ResponseItem(item) => {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let call_id = str_field(item, "call_id");
                    let name = str_field(item, "name");
                    let input = str_field(item, "arguments");
                    self.set_activity(describe(&name, &input));
                    self.streaming = None;
                    self.reasoning = None;
                    self.push(Item::Tool {
                        call_id,
                        name,
                        input: cut(&input, MAX_TOOL_FIELD_BYTES),
                        output: String::new(),
                        running: true,
                        is_error: false,
                    });
                }
            }
            StreamEvent::ToolStart { call_id, name } => {
                self.tool_calls += 1;
                if self.tool_index(call_id).is_none() {
                    self.push(Item::Tool {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        input: String::new(),
                        output: String::new(),
                        running: true,
                        is_error: false,
                    });
                    self.set_activity(describe(name, ""));
                }
            }
            StreamEvent::ToolUpdate {
                call_id, output, ..
            } => {
                if let Some(index) = self.tool_index(call_id) {
                    self.set_output(index, output, true, false);
                }
            }
            StreamEvent::ToolOutput {
                call_id,
                output,
                is_error,
                ..
            } => {
                if let Some(index) = self.tool_index(call_id) {
                    self.set_output(index, output, false, *is_error);
                }
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                self.tokens = self.tokens.saturating_add(input_tokens + output_tokens);
                self.revision += 1;
            }
            // Only the main agent is steered; its plan is the one drafted live.
            StreamEvent::Steered(_) | StreamEvent::ToolArgumentsDelta { .. } => {}
        }
    }

    /// A message the parent sent the agent (`send_message`).
    pub(crate) fn note(&mut self, text: &str) {
        self.push(Item::Note(cut(text, MAX_TOOL_FIELD_BYTES)));
    }

    /// The agent ended: running tool calls are no longer running.
    pub(crate) fn finish(&mut self, activity: &str) {
        for item in &mut self.items {
            if let Item::Tool { running, .. } = item {
                *running = false;
            }
        }
        self.set_activity(activity.to_string());
    }

    pub(crate) fn items_json(&self) -> Vec<Value> {
        let mut items = Vec::with_capacity(self.items.len() + 1);
        let mut rest = self.items.iter();
        if let Some(first @ Item::Task(_)) = self.items.first() {
            items.push(first.json());
            rest.next();
        }
        if self.trimmed {
            items.push(json!({ "role": "user", "content": "… earlier steps trimmed" }));
        }
        items.extend(rest.map(Item::json));
        items
    }

    fn tool_index(&self, call_id: &str) -> Option<usize> {
        self.items
            .iter()
            .rposition(|item| matches!(item, Item::Tool { call_id: known, .. } if known == call_id))
    }

    fn set_output(&mut self, index: usize, text: &str, running: bool, error: bool) {
        let before = self.items[index].bytes();
        if let Item::Tool {
            output,
            running: is_running,
            is_error,
            ..
        } = &mut self.items[index]
        {
            *output = tail(text, MAX_TOOL_FIELD_BYTES);
            *is_running = running;
            *is_error = error;
        }
        self.bytes = self.bytes - before + self.items[index].bytes();
        self.revision += 1;
        self.enforce();
    }

    fn append(&mut self, index: usize, delta: &str, limit: usize) {
        let before = self.items[index].bytes();
        if let Item::Assistant(text) | Item::Reasoning(text) = &mut self.items[index] {
            if text.len() < limit {
                text.push_str(delta);
                if text.len() > limit {
                    *text = cut(text, limit);
                }
            }
        }
        self.bytes = self.bytes - before + self.items[index].bytes();
        self.revision += 1;
        self.enforce();
    }

    fn push(&mut self, item: Item) {
        self.bytes += item.bytes();
        self.items.push(item);
        self.revision += 1;
        self.enforce();
    }

    fn remove(&mut self, index: usize) {
        let item = self.items.remove(index);
        self.bytes -= item.bytes();
        self.shift_after(index);
        self.revision += 1;
    }

    /// Keeps the task (first item) and drops the oldest steps after it until
    /// both bounds hold.
    fn enforce(&mut self) {
        while self.items.len() > MAX_ITEMS || (self.bytes > MAX_TEXT_BYTES && self.items.len() > 2)
        {
            let index = usize::from(matches!(self.items.first(), Some(Item::Task(_))));
            if index >= self.items.len() - 1 {
                break;
            }
            let item = self.items.remove(index);
            self.bytes -= item.bytes();
            self.trimmed = true;
            self.shift_after(index);
        }
    }

    fn shift_after(&mut self, removed: usize) {
        for slot in [&mut self.streaming, &mut self.reasoning] {
            *slot = match *slot {
                Some(index) if index == removed => None,
                Some(index) if index > removed => Some(index - 1),
                other => other,
            };
        }
    }

    fn set_activity(&mut self, activity: String) {
        let activity = one_line(&activity, MAX_ACTIVITY_CHARS);
        if activity != self.activity {
            self.activity = activity;
            self.revision += 1;
        }
    }
}

/// The agent's latest action in a few words: "Running cargo test",
/// "Editing src/lib.rs", "Searching for fn main".
fn describe(name: &str, arguments: &str) -> String {
    let args = serde_json::from_str::<Value>(arguments).unwrap_or(Value::Null);
    let field = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    let path = || {
        let path = field("path");
        if path.is_empty() {
            field("file_path")
        } else {
            path
        }
    };
    let text = match name {
        "bash" | "exec_command" | "shell_command" | "execute" => {
            let command = if field("command").is_empty() {
                field("cmd")
            } else {
                field("command")
            };
            format!("Running {command}")
        }
        "read" => format!("Reading {}", path()),
        "write" => format!("Writing {}", path()),
        "str_replace" | "hashline_edit" | "edit" => format!("Editing {}", path()),
        "apply_patch" => "Applying a patch".to_string(),
        "ls" => format!("Listing {}", path()),
        "ripgrep" => format!("Searching for {}", field("pattern")),
        "outline" => format!("Outlining {}", path()),
        "web_search" => format!("Searching the web for {}", field("query")),
        "web_fetch" => format!("Fetching {}", field("url")),
        "generate_image" => "Generating an image".to_string(),
        "spawn_agent" => format!("Starting subagent {}", field("task_name")),
        "wait_agent" => "Waiting for subagents".to_string(),
        "update_plan" => "Updating the plan".to_string(),
        other => format!("Using {other}"),
    };
    text.trim_end().to_string()
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn one_line(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let mut cut: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The first `max` bytes (on a char boundary).
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The last `max` bytes (on a char boundary): tool output matters at its end.
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, arguments: Value) -> StreamEvent {
        StreamEvent::ResponseItem(json!({
            "type": "function_call", "call_id": id, "name": name,
            "arguments": arguments.to_string(),
        }))
    }

    #[test]
    fn records_text_tools_and_the_latest_action() {
        let mut trace = SubagentTrace::new("find the bug");
        trace.record(&StreamEvent::CallStart);
        trace.record(&StreamEvent::Delta("Looking ".into()));
        trace.record(&StreamEvent::Delta("around.".into()));
        trace.record(&call("c1", "bash", json!({ "command": "cargo test -p x" })));
        assert_eq!(trace.activity(), "Running cargo test -p x");
        trace.record(&StreamEvent::ToolStart {
            call_id: "c1".into(),
            name: "bash".into(),
        });
        trace.record(&StreamEvent::ToolOutput {
            call_id: "c1".into(),
            name: "bash".into(),
            output: "ok".into(),
            model_output: "ok".into(),
            is_error: false,
        });
        let items = trace.items_json();
        assert_eq!(
            items[0],
            json!({ "role": "user", "content": "find the bug" })
        );
        assert_eq!(
            items[1],
            json!({ "role": "assistant", "content": "Looking around." })
        );
        assert_eq!(items[2]["role"], "tool");
        assert_eq!(items[2]["name"], "bash");
        assert_eq!(items[2]["output"], "ok");
        assert_eq!(items[2]["running"], false);
        assert_eq!(
            items[2]["input"],
            json!({ "command": "cargo test -p x" }).to_string()
        );
        assert_eq!(trace.tool_calls(), 1);
    }

    #[test]
    fn a_retry_drops_the_partial_reply() {
        let mut trace = SubagentTrace::new("task");
        trace.record(&StreamEvent::CallStart);
        trace.record(&StreamEvent::Delta("half".into()));
        trace.record(&StreamEvent::Retrying {
            attempt: 1,
            max_attempts: 3,
            reason: "timeout".into(),
            delay_ms: 10,
        });
        trace.record(&StreamEvent::CallStart);
        trace.record(&StreamEvent::Delta("whole".into()));
        let items = trace.items_json();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["content"], "whole");
    }

    #[test]
    fn the_transcript_stays_bounded_and_keeps_the_task() {
        let mut trace = SubagentTrace::new("the task");
        for index in 0..(MAX_ITEMS * 2) {
            let id = format!("c{index}");
            trace.record(&call(&id, "read", json!({ "path": "a.rs" })));
            trace.record(&StreamEvent::ToolOutput {
                call_id: id,
                name: "read".into(),
                output: "x".repeat(8 * 1024),
                model_output: String::new(),
                is_error: false,
            });
        }
        let items = trace.items_json();
        assert!(items.len() <= MAX_ITEMS + 1, "{}", items.len());
        assert_eq!(items[0]["content"], "the task");
        assert_eq!(items[1]["content"], "… earlier steps trimmed");
        let text: usize = items.iter().map(|item| item.to_string().len()).sum();
        assert!(text < MAX_TEXT_BYTES * 2, "{text}");
        // Output keeps its end.
        assert!(items.last().unwrap()["output"]
            .as_str()
            .unwrap()
            .starts_with('…'));
    }

    #[test]
    fn finish_stops_running_tools() {
        let mut trace = SubagentTrace::new("t");
        trace.record(&call("c1", "bash", json!({ "command": "sleep 100" })));
        trace.finish("Interrupted");
        assert_eq!(trace.items_json()[1]["running"], false);
        assert_eq!(trace.activity(), "Interrupted");
    }
}
