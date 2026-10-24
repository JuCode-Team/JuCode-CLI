//! Append-only state under `~/.jucode/daemon/`. The daemon is the only
//! writer (`Store::open` holds a lock on the directory), so each log is read
//! from disk once and then kept in memory alongside its appends; every read
//! folds the whole log. Opening compacts the logs that only ever grow: the
//! session log down to one line per fact, and decided actions out of theirs.

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
const DEVICES: &str = "devices.jsonl";
/// The computer this state belongs to (see `claim_machine`).
const MACHINE: &str = "machine";
const TOKEN: &str = "token";
const SETTINGS: &str = "settings.json";
const WORKSPACES: &str = "workspaces.json";

const LOCK: &str = "lock";

/// How long a closed action stays listed (and can be reopened).
pub const CLOSED_KEEP_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Why an open action was put away without a decision: by the user, or by
/// a newer run of its scheduled task (`superseded`). It can be reopened
/// until it ages out.
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
    /// The scheduled task whose run this session is, if any.
    pub schedule: Option<String>,
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
    /// The project it belongs to ("": none; None: recorded before sessions
    /// named theirs, so the one whose main directory is `cwd`).
    pub project: Option<String>,
    /// ACP: the built-in agent it runs (None: a desktop-registered one).
    pub agent: Option<String>,
    /// ACP: the agent's own id for the conversation.
    pub agent_session: Option<String>,
    /// Claude / Codex: the custom provider it last ran on off the gateway
    /// ("": the tool's own configuration; None: not recorded).
    pub tool_provider: Option<String>,
    /// The git worktree the daemon made for it (a scheduled run's):
    /// `{repo, path, branch, base, slug}`.
    pub worktree: Option<Value>,
    /// The session that started it (`start_session`).
    pub parent: Option<String>,
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
        // Before the session log is compacted: it still says which
        // sessions were agents'. A failed step runs again on the next start;
        // the daemon starts either way (what is left is only put aside).
        if let Err(error) = crate::migrate::remove_agents(&dir) {
            jucode_agent_core::log_warn!(
                "daemon",
                "agent data not put aside",
                error = error.to_string()
            );
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

    /// Rewrites the session and action logs without the lines their folds
    /// no longer need. Runs before anything else reads the store.
    fn compact(&self) -> io::Result<()> {
        let mut sessions = Vec::new();
        for record in self.sessions() {
            let mut open = json!({
                "kind": "open", "session": record.id, "cwd": record.cwd.display().to_string(),
                "at": record.created_at,
            });
            if let Some(schedule) = &record.schedule {
                open["schedule"] = json!(schedule);
            }
            if let Some(engine) = &record.engine {
                open["engine"] = json!(engine);
            }
            if record.gateway {
                open["gateway"] = json!(true);
            }
            if let Some(agent) = &record.agent {
                open["acp_agent"] = json!(agent);
            }
            if let Some(project) = &record.project {
                open["project"] = json!(project);
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
            if let Some(conversation) = &record.agent_session {
                meta["agent_session"] = json!(conversation);
            }
            if let Some(provider) = &record.tool_provider {
                meta["tool_provider"] = json!(provider);
            }
            if let Some(tree) = &record.worktree {
                meta["worktree"] = tree.clone();
            }
            if let Some(parent) = &record.parent {
                meta["parent"] = json!(parent);
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
        self.rewrite(ACTIONS, actions)
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

    pub fn record_session(&self, id: &str, cwd: &std::path::Path) -> io::Result<()> {
        self.record_engine_session(id, cwd, None, None, false, None, None)
    }

    /// Like `record_session`, for a session run by `engine` (None: jucode),
    /// as a run of `schedule` or not; `agent`: the ACP preset it runs (kept
    /// as `acp_agent`: `agent` marked the sessions of the removed agents,
    /// see `migrate`); `project`: the project it belongs to ("": none).
    #[allow(clippy::too_many_arguments)]
    pub fn record_engine_session(
        &self,
        id: &str,
        cwd: &std::path::Path,
        schedule: Option<&str>,
        engine: Option<&str>,
        gateway: bool,
        agent: Option<&str>,
        project: Option<&str>,
    ) -> io::Result<()> {
        let mut entry = json!({
            "kind": "open", "session": id, "cwd": cwd.display().to_string(), "at": now(),
        });
        if let Some(schedule) = schedule {
            entry["schedule"] = json!(schedule);
        }
        if let Some(engine) = engine {
            entry["engine"] = json!(engine);
        }
        if gateway {
            entry["gateway"] = json!(true);
        }
        if let Some(agent) = agent {
            entry["acp_agent"] = json!(agent);
        }
        if let Some(project) = project {
            entry["project"] = json!(project);
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
    /// (`title`, `archived`, `hidden`, `group`, `project`; the daemon's own `gateway`,
    /// `agent_session` and `tool_provider`); fields left out are
    /// unchanged, an empty title goes back to the engine's label and an
    /// empty group to automatic routing and an empty project to none. False
    /// when `changes` names none.
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
        if let Some(project) = changes["project"].as_str() {
            entry["project"] = json!(project.trim());
            changed = true;
        }
        if let Some(gateway) = changes["gateway"].as_bool() {
            entry["gateway"] = json!(gateway);
            changed = true;
        }
        if let Some(conversation) = changes["agent_session"].as_str() {
            entry["agent_session"] = json!(conversation);
            changed = true;
        }
        if let Some(provider) = changes["tool_provider"].as_str() {
            entry["tool_provider"] = json!(provider);
            changed = true;
        }
        if changes["worktree"].is_object() {
            entry["worktree"] = changes["worktree"].clone();
            changed = true;
        }
        if let Some(parent) = changes["parent"].as_str() {
            entry["parent"] = json!(parent);
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
                            schedule: entry["schedule"].as_str().map(str::to_string),
                            created_at: entry["at"].as_u64().unwrap_or_default(),
                            closed: false,
                            title: None,
                            title_auto: false,
                            archived: false,
                            hidden: false,
                            engine: entry["engine"].as_str().map(str::to_string),
                            gateway: false,
                            group: None,
                            project: None,
                            agent: None,
                            agent_session: None,
                            tool_provider: None,
                            worktree: None,
                            parent: None,
                        }
                    });
                    record.closed = false;
                    record.gateway = entry["gateway"] == true;
                    record.agent = entry["acp_agent"].as_str().map(str::to_string);
                    if let Some(project) = entry["project"].as_str() {
                        record.project = Some(project.to_string());
                    }
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
                        if let Some(project) = entry["project"].as_str() {
                            record.project = Some(project.to_string());
                        }
                        if let Some(gateway) = entry["gateway"].as_bool() {
                            record.gateway = gateway;
                        }
                        if let Some(conversation) = entry["agent_session"].as_str() {
                            record.agent_session = Some(conversation.to_string());
                        }
                        if let Some(provider) = entry["tool_provider"].as_str() {
                            record.tool_provider = Some(provider.to_string());
                        }
                        if entry["worktree"].is_object() {
                            record.worktree = Some(entry["worktree"].clone());
                        }
                        if let Some(parent) = entry["parent"].as_str() {
                            record.parent = Some(parent.to_string());
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
        let decided = decided_ids(&entries);
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
        let decided = decided_ids(&entries);
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

    /// Closes an open action without deciding it; false when it is not
    /// open (unknown, decided or already closed).
    pub fn close_action(&self, id: &str, by: &str, reason: &str) -> io::Result<bool> {
        let _guard = self.lock();
        if self.action_state(id) != Some(false) {
            return Ok(false);
        }
        self.append_locked(
            ACTIONS,
            json!({ "kind": "closed", "id": id, "by": by, "reason": reason, "at": now() }),
        )?;
        Ok(true)
    }

    /// Puts a closed action back among the open ones; false when it is not
    /// closed.
    pub fn reopen_action(&self, id: &str) -> io::Result<bool> {
        let _guard = self.lock();
        if self.action_state(id) != Some(true) {
            return Ok(false);
        }
        self.append_locked(
            ACTIONS,
            json!({ "kind": "reopened", "id": id, "at": now() }),
        )?;
        Ok(true)
    }

    /// None: unknown or decided; Some(closed) otherwise.
    fn action_state(&self, id: &str) -> Option<bool> {
        let entries = self.read(ACTIONS);
        if !entries
            .iter()
            .any(|e| e["kind"] == "deferred" && e["action"]["id"] == id)
            || decided_ids(&entries).contains(id)
        {
            return None;
        }
        Some(closures(&entries).contains_key(id))
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

/// Ids of decided actions.
fn decided_ids(entries: &[Value]) -> HashSet<&str> {
    entries
        .iter()
        .filter(|entry| entry["kind"] == "decided")
        .filter_map(|entry| entry["id"].as_str())
        .collect()
}

/// Closed actions by id, each with its latest `closed` entry; reopening one
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
        store.record_session("b", Path::new("/p")).unwrap();
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
            .record_session("a", std::path::Path::new("/p/a"))
            .unwrap();
        store
            .record_engine_session(
                "b",
                std::path::Path::new("/p/b"),
                Some("sch-1"),
                None,
                false,
                None,
                None,
            )
            .unwrap();
        store.record_session_closed("a").unwrap();
        let sessions = store.sessions();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "a");
        assert!(sessions[0].closed);
        assert!(!sessions[1].closed);
        assert_eq!(sessions[1].schedule.as_deref(), Some("sch-1"));
        store
            .record_session("a", std::path::Path::new("/p/a"))
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
            .record_engine_session("a", cwd, Some("sch-1"), Some("claude"), true, None, None)
            .unwrap();
        store.record_session("b", cwd).unwrap();
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
        store
            .record_engine_session("a", cwd, Some("sch-1"), Some("claude"), true, None, None)
            .unwrap();
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

        let dir = store.dir.clone();
        let before = (store.sessions(), store.open_actions());
        let lines = |file: &str| fs::read_to_string(dir.join(file)).unwrap().lines().count();
        let session_lines = lines(SESSIONS);
        drop(store);

        let store = Store::open(dir.clone()).unwrap();
        assert_eq!(store.sessions(), before.0);
        assert_eq!(store.open_actions(), before.1);
        assert!(lines(SESSIONS) < session_lines);
        assert_eq!(lines(ACTIONS), 1);
        // A fresh read from disk agrees with the cached copy.
        drop(store);
        let store = Store::open(dir).unwrap();
        assert_eq!(store.sessions(), before.0);
    }

    #[test]
    fn a_session_keeps_its_project_through_compaction() {
        let store = store("project");
        store
            .record_session("old", std::path::Path::new("/p/app"))
            .unwrap();
        store
            .record_engine_session(
                "new",
                std::path::Path::new("/p/other"),
                None,
                None,
                false,
                None,
                Some("p1"),
            )
            .unwrap();
        store
            .record_session_meta("old", &json!({ "project": "" }))
            .unwrap();
        let dir = store.dir.clone();
        drop(store);
        let store = Store::open(dir).unwrap();
        let projects: Vec<Option<String>> =
            store.sessions().into_iter().map(|r| r.project).collect();
        assert_eq!(projects, [Some(String::new()), Some("p1".into())]);
    }

    #[test]
    fn a_session_remembers_whether_it_last_ran_through_the_gateway() {
        let store = store("gateway");
        let cwd = std::path::Path::new("/p");
        store
            .record_engine_session("c", cwd, None, Some("claude"), true, None, None)
            .unwrap();
        assert!(store.sessions()[0].gateway);
        store.record_session_closed("c").unwrap();
        store
            .record_engine_session("c", cwd, None, Some("claude"), false, None, None)
            .unwrap();
        assert!(!store.sessions()[0].gateway);
    }

    #[test]
    fn an_acp_session_keeps_its_agent_and_the_agents_conversation() {
        let dir =
            std::env::temp_dir().join(format!("jucode-daemon-store-acp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = Store::open(dir.clone()).unwrap();
        let cwd = std::path::Path::new("/p");
        store
            .record_engine_session("s", cwd, None, Some("acp"), false, Some("omp"), None)
            .unwrap();
        assert!(store
            .record_session_meta("s", &json!({ "agent_session": "a-1" }))
            .unwrap());
        assert!(store
            .record_session_meta("s", &json!({ "tool_provider": "p1" }))
            .unwrap());
        store.record_session_closed("s").unwrap();
        store
            .record_engine_session("s", cwd, None, Some("acp"), true, Some("omp"), None)
            .unwrap();
        let record = store.sessions()[0].clone();
        assert_eq!(record.agent.as_deref(), Some("omp"));
        assert_eq!(record.agent_session.as_deref(), Some("a-1"));
        assert_eq!(record.tool_provider.as_deref(), Some("p1"));
        // Compacted on the next open, all stay.
        drop(store);
        let store = Store::open(dir).unwrap();
        assert_eq!(store.sessions()[0], record);
    }

    #[test]
    fn a_session_keeps_its_group_until_cleared() {
        let store = store("group");
        store
            .record_engine_session(
                "c",
                std::path::Path::new("/p"),
                None,
                Some("codex"),
                true,
                None,
                None,
            )
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
    fn a_closed_action_leaves_the_open_list_until_reopened() {
        let store = store("closed-actions");
        let action = DeferredAction::from_json(&json!({
            "id": "a1", "session_id": "s1", "cwd": "/p", "call_id": "c", "name": "bash",
            "arguments": "{}", "summary": "ls", "digest": "d", "created_at": 1,
        }))
        .unwrap();
        store.record_deferred(&action).unwrap();

        assert!(store
            .close_action("a1", "superseded", "新一次运行")
            .unwrap());
        assert!(!store.close_action("a1", "user", "again").unwrap());
        assert!(store.open_actions().is_empty());
        let closed = store.closed_actions(0);
        assert_eq!(closed[0].0.id, "a1");
        assert_eq!(
            (closed[0].1.by.as_str(), closed[0].1.reason.as_str()),
            ("superseded", "新一次运行")
        );
        assert!(store.closed_actions(now() + 1).is_empty());

        // Closed actions survive a restart's compaction, still reopenable.
        let dir = store.dir.clone();
        drop(store);
        let store = Store::open(dir).unwrap();
        assert_eq!(store.closed_actions(0).len(), 1);
        assert!(store.reopen_action("a1").unwrap());
        assert!(!store.reopen_action("a1").unwrap());
        assert_eq!(store.open_actions()[0].id, "a1");
        // Decided actions neither close nor reopen.
        store.record_decided("a1", true).unwrap();
        assert!(!store.close_action("a1", "user", "").unwrap());
        assert!(!store.close_action("missing", "user", "").unwrap());
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
