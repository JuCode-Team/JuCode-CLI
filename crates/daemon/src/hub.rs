//! Shared daemon state: hosted sessions, connected clients and who watches
//! what, plus message routing and timers for long-lived agents. A session is
//! attended while at least one client watches it.

use crate::{
    agents::Agents,
    session,
    store::{now, Message, Store, Timer},
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

/// Runs that may be in progress at once across all sessions; messages that
/// would start another run wait for a free slot.
pub const MAX_RUNNING: usize = 4;

pub struct Hub {
    pub store: Store,
    pub agents: Agents,
    pub version: &'static str,
    sessions: Mutex<HashMap<String, Hosted>>,
    /// Sessions whose engine is running a turn or has queued messages.
    busy: Mutex<HashSet<String>>,
    /// When each session last received a message (ms), for routing.
    last_active: Mutex<HashMap<String, u64>>,
    /// Serializes message delivery (scheduler ticks, sends, tool calls).
    delivering: Mutex<()>,
    next_id: AtomicU64,
    clients: Mutex<HashMap<u64, Sender<String>>>,
    next_client: AtomicU64,
}

struct Hosted {
    ops: Sender<Value>,
    cwd: PathBuf,
    watchers: HashSet<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl Hub {
    pub fn new(store: Store, agents: Agents, version: &'static str) -> Arc<Self> {
        Arc::new(Self {
            store,
            agents,
            version,
            sessions: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
            last_active: Mutex::new(HashMap::new()),
            delivering: Mutex::new(()),
            next_id: AtomicU64::new(0),
            clients: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
        })
    }

    pub fn add_client(&self, outbox: Sender<String>) -> u64 {
        let id = self.next_client.fetch_add(1, Ordering::SeqCst);
        lock(&self.clients).insert(id, outbox);
        id
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
        lock(&self.clients).retain(|_, outbox| outbox.send(text.clone()).is_ok());
    }

    pub fn send_to(&self, client: u64, frame: &Value) {
        if let Some(outbox) = lock(&self.clients).get(&client) {
            let _ = outbox.send(frame.to_string());
        }
    }

    /// Starts a new session in `cwd`, or in the agent's directory for an
    /// agent session.
    pub fn create_session(
        self: &Arc<Self>,
        cwd: Option<PathBuf>,
        agent: Option<&str>,
    ) -> Result<String, String> {
        let cwd = match agent {
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
        let (id, ops) = session::spawn(Arc::clone(self), cwd.clone(), None, agent.clone())?;
        self.store
            .record_session(&id, &cwd, agent.as_deref())
            .map_err(|error| error.to_string())?;
        self.host(id.clone(), ops, cwd);
        self.broadcast(&self.agents_json());
        Ok(id)
    }

    /// Hosts a session recorded earlier (after a restart or a close). A
    /// session that is already hosted is left as it is.
    pub fn open_session(self: &Arc<Self>, id: &str) -> Result<(), String> {
        if lock(&self.sessions).contains_key(id) {
            return Ok(());
        }
        let record = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == id)
            .ok_or_else(|| format!("unknown session {id}"))?;
        let (_, ops) = session::spawn(
            Arc::clone(self),
            record.cwd.clone(),
            Some(id.to_string()),
            record.agent.clone(),
        )?;
        if record.closed {
            self.store
                .record_session(id, &record.cwd, record.agent.as_deref())
                .map_err(|error| error.to_string())?;
        }
        self.host(id.to_string(), ops, record.cwd);
        Ok(())
    }

    fn host(&self, id: String, ops: Sender<Value>, cwd: PathBuf) {
        lock(&self.sessions).insert(
            id,
            Hosted {
                ops,
                cwd,
                watchers: HashSet::new(),
            },
        );
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
    pub fn session_ended(&self, id: &str) {
        if lock(&self.sessions).remove(id).is_some() {
            let _ = self.store.record_session_closed(id);
        }
        lock(&self.busy).remove(id);
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

    /// Called by a session thread when its engine starts or finishes work.
    pub fn set_busy(&self, session: &str, busy: bool) {
        let changed = if busy {
            lock(&self.busy).insert(session.to_string())
        } else {
            lock(&self.busy).remove(session)
        };
        if changed {
            self.broadcast(&self.agents_json());
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

    /// Turns due timers into messages. The timer id is the message's dedupe
    /// key, so a timer that fired just before a crash fires only once.
    pub fn fire_due_timers(self: &Arc<Self>) {
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
        self.deliver_pending();
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
                self.open_session(&session)?;
                session
            }
            None => self.create_session(None, Some(&message.to))?,
        };
        self.forward(
            &session,
            json!({ "op": "user_message", "content": delivery_text(message) }),
        )?;
        // A delivered message starts (or queues) a run; count it before the
        // engine reports its status so the next message sees the slot taken.
        lock(&self.busy).insert(session.clone());
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
        let sessions = lock(&self.sessions);
        let list: Vec<Value> = self
            .store
            .sessions()
            .into_iter()
            .map(|record| {
                let hosted = sessions.get(&record.id);
                json!({
                    "session": record.id,
                    "cwd": hosted.map(|h| h.cwd.clone()).unwrap_or(record.cwd).display().to_string(),
                    "created_at": record.created_at,
                    "agent": record.agent,
                    "open": hosted.is_some(),
                    "watchers": hosted.map(|h| h.watchers.len()).unwrap_or(0),
                })
            })
            .collect();
        json!({ "type": "sessions", "sessions": list })
    }
}

/// How a message reads in the receiving session: the user's own words
/// as-is, anything else with a line saying where it came from.
fn delivery_text(message: &Message) -> String {
    if message.from == "user" {
        return message.body.clone();
    }
    let origin = match message.from.split_once(':') {
        Some(("agent", id)) => format!("message from agent {id}"),
        Some(("timer", id)) => format!("timer {id} fired"),
        _ => format!("message from {}", message.from),
    };
    format!("[{origin} · {}]\n{}", message.id, message.body)
}
