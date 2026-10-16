//! Once: the long-lived agents and dispatch are gone (2026-10). Before the
//! store reads its logs, what they left is put aside in
//! `~/.jucode/backup-agents-<time>/`, nothing deleted:
//!
//! - each agent's scheduled tasks become scheduled tasks of their own, in the
//!   agent's directory on the jucode engine in its approval mode, switched
//!   on or off as they were (off when the directory was the agent's own
//!   folder, which goes into the backup);
//! - the agents' sessions are archived, and their deferred actions closed;
//! - `~/.jucode/agents/` and the daemon's agent logs move to the backup.
//!
//! It runs while any of that is still there; each step can run again.

use crate::{schedules, store::write_private};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::Path,
};

/// The daemon's files only the agents and dispatch used.
const AGENT_LOGS: [&str; 7] = [
    "messages.jsonl",
    "timers.jsonl",
    "questions.jsonl",
    "reports.jsonl",
    "tasks.jsonl",
    "runs.jsonl",
    "dispatches.json",
];

/// `daemon`: the state directory (`~/.jucode/daemon`), locked by the caller.
pub(crate) fn remove_agents(daemon: &Path) -> io::Result<()> {
    let Some(root) = daemon.parent() else {
        return Ok(());
    };
    let agents = root.join("agents");
    let logs: Vec<&str> = AGENT_LOGS
        .into_iter()
        .filter(|file| daemon.join(file).exists())
        .collect();
    if !agents.is_dir() && logs.is_empty() {
        return Ok(());
    }
    if agents.is_dir() {
        move_schedules(daemon, &agents)?;
    }
    let sessions = agent_sessions(daemon);
    if !sessions.is_empty() {
        archive(daemon, &sessions)?;
        close_actions(daemon, &sessions)?;
    }
    let backup = root.join(format!(
        "backup-agents-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    fs::create_dir_all(backup.join("daemon"))?;
    for file in logs {
        fs::rename(daemon.join(file), backup.join("daemon").join(file))?;
    }
    if agents.is_dir() {
        fs::rename(&agents, backup.join("agents"))?;
    }
    jucode_agent_core::log_warn!(
        "daemon",
        "agents removed; their data is in the backup",
        backup = backup.display().to_string()
    );
    Ok(())
}

/// Every agent's `schedules.json` into the daemon's, skipping ids it has.
fn move_schedules(daemon: &Path, agents: &Path) -> io::Result<()> {
    let path = daemon.join(schedules::FILE);
    let mut list: Vec<Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let known: HashSet<String> = list
        .iter()
        .filter_map(|s| s["id"].as_str().map(str::to_string))
        .collect();
    let mut added = false;
    for entry in fs::read_dir(agents)?.flatten() {
        let dir = entry.path();
        let read = |file: &str| -> Value {
            fs::read_to_string(dir.join(file))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default()
        };
        let agent = read("agent.json");
        // The agent's own folder goes into the backup with it.
        let cwd = agent["cwd"]
            .as_str()
            .filter(|cwd| !Path::new(cwd).starts_with(agents))
            .filter(|cwd| Path::new(cwd).is_dir());
        let mode = match agent["approval_mode"].as_str() {
            Some("manual") => "ask",
            Some("auto-edit") => "edits",
            Some("full-access") => "all",
            _ => "auto",
        };
        for old in read("schedules.json").as_array().into_iter().flatten() {
            let Some(id) = old["id"].as_str().filter(|id| !known.contains(*id)) else {
                continue;
            };
            list.push(json!({
                "id": id,
                "name": old["name"],
                "prompt": old["prompt"],
                "cwd": cwd,
                "engine": "jucode",
                "mode": mode,
                "options": {},
                // A disabled agent's tasks did not fire.
                "enabled": old["enabled"] == true && agent["enabled"] != false && cwd.is_some(),
                "repeat": old["repeat"],
                "time": old["time"],
                "days": old["days"],
                "date": old["date"],
                "created_at": old["created_at"].as_u64().unwrap_or_default() * 1000,
            }));
            added = true;
        }
    }
    if added {
        write_private(&path, format!("{:#}\n", json!(list)).as_bytes())?;
    }
    Ok(())
}

/// The sessions an agent ran, from the session log.
fn agent_sessions(daemon: &Path) -> HashSet<String> {
    lines(&daemon.join("sessions.jsonl"))
        .filter(|entry| entry["kind"] == "open" && entry["agent"].is_string())
        .filter_map(|entry| entry["session"].as_str().map(str::to_string))
        .collect()
}

fn archive(daemon: &Path, sessions: &HashSet<String>) -> io::Result<()> {
    let at = crate::store::now();
    append(
        &daemon.join("sessions.jsonl"),
        sessions
            .iter()
            .map(|id| json!({ "kind": "meta", "session": id, "archived": true, "at": at })),
    )
}

/// Closes the deferred actions of `sessions` that are still open.
fn close_actions(daemon: &Path, sessions: &HashSet<String>) -> io::Result<()> {
    let path = daemon.join("actions.jsonl");
    let entries: Vec<Value> = lines(&path).collect();
    // Decided, or closed and not reopened since.
    let mut settled: HashSet<&str> = HashSet::new();
    for entry in &entries {
        let Some(id) = entry["id"].as_str() else {
            continue;
        };
        match entry["kind"].as_str() {
            Some("decided" | "closed") => {
                settled.insert(id);
            }
            Some("reopened") => {
                settled.remove(id);
            }
            _ => {}
        }
    }
    let at = crate::store::now();
    let closing: Vec<Value> = entries
        .iter()
        .filter(|e| e["kind"] == "deferred")
        .filter(|e| e["action"]["session_id"].as_str().is_some_and(|s| sessions.contains(s)))
        .filter_map(|e| e["action"]["id"].as_str())
        .filter(|id| !settled.contains(id))
        .map(|id| {
            json!({ "kind": "closed", "id": id, "by": "migration", "reason": "Agent 功能已移除", "at": at })
        })
        .collect();
    append(&path, closing.into_iter())
}

fn lines(path: &Path) -> impl Iterator<Item = Value> {
    String::from_utf8_lossy(&fs::read(path).unwrap_or_default())
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect::<Vec<Value>>()
        .into_iter()
}

fn append(path: &Path, entries: impl Iterator<Item = Value>) -> io::Result<()> {
    let text: String = entries.map(|entry| format!("{entry}\n")).collect();
    if text.is_empty() {
        return Ok(());
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // A line torn by a crash ends without a newline.
    let torn = fs::read(path)?.last().is_some_and(|byte| *byte != b'\n');
    if torn {
        file.write_all(b"\n")?;
    }
    file.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{now, Store};

    #[test]
    fn agents_leave_their_schedules_and_archive_their_sessions() {
        let root = std::env::temp_dir().join(format!(
            "jucode-migrate-agents-{}-{}",
            std::process::id(),
            now()
        ));
        let daemon = root.join("daemon");
        let agents = root.join("agents");
        let project = root.join("project");
        for dir in [
            &daemon,
            &project,
            &agents.join("ops"),
            &agents.join("own").join("home"),
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        let write = |path: &Path, value: Value| fs::write(path, value.to_string()).unwrap();
        write(
            &agents.join("ops").join("agent.json"),
            json!({ "cwd": project, "approval_mode": "full-access" }),
        );
        write(
            &agents.join("ops").join("schedules.json"),
            json!([{ "id": "sch-a", "agent": "ops", "name": "巡检", "prompt": "check",
                "enabled": true, "repeat": "daily", "time": "09:00", "days": [],
                "created_at": 7, "new_session": true, "by_agent": false }]),
        );
        fs::create_dir_all(agents.join("off")).unwrap();
        write(
            &agents.join("off").join("agent.json"),
            json!({ "cwd": project, "enabled": false }),
        );
        write(
            &agents.join("off").join("schedules.json"),
            json!([{ "id": "sch-c", "name": "周报", "prompt": "sum up", "enabled": true,
                "repeat": "daily", "time": "08:00", "days": [], "created_at": 1 }]),
        );
        write(
            &agents.join("own").join("agent.json"),
            json!({ "cwd": agents.join("own").join("home") }),
        );
        write(
            &agents.join("own").join("schedules.json"),
            json!([{ "id": "sch-b", "name": "日报", "prompt": "write", "enabled": true,
                "repeat": "weekly", "time": "18:00", "days": [5], "created_at": 1 }]),
        );
        fs::write(
            daemon.join("sessions.jsonl"),
            [
                json!({ "kind": "open", "session": "a1", "cwd": "/p", "agent": "ops", "at": 1 }),
                json!({ "kind": "open", "session": "u1", "cwd": "/p", "agent": null, "at": 1 }),
            ]
            .map(|e| format!("{e}\n"))
            .concat(),
        )
        .unwrap();
        let action = |id: &str, session: &str| {
            json!({ "kind": "deferred", "action": { "id": id, "session_id": session, "cwd": "/p",
                "call_id": id, "name": "bash", "arguments": "{}", "summary": "ls", "digest": id,
                "created_at": 1 } })
        };
        fs::write(
            daemon.join("actions.jsonl"),
            [action("x", "a1"), action("y", "u1")]
                .map(|e| format!("{e}\n"))
                .concat(),
        )
        .unwrap();
        fs::write(daemon.join("runs.jsonl"), "{}\n").unwrap();

        let store = Store::open(daemon.clone()).unwrap();
        let sessions = store.sessions();
        assert!(sessions.iter().any(|s| s.id == "a1" && s.archived));
        assert!(sessions.iter().any(|s| s.id == "u1" && !s.archived));
        let open: Vec<String> = store.open_actions().into_iter().map(|a| a.id).collect();
        assert_eq!(open, ["y"]);
        assert_eq!(store.closed_actions(0)[0].1.by, "migration");

        let moved = schedules::load(&daemon);
        let a = moved.iter().find(|s| s.id == "sch-a").unwrap();
        assert_eq!(
            (
                a.cwd.as_deref(),
                a.mode.as_str(),
                a.enabled,
                a.engine.as_deref()
            ),
            (Some(project.as_path()), "all", true, None)
        );
        assert_eq!(a.created_at, 7000);
        assert!(a.next_run_at.is_some());
        let b = moved.iter().find(|s| s.id == "sch-b").unwrap();
        assert_eq!(
            (b.cwd.as_deref(), b.enabled),
            (None, false),
            "its folder is gone"
        );
        let c = moved.iter().find(|s| s.id == "sch-c").unwrap();
        assert!(!c.enabled, "a disabled agent's task did not fire");

        assert!(!agents.exists() && !daemon.join("runs.jsonl").exists());
        let backup = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("backup-agents-")
            })
            .unwrap()
            .path();
        assert!(backup.join("agents/ops/agent.json").exists());
        assert!(backup.join("daemon/runs.jsonl").exists());

        // Nothing left to move: opening again changes nothing.
        drop(store);
        let store = Store::open(daemon.clone()).unwrap();
        assert_eq!(store.sessions(), sessions);
        assert_eq!(schedules::load(&daemon).len(), 3);
        let _ = fs::remove_dir_all(root);
    }
}
