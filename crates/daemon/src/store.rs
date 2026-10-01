//! Append-only state under `~/.jucode/daemon/`. The daemon is the only
//! writer (`Store::open` holds a lock on the directory), so each log is read
//! from disk once and then kept in memory alongside its appends; every read
//! folds the whole log. Opening compacts the logs that only ever grow: the
//! session log down to one line per fact, and decided actions and finished
//! timers out of theirs.

use jucode_agent_core::actions::DeferredAction;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

const SESSIONS: &str = "sessions.jsonl";
const ACTIONS: &str = "actions.jsonl";
const MESSAGES: &str = "messages.jsonl";
const TIMERS: &str = "timers.jsonl";
const QUESTIONS: &str = "questions.jsonl";
const REPORTS: &str = "reports.jsonl";
const DEVICES: &str = "devices.jsonl";
/// The computer this state belongs to (see `claim_machine`).
const MACHINE: &str = "machine";
const TOKEN: &str = "token";
const SETTINGS: &str = "settings.json";
const WORKSPACES: &str = "workspaces.json";

const LOCK: &str = "lock";

pub struct Store {
    dir: PathBuf,
    write: Mutex<()>,
    /// Parsed logs by file name, loaded on first read.
    logs: Mutex<HashMap<&'static str, Arc<Vec<Value>>>>,
    /// Held for the store's lifetime: a second daemon on the same directory
    /// would write behind this one's cached logs.
    _lock: fs::File,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub id: String,
    pub cwd: PathBuf,
    /// The long-lived agent the session belongs to, if any.
    pub agent: Option<String>,
    pub created_at: u64,
    pub closed: bool,
    /// Set by a client; None: the engine's own label for the session.
    pub title: Option<String>,
    /// The daemon wrote `title` (first line, or the title model), so it may
    /// write it again; a title a client set stays.
    pub title_auto: bool,
    pub archived: bool,
    /// Removed from session lists (its conversation stays on disk).
    pub hidden: bool,
    /// The engine running it; None: jucode.
    pub engine: Option<String>,
    /// Claude / Codex: it last ran through the JuCode gateway.
    pub gateway: bool,
    /// The JuCode group its gateway requests route to (None: automatic).
    pub group: Option<String>,
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
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK))?;
        if let Err(error) = lock.try_lock() {
            return Err(match error {
                fs::TryLockError::WouldBlock => io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("another daemon is using {}", dir.display()),
                ),
                fs::TryLockError::Error(error) => error,
            });
        }
        let store = Self {
            dir,
            write: Mutex::new(()),
            logs: Mutex::new(HashMap::new()),
            _lock: lock,
        };
        store.compact()?;
        Ok(store)
    }

    /// Rewrites the session, action and timer logs without the lines their
    /// folds no longer need. Runs before anything else reads the store.
    fn compact(&self) -> io::Result<()> {
        let mut sessions = Vec::new();
        for record in self.sessions() {
            let mut open = json!({
                "kind": "open", "session": record.id, "cwd": record.cwd.display().to_string(),
                "agent": record.agent, "at": record.created_at,
            });
            if let Some(engine) = &record.engine {
                open["engine"] = json!(engine);
            }
            if record.gateway {
                open["gateway"] = json!(true);
            }
            sessions.push(open);
            let mut meta = json!({ "kind": "meta", "session": record.id });
            if let Some(title) = &record.title {
                meta["title"] = json!(title);
                if record.title_auto {
                    meta["title_auto"] = json!(true);
                }
            }
            for (flag, value) in [("archived", record.archived), ("hidden", record.hidden)] {
                if value {
                    meta[flag] = json!(true);
                }
            }
            if let Some(group) = &record.group {
                meta["group"] = json!(group);
            }
            if meta.as_object().is_some_and(|fields| fields.len() > 2) {
                sessions.push(meta);
            }
            if record.closed {
                sessions.push(json!({ "kind": "close", "session": record.id }));
            }
        }
        self.rewrite(SESSIONS, sessions)?;
        let actions = self
            .open_actions()
            .iter()
            .map(|action| json!({ "kind": "deferred", "action": action.to_json() }))
            .collect();
        self.rewrite(ACTIONS, actions)?;
        let active: HashSet<String> = self.active_timers().into_iter().map(|t| t.id).collect();
        let timers = self
            .read(TIMERS)
            .iter()
            .filter(|entry| entry["kind"] == "set")
            .filter(|entry| entry["id"].as_str().is_some_and(|id| active.contains(id)))
            .cloned()
            .collect();
        self.rewrite(TIMERS, timers)
    }

    /// Replaces a log with `entries` (temp file + rename) when that drops
    /// lines, and keeps them as its cached copy.
    fn rewrite(&self, file: &'static str, entries: Vec<Value>) -> io::Result<()> {
        let _guard = self.lock();
        if entries.len() >= self.read(file).len() {
            return Ok(());
        }
        let text: String = entries.iter().map(|entry| format!("{entry}\n")).collect();
        let temp = self.dir.join(format!("{file}.tmp"));
        let mut out = fs::File::create(&temp)?;
        out.write_all(text.as_bytes())?;
        // On disk before the rename replaces the only other copy.
        out.sync_all()?;
        fs::rename(&temp, self.dir.join(file))?;
        self.cached().insert(file, Arc::new(entries));
        Ok(())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A daemon setting from `settings.json`; Null when unset.
    pub fn setting(&self, key: &str) -> Value {
        self.settings()[key].clone()
    }

    pub fn set_setting(&self, key: &str, value: Value) -> io::Result<()> {
        let _guard = self.lock();
        let mut settings = self.settings();
        settings[key] = value;
        fs::write(self.dir.join(SETTINGS), format!("{settings:#}\n"))
    }

    fn settings(&self) -> Value {
        fs::read_to_string(self.dir.join(SETTINGS))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    }

    /// Records which computer this state belongs to. State copied from
    /// another computer (see `jucode_agent_core::machine`) would answer to
    /// that computer's token and paired devices: here it gets a new token
    /// and its devices are unpaired. Returns true when that happened.
    pub fn claim_machine(&self) -> io::Result<bool> {
        let Some(current) = jucode_agent_core::machine::machine_id() else {
            return Ok(false);
        };
        let path = self.dir.join(MACHINE);
        let recorded = fs::read_to_string(&path).unwrap_or_default();
        let recorded = recorded.trim();
        if recorded == current {
            return Ok(false);
        }
        let copied = !recorded.is_empty();
        if copied {
            match fs::remove_file(self.dir.join(TOKEN)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            for device in self.devices().into_iter().filter(|device| !device.revoked) {
                self.record_device_revoked(&device.id)?;
            }
        }
        fs::write(&path, format!("{current}\n"))?;
        Ok(copied)
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
        write_private(&path, format!("{token}\n").as_bytes())?;
        Ok(token)
    }

    pub fn record_session(
        &self,
        id: &str,
        cwd: &std::path::Path,
        agent: Option<&str>,
    ) -> io::Result<()> {
        self.record_engine_session(id, cwd, agent, None, false)
    }

    /// Like `record_session`, for a session run by `engine` (None: jucode).
    pub fn record_engine_session(
        &self,
        id: &str,
        cwd: &std::path::Path,
        agent: Option<&str>,
        engine: Option<&str>,
        gateway: bool,
    ) -> io::Result<()> {
        let mut entry = json!({
            "kind": "open", "session": id, "cwd": cwd.display().to_string(),
            "agent": agent, "at": now(),
        });
        if let Some(engine) = engine {
            entry["engine"] = json!(engine);
        }
        if gateway {
            entry["gateway"] = json!(true);
        }
        self.append(SESSIONS, entry)
    }

    pub fn record_session_closed(&self, id: &str) -> io::Result<()> {
        self.append(
            SESSIONS,
            json!({ "kind": "close", "session": id, "at": now() }),
        )
    }

    /// Renames, (un)archives, hides or regroups a session from `changes`
    /// (`title`, `archived`, `hidden`, `group`); fields left out are
    /// unchanged, an empty title goes back to the engine's label and an
    /// empty group to automatic routing. False when `changes` names none.
    pub fn record_session_meta(&self, id: &str, changes: &Value) -> io::Result<bool> {
        let mut entry = json!({ "kind": "meta", "session": id, "at": now() });
        let mut changed = false;
        if let Some(title) = changes["title"].as_str() {
            entry["title"] = json!(title.trim());
            if changes["title_auto"] == true {
                entry["title_auto"] = json!(true);
            }
            changed = true;
        }
        for flag in ["archived", "hidden"] {
            if let Some(value) = changes[flag].as_bool() {
                entry[flag] = json!(value);
                changed = true;
            }
        }
        if let Some(group) = changes["group"].as_str() {
            entry["group"] = json!(group.trim());
            changed = true;
        }
        if let Some(gateway) = changes["gateway"].as_bool() {
            entry["gateway"] = json!(gateway);
            changed = true;
        }
        if changed {
            self.append(SESSIONS, entry)?;
        }
        Ok(changed)
    }

    /// Workspaces and their projects: `{rev, workspaces: [...]}`. `rev`
    /// counts saves so a client can tell whether it saw the latest.
    pub fn workspaces(&self) -> Value {
        fs::read_to_string(self.dir.join(WORKSPACES))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(|doc| doc["workspaces"].is_array())
            .unwrap_or_else(|| json!({ "rev": 0, "workspaces": [] }))
    }

    /// Changes the workspace list under the write lock and saves it with the
    /// next `rev`. `change` sees the current list; an error saves nothing.
    pub fn update_workspaces(
        &self,
        change: impl FnOnce(&mut Vec<Value>) -> Result<(), String>,
    ) -> Result<Value, String> {
        let _guard = self.lock();
        let doc = self.workspaces();
        let mut list = doc["workspaces"].as_array().cloned().unwrap_or_default();
        change(&mut list)?;
        let doc = json!({ "rev": doc["rev"].as_u64().unwrap_or(0) + 1, "workspaces": list });
        let path = self.dir.join(WORKSPACES);
        let temp = self.dir.join(format!("{WORKSPACES}.tmp"));
        fs::write(&temp, format!("{doc:#}\n"))
            .and_then(|()| fs::rename(&temp, &path))
            .map_err(|error| error.to_string())?;
        Ok(doc)
    }

    /// Sessions in the order they were first opened. Reopening a closed
    /// session clears `closed`.
    pub fn sessions(&self) -> Vec<SessionRecord> {
        let mut order = Vec::new();
        let mut records: BTreeMap<String, SessionRecord> = BTreeMap::new();
        for entry in self.read(SESSIONS).iter() {
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
                            title: None,
                            title_auto: false,
                            archived: false,
                            hidden: false,
                            engine: entry["engine"].as_str().map(str::to_string),
                            gateway: false,
                            group: None,
                        }
                    });
                    record.closed = false;
                    record.gateway = entry["gateway"] == true;
                }
                Some("close") => {
                    if let Some(record) = records.get_mut(id) {
                        record.closed = true;
                    }
                }
                Some("meta") => {
                    if let Some(record) = records.get_mut(id) {
                        if let Some(title) = entry["title"].as_str() {
                            record.title = Some(title.to_string()).filter(|t| !t.is_empty());
                            record.title_auto = entry["title_auto"] == true;
                        }
                        if let Some(archived) = entry["archived"].as_bool() {
                            record.archived = archived;
                        }
                        if let Some(hidden) = entry["hidden"].as_bool() {
                            record.hidden = hidden;
                        }
                        if let Some(group) = entry["group"].as_str() {
                            record.group = Some(group.to_string()).filter(|g| !g.is_empty());
                        }
                        if let Some(gateway) = entry["gateway"].as_bool() {
                            record.gateway = gateway;
                        }
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

    /// The newest `limit` messages (to `agent`, when given), newest first,
    /// each with where it went: `pending`, `delivered` (with its session) or
    /// `undeliverable` (with the reason).
    pub fn message_log(&self, agent: Option<&str>, limit: usize) -> Vec<Value> {
        let entries = self.read(MESSAGES);
        let settled: HashMap<&str, &Value> = entries
            .iter()
            .filter(|entry| entry["kind"] == "delivered" || entry["kind"] == "undeliverable")
            .filter_map(|entry| Some((entry["id"].as_str()?, entry)))
            .collect();
        entries
            .iter()
            .rev()
            .filter(|entry| entry["kind"] == "message")
            .filter(|entry| agent.is_none_or(|agent| entry["to"] == agent))
            .take(limit)
            .map(|entry| {
                let outcome = entry["id"].as_str().and_then(|id| settled.get(id));
                json!({
                    "id": entry["id"],
                    "agent": entry["to"],
                    "from": entry["from"],
                    "body": entry["body"],
                    "at": entry["at"],
                    "status": outcome.map_or(json!("pending"), |o| o["kind"].clone()),
                    "session": outcome.map_or(Value::Null, |o| o["session"].clone()),
                    "reason": outcome.map_or(Value::Null, |o| o["reason"].clone()),
                    "settled_at": outcome.map_or(Value::Null, |o| o["at"].clone()),
                })
            })
            .collect()
    }

    /// The session a delivered message went to.
    pub fn delivered_session(&self, id: &str) -> Option<String> {
        self.read(MESSAGES)
            .iter()
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
        self.device_for_hash(&token_hash(token))
    }

    /// The active device with this token hash. A relay device's hash is of
    /// its Noise static key.
    pub fn device_for_hash(&self, hash: &str) -> Option<Device> {
        self.devices()
            .into_iter()
            .find(|device| !device.revoked && device.token_hash == hash)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn cached(&self) -> std::sync::MutexGuard<'_, HashMap<&'static str, Arc<Vec<Value>>>> {
        self.logs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn append(&self, file: &'static str, value: Value) -> io::Result<()> {
        let _guard = self.lock();
        self.append_locked(file, value)
    }

    fn append_locked(&self, file: &'static str, value: Value) -> io::Result<()> {
        let mut out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(file))?;
        out.write_all(format!("{value}\n").as_bytes())?;
        if let Some(entries) = self.cached().get_mut(file) {
            Arc::make_mut(entries).push(value);
        }
        Ok(())
    }

    /// Every parseable line; a torn last line from a crash is skipped.
    fn read(&self, file: &'static str) -> Arc<Vec<Value>> {
        let mut logs = self.cached();
        if let Some(entries) = logs.get(file) {
            return Arc::clone(entries);
        }
        let entries: Arc<Vec<Value>> = Arc::new(
            fs::read_to_string(self.dir.join(file))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
        );
        logs.insert(file, Arc::clone(&entries));
        entries
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
    bytes_hash(token.as_bytes())
}

/// Lowercase hex SHA-256.
pub fn bytes_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Writes a file only the owner can read, atomically: a reader sees the old
/// contents or the new ones, never an empty or partial file.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("write_private needs a file path"))?;
    let temp = path.with_file_name(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&temp)
        .and_then(|mut file| file.write_all(contents).and_then(|_| file.sync_all()))
        .and_then(|_| fs::rename(&temp, path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
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
    fn only_one_store_opens_a_directory() {
        let store = store("lock");
        let error = Store::open(store.dir.clone()).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        let dir = store.dir.clone();
        drop(store);
        assert!(Store::open(dir).is_ok());
    }

    #[test]
    fn reopening_compacts_the_logs_without_changing_what_they_say() {
        let store = store("compact");
        let cwd = std::path::Path::new("/p");
        store
            .record_engine_session("a", cwd, Some("ops"), Some("claude"), true)
            .unwrap();
        store.record_session("b", cwd, None).unwrap();
        for title in ["one", "two", "three"] {
            store
                .record_session_meta("a", &json!({ "title": title, "title_auto": true }))
                .unwrap();
        }
        store
            .record_session_meta("b", &json!({ "archived": true, "hidden": true }))
            .unwrap();
        store
            .record_session_meta("b", &json!({ "hidden": false }))
            .unwrap();
        store.record_session_closed("a").unwrap();
        store.record_session("a", cwd, Some("ops")).unwrap();
        store.record_session_closed("b").unwrap();
        let deferred = |id: &str| {
            DeferredAction::from_json(&json!({
                "id": id, "session_id": "a", "cwd": "/p", "call_id": id, "name": "bash",
                "arguments": "{}", "summary": "ls", "digest": id, "created_at": 1,
            }))
            .unwrap()
        };
        store.record_deferred(&deferred("x")).unwrap();
        store.record_deferred(&deferred("y")).unwrap();
        store.record_decided("x", true).unwrap();
        for (id, fire_at) in [("t1", 10), ("t2", 20)] {
            store
                .record_timer(&Timer {
                    id: id.into(),
                    agent: "ops".into(),
                    session: None,
                    fire_at,
                    body: "wake".into(),
                })
                .unwrap();
        }
        store.record_timer_done("t1", "fired").unwrap();

        let dir = store.dir.clone();
        let before = (
            store.sessions(),
            store.open_actions(),
            store.active_timers(),
        );
        let lines = |file: &str| fs::read_to_string(dir.join(file)).unwrap().lines().count();
        let session_lines = lines(SESSIONS);
        drop(store);

        let store = Store::open(dir.clone()).unwrap();
        assert_eq!(store.sessions(), before.0);
        assert_eq!(store.open_actions(), before.1);
        assert_eq!(
            store
                .active_timers()
                .iter()
                .map(|t| &t.id)
                .collect::<Vec<_>>(),
            before.2.iter().map(|t| &t.id).collect::<Vec<_>>()
        );
        assert!(lines(SESSIONS) < session_lines);
        assert_eq!(lines(ACTIONS), 1);
        assert_eq!(lines(TIMERS), 1);
        // A fresh read from disk agrees with the cached copy.
        drop(store);
        let store = Store::open(dir).unwrap();
        assert_eq!(store.sessions(), before.0);
    }

    #[test]
    fn a_session_remembers_whether_it_last_ran_through_the_gateway() {
        let store = store("gateway");
        let cwd = std::path::Path::new("/p");
        store
            .record_engine_session("c", cwd, None, Some("claude"), true)
            .unwrap();
        assert!(store.sessions()[0].gateway);
        store.record_session_closed("c").unwrap();
        store
            .record_engine_session("c", cwd, None, Some("claude"), false)
            .unwrap();
        assert!(!store.sessions()[0].gateway);
    }

    #[test]
    fn a_session_keeps_its_group_until_cleared() {
        let store = store("group");
        store
            .record_engine_session("c", std::path::Path::new("/p"), None, Some("codex"), true)
            .unwrap();
        assert_eq!(store.sessions()[0].group, None);
        assert!(store
            .record_session_meta("c", &json!({ "group": " g-1 " }))
            .unwrap());
        assert_eq!(store.sessions()[0].group.as_deref(), Some("g-1"));
        // Other changes leave it; an empty group goes back to automatic.
        store
            .record_session_meta("c", &json!({ "title": "t" }))
            .unwrap();
        assert_eq!(store.sessions()[0].group.as_deref(), Some("g-1"));
        store
            .record_session_meta("c", &json!({ "group": "" }))
            .unwrap();
        assert_eq!(store.sessions()[0].group, None);
    }

    #[test]
    fn group_and_gateway_changes_survive_a_restart() {
        let store = store("compact-group");
        let dir = store.dir.clone();
        store
            .record_engine_session("c", std::path::Path::new("/p"), None, Some("claude"), false)
            .unwrap();
        store
            .record_session_meta("c", &json!({ "group": "g-1" }))
            .unwrap();
        assert!(store
            .record_session_meta("c", &json!({ "gateway": true }))
            .unwrap());
        drop(store);
        // Opening compacts the log.
        let reopened = Store::open(dir).unwrap();
        let record = &reopened.sessions()[0];
        assert_eq!(record.group.as_deref(), Some("g-1"));
        assert!(record.gateway);
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
        let log = store.message_log(Some("ops"), 2);
        assert_eq!(log.len(), 2);
        assert_eq!(
            (log[0]["id"].as_str(), log[0]["status"].as_str()),
            (Some("m4"), Some("pending"))
        );
        assert_eq!(log[1]["status"], "undeliverable");
        assert_eq!(log[1]["reason"], "agent gone");
        let all = store.message_log(None, 10);
        assert_eq!(
            (all[2]["status"].as_str(), all[2]["session"].as_str()),
            (Some("delivered"), Some("s1"))
        );
        assert!(store.message_log(Some("other"), 10).is_empty());
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
    fn state_copied_from_another_computer_gets_a_new_token_and_no_devices() {
        let store = store("machine");
        let Some(current) = jucode_agent_core::machine::machine_id() else {
            return;
        };
        let token = store.token().unwrap();
        store
            .record_device(&Device {
                id: "d1".to_string(),
                name: "phone".to_string(),
                token_hash: token_hash("secret"),
                paired_at: 1,
                revoked: false,
            })
            .unwrap();
        // First run here: claimed, nothing reset.
        assert!(!store.claim_machine().unwrap());
        assert_eq!(store.token().unwrap(), token);
        assert!(store.device_for_token("secret").is_some());
        assert_eq!(
            fs::read_to_string(store.dir.join(MACHINE)).unwrap().trim(),
            current
        );
        // The same state on another computer.
        fs::write(store.dir.join(MACHINE), "another-computer\n").unwrap();
        assert!(store.claim_machine().unwrap());
        assert_ne!(store.token().unwrap(), token);
        assert!(store.device_for_token("secret").is_none());
        assert!(!store.claim_machine().unwrap());
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
