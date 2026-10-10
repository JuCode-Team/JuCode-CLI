//! Conversation titles written by a model. A session is titled after its
//! first line at once; once its first turn ends, the title model
//! (`title_model`, else the main model) names it from a short context, and
//! renames it as the conversation moves on: after turns 1 and 3, then every
//! fifth. A title a client set by hand is never replaced.
//!
//! The same model writes an agent session's handoff note after each turn:
//! what was asked, done and concluded, for the agent's next sessions.

use serde_json::Value;

/// How much of each part of the conversation the model sees.
const FIRST_LIMIT: usize = 600;
const LATEST_LIMIT: usize = 600;
const REPLY_LIMIT: usize = 800;
/// Longest title kept from the model's reply.
const TITLE_LIMIT: usize = 30;
/// The end of the latest reply a handoff note is written from (where the
/// conclusion is).
const TAIL_LIMIT: usize = 1500;
/// Longest handoff note kept.
const HANDOFF_LIMIT: usize = 600;

pub const HANDOFF_SYSTEM: &str = "You write the handoff note a long-lived coding agent leaves \
for its next sessions. From the excerpt, write at most 5 short lines in the language the user \
writes in: what was asked, what was done, the conclusion, and what is left open. Plain lines, \
no headings, no preamble. If a previous note is given, update it rather than repeat it.";

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
    /// The latest reply's last TAIL_LIMIT characters.
    tail: String,
    replying: bool,
    done: u32,
    /// A turn has started and not yet ended.
    running: bool,
    /// The latest error of the running turn; cleared by output after it.
    error: Option<String>,
    /// The error that ended the latest turn, until taken.
    failed: Option<String>,
}

