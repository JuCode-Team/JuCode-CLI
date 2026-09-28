//! Append-only state under `~/.jucode/daemon/`. The daemon is the only
//! writer; every read folds the whole log, which stays small (one line per
//! session opened or closed, per action deferred or decided).

use jucode_agent_core::actions::DeferredAction;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const SESSIONS: &str = "sessions.jsonl";
const ACTIONS: &str = "actions.jsonl";
const MESSAGES: &str = "messages.jsonl";
const TIMERS: &str = "timers.jsonl";
const QUESTIONS: &str = "questions.jsonl";
const REPORTS: &str = "reports.jsonl";
const DEVICES: &str = "devices.jsonl";
const TOKEN: &str = "token";

pub struct Store {
    dir: PathBuf,
    write: Mutex<()>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub id: String,
    pub cwd: PathBuf,
    /// The long-lived agent the session belongs to, if any.
    pub agent: Option<String>,
    pub created_at: u64,
    pub closed: bool,
}

/// A message for an agent: from the user, another agent or a timer.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: String,
    pub to: String,
    /// `user`, `agent:<id>` or `timer:<id>`.
    pub from: String,
    pub body: String,
    /// Deliver into this session instead of routing.
    pub session: Option<String>,
    /// Deliver into the session that received this earlier message.
    pub reply_to: Option<String>,
    /// A second message with the same key is dropped.
    pub dedupe_key: Option<String>,
    pub at: u64,
}

/// A question an agent asked while it kept working. Answered by the user,
/// or by the deadline passing (the agent then goes with its default).
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub id: String,
    pub agent: String,
    pub session: String,
    pub title: String,
    pub body: String,
    /// What the agent assumes meanwhile.
    pub assumption: String,
    /// What the agent will do if nobody answers in time.
    pub default_action: String,
    /// `low`, `normal` or `high`.
    pub importance: String,
    pub due_at: Option<u64>,
    pub asked_at: u64,
}

/// Something an agent reports for the user to read; wakes nobody.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub id: String,
    pub agent: String,
    pub session: String,
    pub title: String,
    pub body: String,
    pub at: u64,
    pub read: bool,
}

/// A paired remote device (a phone's browser). Only a hash of its token is
/// kept, so the state directory never holds a usable device token.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub paired_at: u64,
    pub revoked: bool,
}

/// A one-shot timer that wakes an agent with `body` at `fire_at` (ms).
#[derive(Debug, Clone, PartialEq)]
pub struct Timer {
    pub id: String,
    pub agent: String,
    /// Wake this session; None opens a new one.
    pub session: Option<String>,
    pub fire_at: u64,
    pub body: String,
}

