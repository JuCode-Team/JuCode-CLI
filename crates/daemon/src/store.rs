//! Append-only state under `~/.jucode/daemon/`. The daemon is the only
//! writer (`Store::open` holds a lock on the directory), so each log is read
//! from disk once and then kept in memory alongside its appends; every read
//! folds the whole log. Opening compacts the logs that only ever grow: the
//! session and run logs down to one line per fact, decided actions and
//! finished timers out of theirs, and messages, questions and reports once
//! they are settled and old.

use jucode_agent_core::actions::DeferredAction;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
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
const TASKS: &str = "tasks.jsonl";
const RUNS: &str = "runs.jsonl";
const DEVICES: &str = "devices.jsonl";
/// The computer this state belongs to (see `claim_machine`).
const MACHINE: &str = "machine";
const TOKEN: &str = "token";
const SETTINGS: &str = "settings.json";
const WORKSPACES: &str = "workspaces.json";

const LOCK: &str = "lock";

/// How long a closed question or action stays listed (and can be reopened).
pub const CLOSED_KEEP_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Settled messages and questions older than this leave their logs.
const HISTORY_KEEP_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Reports older than this leave theirs, except the newest `REPORTS_KEPT`.
const REPORTS_KEEP_MS: u64 = 60 * 24 * 60 * 60 * 1000;
const REPORTS_KEPT: usize = 100;

/// What waits for the user: an agent's question or a deferred action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ItemKind {
    Question,
    Action,
}

impl ItemKind {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "question" => Ok(Self::Question),
            "action" => Ok(Self::Action),
            other => Err(format!(
                "unknown item kind '{other}': use question or action"
            )),
        }
    }

    fn file(self) -> &'static str {
        match self {
            Self::Question => QUESTIONS,
            Self::Action => ACTIONS,
        }
    }

    /// The log entry that settles an item for good (answered or decided).
    fn settled(self) -> &'static str {
        match self {
            Self::Question => "answered",
            Self::Action => "decided",
        }
    }

    fn opened(self) -> &'static str {
        match self {
            Self::Question => "asked",
            Self::Action => "deferred",
        }
    }
}

/// Why an open item was put away without an answer: by the user, by an
/// agent (`agent:<id>`) or by a newer run of its scheduled task
/// (`superseded`). It can be reopened until it ages out.
#[derive(Debug, Clone, PartialEq)]
pub struct Closure {
    pub by: String,
    pub reason: String,
    pub at: u64,
}

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
    /// The task it was asked for (see `tasks`); it stays open across runs.
    pub task: Option<String>,
    /// The agent's name for the question: asking again with the same key
    /// in the same task updates the open one instead of adding another.
    pub key: Option<String>,
}

/// A task of an agent that is not a scheduled one (those are their
/// schedule): what the user, another agent or the agent's own session set
/// it to do. It has one session; every message into it is one of its runs.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskRecord {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub instruction: String,
    /// `user`, `agent:<id>` or another message origin.
    pub origin: String,
    pub session: Option<String>,
    pub created_at: u64,
    pub closed: Option<Closure>,
}

/// What a run concluded: by the agent (`finish`) or inferred afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// `quiet`, `done`, `needs_you` or `failed`.
    pub verdict: String,
    /// For the user, a few lines.
    pub summary: String,
    /// For the next run of the task.
    pub next: String,
    /// `agent` or `inferred`.
    pub source: String,
    pub at: u64,
}

/// One execution of a task: a message delivered into its session (or a
/// scheduled time that was skipped), from start to the end of the turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub id: String,
    pub agent: String,
    /// A task id or a schedule id.
    pub task: String,
    /// None: skipped before it reached a session.
    pub session: Option<String>,
    /// `user`, `schedule`, `reply`, `answer`, `timer`, `agent` or `other`.
    pub trigger: String,
    /// The message that started it.
    pub message: Option<String>,
    /// `queued`, `running`, `succeeded`, `failed`, `interrupted`,
    /// `skipped` or `cancelled`.
    pub status: String,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub outcome: Option<Outcome>,
}

