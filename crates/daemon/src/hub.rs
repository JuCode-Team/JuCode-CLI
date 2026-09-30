//! Shared daemon state: hosted sessions, connected clients and who watches
//! what, plus message routing and timers for long-lived agents. A session is
//! attended while at least one client watches it.

use crate::{
    agents::Agents,
    engines,
    relay::Relay,
    session,
    store::{
        now, random_hex, token_hash, Device, Message, Question, Report, SessionRecord, Store, Timer,
    },
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::Sender,
        Arc, Mutex, MutexGuard,
    },
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
    pub version: &'static str,
    pub relay: Relay,
    sessions: Mutex<HashMap<String, Hosted>>,
    /// Sessions whose engine is running a turn or has queued messages.
    busy: Mutex<HashSet<String>>,
    /// Delivered messages a session thread has not processed yet. They hold
    /// a running slot: the engine still reports "ready" until it has read
    /// the message, and that must not free the slot early.
    claims: Mutex<HashMap<String, usize>>,
    /// When each session last received a message (ms), for routing.
    last_active: Mutex<HashMap<String, u64>>,
    /// Serializes message delivery (scheduler ticks, sends, tool calls).
    delivering: Mutex<()>,
    next_id: AtomicU64,
    next_generation: AtomicU64,
    clients: Mutex<HashMap<u64, Client>>,
    /// Pairing code → expiry (ms). Single use.
    pairings: Mutex<HashMap<String, u64>>,
    next_client: AtomicU64,
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
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
        Arc::new(Self {
            relay: Relay::new(relay, &store),
            store,
            agents,
            version,
            sessions: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
            claims: Mutex::new(HashMap::new()),
            last_active: Mutex::new(HashMap::new()),
            delivering: Mutex::new(()),
            next_id: AtomicU64::new(0),
            next_generation: AtomicU64::new(0),
            clients: Mutex::new(HashMap::new()),
            pairings: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
        })
    }

    pub fn add_client(&self, outbox: Sender<String>, device: Option<String>) -> u64 {
        let id = self.next_client.fetch_add(1, Ordering::SeqCst);
        lock(&self.clients).insert(id, Client { outbox, device });
        id
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

    /// Drops the client and its watches; sessions it was the last watcher of
    /// become unattended.
    pub fn remove_client(&self, client: u64) {
        lock(&self.clients).remove(&client);
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
        let (id, ops, generation) = match engine {
            None => session::spawn(Arc::clone(self), cwd.clone(), None, agent.clone())?,
            Some(kind) => {
                let id = engines::new_uuid()?;
                let (ops, generation) = engines::spawn(
                    Arc::clone(self),
                    kind,
                    id.clone(),
                    cwd.clone(),
                    options,
                    vec![],
                )?;
                (id, ops, generation)
            }
        };
        self.store
            .record_engine_session(&id, &cwd, agent.as_deref(), engine.map(engines::Kind::name))
            .map_err(|error| error.to_string())?;
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
                    archived: false,
                    engine: engine.map(|kind| kind.name().to_string()),
                }
            }
            (None, None) => return Err(format!("unknown session {id}")),
        };
        let kind = engines::Kind::parse(record.engine.as_deref().unwrap_or_default())?;
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
                let transcript = match kind {
                    engines::Kind::Claude => engines::claude::transcript(&record.cwd, id),
                };
                let options = engines::Options {
                    resume: Some(id.to_string()),
                    ..options
                };
                engines::spawn(
                    Arc::clone(self),
                    kind,
                    id.to_string(),
                    record.cwd.clone(),
                    options,
                    transcript,
                )?
            }
        };
        if record.closed {
            self.store
                .record_engine_session(
                    id,
                    &record.cwd,
                    record.agent.as_deref(),
                    record.engine.as_deref(),
                )
                .map_err(|error| error.to_string())?;
        }
        self.host(id.to_string(), ops, record.cwd, generation);
        Ok(())
    }

    fn host(&self, id: String, ops: Sender<Value>, cwd: PathBuf, generation: u64) {
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
        self.broadcast(&json!({ "type": "session_closed", "session": id }));
    }

    pub fn forward(&self, session: &str, op: Value) -> Result<(), String> {
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
                value["busy"] = json!(sessions.iter().any(|id| busy.contains(*id)));
                value
            })
            .collect();
        json!({ "type": "agents", "agents": list })
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

    /// One scheduler pass: due timers and overdue questions become messages,
    /// then everything pending is delivered.
    pub fn tick(self: &Arc<Self>) {
        self.fire_due_timers();
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

    pub fn questions_json(&self) -> Value {
        let list: Vec<Value> = self
            .store
            .open_questions()
            .iter()
            .map(|question| {
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
            })
            .collect();
        json!({ "type": "questions", "questions": list })
    }

    pub fn reports_json(&self, limit: usize) -> Value {
        let list: Vec<Value> = self.store.reports(limit).iter().map(report_json).collect();
        json!({ "type": "reports", "reports": list })
    }

    pub fn actions_json(&self) -> Value {
        let list: Vec<Value> = self
            .store
            .open_actions()
            .iter()
            .map(|action| action.to_json())
            .collect();
        json!({ "type": "actions", "actions": list })
    }

    /// Turns due timers into messages. The timer id is the message's dedupe
    /// key, so a timer that fired just before a crash fires only once.
    fn fire_due_timers(self: &Arc<Self>) {
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
        if message.from != "user" {
            return Ok(None);
        }
        // The user continues the conversation they had with this agent most
        // recently.
        let last_active = lock(&self.last_active);
        Ok(own
            .iter()
            .max_by_key(|record| {
                last_active
                    .get(&record.id)
                    .copied()
                    .unwrap_or(record.created_at)
            })
            .map(|record| record.id.clone()))
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
        lock(&self.last_active).insert(session.clone(), now());
        self.store
            .record_delivered(&message.id, &session)
            .map_err(|error| error.to_string())?;
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
    pub fn sessions_json(&self) -> Value {
        let records = self.store.sessions();
        let saved = saved_by_id(records.iter().map(|record| record.cwd.as_path()));
        let sessions = lock(&self.sessions);
        let list: Vec<Value> = records
            .into_iter()
            .map(|record| {
                let hosted = sessions.get(&record.id);
                let cwd = hosted.map(|h| h.cwd.clone()).unwrap_or(record.cwd);
                let summary = saved.get(&record.id);
                json!({
                    "session": record.id,
                    "chat": jucode_agent_core::chat::is_chat_dir(&cwd),
                    "cwd": cwd.display().to_string(),
                    "created_at": record.created_at,
                    "updated_at": summary.map_or(record.created_at, |s| s.updated_at * 1000),
                    "title": record.title.or_else(|| summary.map(|s| s.label.clone())),
                    "archived": record.archived,
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
                "title": record.and_then(|r| r.title.clone()).unwrap_or(label),
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
            .collect();
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

fn delivery_text(message: &Message) -> String {
    if message.from == "user" {
        return message.body.clone();
    }
    let origin = match message.from.split_once(':') {
        Some(("agent", id)) => format!("message from agent {id}"),
        Some(("timer", id)) => format!("timer {id} fired"),
        Some(("question", id)) => format!("answer to your question {id}"),
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
