//! Deferred actions: gated tool calls made while no client is watching the
//! session. Instead of blocking the turn on an approval prompt, the call is
//! recorded here and the model is told it was submitted for confirmation.
//! A later decision runs the call with its original arguments (or declines
//! it) and reports the outcome back to the session as a message.

use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct DeferredAction {
    pub id: String,
    pub session_id: String,
    pub cwd: PathBuf,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub summary: String,
    /// Path of the subagent that issued the call; None for the main agent.
    pub subagent_id: Option<String>,
    /// Same tool, arguments and cwd give the same digest, so a decision is
    /// reused instead of asking again for an identical call.
    pub digest: String,
    pub created_at: u64,
}

impl DeferredAction {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "session_id": self.session_id,
            "cwd": self.cwd.display().to_string(),
            "call_id": self.call_id,
            "name": self.name,
            "arguments": self.arguments,
            "summary": self.summary,
            "subagent_id": self.subagent_id,
            "digest": self.digest,
            "created_at": self.created_at,
        })
    }
}

pub fn action_digest(name: &str, arguments: &str, cwd: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let cwd = cwd.display().to_string();
    for part in [name, arguments, cwd.as_str()] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The message that wakes the session once a deferred action is decided.
pub fn decision_message(action: &DeferredAction, outcome: Option<(&str, bool)>) -> String {
    match outcome {
        None => format!(
            "[deferred action {} declined]\nThe user declined `{}` ({}). Do not retry it; continue with a different approach or ask how to proceed.",
            action.id, action.name, action.summary
        ),
        Some((output, is_error)) => format!(
            "[deferred action {} approved and executed{}]\n`{}` ({})\nresult:\n{}",
            action.id,
            if is_error { ", failed" } else { "" },
            action.name,
            action.summary,
            output
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn digest_depends_on_tool_arguments_and_cwd() {
        let base = action_digest("bash", r#"{"command":"make"}"#, Path::new("/a"));
        assert_eq!(
            base,
            action_digest("bash", r#"{"command":"make"}"#, Path::new("/a"))
        );
        assert_ne!(
            base,
            action_digest("bash", r#"{"command":"make"}"#, Path::new("/b"))
        );
        assert_ne!(
            base,
            action_digest("bash", r#"{"command":"make test"}"#, Path::new("/a"))
        );
        // The separator keeps field boundaries: "ab"+"c" differs from "a"+"bc".
        assert_ne!(
            action_digest("ab", "c", Path::new("/a")),
            action_digest("a", "bc", Path::new("/a"))
        );
    }
}
