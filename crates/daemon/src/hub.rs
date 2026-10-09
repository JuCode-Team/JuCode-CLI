//! Shared daemon state: hosted sessions, connected clients and who watches
//! what, plus message routing and timers for long-lived agents. A session is
//! attended while at least one client watches it.

use crate::{
    agents::Agents,
    engines,
    relay::Relay,
    schedules::{self, Schedule},
    session,
    store::{
        now, random_hex, token_hash, Closure, Device, ItemKind, Message, Question, Report,
        SessionRecord, Store, Timer, CLOSED_KEEP_MS,
    },
    titles::{self, Turns},
    usage::{SessionInfo, Usage},
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::Sender,
        Arc, Mutex, MutexGuard, Weak,
    },
    thread,
};

/// How long a pairing code shown on the desktop stays valid.
const PAIRING_TTL_MS: u64 = 5 * 60 * 1000;
/// Pairing codes avoid characters that are easy to misread (0/O, 1/I).
const PAIRING_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Runs that may be in progress at once across all sessions; messages that
/// would start another run wait for a free slot.
pub const MAX_RUNNING: usize = 4;

pub struct Hub {
    pub store: Store,
    pub agents: Agents,
    pub dispatch: crate::dispatch::Dispatches,
    pub requirements: crate::requirements::Requirements,
    pub uploads: crate::uploads::Uploads,
    pub push: crate::push::Push,
    /// Remote terminals, each owned by the client that opened it.
    pub terminals: crate::terminal::Terminals,
    pub version: &'static str,
    pub relay: Relay,
    sessions: Mutex<HashMap<String, Hosted>>,
    /// Sessions whose engine is running a turn or has queued messages.
    busy: Mutex<HashSet<String>>,
    /// A newer daemon waits to replace this one (`restart_when_idle`).
    restart_pending: AtomicBool,
    /// Delivered messages a session thread has not processed yet. They hold
    /// a running slot: the engine still reports "ready" until it has read
    /// the message, and that must not free the slot early.
    claims: Mutex<HashMap<String, usize>>,
    /// Every agent's scheduled tasks (see `schedules`).
    pub(crate) schedules: Mutex<Vec<Schedule>>,
    /// Cancellation and firing must not both succeed for the same timer.
    timer_updates: Mutex<()>,
    /// Serializes message delivery (scheduler ticks, sends, tool calls).
    delivering: Mutex<()>,
    next_id: AtomicU64,
    next_generation: AtomicU64,
    clients: Mutex<HashMap<u64, Client>>,
    /// Pairing code → expiry (ms). Single use.
    pairings: Mutex<HashMap<String, u64>>,
    next_client: AtomicU64,
    /// Sessions created here that have not had a user message yet: the
    /// first one becomes their title.
    untitled: Mutex<HashSet<String>>,
    /// Images sent with a message, by session and the message's text, until
    /// the engine echoes it: no engine's `user_message` carries them.
    sent_images: Mutex<HashMap<String, Vec<(String, Value)>>>,
    /// Each session's conversation so far, for its model-written title.
    turns: Mutex<HashMap<String, Turns>>,
    /// Sessions whose title is being written now.
    titling: Mutex<HashSet<String>>,
    /// Agent sessions whose handoff note is being written now.
    handing_off: Mutex<HashSet<String>>,
    /// This hub, for the threads it starts.
    me: Weak<Hub>,
    /// Each turn's token usage (see `usage`).
    pub usage: Usage,
}

struct Client {
    outbox: Sender<String>,
    /// The paired device this connection authenticated as; None for a local
    /// client holding the daemon token.
    device: Option<String>,
}

