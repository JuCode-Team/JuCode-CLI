//! Extensions a host process adds to an engine: extra tools the host runs
//! itself and text appended to the system prompt each turn. The daemon uses
//! this for long-lived agents (their brief, messaging and timers) without
//! agent-core knowing about agents.

use serde_json::Value;
use std::sync::Arc;

/// Runs a host tool: `(name, JSON arguments)` → `(output, is_error)`.
pub type HostToolRunner = Arc<dyn Fn(&str, &str) -> (String, bool) + Send + Sync>;

#[derive(Clone)]
pub struct HostExtensions {
    /// Function-tool definitions, in the same shape as the built-in tools.
    pub tools: Vec<Value>,
    pub run_tool: HostToolRunner,
    /// Appended to the system prompt at the start of every turn, so the
    /// host can reflect state that changed since the last turn.
    pub prompt: Arc<dyn Fn() -> String + Send + Sync>,
}

impl HostExtensions {
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
    }
}
