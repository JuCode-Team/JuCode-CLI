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

    /// Reads back a record written by `to_json`; None when a field is missing.
    pub fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            id: text("id")?,
            session_id: text("session_id")?,
            cwd: PathBuf::from(text("cwd")?),
            call_id: text("call_id")?,
            name: text("name")?,
            arguments: text("arguments")?,
            summary: text("summary")?,
            subagent_id: text("subagent_id"),
            digest: text("digest")?,
            created_at: value.get("created_at").and_then(Value::as_u64)?,
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
/// Clients recognise the header line and show it as a notice, not a user
/// message (JuCode-Desktop `src/lib/delivery.ts`): keep the two in step.
pub fn decision_message(action: &DeferredAction, outcome: Option<(&str, bool)>) -> String {
    let call = if action.summary.is_empty() {
        format!("`{}`", action.name)
    } else {
        format!("`{}` ({})", action.name, action.summary)
    };
    match outcome {
        None => format!(
            "[deferred action {} declined]\nThe user declined {call}. Do not retry it; continue with a different approach or ask how to proceed.",
            action.id
        ),
        Some((output, is_error)) => format!(
            "[deferred action {} approved and executed{}]\n{call}\nresult:\n{output}",
            action.id,
            if is_error { ", failed" } else { "" },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn record_round_trips_through_json() {
        let action = DeferredAction {
            id: "act-1".to_string(),
            session_id: "s1".to_string(),
            cwd: PathBuf::from("/work"),
            call_id: "call_1".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"make"}"#.to_string(),
            summary: "make".to_string(),
            subagent_id: None,
            digest: action_digest("bash", r#"{"command":"make"}"#, Path::new("/work")),
            created_at: 42,
        };
        assert_eq!(DeferredAction::from_json(&action.to_json()), Some(action));
        assert_eq!(DeferredAction::from_json(&json!({ "id": "x" })), None);
    }

    #[test]
    fn decision_message_leaves_out_an_empty_summary() {
        let mut action = DeferredAction {
            id: "act-1".to_string(),
            session_id: "s1".to_string(),
            cwd: PathBuf::from("/work"),
            call_id: "call_1".to_string(),
            name: "write_stdin".to_string(),
            arguments: r#"{"session_id":3,"chars":""}"#.to_string(),
            summary: String::new(),
            subagent_id: None,
            digest: String::new(),
            created_at: 0,
        };
        assert_eq!(
            decision_message(&action, Some(("{}", true))),
            "[deferred action act-1 approved and executed, failed]\n`write_stdin`\nresult:\n{}"
        );
        action.summary = "make".to_string();
        assert!(decision_message(&action, None).starts_with(
            "[deferred action act-1 declined]\nThe user declined `write_stdin` (make). "
        ));
    }

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