struct Hosted {
    ops: Sender<Value>,
    cwd: PathBuf,
    watchers: HashSet<u64>,
    /// Which engine thread backs this entry (see `session_ended`).
    generation: u64,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl Hub {
    /// `relay` is the relay base URL; None turns the relay off whatever the
    /// setting says (`--no-relay`).
    pub fn new(
        store: Store,
        agents: Agents,
        version: &'static str,
        relay: Option<String>,
    ) -> Arc<Self> {
        let schedules = Mutex::new(schedules::load(&agents));
        // `~/.jucode`: uploads live beside the daemon's state, not in it.
        let jucode_dir = store.dir().parent().unwrap_or(store.dir()).to_path_buf();
        let uploads = crate::uploads::Uploads::load(&jucode_dir);
        let requirements = crate::requirements::Requirements::load(
            store.dir(),
            uploads.dir().join("requirements"),
            &store.workspaces(),
        );
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            schedules,
            timer_updates: Mutex::new(()),
            turns: Mutex::new(HashMap::new()),
            titling: Mutex::new(HashSet::new()),
            handing_off: Mutex::new(HashSet::new()),
            relay: Relay::new(relay, &store),
            usage: Usage::new(&store),
            dispatch: crate::dispatch::Dispatches::load(store.dir()),
            requirements,
            uploads,
            push: crate::push::Push::load(store.dir()),
            terminals: Default::default(),
            store,
            agents,
            version,
            sessions: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
            restart_pending: AtomicBool::new(false),
            claims: Mutex::new(HashMap::new()),
            delivering: Mutex::new(()),
            next_id: AtomicU64::new(0),
            next_generation: AtomicU64::new(0),
            clients: Mutex::new(HashMap::new()),
            pairings: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            untitled: Mutex::new(HashSet::new()),
            sent_images: Mutex::new(HashMap::new()),
        })
    }

    pub fn add_client(&self, outbox: Sender<String>, device: Option<String>) -> u64 {
        let id = self.next_client.fetch_add(1, Ordering::SeqCst);
        lock(&self.clients).insert(id, Client { outbox, device });
        id
    }

    /// The paired device a connection authenticated as (None: local).
    pub fn device_of(&self, client: u64) -> Option<String> {
        lock(&self.clients).get(&client)?.device.clone()
    }

    /// Whether the connection is a local client (not a paired device).
    pub fn is_local(&self, client: u64) -> bool {
        lock(&self.clients)
            .get(&client)
            .is_some_and(|client| client.device.is_none())
    }

    /// A new single-use pairing code and its expiry (ms).
    pub fn start_pairing(&self) -> Result<(String, u64), String> {
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).map_err(|error| error.to_string())?;
        let code: String = bytes
            .iter()
            .map(|byte| PAIRING_ALPHABET[*byte as usize % PAIRING_ALPHABET.len()] as char)
            .collect();
        let expires_at = now() + PAIRING_TTL_MS;
        let mut pairings = lock(&self.pairings);
        pairings.retain(|_, expiry| *expiry > now());
        pairings.insert(code.clone(), expires_at);
        Ok((code, expires_at))
    }

    /// Trades a pairing code for a new device and its token. The token is
    /// returned once and only its hash is stored.
    pub fn pair(&self, code: &str, name: &str) -> Result<(Device, String), String> {
        let token = random_hex(32).map_err(|error| error.to_string())?;
        let device = self.pair_hash(code, name, token_hash(&token))?;
        Ok((device, token))
    }

    /// Trades a pairing code for a new device identified by `token_hash`
    /// (a token's hash, or a relay client's static key hash).
    pub fn pair_hash(&self, code: &str, name: &str, token_hash: String) -> Result<Device, String> {
        let code = code.trim().to_ascii_uppercase();
        let valid = lock(&self.pairings)
            .remove(&code)
            .is_some_and(|expiry| expiry > now());
        if !valid {
            return Err("pairing code is wrong or expired".to_string());
        }
        let name = name.trim();
        let device = Device {
            id: self.new_id("dev"),
            name: if name.is_empty() { "device" } else { name }
                .chars()
                .take(60)
                .collect(),
            token_hash,
            paired_at: now(),
            revoked: false,
        };
        self.store
            .record_device(&device)
            .map_err(|error| error.to_string())?;
        Ok(device)
    }

    /// Revokes a device and drops its open connections.
    pub fn revoke_device(&self, id: &str) -> Result<(), String> {
        if !self
            .store
            .devices()
            .iter()
            .any(|device| device.id == id && !device.revoked)
        {
            return Err(format!("unknown device {id}"));
        }
        self.store
            .record_device_revoked(id)
            .map_err(|error| error.to_string())?;
        // Dropping the outbox ends that connection's loop.
        lock(&self.clients).retain(|_, client| client.device.as_deref() != Some(id));
        self.push.forget_device(id);
        Ok(())
    }

    pub fn devices_json(&self) -> Value {
        let list: Vec<Value> = self
            .store
            .devices()
            .iter()
            .filter(|device| !device.revoked)
            .map(|device| json!({ "id": device.id, "name": device.name, "paired_at": device.paired_at }))
            .collect();
        json!({ "type": "devices", "devices": list })
    }

    /// Drops the client, its watches and its terminals; sessions it was the
    /// last watcher of become unattended.
    pub fn remove_client(&self, client: u64) {
        lock(&self.clients).remove(&client);
        self.terminals.close_client(client);
        let watched: Vec<String> = lock(&self.sessions)
            .iter()
            .filter(|(_, hosted)| hosted.watchers.contains(&client))
            .map(|(id, _)| id.clone())
            .collect();
        for session in watched {
            self.set_watch(client, &session, false);
        }
    }

    pub fn broadcast(&self, frame: &Value) {
        let text = frame.to_string();
        lock(&self.clients).retain(|_, client| client.outbox.send(text.clone()).is_ok());
    }

    pub fn send_to(&self, client: u64, frame: &Value) {
        if let Some(client) = lock(&self.clients).get(&client) {
            let _ = client.outbox.send(frame.to_string());
        }
    }

    /// Starts a new session in `cwd`, in the agent's directory for an agent
    /// session, or in the chats directory for a chat.
    pub fn create_session(
        self: &Arc<Self>,
        cwd: Option<PathBuf>,
        agent: Option<&str>,
        chat: bool,
    ) -> Result<String, String> {
        self.create_engine_session(cwd, agent, chat, None, engines::Options::default())
    }

    /// `create_session` with the engine to run (None: jucode) and its start
    /// options.
    pub fn create_engine_session(
        self: &Arc<Self>,
        cwd: Option<PathBuf>,
        agent: Option<&str>,
        chat: bool,
        engine: Option<engines::Kind>,
        options: engines::Options,
    ) -> Result<String, String> {
        if engine.is_some() && agent.is_some() {
            return Err("agents run on the jucode engine".to_string());
        }
        let cwd = match agent {
            None if chat => {
                jucode_agent_core::chat::ensure_chats_dir().map_err(|error| error.to_string())?
            }
            Some(id) => {
                let agent = self
                    .agents
                    .get(id)
                    .ok_or_else(|| format!("unknown agent {id}"))?;
                if !agent.enabled {
                    return Err(format!("agent {id} is disabled"));
                }
                agent.cwd
            }
            None => cwd.ok_or_else(|| "session_create requires cwd or agent".to_string())?,
        };
        if !cwd.is_dir() {
            return Err(format!("not a directory: {}", cwd.display()));
        }
        let agent = agent.map(str::to_string);
        let gateway = options.gateway == Some(true);
        let (id, ops, generation) = match engine {
            None => session::spawn(Arc::clone(self), cwd.clone(), None, agent.clone())?,
            Some(kind) => {
                // Claude Code takes its conversation id from us; others name it.
                let id = match kind {
                    engines::Kind::Claude | engines::Kind::Acp => Some(engines::new_uuid()?),
                    engines::Kind::Codex => None,
                };
                engines::spawn(Arc::clone(self), kind, id, cwd.clone(), options, vec![])?
            }
        };
        self.store
            .record_engine_session(
                &id,
                &cwd,
                agent.as_deref(),
                engine.map(engines::Kind::name),
                gateway,
            )
            .map_err(|error| error.to_string())?;
        lock(&self.untitled).insert(id.clone());
        self.host(id.clone(), ops, cwd, generation);
        self.broadcast(&self.agents_json());
        Ok(id)
    }

    /// Hosts a session recorded earlier (after a restart or a close). A
    /// session that is already hosted is left as it is.
    /// `cwd` also opens a session the daemon never hosted (one saved by
    /// the TUI or `jucode serve` in that directory).
    pub fn open_session(self: &Arc<Self>, id: &str, cwd: Option<PathBuf>) -> Result<(), String> {
        self.open_engine_session(id, cwd, None, engines::Options::default())
    }

    /// `open_session` with start options; `engine` names the engine of a
    /// session the daemon never hosted (a Claude Code conversation saved in
    /// `cwd`).
    pub fn open_engine_session(
        self: &Arc<Self>,
        id: &str,
        cwd: Option<PathBuf>,
        engine: Option<engines::Kind>,
        options: engines::Options,
    ) -> Result<(), String> {
        if lock(&self.sessions).contains_key(id) {
            return Ok(());
        }
        let known = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == id);
        let record = match (known, cwd) {
            (Some(record), _) => record,
            (None, Some(cwd)) => {
                let saved = match engine {
                    None => jucode_agent_core::saved_sessions(&cwd)
                        .map_err(|error| error.to_string())?
                        .iter()
                        .any(|summary| summary.id == id),
                    Some(engines::Kind::Claude) => engines::claude::saved(&cwd)
                        .iter()
                        .any(|(saved, _, _)| saved == id),
                    Some(engines::Kind::Codex) => engines::codex::saved(&cwd)
                        .iter()
                        .any(|(saved, _, _)| saved == id),
                    // ACP agents keep no conversations to reopen.
                    Some(engines::Kind::Acp) => false,
                };
                if !saved {
                    return Err(format!("no session {id} in {}", cwd.display()));
                }
                SessionRecord {
                    id: id.to_string(),
                    cwd,
                    agent: None,
                    created_at: now(),
                    closed: true,
                    title: None,
                    title_auto: false,
                    archived: false,
                    hidden: false,
                    engine: engine.map(|kind| kind.name().to_string()),
                    gateway: false,
                    group: None,
                }
            }
            (None, None) => return Err(format!("unknown session {id}")),
        };
        let kind = engines::Kind::parse(record.engine.as_deref().unwrap_or_default())?;
        let mut gateway = false;
        let (ops, generation) = match kind {
            None => {
                let (_, ops, generation) = session::spawn(
                    Arc::clone(self),
                    record.cwd.clone(),
                    Some(id.to_string()),
                    record.agent.clone(),
                )?;
                (ops, generation)
            }
            Some(kind) => {
                // A conversation with no turn yet was never saved: start it
                // again under the same id instead of resuming it.
                // Codex resumes through its protocol, which also sends the
                // history back.
                let saved = match kind {
                    engines::Kind::Claude => engines::claude::is_saved(&record.cwd, id),
                    engines::Kind::Codex => true,
                    engines::Kind::Acp => false,
                };
                let transcript = match kind {
                    engines::Kind::Claude => engines::claude::transcript(&record.cwd, id),
                    engines::Kind::Codex | engines::Kind::Acp => Vec::new(),
                };
                // A client that does not say (a phone) keeps how it last ran.
                let options = engines::Options {
                    resume: saved.then(|| id.to_string()),
                    gateway: Some(options.gateway.unwrap_or(record.gateway)),
                    ..options
                };
                gateway = options.gateway == Some(true);
                // A new start keeps the recorded id; a resume opens it.
                let fresh = options.resume.is_none().then(|| id.to_string());
                let (_, ops, generation) = engines::spawn(
                    Arc::clone(self),
                    kind,
                    fresh,
                    record.cwd.clone(),
                    options,
                    transcript,
                )?;
                (ops, generation)
            }
        };
        if record.closed {
            self.store
                .record_engine_session(
                    id,
                    &record.cwd,
                    record.agent.as_deref(),
                    record.engine.as_deref(),
                    gateway,
                )
                .map_err(|error| error.to_string())?;
        }
        self.host(id.to_string(), ops, record.cwd, generation);
        Ok(())
    }

    pub(crate) fn host(&self, id: String, ops: Sender<Value>, cwd: PathBuf, generation: u64) {
        lock(&self.sessions).insert(
            id,
            Hosted {
                ops,
                cwd,
                watchers: HashSet::new(),
                generation,
            },
        );
    }

    /// A number unique to one engine thread, tying it to its `Hosted` entry.
    pub fn next_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::SeqCst)
    }

    pub fn close_session(&self, id: &str) -> Result<(), String> {
        let hosted = lock(&self.sessions)
            .remove(id)
            .ok_or_else(|| format!("session {id} is not open"))?;
        let _ = hosted.ops.send(json!({ "op": "shutdown" }));
        self.store
            .record_session_closed(id)
            .map_err(|error| error.to_string())
    }

    /// Called by a session thread when its engine stops on its own (`/quit`
    /// or a failed open), so the session no longer counts as hosted.
    ///
    /// `generation` is the ending thread's: when the session was closed and
    /// reopened while that thread was still finishing, the entry now belongs
    /// to the new engine, which must stay hosted.
    pub fn session_ended(&self, id: &str, generation: u64) {
        {
            let mut sessions = lock(&self.sessions);
            match sessions.get(id) {
                Some(hosted) if hosted.generation == generation => {
                    sessions.remove(id);
                    let _ = self.store.record_session_closed(id);
                }
                Some(_) => return,
                None => {}
            }
        }
        lock(&self.busy).remove(id);
        lock(&self.claims).remove(id);
        self.usage.close(id);
        self.broadcast(&json!({ "type": "session_closed", "session": id }));
    }

    /// Sends `op` to every open JuCode session (other engines have no MCP
    /// servers of the daemon's).
    pub fn forward_to_jucode_sessions(&self, op: &Value) {
        let jucode: HashSet<String> = self
            .store
            .sessions()
            .into_iter()
            .filter(|record| record.engine.is_none())
            .map(|record| record.id)
            .collect();
        for (id, hosted) in lock(&self.sessions).iter() {
            if jucode.contains(id) {
                let _ = hosted.ops.send(op.clone());
            }
        }
    }

    pub fn forward(&self, session: &str, mut op: Value) -> Result<(), String> {
        if op["op"] == "set_approval_mode" && crate::requirements::gated(self, session) {
            op["mode"] = json!(crate::requirements::read_only(self, session));
        }
        let sessions = lock(&self.sessions);
        let hosted = sessions
            .get(session)
            .ok_or_else(|| format!("session {session} is not open"))?;
        hosted
            .ops
            .send(op)
            .map_err(|_| format!("session {session} has stopped"))
    }

    pub fn set_watch(&self, client: u64, session: &str, watch: bool) {
        let mut sessions = lock(&self.sessions);
        let Some(hosted) = sessions.get_mut(session) else {
            return;
        };
        let before = !hosted.watchers.is_empty();
        if watch {
            hosted.watchers.insert(client);
        } else {
            hosted.watchers.remove(&client);
        }
        let after = !hosted.watchers.is_empty();
        if watch {
            let _ = hosted
                .ops
                .send(json!({ "op": "snapshot", "client": client }));
        }
        if before != after {
            let _ = hosted
                .ops
                .send(json!({ "op": "set_attended", "attended": after }));
        }
    }

    /// Called by a session thread with its engine's state every tick. A
    /// The desktop brought a newer daemon (an app update): this one exits
    /// once no session is running, and the desktop starts the new one, which
    /// reopens the sessions. Running turns are never cut off.
    pub fn restart_when_idle(&self) {
        if self.restart_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(hub) = self.me.upgrade() else {
            return;
        };
        thread::spawn(move || loop {
            if lock(&hub.busy).is_empty() {
                hub.broadcast(&json!({ "type": "daemon_restarting" }));
                thread::sleep(std::time::Duration::from_millis(300));
                std::process::exit(0);
            }
            thread::sleep(std::time::Duration::from_secs(1));
        });
    }

    /// session with an unprocessed delivery stays busy.
    pub fn set_busy(&self, session: &str, busy: bool) {
        let busy = busy
            || lock(&self.claims)
                .get(session)
                .is_some_and(|count| *count > 0);
        let changed = if busy {
            lock(&self.busy).insert(session.to_string())
        } else {
            lock(&self.busy).remove(session)
        };
        if changed {
            self.broadcast(&self.agents_json());
        }
    }

    /// A session is running a turn or has messages waiting.
    pub fn is_busy(&self, session: &str) -> bool {
        lock(&self.busy).contains(session)
    }

    /// The end of a session's latest reply, as far as this daemon saw it.
    pub fn last_reply(&self, session: &str) -> Option<String> {
        let turns = lock(&self.turns);
        let tail = turns.get(session)?.tail().trim();
        (!tail.is_empty()).then(|| tail.to_string())
    }

    /// Sends `content` to a session as the user's next message, holding its
    /// running slot like a delivered message (see `deliver`).
    pub fn send_to_session(&self, session: &str, content: &str) -> Result<(), String> {
        *lock(&self.claims).entry(session.to_string()).or_default() += 1;
        lock(&self.busy).insert(session.to_string());
        self.forward(
            session,
            json!({ "op": "user_message", "content": content, "claimed": true }),
        )
        .inspect_err(|_| self.release_claim(session))
    }

    /// Notifies the paired phones (see `push`).
    /// This hub, for a thread that outlives the caller's borrow.
    pub fn handle(&self) -> Option<Arc<Hub>> {
        self.me.upgrade()
    }

    pub fn notify(&self, title: &str, body: &str, tag: &str) {
        if let Some(hub) = self.me.upgrade() {
            crate::push::notify(&hub, title, body, tag);
        }
    }

    /// `notify` whose notification opens `url` on the remote page.
    pub fn notify_at(&self, title: &str, body: &str, tag: &str, url: &str) {
        if let Some(hub) = self.me.upgrade() {
            crate::push::notify_at(&hub, title, body, tag, url);
        }
    }

    /// A session's conversation as the title model sees it for a note: its
    /// first and latest requests and the end of its latest reply.
    pub fn turn_excerpt(&self, session: &str) -> Option<String> {
        let title = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
            .and_then(|record| record.title)
            .unwrap_or_default();
        lock(&self.turns)
            .get(session)
            .map(|turns| turns.handoff_prompt(&title, None))
    }

    /// The session thread has handed a delivered message to its engine.
    pub fn release_claim(&self, session: &str) {
        let mut claims = lock(&self.claims);
        if let Some(count) = claims.get_mut(session) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                claims.remove(session);
            }
        }
    }

    pub fn agents_json(&self) -> Value {
        let records = self.store.sessions();
        let busy = lock(&self.busy).clone();
        let list: Vec<Value> = self
            .agents
            .list()
            .into_iter()
            .map(|agent| {
                let sessions: Vec<&str> = records
                    .iter()
                    .filter(|record| record.agent.as_deref() == Some(agent.id.as_str()))
                    .map(|record| record.id.as_str())
                    .collect();
                let mut value = agent.to_json();
                value["summary"] = json!(self.agents.summary(&agent.id));
                value["sessions"] = json!(sessions.len());
                let running: Vec<&str> = sessions
                    .iter()
                    .copied()
                    .filter(|id| busy.contains(*id))
                    .collect();
                value["busy"] = json!(!running.is_empty());
                value["running"] = json!(running);
                value
            })
            .collect();
        json!({ "type": "agents", "agents": list })
    }

    /// Deletes an agent none of whose sessions is running: its folder
    /// (brief, memory, schedules), its timers and its open questions. Its
    /// open sessions close; their records stay. Messages still waiting for
    /// it become undeliverable on the next delivery pass.
    pub fn delete_agent(&self, id: &str) -> Result<(), String> {
        // No message reaches the agent (or starts a session of it) meanwhile.
        let _guard = lock(&self.delivering);
        if self.agents.get(id).is_none() {
            return Err(format!("unknown agent {id}"));
        }
        let own: Vec<String> = self
            .store
            .sessions()
            .into_iter()
            .filter(|record| record.agent.as_deref() == Some(id))
            .map(|record| record.id)
            .collect();
        if own.iter().any(|session| lock(&self.busy).contains(session)) {
            return Err(format!(
                "agent {id} is working; delete it once its sessions are idle"
            ));
        }
        // Holding the schedules while the folder goes keeps a run that fires
        // meanwhile from writing schedules.json back into it.
        let mut schedules = lock(&self.schedules);
        self.agents.delete(id)?;
        schedules.retain(|schedule| schedule.agent != id);
        drop(schedules);
        for session in &own {
            let _ = self.close_session(session);
        }
        for timer in self.store.active_timers() {
            if timer.agent == id {
                let _ = self.cancel_timer(&timer.id, None);
            }
        }
        for question in self.store.open_questions() {
            if question.agent == id {
                let _ = self.store.record_answer(&question.id, "", "agent_deleted");
            }
        }
        self.broadcast(&self.agents_json());
        self.broadcast(&self.schedules_json(None));
        self.broadcast(&self.questions_json());
        Ok(())
    }

    /// A fresh id with a readable prefix (`m-…`, `t-…`).
    pub fn new_id(&self, prefix: &str) -> String {
        format!(
            "{prefix}-{}-{}",
            now(),
            self.next_id.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// Records a message and tries to deliver it right away. Returns false
    /// when a message with the same dedupe key was already recorded.
    pub fn send_message(self: &Arc<Self>, message: Message) -> Result<bool, String> {
        if self.agents.get(&message.to).is_none() {
            return Err(format!("unknown agent {}", message.to));
        }
        let fresh = self
            .store
            .record_message(&message)
            .map_err(|error| error.to_string())?;
        if fresh {
            self.deliver_pending();
        }
        Ok(fresh)
    }

    pub fn set_timer(&self, timer: &Timer) -> Result<(), String> {
        self.store
            .record_timer(timer)
            .map_err(|error| error.to_string())
    }

    pub fn cancel_timer(&self, id: &str, agent: Option<&str>) -> Result<(), String> {
        let _guard = lock(&self.timer_updates);
        if !self
            .store
            .active_timers()
            .iter()
            .any(|timer| timer.id == id && agent.is_none_or(|agent| timer.agent == agent))
        {
            return Err(format!("no active timer {id}"));
        }
        self.store
            .record_timer_done(id, "cancelled")
            .map_err(|error| error.to_string())
    }

    /// One scheduler pass: due timers, due schedules and overdue questions
    /// become messages, then everything pending is delivered.
    pub fn tick(self: &Arc<Self>) {
        self.fire_due_timers();
        self.fire_due_schedules();
        self.expire_questions();
        self.deliver_pending();
    }

    pub fn ask(&self, question: &Question) -> Result<(), String> {
        self.store
            .record_question(question)
            .map_err(|error| error.to_string())?;
        self.broadcast(&self.questions_json());
        Ok(())
    }

    /// Answers a question: the answer goes back to the session that asked,
    /// waking it. `by` is `user` or `deadline`. Errors when the question is
    /// unknown or already answered.
    pub fn answer_question(
        self: &Arc<Self>,
        id: &str,
        answer: &str,
        by: &str,
    ) -> Result<(), String> {
        let question = self
            .store
            .question(id)
            .ok_or_else(|| format!("unknown question {id}"))?;
        if !self
            .store
            .record_answer(id, answer, by)
            .map_err(|error| error.to_string())?
        {
            return Err(format!("question {id} is already answered"));
        }
        self.broadcast(&self.questions_json());
        let body = if by == "deadline" {
            format!(
                "Q: {}\nNo answer by the deadline. Go ahead with your default: {}",
                question.title, question.default_action
            )
        } else {
            format!("Q: {}\nA: {answer}", question.title)
        };
        // The dispatcher has no `open_items` tool.
        let others = match question.agent.as_str() {
            crate::dispatch::AGENT => 0,
            agent => self.open_items_of(agent).len(),
        };
        let body = if others > 0 {
            format!(
                "{body}\n\nYou have {others} other open item(s) waiting for the user. Check them with `open_items` and close any this answer made unnecessary."
            )
        } else {
            body
        };
        self.store
            .record_message(&Message {
                id: self.new_id("m"),
                to: question.agent,
                from: format!("question:{id}"),
                body,
                session: Some(question.session),
                reply_to: None,
                dedupe_key: Some(format!("question:{id}")),
                at: now(),
            })
            .map_err(|error| error.to_string())?;
        self.deliver_pending();
        Ok(())
    }

    fn expire_questions(self: &Arc<Self>) {
        for question in self.store.open_questions() {
            if question.due_at.is_some_and(|due| due <= now()) {
                let _ = self.answer_question(&question.id, "", "deadline");
            }
        }
    }

    pub fn post_report(&self, report: &Report) -> Result<(), String> {
        self.store
            .record_report(report)
            .map_err(|error| error.to_string())?;
        self.broadcast(&json!({ "type": "report_posted", "report": report_json(report) }));
        Ok(())
    }

    /// Open questions, and the ones closed lately (each with who closed it,
    /// why and when) so a client can offer to reopen them.
    pub fn questions_json(&self) -> Value {
        let list: Vec<Value> = self
            .store
            .open_questions()
            .iter()
            .map(question_json)
            .collect();
        let closed: Vec<Value> = self
            .store
            .closed_questions(now().saturating_sub(CLOSED_KEEP_MS))
            .iter()
            .map(|(question, closure)| with_closure(question_json(question), closure))
            .collect();
        json!({ "type": "questions", "questions": list, "closed": closed })
    }

    pub fn reports_json(&self, limit: usize) -> Value {
        let list: Vec<Value> = self.store.reports(limit).iter().map(report_json).collect();
        json!({ "type": "reports", "reports": list })
    }

    /// Open actions, and the ones closed lately (see `questions_json`).
    pub fn actions_json(&self) -> Value {
        let list: Vec<Value> = self
            .store
            .open_actions()
            .iter()
            .map(|action| action.to_json())
            .collect();
        let closed: Vec<Value> = self
            .store
            .closed_actions(now().saturating_sub(CLOSED_KEEP_MS))
            .iter()
            .map(|(action, closure)| with_closure(action.to_json(), closure))
            .collect();
        json!({ "type": "actions", "actions": list, "closed": closed })
    }

    /// `agent`'s open questions and the open actions of its sessions, as
    /// its `open_items` tool lists them.
    pub fn open_items_of(&self, agent: &str) -> Vec<Value> {
        let own: HashSet<String> = self
            .store
            .sessions()
            .into_iter()
            .filter(|record| record.agent.as_deref() == Some(agent))
            .map(|record| record.id)
            .collect();
        let questions = self
            .store
            .open_questions()
            .into_iter()
            .filter(|question| question.agent == agent)
            .map(|question| {
                json!({
                    "kind": "question", "id": question.id, "session": question.session,
                    "title": question.title, "assumption": question.assumption,
                    "asked_at_unix": question.asked_at / 1000,
                })
            });
        let actions = self
            .store
            .open_actions()
            .into_iter()
            .filter(|action| own.contains(&action.session_id))
            .map(|action| {
                json!({
                    "kind": "action", "id": action.id, "session": action.session_id,
                    "tool": action.name, "summary": action.summary,
                    "asked_at_unix": action.created_at / 1000,
                })
            });
        questions.chain(actions).collect()
    }

    /// Closes an open question or action without answering it. `by` is
    /// `user`, `agent:<id>` or `superseded`; an agent closing one tells the
    /// user's phone, since nobody asked for it.
    pub fn close_item(
        &self,
        kind: ItemKind,
        id: &str,
        by: &str,
        reason: &str,
    ) -> Result<(), String> {
        let title = match kind {
            ItemKind::Question => self.store.question(id).map(|q| q.title),
            ItemKind::Action => self
                .store
                .open_actions()
                .into_iter()
                .find(|action| action.id == id)
                .map(|action| format!("{} {}", action.name, action.summary)),
        };
        if !self
            .store
            .close_item(kind, id, by, reason.trim())
            .map_err(|error| error.to_string())?
        {
            return Err(format!("{id} is not open"));
        }
        self.broadcast_items(kind);
        if by.starts_with("agent:") {
            let title = title.unwrap_or_else(|| id.to_string());
            self.notify(&format!("已关闭：{title}"), reason, &format!("closed:{id}"));
        }
        Ok(())
    }

    /// Reopens a closed item; its session comes back out of the archive.
    pub fn reopen_item(&self, kind: ItemKind, id: &str) -> Result<(), String> {
        let session = match kind {
            ItemKind::Question => self.store.question(id).map(|q| q.session),
            ItemKind::Action => self
                .store
                .closed_actions(0)
                .into_iter()
                .find(|(action, _)| action.id == id)
                .map(|(action, _)| action.session_id),
        };
        if !self
            .store
            .reopen_item(kind, id)
            .map_err(|error| error.to_string())?
        {
            return Err(format!("{id} is not closed"));
        }
        self.broadcast_items(kind);
        let archived = self
            .store
            .sessions()
            .into_iter()
            .any(|record| Some(&record.id) == session.as_ref() && record.archived);
        if let (true, Some(session)) = (archived, session) {
            if let Ok(true) = self
                .store
                .record_session_meta(&session, &json!({ "archived": false }))
            {
                self.broadcast(&self.sessions_json());
            }
        }
        Ok(())
    }

    fn broadcast_items(&self, kind: ItemKind) {
        self.broadcast(&match kind {
            ItemKind::Question => self.questions_json(),
            ItemKind::Action => self.actions_json(),
        });
    }

    /// A scheduled task started a new run in `current`: its earlier runs
    /// are done with. Their open questions and actions close (the new run
    /// asks again if it still needs to) and the runs are archived, so the
    /// agent's page shows the latest run. A run still working is left.
    pub(crate) fn retire_schedule_runs(&self, schedule: &str, name: &str, current: &str) {
        let earlier: Vec<String> = self
            .store
            .sessions_reached_from(&format!("schedule:{schedule}"))
            .into_iter()
            .filter(|session| session != current && !self.is_busy(session))
            .collect();
        if earlier.is_empty() {
            return;
        }
        let reason = format!("定时任务「{name}」已开始新一次运行");
        for question in self.store.open_questions() {
            if earlier.contains(&question.session) {
                let _ = self.close_item(ItemKind::Question, &question.id, "superseded", &reason);
            }
        }
        for action in self.store.open_actions() {
            if earlier.contains(&action.session_id) {
                let _ = self.close_item(ItemKind::Action, &action.id, "superseded", &reason);
            }
        }
        let mut archived = false;
        for record in self.store.sessions() {
            if earlier.contains(&record.id) && !record.archived {
                archived |= self
                    .store
                    .record_session_meta(&record.id, &json!({ "archived": true }))
                    .unwrap_or(false);
            }
        }
        if archived {
            self.broadcast(&self.sessions_json());
        }
    }

    /// Turns due timers into messages. The timer id is the message's dedupe
    /// key, so a timer that fired just before a crash fires only once.
    fn fire_due_timers(self: &Arc<Self>) {
        let _guard = lock(&self.timer_updates);
        let due: Vec<Timer> = self
            .store
            .active_timers()
            .into_iter()
            .filter(|timer| timer.fire_at <= now())
            .collect();
        for timer in due {
            let message = Message {
                id: self.new_id("m"),
                to: timer.agent.clone(),
                from: format!("timer:{}", timer.id),
                body: timer.body.clone(),
                session: timer.session.clone(),
                reply_to: None,
                dedupe_key: Some(format!("timer:{}", timer.id)),
                at: now(),
            };
            match self.store.record_message(&message) {
                Ok(_) => {
                    let _ = self.store.record_timer_done(&timer.id, "fired");
                }
                Err(error) => {
                    jucode_agent_core::log_warn!(
                        "daemon",
                        "timer not fired",
                        error = error.to_string()
                    );
                }
            }
        }
    }

    /// Delivers every pending message that can go now, oldest first. A
    /// message that would start a new run waits while `MAX_RUNNING` runs are
    /// in progress; one for a busy session joins that session's queue.
    pub fn deliver_pending(self: &Arc<Self>) {
        let _guard = lock(&self.delivering);
        for message in self.store.pending_messages() {
            match self.route(&message) {
                Ok(target) => {
                    let busy = target
                        .as_deref()
                        .is_some_and(|session| lock(&self.busy).contains(session));
                    if !busy && lock(&self.busy).len() >= MAX_RUNNING {
                        continue;
                    }
                    if let Err(error) = self.deliver(&message, target) {
                        jucode_agent_core::log_warn!("daemon", "delivery failed", error = error);
                    }
                }
                Err(reason) => {
                    let _ = self.store.record_undeliverable(&message.id, &reason);
                }
            }
        }
    }

    /// The session a message goes to (None: a new session). Errors mean it
    /// can never be delivered.
    fn route(&self, message: &Message) -> Result<Option<String>, String> {
        let agent = self
            .agents
            .get(&message.to)
            .ok_or_else(|| format!("unknown agent {}", message.to))?;
        if !agent.enabled {
            return Err(format!("agent {} is disabled", agent.id));
        }
        let own: Vec<_> = self
            .store
            .sessions()
            .into_iter()
            .filter(|record| record.agent.as_deref() == Some(agent.id.as_str()))
            .collect();
        let owned = |session: &str| own.iter().any(|record| record.id == session);
        if let Some(session) = &message.session {
            return if owned(session) {
                Ok(Some(session.clone()))
            } else {
                Err(format!("session {session} does not belong to {}", agent.id))
            };
        }
        if let Some(earlier) = &message.reply_to {
            if let Some(session) = self.store.delivered_session(earlier).filter(|s| owned(s)) {
                return Ok(Some(session));
            }
        }
        // Anything else is a new task, in a new session; what earlier
        // sessions concluded reaches it through their handoff notes.
        Ok(None)
    }

    fn deliver(self: &Arc<Self>, message: &Message, target: Option<String>) -> Result<(), String> {
        let session = match target {
            Some(session) => {
                self.open_session(&session, None)?;
                session
            }
            None => self.create_session(None, Some(&message.to), false)?,
        };
        // A delivered message starts (or queues) a run: claim the slot before
        // forwarding, so the next message sees it taken.
        *lock(&self.claims).entry(session.clone()).or_default() += 1;
        lock(&self.busy).insert(session.clone());
        if let Err(error) = self.forward(
            &session,
            json!({ "op": "user_message", "content": delivery_text(message), "claimed": true }),
        ) {
            self.release_claim(&session);
            return Err(error);
        }
        self.store
            .record_delivered(&message.id, &session)
            .map_err(|error| error.to_string())?;
        if let Some(schedule) = message.from.strip_prefix("schedule:") {
            self.schedule_delivered(schedule, &session);
        }
        self.broadcast(&json!({
            "type": "message_delivered",
            "id": message.id,
            "agent": message.to,
            "from": message.from,
            "session": session,
        }));
        Ok(())
    }

    /// Every recorded session with whether it is hosted right now.
    /// Every event a session publishes: its title follows the conversation
    /// (see `titles`).
    pub fn observe(&self, session: &str, event: &Value) {
        self.usage.observe(session, event, || {
            let record = self
                .store
                .sessions()
                .into_iter()
                .find(|record| record.id == session)?;
            Some(SessionInfo {
                engine: record.engine.unwrap_or_else(|| "jucode".to_string()),
                gateway: record.gateway,
                cwd: record.cwd.to_string_lossy().into_owned(),
            })
        });
        if event["type"] == "user_message" {
            self.note_user_message(session, event["content"].as_str().unwrap_or_default());
        }
        // A name the session has in Claude Code (given there, or the one we
        // gave it) is the user's.
        if event["type"] == "session_title" {
            self.adopt_engine_title(session, event["title"].as_str().unwrap_or_default());
        }
        let (due, ended, failed) = {
            let mut turns = lock(&self.turns);
            let turns = turns.entry(session.to_string()).or_default();
            let before = turns.done();
            let due = turns.observe(event);
            (due, turns.done() != before, turns.take_failed())
        };
        if due {
            self.retitle(session);
        }
        if let Some(error) = failed {
            self.report_failed_turn(session, &error);
        }
        if ended {
            self.write_handoff(session);
        }
        if let Some(hub) = self.me.upgrade() {
            crate::dispatch::observe(&hub, session, event);
            crate::requirements::observe(&hub, session, event, ended);
        }
    }

    /// An agent session's turn ended on an error. Nobody may be watching
    /// (a scheduled run), so it becomes a report and a notification instead
    /// of only a line in the live view.
    fn report_failed_turn(&self, session: &str, error: &str) {
        let Some(record) = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
        else {
            return;
        };
        let Some(agent) = record.agent else {
            return;
        };
        let title = format!("运行中断：{}", record.title.as_deref().unwrap_or(session));
        let report = Report {
            id: self.new_id("r"),
            agent,
            session: session.to_string(),
            body: format!("这一轮因错误中断，没有完成：\n{error}"),
            title,
            at: now(),
            read: false,
        };
        match self.post_report(&report) {
            Ok(()) => self.notify(&report.title, error, session),
            Err(error) => {
                jucode_agent_core::log_warn!("daemon", "failure report not saved", error = error)
            }
        }
    }

    /// An agent session's turn ended: the title model rewrites its handoff
    /// note in the background (see `titles`). One at a time per session; a
    /// turn that ends meanwhile is covered by the next.
    fn write_handoff(&self, session: &str) {
        let Some(record) = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
        else {
            return;
        };
        let Some(agent) = record.agent.clone() else {
            return;
        };
        let Some(hub) = self.me.upgrade() else {
            return;
        };
        if !lock(&self.handing_off).insert(session.to_string()) {
            return;
        }
        let title = record.title.clone().unwrap_or_default();
        let previous = self.agents.handoff(&agent, session);
        let Some(prompt) = lock(&self.turns)
            .get(session)
            .map(|turns| turns.handoff_prompt(&title, previous.as_deref()))
        else {
            lock(&self.handing_off).remove(session);
            return;
        };
        let id = session.to_string();
        thread::spawn(move || {
            let reply = jucode_agent_core::title_completion(titles::HANDOFF_SYSTEM, &prompt);
            lock(&hub.handing_off).remove(&id);
            let note = match reply {
                Ok(reply) => titles::clean_handoff(&reply),
                Err(error) => {
                    jucode_agent_core::log_warn!("daemon", "handoff note failed", error = error);
                    None
                }
            };
            let Some(note) = note else { return };
            // The title as it is now: the model may have renamed it meanwhile.
            let title = hub
                .store
                .sessions()
                .into_iter()
                .find(|record| record.id == id)
                .and_then(|record| record.title)
                .unwrap_or(title);
            if let Err(error) = hub.agents.save_handoff(&agent, &id, &title, &note, now()) {
                jucode_agent_core::log_warn!("daemon", "handoff note not saved", error = error);
            }
        });
    }

    /// Asks the title model for a title in the background, unless a client
    /// named the session by hand.
    fn retitle(&self, session: &str) {
        let Some(record) = self.auto_titled(session) else {
            return;
        };
        let Some(hub) = self.me.upgrade() else {
            return;
        };
        if !lock(&self.titling).insert(session.to_string()) {
            return;
        }
        let project = record
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(prompt) = lock(&self.turns)
            .get(session)
            .map(|turns| turns.prompt(&project, record.title.as_deref()))
        else {
            lock(&self.titling).remove(session);
            return;
        };
        let id = session.to_string();
        thread::spawn(move || {
            let reply = jucode_agent_core::title_completion(titles::SYSTEM, &prompt);
            lock(&hub.titling).remove(&id);
            let title = match reply {
                Ok(reply) => titles::clean(&reply),
                Err(error) => {
                    jucode_agent_core::log_warn!(
                        "daemon",
                        "conversation title failed",
                        error = error
                    );
                    None
                }
            };
            // Renamed by hand meanwhile, or unchanged: nothing to do.
            let Some(title) = title else { return };
            let Some(record) = hub.auto_titled(&id) else {
                return;
            };
            if record.title.as_deref() != Some(title.as_str())
                && hub
                    .store
                    .record_session_meta(&id, &json!({ "title": title, "title_auto": true }))
                    .is_ok()
            {
                hub.broadcast(&hub.sessions_json());
            }
        });
    }

    fn adopt_engine_title(&self, session: &str, title: &str) {
        let title = title.trim();
        let current = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
            .and_then(|record| record.title);
        if title.is_empty() || current.as_deref() == Some(title) {
            return;
        }
        if let Ok(true) = self
            .store
            .record_session_meta(session, &json!({ "title": title }))
        {
            self.broadcast(&self.sessions_json());
        }
    }

    /// The session's record when its title is ours to write (none yet, or
    /// one the daemon wrote).
    fn auto_titled(&self, session: &str) -> Option<SessionRecord> {
        self.store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
            .filter(|record| record.title.is_none() || record.title_auto)
    }

    /// A `user_message` op with images: its echo will show them.
    pub fn note_sent_images(&self, session: &str, op: &Value) {
        if op["op"] == "user_message" && op["images"].as_array().is_some_and(|i| !i.is_empty()) {
            let text = op["content"].as_str().unwrap_or_default().to_string();
            lock(&self.sent_images)
                .entry(session.to_string())
                .or_default()
                .push((text, op["images"].clone()));
        }
    }

    /// The images of the message a `user_message` event echoes, so every
    /// client (the remote page too) shows them.
    pub fn attach_sent_images(&self, session: &str, event: &mut Value) {
        if event["type"] != "user_message" || !event["images"].is_null() {
            return;
        }
        let text = event["content"].as_str().unwrap_or_default();
        let mut sent = lock(&self.sent_images);
        let Some(list) = sent.get_mut(session) else {
            return;
        };
        if let Some(at) = list.iter().position(|(t, _)| t == text) {
            event["images"] = list.remove(at).1;
        }
    }

    /// A session accepted a user message: a new session is titled after
    /// its first one (first line, 40 characters); clients show the daemon's titles.
    pub fn note_user_message(&self, session: &str, content: &str) {
        if !lock(&self.untitled).remove(session) {
            return;
        }
        let Some(title) = title_from(content) else {
            return;
        };
        let untouched = self
            .store
            .sessions()
            .iter()
            .any(|record| record.id == session && record.title.is_none());
        if untouched
            && self
                .store
                .record_session_meta(session, &json!({ "title": title, "title_auto": true }))
                .is_ok()
        {
            self.broadcast(&self.sessions_json());
        }
    }

    pub fn sessions_json(&self) -> Value {
        let records = self.store.sessions();
        let saved = saved_by_id(records.iter().map(|record| record.cwd.as_path()));
        let sessions = lock(&self.sessions);
        let list: Vec<Value> = records
            .into_iter()
            // An agent's session stays on its agent's page even when a
            // client hid it from a session list (desktops listed them there
            // before they moved to the workbench).
            .filter(|record| !record.hidden || record.agent.is_some())
            .map(|record| {
                let hosted = sessions.get(&record.id);
                let cwd = hosted.map_or_else(|| record.cwd.clone(), |h| h.cwd.clone());
                let summary = saved.get(&record.id);
                json!({
                    "session": record.id,
                    "chat": jucode_agent_core::chat::is_chat_dir(&cwd),
                    "cwd": cwd.display().to_string(),
                    "created_at": record.created_at,
                    "updated_at": summary.map_or(record.created_at, |s| s.updated_at * 1000),
                    "title": shown_title(&record, summary.map(|s| s.label.as_str())),
                    "archived": record.archived,
                    "group": record.group,
                    "gateway": record.gateway,
                    "engine": record.engine.as_deref().unwrap_or("jucode"),
                    "agent": record.agent,
                    "open": hosted.is_some(),
                    "watchers": hosted.map(|h| h.watchers.len()).unwrap_or(0),
                })
            })
            .collect();
        json!({ "type": "sessions", "sessions": list })
    }

    /// Every session saved in `cwd`, newest first, whoever ran it (the
    /// daemon, the TUI or `jucode serve`), with the daemon's title and
    /// archive state for the ones it knows.
    pub fn session_history(&self, cwd: &std::path::Path) -> Result<Value, String> {
        let saved = jucode_agent_core::saved_sessions(cwd).map_err(|error| error.to_string())?;
        let records: HashMap<String, SessionRecord> = self
            .store
            .sessions()
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect();
        let sessions = lock(&self.sessions);
        let item = |id: String, label: String, updated_at: u64, entries: Value, engine: &str| {
            let record = records.get(&id);
            json!({
                "title": match record {
                    Some(record) => shown_title(record, Some(&label)),
                    None => title_from(&label),
                },
                "updated_at": updated_at,
                "entries": entries,
                "archived": record.is_some_and(|r| r.archived),
                "agent": record.and_then(|r| r.agent.clone()),
                "open": sessions.contains_key(&id),
                "engine": engine,
                "session": id,
            })
        };
        let mut list: Vec<Value> = saved
            .into_iter()
            .map(|s| {
                item(
                    s.id,
                    s.label,
                    s.updated_at * 1000,
                    json!(s.entries),
                    "jucode",
                )
            })
            .chain(
                engines::claude::saved(cwd)
                    .into_iter()
                    .map(|(id, title, updated_at)| {
                        item(id, title, updated_at, Value::Null, "claude")
                    }),
            )
            .chain(
                engines::codex::saved(cwd)
                    .into_iter()
                    .map(|(id, title, updated_at)| {
                        item(id, title, updated_at, Value::Null, "codex")
                    }),
            )
            .collect();
        list.retain(|item| {
            !records
                .get(item["session"].as_str().unwrap_or_default())
                .is_some_and(|record| record.hidden)
        });
        list.sort_by_key(|item| std::cmp::Reverse(item["updated_at"].as_u64().unwrap_or(0)));
        Ok(json!({ "type": "session_history", "cwd": cwd, "sessions": list }))
    }
}

/// Saved-session summaries of the given directories, by session id.
fn saved_by_id<'a>(
    dirs: impl Iterator<Item = &'a std::path::Path>,
) -> HashMap<String, jucode_agent_core::SessionSummary> {
    let dirs: HashSet<&std::path::Path> = dirs.collect();
    dirs.into_iter()
        .flat_map(|dir| jucode_agent_core::saved_sessions(dir).unwrap_or_default())
        .map(|summary| (summary.id.clone(), summary))
        .collect()
}

