//! Shared daemon state: hosted sessions, connected clients and who watches
//! what. A session is attended while at least one client watches it.

use crate::{session, store::Store};
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

pub struct Hub {
    pub store: Store,
    pub version: &'static str,
    sessions: Mutex<HashMap<String, Hosted>>,
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
    pub fn new(store: Store, version: &'static str) -> Arc<Self> {
        Arc::new(Self {
            store,
            version,
            sessions: Mutex::new(HashMap::new()),
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

    /// Starts a new session in `cwd`.
    pub fn create_session(self: &Arc<Self>, cwd: PathBuf) -> Result<String, String> {
        if !cwd.is_dir() {
            return Err(format!("not a directory: {}", cwd.display()));
        }
        let (id, ops) = session::spawn(Arc::clone(self), cwd.clone(), None)?;
        self.store
            .record_session(&id, &cwd)
            .map_err(|error| error.to_string())?;
        self.host(id.clone(), ops, cwd);
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
        let (_, ops) = session::spawn(Arc::clone(self), record.cwd.clone(), Some(id.to_string()))?;
        if record.closed {
            self.store
                .record_session(id, &record.cwd)
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
        if before != after {
            let _ = hosted
                .ops
                .send(json!({ "op": "set_attended", "attended": after }));
        }
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
                    "open": hosted.is_some(),
                    "watchers": hosted.map(|h| h.watchers.len()).unwrap_or(0),
                })
            })
            .collect();
        json!({ "type": "sessions", "sessions": list })
    }
}