impl Run {
    pub fn ended(&self) -> bool {
        self.ended_at.is_some()
    }
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
        // A child this process (or the previous holder) is spawning keeps the
        // lock between fork and exec for a few milliseconds; another daemon
        // keeps it for good.
        let mut attempts = 0;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) if attempts < 25 => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("another daemon is using {}", dir.display()),
                    ))
                }
                Err(fs::TryLockError::Error(error)) => return Err(error),
            }
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
        let mut actions: Vec<Value> = self
            .open_actions()
            .iter()
            .map(|action| json!({ "kind": "deferred", "action": action.to_json() }))
            .collect();
        // Closed ones stay while they can still be reopened.
        for (action, closure) in self.closed_actions(now().saturating_sub(CLOSED_KEEP_MS)) {
            actions.push(json!({ "kind": "deferred", "action": action.to_json() }));
            actions.push(json!({
                "kind": "closed", "id": action.id, "by": closure.by,
                "reason": closure.reason, "at": closure.at,
            }));
        }
        self.rewrite(ACTIONS, actions)?;
        let active: HashSet<String> = self.active_timers().into_iter().map(|t| t.id).collect();
        let timers = self
            .read(TIMERS)
            .iter()
            .filter(|entry| entry["kind"] == "set")
            .filter(|entry| entry["id"].as_str().is_some_and(|id| active.contains(id)))
            .cloned()
            .collect();
        self.rewrite(TIMERS, timers)?;
        self.compact_history()
    }

    /// Runs folded to their final state; settled messages and questions
    /// older than `HISTORY_KEEP_MS`, and old reports, dropped.
    fn compact_history(&self) -> io::Result<()> {
        let since = now().saturating_sub(HISTORY_KEEP_MS);
        let mut runs = Vec::new();
        for run in self.runs() {
            runs.push(json!({
                "kind": "started", "id": run.id, "agent": run.agent, "task": run.task,
                "session": run.session, "trigger": run.trigger, "message": run.message,
                "status": run.status, "at": run.started_at, "ended_at": run.ended_at,
            }));
            if let Some(o) = &run.outcome {
                runs.push(json!({
                    "kind": "outcome", "id": run.id, "verdict": o.verdict, "summary": o.summary,
                    "next": o.next, "source": o.source, "at": o.at,
                }));
            }
        }
        self.rewrite(RUNS, runs)?;

        // A message stays while pending or settled lately.
        let entries = self.read(MESSAGES);
        let settled_at: HashMap<&str, u64> = entries
            .iter()
            .filter(|entry| entry["kind"] == "delivered" || entry["kind"] == "undeliverable")
            .filter_map(|entry| {
                Some((
                    entry["id"].as_str()?,
                    entry["at"].as_u64().unwrap_or_default(),
                ))
            })
            .collect();
        let gone: HashSet<&str> = settled_at
            .iter()
            .filter(|(_, at)| **at < since)
            .map(|(id, _)| *id)
            .collect();
        let kept: Vec<Value> = entries
            .iter()
            .filter(|entry| !entry["id"].as_str().is_some_and(|id| gone.contains(id)))
            .cloned()
            .collect();
        self.rewrite(MESSAGES, kept)?;

        // A question stays while open, or answered / closed lately; only
        // its latest wording is kept.
        let entries = self.read(QUESTIONS);
        let closed = closures(&entries);
        let mut done_at: HashMap<&str, u64> = entries
            .iter()
            .filter(|entry| entry["kind"] == "answered")
            .filter_map(|entry| {
                Some((
                    entry["id"].as_str()?,
                    entry["at"].as_u64().unwrap_or_default(),
                ))
            })
            .collect();
        for (id, entry) in &closed {
            done_at.insert(id, entry["at"].as_u64().unwrap_or_default());
        }
        let latest: HashSet<*const Value> = latest_asked(&entries)
            .into_iter()
            .map(|e| e as *const Value)
            .collect();
        let kept: Vec<Value> = entries
            .iter()
            .filter(|entry| {
                let id = entry["id"].as_str().unwrap_or_default();
                if done_at.get(id).is_some_and(|at| *at < since) {
                    return false;
                }
                entry["kind"] != "asked" || latest.contains(&(*entry as *const Value))
            })
            .cloned()
            .collect();
        self.rewrite(QUESTIONS, kept)?;

        // Reports: the newest REPORTS_KEPT, and any from the last 60 days.
        let entries = self.read(REPORTS);
        let report_since = now().saturating_sub(REPORTS_KEEP_MS);
        let newest: HashSet<&str> = entries
            .iter()
            .rev()
            .filter(|entry| entry["kind"] == "posted")
            .take(REPORTS_KEPT)
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        let old: HashSet<&str> = entries
            .iter()
            .filter(|entry| entry["kind"] == "posted")
            .filter(|entry| entry["at"].as_u64().unwrap_or_default() < report_since)
            .filter_map(|entry| entry["id"].as_str())
            .filter(|id| !newest.contains(id))
            .collect();
        let kept: Vec<Value> = entries
            .iter()
            .filter(|entry| !entry["id"].as_str().is_some_and(|id| old.contains(id)))
            .cloned()
            .collect();
        self.rewrite(REPORTS, kept)
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
        // A torn settings.json reads as no settings at all.
        write_private(
            &self.dir.join(SETTINGS),
            format!("{settings:#}\n").as_bytes(),
        )
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

    /// Deferred actions neither decided nor closed, oldest first.
    pub fn open_actions(&self) -> Vec<DeferredAction> {
        let entries = self.read(ACTIONS);
        let decided = settled_ids(&entries, ItemKind::Action);
        let closed = closures(&entries);
        entries
            .iter()
            .filter(|entry| entry["kind"] == "deferred")
            .filter_map(|entry| DeferredAction::from_json(&entry["action"]))
            .filter(|action| !decided.contains(action.id.as_str()))
            .filter(|action| !closed.contains_key(action.id.as_str()))
            .collect()
    }

    /// Actions closed since `since` (ms) and still closed, newest first.
    pub fn closed_actions(&self, since: u64) -> Vec<(DeferredAction, Closure)> {
        let entries = self.read(ACTIONS);
        let decided = settled_ids(&entries, ItemKind::Action);
        let closed = closures(&entries);
        let mut list: Vec<(DeferredAction, Closure)> = entries
            .iter()
            .filter(|entry| entry["kind"] == "deferred")
            .filter_map(|entry| DeferredAction::from_json(&entry["action"]))
            .filter(|action| !decided.contains(action.id.as_str()))
            .filter_map(|action| {
                let closure = closure_from(closed.get(action.id.as_str())?)?;
                (closure.at >= since).then_some((action, closure))
            })
            .collect();
        list.sort_by_key(|(_, closure)| std::cmp::Reverse(closure.at));
        list
    }

    /// Closes an open question or action without answering it; false when
    /// it is not open (unknown, settled or already closed).
    pub fn close_item(&self, kind: ItemKind, id: &str, by: &str, reason: &str) -> io::Result<bool> {
        let _guard = self.lock();
        if self.item_state(kind, id) != Some(false) {
            return Ok(false);
        }
        self.append_locked(
            kind.file(),
            json!({ "kind": "closed", "id": id, "by": by, "reason": reason, "at": now() }),
        )?;
        Ok(true)
    }

    /// Puts a closed item back among the open ones; false when it is not
    /// closed.
    pub fn reopen_item(&self, kind: ItemKind, id: &str) -> io::Result<bool> {
        let _guard = self.lock();
        if self.item_state(kind, id) != Some(true) {
            return Ok(false);
        }
        self.append_locked(
            kind.file(),
            json!({ "kind": "reopened", "id": id, "at": now() }),
        )?;
        Ok(true)
    }

    /// None: unknown or settled; Some(closed) otherwise.
    fn item_state(&self, kind: ItemKind, id: &str) -> Option<bool> {
        let entries = self.read(kind.file());
        let opened = |entry: &Value| match kind {
            ItemKind::Question => entry["id"] == id,
            ItemKind::Action => entry["action"]["id"] == id,
        };
        if !entries
            .iter()
            .any(|e| e["kind"] == kind.opened() && opened(e))
            || settled_ids(&entries, kind).contains(id)
        {
            return None;
        }
        Some(closures(&entries).contains_key(id))
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

    /// The sessions messages from `from` (`schedule:<id>`, …) were
    /// delivered to, in delivery order.
    pub fn sessions_reached_from(&self, from: &str) -> Vec<String> {
        let entries = self.read(MESSAGES);
        let sent: HashSet<&str> = entries
            .iter()
            .filter(|entry| entry["kind"] == "message" && entry["from"] == from)
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        let mut sessions: Vec<String> = Vec::new();
        for entry in entries.iter().filter(|entry| entry["kind"] == "delivered") {
            if !entry["id"].as_str().is_some_and(|id| sent.contains(id)) {
                continue;
            }
            if let Some(session) = entry["session"].as_str() {
                if !sessions.iter().any(|s| s == session) {
                    sessions.push(session.to_string());
                }
            }
        }
        sessions
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

    /// Records a question; recording one with the id of an earlier one
    /// replaces what it says (asked again).
    pub fn record_question(&self, question: &Question) -> io::Result<()> {
        self.append(
            QUESTIONS,
            json!({
                "kind": "asked", "id": question.id, "agent": question.agent,
                "session": question.session, "title": question.title, "body": question.body,
                "assumption": question.assumption, "default": question.default_action,
                "importance": question.importance, "due_at": question.due_at,
                "at": question.asked_at, "task": question.task, "key": question.key,
            }),
        )
    }

    /// Records the answer; false when the question was already answered, is
    /// closed or does not exist, so a user answer and a deadline never both
    /// land.
    pub fn record_answer(&self, id: &str, answer: &str, by: &str) -> io::Result<bool> {
        let _guard = self.lock();
        let entries = self.read(QUESTIONS);
        let asked = entries
            .iter()
            .any(|entry| entry["kind"] == "asked" && entry["id"] == id);
        let answered = entries
            .iter()
            .any(|entry| entry["kind"] == "answered" && entry["id"] == id);
        if !asked || answered || closures(&entries).contains_key(id) {
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
            .rev()
            .find(|entry| entry["kind"] == "asked" && entry["id"] == id)
            .and_then(question_from_json)
    }

    /// Questions neither answered nor closed, oldest first.
    pub fn open_questions(&self) -> Vec<Question> {
        let entries = self.read(QUESTIONS);
        let answered = settled_ids(&entries, ItemKind::Question);
        let closed = closures(&entries);
        latest_asked(&entries)
            .into_iter()
            .filter(|entry| {
                !entry["id"]
                    .as_str()
                    .is_some_and(|id| answered.contains(id) || closed.contains_key(id))
            })
            .filter_map(question_from_json)
            .collect()
    }

    /// Questions closed since `since` (ms) and still closed, newest first.
    pub fn closed_questions(&self, since: u64) -> Vec<(Question, Closure)> {
        let entries = self.read(QUESTIONS);
        let closed = closures(&entries);
        let mut list: Vec<(Question, Closure)> = latest_asked(&entries)
            .into_iter()
            .filter_map(|entry| {
                let closure = closure_from(closed.get(entry["id"].as_str()?)?)?;
                (closure.at >= since).then_some((question_from_json(entry)?, closure))
            })
            .collect();
        list.sort_by_key(|(_, closure)| std::cmp::Reverse(closure.at));
        list
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

    pub fn record_task(&self, task: &TaskRecord) -> io::Result<()> {
        self.append(
            TASKS,
            json!({
                "kind": "created", "id": task.id, "agent": task.agent, "title": task.title,
                "instruction": task.instruction, "origin": task.origin,
                "session": task.session, "at": task.created_at,
            }),
        )
    }

    /// Closes a task (one of `tasks` or a scheduled one, by id); false when
    /// it is closed already.
    pub fn close_task(&self, id: &str, by: &str, reason: &str) -> io::Result<bool> {
        let _guard = self.lock();
        if closures(&self.read(TASKS)).contains_key(id) {
            return Ok(false);
        }
        self.append_locked(
            TASKS,
            json!({ "kind": "closed", "id": id, "by": by, "reason": reason, "at": now() }),
        )?;
        Ok(true)
    }

    /// Reopens a closed task; false when it is open.
    pub fn reopen_task(&self, id: &str) -> io::Result<bool> {
        let _guard = self.lock();
        if !closures(&self.read(TASKS)).contains_key(id) {
            return Ok(false);
        }
        self.append_locked(TASKS, json!({ "kind": "reopened", "id": id, "at": now() }))?;
        Ok(true)
    }

    /// Closed tasks (scheduled ones too) by id.
    pub fn task_closures(&self) -> HashMap<String, Closure> {
        closures(&self.read(TASKS))
            .into_iter()
            .filter_map(|(id, entry)| Some((id.to_string(), closure_from(entry)?)))
            .collect()
    }

    /// Takes a run out of the log: it never reached its session.
    pub fn discard_run(&self, id: &str) -> io::Result<()> {
        self.append(RUNS, json!({ "kind": "discarded", "id": id, "at": now() }))
    }

    /// Every task, oldest first.
    pub fn tasks(&self) -> Vec<TaskRecord> {
        let entries = self.read(TASKS);
        let closed = closures(&entries);
        entries
            .iter()
            .filter(|entry| entry["kind"] == "created")
            .filter_map(|entry| {
                let text = |key: &str| entry[key].as_str().map(str::to_string);
                let id = text("id")?;
                Some(TaskRecord {
                    closed: closed
                        .get(id.as_str())
                        .and_then(|entry| closure_from(entry)),
                    agent: text("agent")?,
                    title: text("title").unwrap_or_default(),
                    instruction: text("instruction").unwrap_or_default(),
                    origin: text("origin").unwrap_or_else(|| "user".to_string()),
                    session: text("session"),
                    created_at: entry["at"].as_u64().unwrap_or_default(),
                    id,
                })
            })
            .collect()
    }

    pub fn record_run(&self, run: &Run) -> io::Result<()> {
        self.append(
            RUNS,
            json!({
                "kind": "started", "id": run.id, "agent": run.agent, "task": run.task,
                "session": run.session, "trigger": run.trigger, "message": run.message,
                "status": run.status, "at": run.started_at, "ended_at": run.ended_at,
            }),
        )?;
        if let Some(outcome) = &run.outcome {
            self.record_outcome(&run.id, outcome)?;
        }
        Ok(())
    }

    /// A queued run started working.
    pub fn record_run_running(&self, id: &str) -> io::Result<()> {
        self.append(RUNS, json!({ "kind": "running", "id": id, "at": now() }))
    }

    pub fn record_run_ended(&self, id: &str, status: &str) -> io::Result<()> {
        self.append(
            RUNS,
            json!({ "kind": "ended", "id": id, "status": status, "at": now() }),
        )
    }

    /// Sets what a run concluded, replacing an earlier outcome.
    pub fn record_outcome(&self, id: &str, outcome: &Outcome) -> io::Result<()> {
        self.append(
            RUNS,
            json!({
                "kind": "outcome", "id": id, "verdict": outcome.verdict,
                "summary": outcome.summary, "next": outcome.next,
                "source": outcome.source, "at": outcome.at,
            }),
        )
    }

    /// Every run, oldest first.
    pub fn runs(&self) -> Vec<Run> {
        let entries = self.read(RUNS);
        let mut runs: Vec<Run> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for entry in entries.iter() {
            let Some(id) = entry["id"].as_str() else {
                continue;
            };
            let text = |key: &str| entry[key].as_str().map(str::to_string);
            let at = entry["at"].as_u64().unwrap_or_default();
            if entry["kind"] == "started" {
                let (Some(agent), Some(task)) = (text("agent"), text("task")) else {
                    continue;
                };
                index.insert(id.to_string(), runs.len());
                runs.push(Run {
                    id: id.to_string(),
                    agent,
                    task,
                    session: text("session"),
                    trigger: text("trigger").unwrap_or_else(|| "other".to_string()),
                    message: text("message"),
                    status: text("status").unwrap_or_else(|| "queued".to_string()),
                    started_at: at,
                    ended_at: entry["ended_at"].as_u64(),
                    outcome: None,
                });
                continue;
            }
            if entry["kind"] == "discarded" {
                index.remove(id);
                continue;
            }
            let Some(run) = index.get(id).map(|&i| &mut runs[i]) else {
                continue;
            };
            match entry["kind"].as_str() {
                Some("running") if !run.ended() => {
                    run.status = "running".to_string();
                    run.started_at = at;
                }
                Some("ended") if !run.ended() => {
                    run.status = text("status").unwrap_or_else(|| "succeeded".to_string());
                    run.ended_at = Some(at);
                }
                Some("outcome") => {
                    run.outcome = Some(Outcome {
                        verdict: text("verdict").unwrap_or_else(|| "done".to_string()),
                        summary: text("summary").unwrap_or_default(),
                        next: text("next").unwrap_or_default(),
                        source: text("source").unwrap_or_else(|| "inferred".to_string()),
                        at,
                    });
                }
                _ => {}
            }
        }
        let kept: HashSet<usize> = index.into_values().collect();
        runs.into_iter()
            .enumerate()
            .filter(|(i, _)| kept.contains(i))
            .map(|(_, run)| run)
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
            .read(true)
            .append(true)
            .open(self.dir.join(file))?;
        // A line torn by a crash has no newline: without one first, this
        // record would join it and both would be skipped as unparseable.
        let mut line = format!("{value}\n");
        if ends_torn(&mut out)? {
            line.insert(0, '\n');
        }
        out.write_all(line.as_bytes())?;
        if let Some(entries) = self.cached().get_mut(file) {
            Arc::make_mut(entries).push(value);
        }
        Ok(())
    }

    /// Every parseable line; a torn last line from a crash is skipped (read
    /// lossily: one cut mid-character must not make the whole file unreadable).
    fn read(&self, file: &'static str) -> Arc<Vec<Value>> {
        let mut logs = self.cached();
        if let Some(entries) = logs.get(file) {
            return Arc::clone(entries);
        }
        let entries: Arc<Vec<Value>> = Arc::new(
            String::from_utf8_lossy(&fs::read(self.dir.join(file)).unwrap_or_default())
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
        );
        logs.insert(file, Arc::clone(&entries));
        entries
    }
}

/// Each question's latest `asked` entry, in the order first asked.
fn latest_asked(entries: &[Value]) -> Vec<&Value> {
    let mut order: Vec<&str> = Vec::new();
    let mut latest: HashMap<&str, &Value> = HashMap::new();
    for entry in entries.iter().filter(|entry| entry["kind"] == "asked") {
        let Some(id) = entry["id"].as_str() else {
            continue;
        };
        if latest.insert(id, entry).is_none() {
            order.push(id);
        }
    }
    order.into_iter().map(|id| latest[id]).collect()
}

/// Ids of items answered (questions) or decided (actions).
fn settled_ids(entries: &[Value], kind: ItemKind) -> HashSet<&str> {
    entries
        .iter()
        .filter(|entry| entry["kind"] == kind.settled())
        .filter_map(|entry| entry["id"].as_str())
        .collect()
}

/// Closed items by id, each with its latest `closed` entry; reopening one
/// takes it out.
fn closures(entries: &[Value]) -> HashMap<&str, &Value> {
    let mut closed = HashMap::new();
    for entry in entries {
        let Some(id) = entry["id"].as_str() else {
            continue;
        };
        match entry["kind"].as_str() {
            Some("closed") => {
                closed.insert(id, entry);
            }
            Some("reopened") => {
                closed.remove(id);
            }
            _ => {}
        }
    }
    closed
}

fn closure_from(entry: &Value) -> Option<Closure> {
    Some(Closure {
        by: entry["by"].as_str()?.to_string(),
        reason: entry["reason"].as_str().unwrap_or_default().to_string(),
        at: entry["at"].as_u64().unwrap_or_default(),
    })
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
        task: text("task"),
        key: text("key"),
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

/// Whether a non-empty log's last byte is not a newline.
fn ends_torn(file: &mut fs::File) -> io::Result<bool> {
    use std::io::{Seek, SeekFrom};
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
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
    fn a_line_torn_by_a_crash_does_not_take_the_next_record_with_it() {
        let dir =
            std::env::temp_dir().join(format!("jucode-daemon-store-torn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(SESSIONS),
            "{\"kind\":\"open\",\"session\":\"a\",\"cwd\":\"/p\",\"agent\":null,\"at\":1}\n{\"kind\":\"op",
        )
        .unwrap();
        let store = Store::open(dir).unwrap();
        store.record_session("b", Path::new("/p"), None).unwrap();
        let ids: Vec<String> = store.sessions().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["a", "b"]);
        drop(store);
        let reopened = Store::open(
            std::env::temp_dir().join(format!("jucode-daemon-store-torn-{}", std::process::id())),
        )
        .unwrap();
        let ids: Vec<String> = reopened.sessions().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["a", "b"]);
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
    fn old_settled_history_leaves_the_logs_and_runs_fold() {
        let store = store("history");
        let old = now() - HISTORY_KEEP_MS - 1000;
        let message = |id: &str, at: u64| Message {
            id: id.into(),
            to: "ops".into(),
            from: "user".into(),
            body: id.into(),
            session: None,
            reply_to: None,
            dedupe_key: None,
            at,
        };
        for id in ["old", "recent", "waiting"] {
            store.record_message(&message(id, old)).unwrap();
        }
        store
            .append(
                MESSAGES,
                json!({ "kind": "delivered", "id": "old", "session": "s", "at": old }),
            )
            .unwrap();
        store.record_delivered("recent", "s").unwrap();
        let q = |id: &str| {
            let mut asked = Question {
                id: id.into(),
                agent: "ops".into(),
                session: "s".into(),
                title: "first".into(),
                body: String::new(),
                assumption: String::new(),
                default_action: String::new(),
                importance: "normal".into(),
                due_at: None,
                asked_at: old,
                task: None,
                key: None,
            };
            store.record_question(&asked).unwrap();
            asked.title = "asked again".into();
            store.record_question(&asked).unwrap();
        };
        q("q-old");
        q("q-open");
        store
            .append(QUESTIONS, json!({ "kind": "answered", "id": "q-old", "answer": "y", "by": "user", "at": old }))
            .unwrap();
        let run = Run {
            id: "r1".into(),
            agent: "ops".into(),
            task: "t".into(),
            session: Some("s".into()),
            trigger: "user".into(),
            message: None,
            status: "queued".into(),
            started_at: 1,
            ended_at: None,
            outcome: None,
        };
        store.record_run(&run).unwrap();
        store.record_run_running("r1").unwrap();
        store.record_run_ended("r1", "succeeded").unwrap();
        let before_runs = store.runs();

        let dir = store.dir.clone();
        drop(store);
        let store = Store::open(dir.clone()).unwrap();
        let ids: Vec<String> = store
            .message_log(None, 10)
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["waiting", "recent"]);
        assert_eq!(store.pending_messages()[0].id, "waiting");
        let open = store.open_questions();
        assert_eq!((open.len(), open[0].title.as_str()), (1, "asked again"));
        assert!(store.question("q-old").is_none());
        assert_eq!(store.runs(), before_runs);
        let lines = fs::read_to_string(dir.join(RUNS)).unwrap().lines().count();
        assert_eq!(lines, 1);
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
            task: None,
            key: None,
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
    fn a_closed_item_leaves_the_open_lists_until_reopened() {
        let store = store("closed-items");
        store.record_question(&question("q1")).unwrap();
        let action = DeferredAction::from_json(&json!({
            "id": "a1", "session_id": "s1", "cwd": "/p", "call_id": "c", "name": "bash",
            "arguments": "{}", "summary": "ls", "digest": "d", "created_at": 1,
        }))
        .unwrap();
        store.record_deferred(&action).unwrap();

        assert!(store
            .close_item(ItemKind::Question, "q1", "user", "不需要了")
            .unwrap());
        assert!(!store
            .close_item(ItemKind::Question, "q1", "user", "again")
            .unwrap());
        assert!(store
            .close_item(ItemKind::Action, "a1", "agent:ops", "已修复")
            .unwrap());
        assert!(store.open_questions().is_empty());
        assert!(store.open_actions().is_empty());
        // A deadline does not answer a closed question.
        assert!(!store.record_answer("q1", "", "deadline").unwrap());
        let closed = store.closed_actions(0);
        assert_eq!(closed[0].0.id, "a1");
        assert_eq!(
            (closed[0].1.by.as_str(), closed[0].1.reason.as_str()),
            ("agent:ops", "已修复")
        );
        assert_eq!(store.closed_questions(0)[0].1.reason, "不需要了");
        assert!(store.closed_questions(now() + 1).is_empty());

        // Closed actions survive a restart's compaction, still reopenable.
        let dir = store.dir.clone();
        drop(store);
        let store = Store::open(dir).unwrap();
        assert_eq!(store.closed_actions(0).len(), 1);
        assert!(store.reopen_item(ItemKind::Action, "a1").unwrap());
        assert!(!store.reopen_item(ItemKind::Action, "a1").unwrap());
        assert_eq!(store.open_actions()[0].id, "a1");
        assert!(store.reopen_item(ItemKind::Question, "q1").unwrap());
        assert!(store.record_answer("q1", "yes", "user").unwrap());
        // Settled items neither close nor reopen.
        assert!(!store
            .close_item(ItemKind::Question, "q1", "user", "")
            .unwrap());
        assert!(!store
            .close_item(ItemKind::Question, "missing", "user", "")
            .unwrap());
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