/// How a message reads in the receiving session: the user's own words
/// as-is, anything else with a line saying where it came from.
fn report_json(report: &Report) -> Value {
    json!({
        "id": report.id,
        "agent": report.agent,
        "session": report.session,
        "title": report.title,
        "body": report.body,
        "at": report.at,
        "read": report.read,
    })
}

fn question_json(question: &Question) -> Value {
    json!({
        "id": question.id,
        "agent": question.agent,
        "session": question.session,
        "title": question.title,
        "body": question.body,
        "assumption": question.assumption,
        "default": question.default_action,
        "importance": question.importance,
        "due_at": question.due_at,
        "asked_at": question.asked_at,
    })
}

/// A closed item's JSON with who closed it, why and when.
fn with_closure(mut item: Value, closure: &Closure) -> Value {
    item["closed_by"] = json!(closure.by);
    item["closed_reason"] = json!(closure.reason);
    item["closed_at"] = json!(closure.at);
    item
}

/// A session's title as clients see it: its own, else its first prompt; never
/// an id or a delivery header (which older daemons wrote as the title).
fn shown_title(record: &SessionRecord, label: Option<&str>) -> Option<String> {
    record
        .title
        .clone()
        .filter(|title| {
            !(record.title_auto
                && DELIVERY_HEADERS
                    .iter()
                    .any(|header| title.starts_with(header)))
        })
        .or_else(|| label.and_then(title_from))
}

