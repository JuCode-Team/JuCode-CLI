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
const TOKEN: &str = "token";

pub struct Store {
    dir: PathBuf,
    write: Mutex<()>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub id: String,
    pub cwd: PathBuf,
    pub created_at: u64,
    pub closed: bool,
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
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
        let token = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
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

    pub fn record_session(&self, id: &str, cwd: &std::path::Path) -> io::Result<()> {
        self.append(
            SESSIONS,
            json!({ "kind": "open", "session": id, "cwd": cwd.display().to_string(), "at": now() }),
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

    fn append(&self, file: &str, value: Value) -> io::Result<()> {
        let _guard = self
            .write
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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

fn now() -> u64 {
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
            .record_session("a", std::path::Path::new("/p/a"))
            .unwrap();
        store
            .record_session("b", std::path::Path::new("/p/b"))
            .unwrap();
        store.record_session_closed("a").unwrap();
        let sessions = store.sessions();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "a");
        assert!(sessions[0].closed);
        assert!(!sessions[1].closed);
        store
            .record_session("a", std::path::Path::new("/p/a"))
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
}
