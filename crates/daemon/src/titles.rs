//! Conversation titles written by a model. A session is titled after its
//! first line at once; once its first turn ends, the title model
//! (`title_model`, else the main model) names it from a short context, and
//! renames it as the conversation moves on: after turns 1 and 3, then every
//! fifth. A title a client set by hand is never replaced.

use serde_json::Value;

/// How much of each part of the conversation the model sees.
const FIRST_LIMIT: usize = 600;
const LATEST_LIMIT: usize = 600;
const REPLY_LIMIT: usize = 800;
/// Longest title kept from the model's reply.
const TITLE_LIMIT: usize = 30;

pub const SYSTEM: &str = "You name conversations between a user and a coding assistant. \
Reply with the title only: a short phrase naming the task (at most 20 Chinese characters \
or 8 English words), in the language the user writes in, with no quotes and no trailing \
punctuation. Name the work itself, not the people (not \"User asks about...\"). If the \
current title still fits the conversation, reply with it unchanged.";

/// What a session's conversation has been about, fed from its events.
#[derive(Default)]
pub struct Turns {
    first: String,
    latest: String,
    reply: String,
    replying: bool,
    done: u32,
}

impl Turns {
    /// Takes one session event; true when a turn just ended and the title is
    /// due for another look.
    pub fn observe(&mut self, event: &Value) -> bool {
        match event["type"].as_str() {
            Some("user_message") => {
                let content = event["content"].as_str().unwrap_or_default().trim();
                if self.first.is_empty() {
                    self.first = clip(content, FIRST_LIMIT);
                }
                self.latest = clip(content, LATEST_LIMIT);
                self.reply.clear();
                self.replying = false;
                false
            }
            Some("assistant_start") => {
                self.reply.clear();
                false
            }
            Some("assistant_delta") => {
                if self.reply.chars().count() < REPLY_LIMIT {
                    self.reply
                        .push_str(event["delta"].as_str().unwrap_or_default());
                }
                self.replying = true;
                false
            }
            Some("status") if event["message"] == "ready" && self.replying => {
                self.replying = false;
                self.done += 1;
                due(self.done)
            }
            _ => false,
        }
    }

    /// The request to the title model.
    pub fn prompt(&self, project: &str, current: Option<&str>) -> String {
        let mut text = format!(
            "Project: {project}\nCurrent title: {}\n\nFirst request:\n{}\n",
            current.filter(|t| !t.is_empty()).unwrap_or("(none)"),
            self.first
        );
        if self.latest != self.first {
            text.push_str(&format!("\nLatest request:\n{}\n", self.latest));
        }
        if !self.reply.trim().is_empty() {
            text.push_str(&format!(
                "\nLatest reply (beginning):\n{}\n",
                clip(self.reply.trim(), REPLY_LIMIT)
            ));
        }
        text
    }
}

/// After turns 1 and 3, then every fifth.
fn due(turn: u32) -> bool {
    turn == 1 || turn == 3 || (turn >= 5 && turn.is_multiple_of(5))
}

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// The model's reply as a title: its first line, without surrounding quotes
/// or trailing punctuation.
pub fn clean(reply: &str) -> Option<String> {
    let line = reply.trim().lines().next()?.trim();
    let line = line
        .trim_start_matches(|c: char| "\"'“”‘’「」《》*#".contains(c) || c.is_whitespace())
        .trim_end_matches(|c: char| {
            "\"'“”‘’「」《》*.。!！?？,，;；:：".contains(c) || c.is_whitespace()
        });
    let line = line
        .strip_prefix("Title:")
        .or_else(|| line.strip_prefix("标题："))
        .unwrap_or(line)
        .trim();
    let title = clip(line, TITLE_LIMIT);
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn turn(turns: &mut Turns, user: &str, reply: &str) -> bool {
        turns.observe(&json!({ "type": "user_message", "content": user }));
        turns.observe(&json!({ "type": "assistant_start" }));
        turns.observe(&json!({ "type": "assistant_delta", "delta": reply }));
        turns.observe(&json!({ "type": "status", "message": "ready" }))
    }

    #[test]
    fn titles_are_due_after_turns_one_three_and_every_fifth() {
        let mut turns = Turns::default();
        let due: Vec<bool> = (1..=10).map(|_| turn(&mut turns, "q", "a")).collect();
        assert_eq!(
            due,
            [true, false, true, false, true, false, false, false, false, true]
        );
        // A ready with no reply (startup, an interrupted empty turn) is no turn.
        assert!(!turns.observe(&json!({ "type": "status", "message": "ready" })));
    }

    #[test]
    fn the_prompt_carries_first_and_latest_requests_and_the_reply() {
        let mut turns = Turns::default();
        turn(&mut turns, "修复登录页跳转", "先看路由");
        let first = turns.prompt("crm", None);
        assert!(first.contains("Project: crm"));
        assert!(first.contains("Current title: (none)"));
        assert!(first.contains("修复登录页跳转"));
        assert!(!first.contains("Latest request"));
        assert!(first.contains("先看路由"));
        turn(&mut turns, "顺便加个导出按钮", "好的");
        let later = turns.prompt("crm", Some("修复登录页跳转"));
        assert!(later.contains("Current title: 修复登录页跳转"));
        assert!(later.contains("Latest request:\n顺便加个导出按钮"));
        assert!(!later.contains("先看路由"));
    }

    #[test]
    fn replies_become_clean_titles() {
        assert_eq!(
            clean("「修复登录跳转」。\n解释").as_deref(),
            Some("修复登录跳转")
        );
        assert_eq!(
            clean("\"Fix login redirect\"").as_deref(),
            Some("Fix login redirect")
        );
        assert_eq!(
            clean("Title: Add CSV export").as_deref(),
            Some("Add CSV export")
        );
        assert_eq!(clean("  \n"), None);
        assert_eq!(clean(&"长".repeat(50)).unwrap().chars().count(), 30);
    }
}