/// How `delivery_text` starts the line naming where a message came from.
/// Clients show a message with one of these headers as a notice, not as
/// something the user wrote (JuCode-Desktop `src/lib/delivery.ts`).
const DELIVERY_HEADERS: [&str; 5] = [
    "[message from ",
    "[timer ",
    "[scheduled task ",
    "[answer to your question ",
    "[task ",
];

/// `text` without the delivery header line `delivery_text` put first: what
/// was actually asked, for titles and handoff notes.
pub(crate) fn without_delivery_header(text: &str) -> &str {
    let text = text.trim_start();
    if DELIVERY_HEADERS
        .iter()
        .any(|header| text.starts_with(header))
    {
        return text.split_once('\n').map_or("", |(_, rest)| rest);
    }
    text
}

/// A title from what was asked: its first line, at most 40 characters.
pub(crate) fn title_from(text: &str) -> Option<String> {
    let title: String = without_delivery_header(text)
        .trim()
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(40)
        .collect();
    let title = title
        .trim()
        .trim_end_matches([':', '：'])
        .trim()
        .to_string();
    (!title.is_empty()).then_some(title)
}

fn delivery_text(message: &Message) -> String {
    if message.from == "user" {
        return message.body.clone();
    }
    let origin = match message.from.split_once(':') {
        Some(("agent", id)) => format!("message from agent {id}"),
        Some(("timer", id)) => format!("timer {id} fired"),
        Some(("schedule", id)) => format!("scheduled task {id}"),
        Some(("question", id)) => format!("answer to your question {id}"),
        Some(("task", id)) => format!("task {id} update"),
        _ => format!("message from {}", message.from),
    };
    format!("[{origin} · {}]\n{}", message.id, message.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{store::now, Agents};
    use std::{fs, sync::mpsc};

    #[test]
    fn titles_come_from_what_was_asked_never_a_delivery_header() {
        let scheduled = delivery_text(&Message {
            id: "m-1".into(),
            to: "ops".into(),
            from: "schedule:sch-1".into(),
            body: "定时任务「每日巡检」：\n检查部署".into(),
            session: None,
            reply_to: None,
            dedupe_key: None,
            at: 0,
        });
        assert_eq!(
            title_from(&scheduled).as_deref(),
            Some("定时任务「每日巡检」")
        );
        assert_eq!(
            without_delivery_header(&scheduled),
            "定时任务「每日巡检」：\n检查部署"
        );
        assert_eq!(
            title_from("  修复登录跳转\n细节").as_deref(),
            Some("修复登录跳转")
        );
        assert_eq!(title_from("[timer t-1 fired · m-2]\n"), None);

        // An older daemon's header title gives way to the first prompt.
        let record = SessionRecord {
            id: "s1".into(),
            cwd: "/tmp".into(),
            agent: Some("ops".into()),
            created_at: 0,
            closed: false,
            title: Some("[scheduled task sch-1 · m-".into()),
            title_auto: true,
            archived: false,
            hidden: false,
            engine: None,
            gateway: false,
            group: None,
        };
        assert_eq!(
            shown_title(&record, Some(&scheduled)).as_deref(),
            Some("定时任务「每日巡检」")
        );
        assert_eq!(shown_title(&record, Some("")), None);
        let named = SessionRecord {
            title: Some("我的标题".into()),
            title_auto: false,
            ..record
        };
        assert_eq!(shown_title(&named, None).as_deref(), Some("我的标题"));
    }

    #[test]
    fn cancelled_timers_stay_cancelled_and_cannot_race_firing() {
        let dir =
            std::env::temp_dir().join(format!("jucode-cancel-{}-{}", std::process::id(), now()));
        let open = || {
            Hub::new(
                Store::open(dir.join("daemon")).unwrap(),
                Agents::open(dir.join("agents")).unwrap(),
                "test",
                None,
            )
        };
        let timer = |id: &str| Timer {
            id: id.into(),
            agent: "ops".into(),
            session: None,
            fire_at: 0,
            body: "remind".into(),
        };
        let hub = open();
        hub.set_timer(&timer("t1")).unwrap();
        assert!(hub.cancel_timer("t1", Some("other")).is_err());
        hub.cancel_timer("t1", Some("ops")).unwrap();
        drop(hub);
        let hub = open();
        hub.fire_due_timers();
        assert!(hub.store.message_log(None, 100).is_empty());
        assert!(hub.cancel_timer("t1", None).is_err());

        // Exactly one wins, including cancellation during a scheduler tick.
        for n in 0..20 {
            let id = format!("race-{n}");
            hub.set_timer(&timer(&id)).unwrap();
            let gate = Arc::new(std::sync::Barrier::new(2));
            let firing = hub.clone();
            let start = gate.clone();
            let worker = thread::spawn(move || {
                start.wait();
                firing.fire_due_timers();
            });
            gate.wait();
            let cancelled = hub.cancel_timer(&id, None).is_ok();
            worker.join().unwrap();
            let fired = hub
                .store
                .message_log(None, 100)
                .iter()
                .any(|m| m["from"] == format!("timer:{id}"));
            assert_ne!(cancelled, fired);
        }
        drop(hub);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_agent_turn_that_fails_becomes_a_report() {
        let dir = std::env::temp_dir().join(format!(
            "jucode-hub-failed-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        hub.store
            .record_engine_session("a1", &dir, Some("ops"), None, false)
            .unwrap();
        hub.store
            .record_engine_session("u1", &dir, None, None, false)
            .unwrap();
        for session in ["a1", "u1"] {
            hub.observe(
                session,
                &json!({ "type": "user_message", "content": "巡检" }),
            );
            hub.observe(session, &json!({ "type": "error", "message": "HTTP 502" }));
            hub.observe(session, &json!({ "type": "status", "message": "ready" }));
        }
        let reports = hub.store.reports(10);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].session, "a1");
        assert_eq!(reports[0].agent, "ops");
        assert!(reports[0].body.contains("HTTP 502"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_new_scheduled_run_retires_the_earlier_ones() {
        let dir = std::env::temp_dir().join(format!(
            "jucode-hub-retire-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        for (message, session) in [("m1", "run1"), ("m2", "run2"), ("m3", "run3")] {
            hub.store
                .record_engine_session(session, &dir, Some("ops"), None, false)
                .unwrap();
            hub.store
                .record_message(&Message {
                    id: message.into(),
                    to: "ops".into(),
                    from: "schedule:sch-1".into(),
                    body: "巡检".into(),
                    session: None,
                    reply_to: None,
                    dedupe_key: None,
                    at: 1,
                })
                .unwrap();
            hub.store.record_delivered(message, session).unwrap();
        }
        let ask = |id: &str, session: &str| Question {
            id: id.into(),
            agent: "ops".into(),
            session: session.into(),
            title: id.into(),
            body: String::new(),
            assumption: String::new(),
            default_action: String::new(),
            importance: "normal".into(),
            due_at: None,
            asked_at: 1,
        };
        hub.ask(&ask("q1", "run1")).unwrap();
        hub.ask(&ask("q3", "run3")).unwrap();
        // run2 is still working: left alone.
        lock(&hub.busy).insert("run2".into());

        hub.retire_schedule_runs("sch-1", "巡检", "run3");
        let open: Vec<String> = hub
            .store
            .open_questions()
            .into_iter()
            .map(|q| q.id)
            .collect();
        assert_eq!(open, ["q3"]);
        assert_eq!(hub.store.closed_questions(0)[0].1.by, "superseded");
        let archived = |id: &str| {
            hub.store
                .sessions()
                .iter()
                .any(|r| r.id == id && r.archived)
        };
        assert!(archived("run1") && !archived("run2") && !archived("run3"));
        assert_eq!(hub.open_items_of("ops").len(), 1);

        // Reopening brings its run back out of the archive.
        hub.reopen_item(ItemKind::Question, "q1").unwrap();
        assert!(!archived("run1"));
        assert_eq!(hub.open_items_of("ops").len(), 2);
        assert!(hub.reopen_item(ItemKind::Question, "q1").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_finishing_engine_leaves_its_successor_hosted() {
        let dir = std::env::temp_dir().join(format!(
            "jucode-hub-generation-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        let (old_ops, _old_rx) = mpsc::channel();
        let old = hub.next_generation();
        hub.host("s1".to_string(), old_ops, dir.clone(), old);
        // Closed and reopened while the old engine thread is still finishing.
        lock(&hub.sessions).remove("s1");
        let (new_ops, _new_rx) = mpsc::channel();
        let new = hub.next_generation();
        hub.host("s1".to_string(), new_ops, dir.clone(), new);

        hub.session_ended("s1", old);
        assert!(lock(&hub.sessions).contains_key("s1"));
        hub.session_ended("s1", new);
        assert!(!lock(&hub.sessions).contains_key("s1"));
        let _ = fs::remove_dir_all(dir);
    }
}