impl Store {
    pub fn open(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            write: Mutex::new(()),
        })
    }

    /// The local client token, created on first use and readable only by
    /// the owner. Local clients read it from this file to connect.
    pub fn token(&self) -> io::Result<String> {
        let path = self.dir.join(TOKEN);
        if let Ok(token) = fs::read_to_string(&path) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Ok(token);
            }
        }
        let token = random_hex(32)?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)?
            .write_all(format!("{token}\n").as_bytes())?;
        Ok(token)
    }

    pub fn record_session(
        &self,
        id: &str,
        cwd: &std::path::Path,
        agent: Option<&str>,
    ) -> io::Result<()> {
        self.append(
            SESSIONS,
            json!({
                "kind": "open", "session": id, "cwd": cwd.display().to_string(),
                "agent": agent, "at": now(),
            }),
        )
    }

    pub fn record_session_closed(&self, id: &str) -> io::Result<()> {
        self.append(
            SESSIONS,
            json!({ "kind": "close", "session": id, "at": now() }),
        )
    }

    /// Sessions in the order they were first opened. Reopening a closed
    /// session clears `closed`.
    pub fn sessions(&self) -> Vec<SessionRecord> {
        let mut order = Vec::new();
        let mut records: BTreeMap<String, SessionRecord> = BTreeMap::new();
        for entry in self.read(SESSIONS) {
            let Some(id) = entry["session"].as_str() else {
                continue;
            };
            match entry["kind"].as_str() {
                Some("open") => {
                    let Some(cwd) = entry["cwd"].as_str() else {
                        continue;
                    };
                    let record = records.entry(id.to_string()).or_insert_with(|| {
                        order.push(id.to_string());
                        SessionRecord {
                            id: id.to_string(),
                            cwd: PathBuf::from(cwd),
                            agent: entry["agent"].as_str().map(str::to_string),
                            created_at: entry["at"].as_u64().unwrap_or_default(),
                            closed: false,
                        }
                    });
                    record.closed = false;
                }
                Some("close") => {
                    if let Some(record) = records.get_mut(id) {
                        record.closed = true;
                    }
                }
                _ => {}
            }
        }
        order
            .into_iter()
            .filter_map(|id| records.remove(&id))
            .collect()
    }

    pub fn record_deferred(&self, action: &DeferredAction) -> io::Result<()> {
        self.append(
            ACTIONS,
            json!({ "kind": "deferred", "action": action.to_json() }),
        )
    }

    pub fn record_decided(&self, id: &str, allow: bool) -> io::Result<()> {
        self.append(
            ACTIONS,
            json!({ "kind": "decided", "id": id, "allow": allow, "at": now() }),
        )
    }

    /// Deferred actions that have not been decided yet, oldest first.
    pub fn open_actions(&self) -> Vec<DeferredAction> {
        let entries = self.read(ACTIONS);
        let decided: HashSet<&str> = entries
            .iter()
            .filter(|entry| entry["kind"] == "decided")
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        entries
            .iter()
            .filter(|entry| entry["kind"] == "deferred")
            .filter_map(|entry| DeferredAction::from_json(&entry["action"]))
            .filter(|action| !decided.contains(action.id.as_str()))
            .collect()
    }

    /// Records a message; returns false (and records nothing) when a message
    /// with the same dedupe key already exists.
    pub fn record_message(&self, message: &Message) -> io::Result<bool> {
        let _guard = self.lock();
        if let Some(key) = &message.dedupe_key {
            let taken = self.read(MESSAGES).iter().any(|entry| {
                entry["kind"] == "message" && entry["dedupe_key"].as_str() == Some(key)
            });
            if taken {
                return Ok(false);
            }
        }
        self.append_locked(
            MESSAGES,
            json!({
                "kind": "message", "id": message.id, "to": message.to, "from": message.from,
                "body": message.body, "session": message.session, "reply_to": message.reply_to,
                "dedupe_key": message.dedupe_key, "at": message.at,
            }),
        )?;
        Ok(true)
    }

    pub fn record_delivered(&self, id: &str, session: &str) -> io::Result<()> {
        self.append(
            MESSAGES,
            json!({ "kind": "delivered", "id": id, "session": session, "at": now() }),
        )
    }

    /// A message that can never be delivered (its agent is gone); it stops
    /// being retried.
    pub fn record_undeliverable(&self, id: &str, reason: &str) -> io::Result<()> {
        self.append(
            MESSAGES,
            json!({ "kind": "undeliverable", "id": id, "reason": reason, "at": now() }),
        )
    }

    /// Messages neither delivered nor undeliverable, oldest first.
    pub fn pending_messages(&self) -> Vec<Message> {
        let entries = self.read(MESSAGES);
        let settled: HashSet<&str> = entries
            .iter()
            .filter(|entry| entry["kind"] == "delivered" || entry["kind"] == "undeliverable")
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        entries
            .iter()
            .filter(|entry| entry["kind"] == "message")
            .filter(|entry| !entry["id"].as_str().is_some_and(|id| settled.contains(id)))
            .filter_map(message_from_json)
            .collect()
    }

    /// The session a delivered message went to.
    pub fn delivered_session(&self, id: &str) -> Option<String> {
        self.read(MESSAGES)
            .into_iter()
            .find(|entry| entry["kind"] == "delivered" && entry["id"] == id)
            .and_then(|entry| entry["session"].as_str().map(str::to_string))
    }

    pub fn record_timer(&self, timer: &Timer) -> io::Result<()> {
        self.append(
            TIMERS,
            json!({
                "kind": "set", "id": timer.id, "agent": timer.agent, "session": timer.session,
                "fire_at": timer.fire_at, "body": timer.body, "at": now(),
            }),
        )
    }

    /// Ends a timer: `fired` or `cancelled`.
    pub fn record_timer_done(&self, id: &str, reason: &str) -> io::Result<()> {
        self.append(
            TIMERS,
            json!({ "kind": "done", "id": id, "reason": reason, "at": now() }),
        )
    }

    /// Timers that have neither fired nor been cancelled, soonest first.
    pub fn active_timers(&self) -> Vec<Timer> {
        let entries = self.read(TIMERS);
        let done: HashSet<&str> = entries
            .iter()
            .filter(|entry| entry["kind"] == "done")
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        let mut timers: Vec<Timer> = entries
            .iter()
            .filter(|entry| entry["kind"] == "set")
            .filter(|entry| !entry["id"].as_str().is_some_and(|id| done.contains(id)))
            .filter_map(|entry| {
                Some(Timer {
                    id: entry["id"].as_str()?.to_string(),
                    agent: entry["agent"].as_str()?.to_string(),
                    session: entry["session"].as_str().map(str::to_string),
                    fire_at: entry["fire_at"].as_u64()?,
                    body: entry["body"].as_str()?.to_string(),
                })
            })
            .collect();
        timers.sort_by_key(|timer| timer.fire_at);
        timers
    }

    pub fn record_question(&self, question: &Question) -> io::Result<()> {
        self.append(
            QUESTIONS,
            json!({
                "kind": "asked", "id": question.id, "agent": question.agent,
                "session": question.session, "title": question.title, "body": question.body,
                "assumption": question.assumption, "default": question.default_action,
                "importance": question.importance, "due_at": question.due_at,
                "at": question.asked_at,
            }),
        )
    }

    /// Records the answer; false when the question was already answered or
    /// does not exist, so a user answer and a deadline never both land.
    pub fn record_answer(&self, id: &str, answer: &str, by: &str) -> io::Result<bool> {
        let _guard = self.lock();
        let entries = self.read(QUESTIONS);
        let asked = entries
            .iter()
            .any(|entry| entry["kind"] == "asked" && entry["id"] == id);
        let answered = entries
            .iter()
            .any(|entry| entry["kind"] == "answered" && entry["id"] == id);
        if !asked || answered {
            return Ok(false);
        }
        self.append_locked(
            QUESTIONS,
            json!({ "kind": "answered", "id": id, "answer": answer, "by": by, "at": now() }),
        )?;
        Ok(true)
    }

    pub fn question(&self, id: &str) -> Option<Question> {
        self.read(QUESTIONS)
            .iter()
            .find(|entry| entry["kind"] == "asked" && entry["id"] == id)
            .and_then(question_from_json)
    }

    /// Unanswered questions, oldest first.
    pub fn open_questions(&self) -> Vec<Question> {
        let entries = self.read(QUESTIONS);
        let answered: HashSet<&str> = entries
            .iter()
            .filter(|entry| entry["kind"] == "answered")
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        entries
            .iter()
            .filter(|entry| entry["kind"] == "asked")
            .filter(|entry| !entry["id"].as_str().is_some_and(|id| answered.contains(id)))
            .filter_map(question_from_json)
            .collect()
    }

    pub fn record_report(&self, report: &Report) -> io::Result<()> {
        self.append(
            REPORTS,
            json!({
                "kind": "posted", "id": report.id, "agent": report.agent,
                "session": report.session, "title": report.title, "body": report.body,
                "at": report.at,
            }),
        )
    }

    pub fn record_report_read(&self, id: &str) -> io::Result<()> {
        self.append(REPORTS, json!({ "kind": "read", "id": id, "at": now() }))
    }

    /// The newest `limit` reports, newest first.
    pub fn reports(&self, limit: usize) -> Vec<Report> {
        let entries = self.read(REPORTS);
        let read: HashSet<&str> = entries
            .iter()
            .filter(|entry| entry["kind"] == "read")
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        entries
            .iter()
            .rev()
            .filter(|entry| entry["kind"] == "posted")
            .filter_map(|entry| {
                let text = |key: &str| entry[key].as_str().map(str::to_string);
                let id = text("id")?;
                Some(Report {
                    read: read.contains(id.as_str()),
                    id,
                    agent: text("agent")?,
                    session: text("session")?,
                    title: text("title")?,
                    body: text("body").unwrap_or_default(),
                    at: entry["at"].as_u64().unwrap_or_default(),
                })
            })
            .take(limit)
            .collect()
    }

    pub fn record_device(&self, device: &Device) -> io::Result<()> {
        self.append(
            DEVICES,
            json!({
                "kind": "paired", "id": device.id, "name": device.name,
                "token_hash": device.token_hash, "at": device.paired_at,
            }),
        )
    }

    pub fn record_device_revoked(&self, id: &str) -> io::Result<()> {
        self.append(DEVICES, json!({ "kind": "revoked", "id": id, "at": now() }))
    }

    pub fn devices(&self) -> Vec<Device> {
        let entries = self.read(DEVICES);
        let revoked: HashSet<&str> = entries
            .iter()
            .filter(|entry| entry["kind"] == "revoked")
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        entries
            .iter()
            .filter(|entry| entry["kind"] == "paired")
            .filter_map(|entry| {
                let id = entry["id"].as_str()?.to_string();
                Some(Device {
                    revoked: revoked.contains(id.as_str()),
                    id,
                    name: entry["name"].as_str().unwrap_or_default().to_string(),
                    token_hash: entry["token_hash"].as_str()?.to_string(),
                    paired_at: entry["at"].as_u64().unwrap_or_default(),
                })
            })
            .collect()
    }

    /// The active device a token belongs to.
    pub fn device_for_token(&self, token: &str) -> Option<Device> {
        let hash = token_hash(token);
        self.devices()
            .into_iter()
            .find(|device| !device.revoked && device.token_hash == hash)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn append(&self, file: &str, value: Value) -> io::Result<()> {
        let _guard = self.lock();
        self.append_locked(file, value)
    }

    fn append_locked(&self, file: &str, value: Value) -> io::Result<()> {
        let mut out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(file))?;
        out.write_all(format!("{value}\n").as_bytes())
    }

    /// Every parseable line; a torn last line from a crash is skipped.
    fn read(&self, file: &str) -> Vec<Value> {
        fs::read_to_string(self.dir.join(file))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

fn question_from_json(entry: &Value) -> Option<Question> {
    let text = |key: &str| entry[key].as_str().map(str::to_string);
    Some(Question {
        id: text("id")?,
        agent: text("agent")?,
        session: text("session")?,
        title: text("title")?,
        body: text("body").unwrap_or_default(),
        assumption: text("assumption").unwrap_or_default(),
        default_action: text("default").unwrap_or_default(),
        importance: text("importance").unwrap_or_else(|| "normal".to_string()),
        due_at: entry["due_at"].as_u64(),
        asked_at: entry["at"].as_u64().unwrap_or_default(),
    })
}

fn message_from_json(entry: &Value) -> Option<Message> {
    let text = |key: &str| entry[key].as_str().map(str::to_string);
    Some(Message {
        id: text("id")?,
        to: text("to")?,
        from: text("from")?,
        body: text("body")?,
        session: text("session"),
        reply_to: text("reply_to"),
        dedupe_key: text("dedupe_key"),
        at: entry["at"].as_u64().unwrap_or_default(),
    })
}

pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `bytes` random bytes as lowercase hex.
pub fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::getrandom(&mut buffer).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(label: &str) -> Store {
        let dir = std::env::temp_dir().join(format!(
            "jucode-daemon-store-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        Store::open(dir).unwrap()
    }

    #[test]
    fn token_is_created_once_and_private() {
        let store = store("token");
        let token = store.token().unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(store.token().unwrap(), token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.dir.join(TOKEN))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn sessions_fold_open_and_close() {
        let store = store("sessions");
        store
            .record_session("a", std::path::Path::new("/p/a"), None)
            .unwrap();
        store
            .record_session("b", std::path::Path::new("/p/b"), Some("ops"))
            .unwrap();
        store.record_session_closed("a").unwrap();
        let sessions = store.sessions();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "a");
        assert!(sessions[0].closed);
        assert!(!sessions[1].closed);
        assert_eq!(sessions[1].agent.as_deref(), Some("ops"));
        store
            .record_session("a", std::path::Path::new("/p/a"), None)
            .unwrap();
        assert!(!store.sessions()[0].closed);
    }

    #[test]
    fn decided_actions_are_not_open() {
        let store = store("actions");
        let action = |id: &str| DeferredAction {
            id: id.to_string(),
            session_id: "s".to_string(),
            cwd: PathBuf::from("/p"),
            call_id: "c".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            summary: "x".to_string(),
            subagent_id: None,
            digest: id.to_string(),
            created_at: 1,
        };
        store.record_deferred(&action("one")).unwrap();
        store.record_deferred(&action("two")).unwrap();
        store.record_decided("one", true).unwrap();
        let open = store.open_actions();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "two");
    }

    fn message(id: &str, key: Option<&str>) -> Message {
        Message {
            id: id.to_string(),
            to: "ops".to_string(),
            from: "user".to_string(),
            body: "hi".to_string(),
            session: None,
            reply_to: None,
            dedupe_key: key.map(str::to_string),
            at: 1,
        }
    }

    #[test]
    fn messages_are_pending_until_settled_and_deduplicated() {
        let store = store("messages");
        assert!(store.record_message(&message("m1", Some("k"))).unwrap());
        assert!(!store.record_message(&message("m2", Some("k"))).unwrap());
        assert!(store.record_message(&message("m3", None)).unwrap());
        assert!(store.record_message(&message("m4", None)).unwrap());
        store.record_delivered("m1", "s1").unwrap();
        store.record_undeliverable("m3", "agent gone").unwrap();
        let pending: Vec<String> = store.pending_messages().into_iter().map(|m| m.id).collect();
        assert_eq!(pending, vec!["m4"]);
        assert_eq!(store.delivered_session("m1").as_deref(), Some("s1"));
    }

    #[test]
    fn timers_are_active_until_done() {
        let store = store("timers");
        let timer = |id: &str, fire_at: u64| Timer {
            id: id.to_string(),
            agent: "ops".to_string(),
            session: None,
            fire_at,
            body: "check".to_string(),
        };
        store.record_timer(&timer("late", 20)).unwrap();
        store.record_timer(&timer("soon", 10)).unwrap();
        store.record_timer(&timer("gone", 5)).unwrap();
        store.record_timer_done("gone", "cancelled").unwrap();
        let ids: Vec<String> = store.active_timers().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["soon", "late"]);
    }

    fn question(id: &str) -> Question {
        Question {
            id: id.to_string(),
            agent: "ops".to_string(),
            session: "s1".to_string(),
            title: "Which region?".to_string(),
            body: String::new(),
            assumption: "eu".to_string(),
            default_action: "deploy to eu".to_string(),
            importance: "normal".to_string(),
            due_at: Some(5),
            asked_at: 1,
        }
    }

    #[test]
    fn a_question_is_answered_once() {
        let store = store("questions");
        store.record_question(&question("q1")).unwrap();
        store.record_question(&question("q2")).unwrap();
        assert!(store.record_answer("q1", "us", "user").unwrap());
        assert!(!store.record_answer("q1", "eu", "deadline").unwrap());
        assert!(!store.record_answer("missing", "x", "user").unwrap());
        let open: Vec<String> = store.open_questions().into_iter().map(|q| q.id).collect();
        assert_eq!(open, vec!["q2"]);
        assert_eq!(store.question("q1").unwrap().default_action, "deploy to eu");
    }

    #[test]
    fn reports_list_newest_first_with_read_state() {
        let store = store("reports");
        for (id, at) in [("r1", 1), ("r2", 2), ("r3", 3)] {
            store
                .record_report(&Report {
                    id: id.to_string(),
                    agent: "ops".to_string(),
                    session: "s1".to_string(),
                    title: id.to_string(),
                    body: String::new(),
                    at,
                    read: false,
                })
                .unwrap();
        }
        store.record_report_read("r2").unwrap();
        let reports = store.reports(2);
        assert_eq!(
            reports.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["r3", "r2"]
        );
        assert!(reports[1].read && !reports[0].read);
    }

    #[test]
    fn a_device_token_works_until_revoked() {
        let store = store("devices");
        store
            .record_device(&Device {
                id: "d1".to_string(),
                name: "phone".to_string(),
                token_hash: token_hash("secret"),
                paired_at: 1,
                revoked: false,
            })
            .unwrap();
        assert_eq!(store.device_for_token("secret").unwrap().id, "d1");
        assert!(store.device_for_token("guess").is_none());
        store.record_device_revoked("d1").unwrap();
        assert!(store.device_for_token("secret").is_none());
        assert!(store.devices()[0].revoked);
        // Only the hash is on disk.
        let log = fs::read_to_string(store.dir.join(DEVICES)).unwrap();
        assert!(!log.contains("\"secret\""));
    }
}