impl Turns {
    /// Takes one session event; true when a turn just ended and the title is
    /// due for another look.
    pub fn observe(&mut self, event: &Value) -> bool {
        match event["type"].as_str() {
            Some("user_message") => {
                self.running = true;
                self.error = None;
            }
            Some("error") if self.running => {
                self.error = Some(event["message"].as_str().unwrap_or_default().to_string());
            }
            Some("assistant_start" | "assistant_delta") => self.error = None,
            Some("status") if event["message"] == "ready" && self.running => {
                self.running = false;
                self.failed = self.error.take();
            }
            _ => {}
        }
        match event["type"].as_str() {
            Some("user_message") => {
                // What was asked, without the line naming where it came from.
                let content = crate::hub::without_delivery_header(
                    event["content"].as_str().unwrap_or_default(),
                )
                .trim();
                if self.first.is_empty() {
                    self.first = clip(content, FIRST_LIMIT);
                }
                self.latest = clip(content, LATEST_LIMIT);
                self.reply.clear();
                self.tail.clear();
                self.replying = false;
                false
            }
            Some("assistant_start") => {
                self.reply.clear();
                self.tail.clear();
                false
            }
            Some("assistant_delta") => {
                let delta = event["delta"].as_str().unwrap_or_default();
                if self.reply.chars().count() < REPLY_LIMIT {
                    self.reply.push_str(delta);
                }
                self.tail.push_str(delta);
                let extra = self.tail.chars().count().saturating_sub(TAIL_LIMIT);
                if extra > 0 {
                    self.tail = self.tail.chars().skip(extra).collect();
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

    /// The end of the latest reply.
    pub fn tail(&self) -> &str {
        &self.tail
    }

    /// The error that ended the latest turn, once.
    pub fn take_failed(&mut self) -> Option<String> {
        self.failed.take()
    }

    /// Turns ended so far.
    pub fn done(&self) -> u32 {
        self.done
    }

    /// The request for a handoff note; `previous` is the session's last one.
    pub fn handoff_prompt(&self, title: &str, previous: Option<&str>) -> String {
        let mut text = format!("Session: {title}\n\nFirst request:\n{}\n", self.first);
        if self.latest != self.first {
            text.push_str(&format!("\nLatest request:\n{}\n", self.latest));
        }
        text.push_str(&format!(
            "\nEnd of the latest reply:\n{}\n",
            self.tail.trim()
        ));
        if let Some(previous) = previous.filter(|p| !p.trim().is_empty()) {
            text.push_str(&format!("\nPrevious note:\n{}\n", previous.trim()));
        }
        text
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

/// The model's reply as a handoff note: trimmed and bounded.
pub fn clean_handoff(reply: &str) -> Option<String> {
    let note = clip(reply.trim(), HANDOFF_LIMIT);
    (!note.is_empty()).then_some(note)
}

/// Paired marks a title may be wrapped in, or end with when they close a
/// name (`定时任务「巡检」`).
const PAIRS: [(char, char); 4] = [('「', '」'), ('《', '》'), ('“', '”'), ('‘', '’')];

/// The model's reply as a title: its first line, without surrounding quotes
/// or trailing punctuation. A closing mark stays when it closes an opening
/// one inside the title.
pub fn clean(reply: &str) -> Option<String> {
    let mut line = reply.trim().lines().next()?.trim();
    loop {
        let before = line;
        line = line
            .trim_start_matches(|c: char| "\"'*#".contains(c) || c.is_whitespace())
            .trim_end_matches(|c: char| "\"'*.。!！?？,，;；:：".contains(c) || c.is_whitespace());
        for (open, close) in PAIRS {
            let opens = line.matches(open).count();
            let closes = line.matches(close).count();
            if let Some(inner) = line
                .strip_prefix(open)
                .and_then(|rest| rest.strip_suffix(close))
            {
                if balanced(inner, open, close) {
                    line = inner;
                    continue;
                }
            }
            if closes > opens {
                line = line.strip_suffix(close).unwrap_or(line);
            } else if opens > closes {
                line = line.strip_prefix(open).unwrap_or(line);
            }
        }
        if line == before {
            break;
        }
    }
    let line = line
        .strip_prefix("Title:")
        .or_else(|| line.strip_prefix("标题："))
        .unwrap_or(line)
        .trim();
    let title = clip(line, TITLE_LIMIT);
    (!title.is_empty()).then_some(title)
}

/// Every `close` in `text` closes an earlier `open`, and none is left open.
fn balanced(text: &str, open: char, close: char) -> bool {
    let mut depth = 0i32;
    for c in text.chars() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth < 0 {
                return false;
            }
        }
    }
    depth == 0
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
    fn a_turn_that_ends_on_an_error_is_failed_once() {
        let mut turns = Turns::default();
        // An error outside a turn (a rejected op) fails nothing.
        turns.observe(&json!({ "type": "error", "message": "nothing to continue" }));
        turns.observe(&json!({ "type": "status", "message": "ready" }));
        assert_eq!(turns.take_failed(), None);

        turns.observe(&json!({ "type": "user_message", "content": "巡检" }));
        turns.observe(&json!({ "type": "error", "message": "HTTP 502" }));
        turns.observe(&json!({ "type": "status", "message": "ready" }));
        assert_eq!(turns.take_failed().as_deref(), Some("HTTP 502"));
        assert_eq!(turns.take_failed(), None);

        // Output after an error means the turn went on.
        turns.observe(&json!({ "type": "user_message", "content": "巡检" }));
        turns.observe(&json!({ "type": "error", "message": "stray" }));
        turn(&mut turns, "巡检", "完成");
        assert_eq!(turns.take_failed(), None);
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
    fn the_handoff_prompt_carries_the_end_of_the_reply() {
        let mut turns = Turns::default();
        let long = format!("{}结论：已修复", "过程".repeat(1000));
        turn(&mut turns, "修复登录页跳转", &long);
        assert_eq!(turns.done(), 1);
        let prompt = turns.handoff_prompt("修复登录跳转", Some("旧的交接"));
        assert!(prompt.contains("结论：已修复"));
        assert!(prompt.contains("Previous note:\n旧的交接"));
        let tail = prompt.split("End of the latest reply:\n").nth(1).unwrap();
        assert!(tail.chars().count() < TAIL_LIMIT + 100);
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

    #[test]
    fn a_closing_mark_that_closes_a_name_stays() {
        assert_eq!(
            clean("定时任务「工单巡检（午夜）」").as_deref(),
            Some("定时任务「工单巡检（午夜）」")
        );
        assert_eq!(
            clean("「巡检」和「对账」").as_deref(),
            Some("「巡检」和「对账」")
        );
        assert_eq!(clean("读《设计》。").as_deref(), Some("读《设计》"));
        assert_eq!(clean("“修复登录”").as_deref(), Some("修复登录"));
        assert_eq!(clean("修复登录」").as_deref(), Some("修复登录"));
        assert_eq!(clean("「修复登录").as_deref(), Some("修复登录"));
    }
}
