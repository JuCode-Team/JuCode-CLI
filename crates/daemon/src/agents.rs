//! Long-lived agents: `~/.jucode/agents/<id>/` holds an agent's brief
//! (`role.md`, `capabilities.md`, `policy.md`, `state.md`), its `memory/`
//! notes, `agent.json` (name, working directory, settings) and
//! `schedules.json` (its scheduled tasks, see `schedules`). The daemon
//! reads the brief into every turn of the agent's sessions; the agent keeps
//! it current with the `brief` tool.

use crate::store::{random_hex, write_private};
use jucode_agent_core::sandbox::{
    default_rules_json, directories_from_json, rules_from_json, rules_to_json, CommandRule,
    SandboxMode, SandboxPolicy,
};
use serde_json::{json, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const BRIEF_FILES: [&str; 4] = ["role.md", "capabilities.md", "policy.md", "state.md"];

/// Handoff notes kept per agent (`handoffs.json`), newest first.
const HANDOFFS_KEPT: usize = 20;
/// How many of them a session's prompt carries.
const HANDOFFS_IN_PROMPT: usize = 3;

pub struct Agents {
    dir: PathBuf,
    /// Held by `delete` and by writers that run on their own threads
    /// (handoff notes), so a note never lands in a folder being removed.
    removing: Mutex<()>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub id: String,
    pub name: String,
    /// Where the agent's sessions run.
    pub cwd: PathBuf,
    pub enabled: bool,
    /// Approval mode for its sessions (`manual`, `auto-edit`, `auto`,
    /// `full-access`); unattended agents default to `auto`.
    pub approval_mode: String,
    /// Sandbox for its shell commands: `read-only`, `workspace-write`
    /// (default) or `full-access`.
    pub sandbox: String,
    pub network: bool,
    /// Directories outside `cwd` it may use, each `ro` or `rw`.
    pub directories: Vec<Directory>,
    pub command_rules: Vec<CommandRule>,
    /// Custom icon, shaped as the clients' tab icons (`{kind, id | value |
    /// markup}`); clients sanitize an SVG before drawing it.
    pub icon: Option<Value>,
    /// `#rrggbb`.
    pub color: Option<String>,
    /// Seeds the generated avatar; agents from before it have none and
    /// clients use the id.
    pub avatar_seed: Option<String>,
    /// The desktop workspace it is listed in; None for agents from before
    /// workspaces had their own (clients put those in the default one). It
    /// runs whichever workspace is open.
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Directory {
    pub path: PathBuf,
    /// `ro` or `rw`.
    pub mode: String,
}

impl Agent {
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "id": self.id,
            "name": self.name,
            "cwd": self.cwd.display().to_string(),
            "enabled": self.enabled,
            "approval_mode": self.approval_mode,
            "sandbox": self.sandbox,
            "network": self.network,
            "directories": self.directories.iter().map(|dir| json!({
                "path": dir.path.display().to_string(),
                "mode": dir.mode,
            })).collect::<Vec<_>>(),
            "command_rules": rules_to_json(&self.command_rules),
        });
        if let Some(icon) = &self.icon {
            value["icon"] = icon.clone();
        }
        if let Some(color) = &self.color {
            value["color"] = json!(color);
        }
        if let Some(seed) = &self.avatar_seed {
            value["avatar_seed"] = json!(seed);
        }
        if let Some(workspace) = &self.workspace {
            value["workspace"] = json!(workspace);
        }
        value
    }

    /// The sandbox its sessions run in.
    pub fn policy(&self) -> Result<SandboxPolicy, String> {
        let dirs = |mode: &str| {
            self.directories
                .iter()
                .filter(|dir| dir.mode == mode)
                .map(|dir| dir.path.clone())
                .collect()
        };
        Ok(SandboxPolicy {
            mode: SandboxMode::parse(&self.sandbox)?,
            writable_dirs: dirs("rw"),
            readable_dirs: dirs("ro"),
            network: self.network,
            rules: self.command_rules.clone(),
        })
    }
}

/// `sandbox_directories`-shaped JSON as the agent's directory list.
fn directories(value: &Value, strict: bool) -> Result<Vec<Directory>, String> {
    let (writable, readable) = directories_from_json(value, strict)?;
    let entry = |path: PathBuf, mode: &str| Directory {
        path,
        mode: mode.to_string(),
    };
    Ok(writable
        .into_iter()
        .map(|path| entry(path, "rw"))
        .chain(readable.into_iter().map(|path| entry(path, "ro")))
        .collect())
}

impl Agents {
    pub fn open(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            removing: Mutex::new(()),
        })
    }

    pub fn list(&self) -> Vec<Agent> {
        let mut agents: Vec<Agent> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| self.get(&entry.file_name().to_string_lossy()))
            // The dispatcher is reached through dispatches, never listed.
            .filter(|agent| agent.id != crate::dispatch::AGENT)
            .collect();
        agents.sort_by(|a, b| a.id.cmp(&b.id));
        agents
    }

    pub fn get(&self, id: &str) -> Option<Agent> {
        if !valid_id(id) {
            return None;
        }
        let text = fs::read_to_string(self.dir.join(id).join("agent.json")).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        Some(Agent {
            id: id.to_string(),
            name: value["name"].as_str().unwrap_or(id).to_string(),
            cwd: PathBuf::from(value["cwd"].as_str()?),
            enabled: value["enabled"].as_bool().unwrap_or(true),
            approval_mode: value["approval_mode"]
                .as_str()
                .unwrap_or("auto")
                .to_string(),
            sandbox: value["sandbox"]
                .as_str()
                .unwrap_or(default_sandbox())
                .to_string(),
            network: value["network"].as_bool().unwrap_or(true),
            // A directory that has gone away is dropped, not an error.
            directories: directories(&value["directories"], false).unwrap_or_default(),
            command_rules: rules_from_json(&value["command_rules"]).unwrap_or_default(),
            icon: value.get("icon").filter(|icon| !icon.is_null()).cloned(),
            color: value["color"].as_str().map(str::to_string),
            avatar_seed: value["avatar_seed"].as_str().map(str::to_string),
            workspace: value["workspace"].as_str().map(str::to_string),
        })
    }

    /// Creates an agent working in `cwd`, with `role` as its first brief.
    /// `appearance` may carry `icon`, `color`, `avatar_seed` and the
    /// `workspace` it is listed in; without a
    /// seed it gets a random one.
    pub fn create(
        &self,
        id: &str,
        name: &str,
        cwd: &Path,
        role: &str,
        appearance: &Value,
    ) -> Result<Agent, String> {
        if !valid_id(id) {
            return Err(format!(
                "invalid agent id '{id}': use 1-40 lowercase letters, digits or '-'"
            ));
        }
        if !cwd.is_dir() {
            return Err(format!("not a directory: {}", cwd.display()));
        }
        let dir = self.dir.join(id);
        if dir.exists() {
            return Err(format!("agent {id} already exists"));
        }
        let agent = Agent {
            id: id.to_string(),
            name: if name.trim().is_empty() {
                id
            } else {
                name.trim()
            }
            .to_string(),
            cwd: cwd.to_path_buf(),
            enabled: true,
            approval_mode: "auto".to_string(),
            sandbox: default_sandbox().to_string(),
            network: true,
            directories: Vec::new(),
            command_rules: rules_from_json(&default_rules_json()).expect("default rules parse"),
            icon: None,
            color: None,
            avatar_seed: None,
            workspace: None,
        };
        let mut settings = agent.to_json();
        settings.as_object_mut().map(|map| map.remove("id"));
        settings["avatar_seed"] = json!(random_hex(8).map_err(|error| error.to_string())?);
        set_appearance(&mut settings, appearance)?;
        set_workspace(&mut settings, appearance);
        let write = || -> io::Result<()> {
            fs::create_dir_all(dir.join("memory"))?;
            fs::write(
                dir.join("agent.json"),
                serde_json::to_string_pretty(&settings)? + "\n",
            )?;
            fs::write(dir.join("role.md"), format!("{}\n", role.trim()))?;
            for file in &BRIEF_FILES[1..] {
                fs::write(dir.join(file), "")?;
            }
            Ok(())
        };
        write().map_err(|error| error.to_string())?;
        self.get(id).ok_or_else(|| format!("unknown agent {id}"))
    }

    /// Changes the settings present in `changes` (`name`, `enabled`,
    /// `approval_mode`, `sandbox`, `network`, `directories`,
    /// `command_rules`, `icon`, `color`, `avatar_seed`, `workspace`), keeping the rest of
    /// `agent.json`; `role` rewrites `role.md`.
    pub fn update(&self, id: &str, changes: &Value) -> Result<Agent, String> {
        if !valid_id(id) {
            return Err(format!("unknown agent {id}"));
        }
        let path = self.dir.join(id).join("agent.json");
        let text = fs::read_to_string(&path).map_err(|_| format!("unknown agent {id}"))?;
        let mut settings: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if let Some(name) = changes["name"].as_str().map(str::trim) {
            if name.is_empty() {
                return Err("agent name must not be empty".to_string());
            }
            settings["name"] = json!(name);
        }
        if let Some(enabled) = changes["enabled"].as_bool() {
            settings["enabled"] = json!(enabled);
        }
        if let Some(mode) = changes["approval_mode"].as_str() {
            if !matches!(mode, "manual" | "auto-edit" | "auto" | "full-access") {
                return Err(format!(
                    "unknown approval mode '{mode}': use manual, auto-edit, auto or full-access"
                ));
            }
            settings["approval_mode"] = json!(mode);
        }
        if let Some(sandbox) = changes["sandbox"].as_str() {
            SandboxMode::parse(sandbox)?;
            settings["sandbox"] = json!(sandbox);
        }
        if let Some(network) = changes["network"].as_bool() {
            settings["network"] = json!(network);
        }
        if !changes["directories"].is_null() {
            directories(&changes["directories"], true)?;
            settings["directories"] = changes["directories"].clone();
        }
        if !changes["command_rules"].is_null() {
            rules_from_json(&changes["command_rules"])?;
            settings["command_rules"] = changes["command_rules"].clone();
        }
        set_appearance(&mut settings, changes)?;
        set_workspace(&mut settings, changes);
        let text = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
        fs::write(&path, text + "\n").map_err(|error| error.to_string())?;
        if let Some(role) = changes["role"].as_str() {
            self.write_brief(id, "role.md", &format!("{}\n", role.trim()))?;
        }
        self.get(id).ok_or_else(|| format!("unknown agent {id}"))
    }

    /// Removes the agent's folder: brief, memory, settings and schedules.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        if !valid_id(id) {
            return Err(format!("unknown agent {id}"));
        }
        let _guard = self
            .removing
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        fs::remove_dir_all(self.dir.join(id)).map_err(|error| error.to_string())
    }

    /// The saved scheduled tasks of agent `id` (`schedules.json`).
    pub fn schedules(&self, id: &str) -> Vec<Value> {
        fs::read_to_string(self.dir.join(id).join("schedules.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save_schedules(&self, id: &str, schedules: &[Value]) -> Result<(), String> {
        let text = serde_json::to_string_pretty(schedules).map_err(|error| error.to_string())?;
        write_private(
            &self.dir.join(id).join("schedules.json"),
            (text + "\n").as_bytes(),
        )
        .map_err(|error| error.to_string())
    }

    /// Agent `id`'s handoff notes, newest first: `{session, title, at, text}`
    /// (`at` in ms), one per session, rewritten as the session goes on.
    pub fn handoffs(&self, id: &str) -> Vec<Value> {
        fs::read_to_string(self.dir.join(id).join("handoffs.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// The handoff note `session` left, if any.
    pub fn handoff(&self, id: &str, session: &str) -> Option<String> {
        self.handoffs(id)
            .into_iter()
            .find(|note| note["session"] == session)
            .and_then(|note| note["text"].as_str().map(str::to_string))
    }

    /// Replaces `session`'s note and moves it to the front.
    pub fn save_handoff(
        &self,
        id: &str,
        session: &str,
        title: &str,
        text: &str,
        at: u64,
    ) -> Result<(), String> {
        let _guard = self
            .removing
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self.get(id).is_none() {
            return Err(format!("unknown agent {id}"));
        }
        let mut notes = self.handoffs(id);
        notes.retain(|note| note["session"] != session);
        notes.insert(
            0,
            json!({ "session": session, "title": title, "at": at, "text": text }),
        );
        notes.truncate(HANDOFFS_KEPT);
        let text = serde_json::to_string_pretty(&notes).map_err(|error| error.to_string())?;
        write_private(
            &self.dir.join(id).join("handoffs.json"),
            (text + "\n").as_bytes(),
        )
        .map_err(|error| error.to_string())
    }

    /// Reads a brief file (`role.md` … `state.md`) or `memory/<name>.md`.
    pub fn read_brief(&self, id: &str, file: &str) -> Result<String, String> {
        let path = self.brief_path(id, file)?;
        match fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn write_brief(&self, id: &str, file: &str, content: &str) -> Result<(), String> {
        let path = self.brief_path(id, file)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(path, content).map_err(|error| error.to_string())
    }

    /// An existing memory note by its file name (`deploy.md`; the
    /// `memory/deploy.md` form `memory_files` lists also works).
    pub fn read_memory(&self, id: &str, file: &str) -> Result<String, String> {
        let name = file.strip_prefix("memory/").unwrap_or(file);
        let file = format!("memory/{name}");
        if self.get(id).is_none() {
            return Err(format!("unknown agent {id}"));
        }
        if !valid_memory_file(&file) {
            return Err(format!(
                "invalid memory file '{name}': use a name like deploy.md"
            ));
        }
        fs::read_to_string(self.dir.join(id).join(&file))
            .map_err(|_| format!("agent {id} has no memory file {name}"))
    }

    /// `memory/<name>.md` files, sorted.
    pub fn memory_files(&self, id: &str) -> Vec<String> {
        let mut files: Vec<String> = fs::read_dir(self.dir.join(id).join("memory"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| format!("memory/{}", entry.file_name().to_string_lossy()))
            .filter(|file| valid_memory_file(file))
            .collect();
        files.sort();
        files
    }

    /// The first non-empty line of the role, as a one-line description.
    pub fn summary(&self, id: &str) -> String {
        self.read_brief(id, "role.md")
            .unwrap_or_default()
            .lines()
            .map(|line| line.trim_start_matches('#').trim())
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_string()
    }

    /// System prompt text for a session of agent `id`: who it is, its brief,
    /// its memory index and the other agents it can message.
    /// The agent's part of the system prompt for `session`: its brief, the
    /// handoff notes of its latest other sessions, and the other agents.
    pub fn prompt(&self, id: &str, session: &str) -> String {
        let Some(agent) = self.get(id) else {
            return String::new();
        };
        let mut prompt = format!(
            "<agent id=\"{}\" name=\"{}\">\n\
             You are a long-lived agent: each task runs in its own session, but your brief below persists \
             and is yours to keep current with the `brief` tool. Record what the next session needs in \
             state.md (progress, open threads) and durable knowledge in memory/<topic>.md; what your latest \
             other sessions concluded is in <recent_handoffs>. Nobody may be watching: work on without \
             waiting. When only the user can decide, `question` them and continue under your assumption; \
             when something is done or blocked, `report` it. Use `timer` to come back to something once, \
             `schedule` to propose recurring work (the user switches it on), and `message_agent` to hand \
             work to another agent.\n",
            agent.id, agent.name
        );
        for file in BRIEF_FILES {
            let text = self.read_brief(id, file).unwrap_or_default();
            let name = file.trim_end_matches(".md");
            prompt.push_str(&format!("<{name}>\n{}\n</{name}>\n", text.trim()));
        }
        let memory = self.memory_files(id);
        prompt.push_str(&format!(
            "<memory_files>{}</memory_files>\n",
            if memory.is_empty() {
                "none yet".to_string()
            } else {
                memory.join(", ")
            }
        ));
        let notes: Vec<String> = self
            .handoffs(id)
            .into_iter()
            .filter(|note| note["session"] != session)
            .take(HANDOFFS_IN_PROMPT)
            .map(|note| {
                format!(
                    "<handoff session=\"{}\" title=\"{}\">\n{}\n</handoff>",
                    note["session"].as_str().unwrap_or_default(),
                    note["title"].as_str().unwrap_or_default(),
                    note["text"].as_str().unwrap_or_default().trim()
                )
            })
            .collect();
        if !notes.is_empty() {
            prompt.push_str(&format!(
                "<recent_handoffs>\n{}\n</recent_handoffs>\n",
                notes.join("\n")
            ));
        }
        let others: Vec<String> = self
            .list()
            .into_iter()
            .filter(|other| other.id != agent.id && other.enabled)
            .map(|other| {
                format!(
                    "- {} ({}): {}",
                    other.id,
                    other.name,
                    self.summary(&other.id)
                )
            })
            .collect();
        if !others.is_empty() {
            prompt.push_str(&format!(
                "<other_agents>\n{}\n</other_agents>\n",
                others.join("\n")
            ));
        }
        prompt.push_str("</agent>");
        prompt
    }

    fn brief_path(&self, id: &str, file: &str) -> Result<PathBuf, String> {
        if !valid_id(id) {
            return Err(format!("unknown agent {id}"));
        }
        if BRIEF_FILES.contains(&file) || valid_memory_file(file) {
            Ok(self.dir.join(id).join(file))
        } else {
            Err(format!(
                "unknown brief file '{file}': use {} or memory/<name>.md",
                BRIEF_FILES.join(", ")
            ))
        }
    }
}

/// Takes `workspace` from `changes` when present: a workspace id, or `null`
/// to leave it to the default workspace.
fn set_workspace(settings: &mut Value, changes: &Value) {
    match changes.get("workspace") {
        Some(Value::String(id)) if !id.trim().is_empty() => {
            settings["workspace"] = json!(id.trim())
        }
        Some(Value::Null) => {
            settings.as_object_mut().map(|map| map.remove("workspace"));
        }
        _ => {}
    }
}

/// Applies `icon`, `color` and `avatar_seed` from `changes`; `null` clears
/// one. Only shapes and sizes are checked here: an SVG icon is sanitized by
/// the clients that draw it.
fn set_appearance(settings: &mut Value, changes: &Value) -> Result<(), String> {
    for key in ["icon", "color", "avatar_seed"] {
        let Some(change) = changes.get(key) else {
            continue;
        };
        let value = match (key, change) {
            (_, Value::Null) => {
                settings.as_object_mut().map(|map| map.remove(key));
                continue;
            }
            ("icon", icon) => valid_icon(icon)?,
            ("color", Value::String(color))
                if color.len() == 7
                    && color.starts_with('#')
                    && color[1..].chars().all(|c| c.is_ascii_hexdigit()) =>
            {
                json!(color.to_ascii_lowercase())
            }
            ("avatar_seed", Value::String(seed)) if !seed.is_empty() && seed.len() <= 64 => {
                json!(seed)
            }
            _ => return Err(format!("invalid {key}: {change}")),
        };
        settings[key] = value;
    }
    Ok(())
}

/// `{kind: builtin, id}`, `{kind: slug, value}` (at most 32 UTF-16 units, as
/// the clients count) or `{kind: svg, markup}` (at most 8192 bytes), with
/// only those fields kept.
fn valid_icon(icon: &Value) -> Result<Value, String> {
    let text = |key: &str| icon[key].as_str().map(str::trim).filter(|s| !s.is_empty());
    match icon["kind"].as_str() {
        Some("builtin") => text("id").map(|id| json!({ "kind": "builtin", "id": id })),
        Some("slug") => text("value")
            .filter(|value| value.encode_utf16().count() <= 32)
            .map(|value| json!({ "kind": "slug", "value": value })),
        Some("svg") => text("markup")
            .filter(|markup| markup.len() <= 8192)
            .map(|markup| json!({ "kind": "svg", "markup": markup })),
        _ => None,
    }
    .ok_or_else(|| {
        "invalid icon: use {kind: builtin, id}, {kind: slug, value} (32 characters at most) or {kind: svg, markup} (8192 bytes at most)".to_string()
    })
}

/// Windows has no sandbox yet: its agents start without one.
fn default_sandbox() -> &'static str {
    if cfg!(windows) {
        "full-access"
    } else {
        "workspace-write"
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_memory_file(file: &str) -> bool {
    file.strip_prefix("memory/")
        .and_then(|name| name.strip_suffix(".md"))
        .is_some_and(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents(label: &str) -> (Agents, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "jucode-daemon-agents-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        (Agents::open(root.join("agents")).unwrap(), work)
    }

    #[test]
    fn create_writes_the_brief_and_reads_back() {
        let (agents, work) = agents("create");
        let agent = agents
            .create(
                "ops",
                "Ops",
                &work,
                "# Keeps the deploys green",
                &Value::Null,
            )
            .unwrap();
        assert_eq!(agents.get("ops"), Some(agent.clone()));
        assert_eq!(agent.approval_mode, "auto");
        assert_eq!(agents.summary("ops"), "Keeps the deploys green");
        assert!(agents
            .create("ops", "Ops", &work, "", &Value::Null)
            .is_err());
        assert!(agents
            .create("Bad Id", "x", &work, "", &Value::Null)
            .is_err());
        assert!(agents
            .create("nowhere", "x", &work.join("missing"), "", &Value::Null)
            .is_err());
    }

    use jucode_agent_core::sandbox::RuleAction;

    #[test]
    fn update_changes_only_the_given_settings() {
        let (agents, work) = agents("update");
        agents
            .create("ops", "Ops", &work, "role", &Value::Null)
            .unwrap();
        let updated = agents
            .update(
                "ops",
                &json!({ "enabled": false, "approval_mode": "manual" }),
            )
            .unwrap();
        assert_eq!(updated.name, "Ops");
        assert!(!updated.enabled);
        assert_eq!(updated.approval_mode, "manual");
        assert!(agents
            .update("ops", &json!({ "approval_mode": "yolo" }))
            .is_err());
        assert!(agents.update("missing", &json!({ "name": "x" })).is_err());
        assert!(agents.update("ops", &json!({ "sandbox": "open" })).is_err());
        assert!(agents
            .update(
                "ops",
                &json!({ "directories": [{ "path": "relative", "mode": "rw" }] })
            )
            .is_err());
        let logs = work.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let updated = agents
            .update(
                "ops",
                &json!({
                    "sandbox": "read-only",
                    "network": false,
                    "directories": [{ "path": logs, "mode": "ro" }],
                }),
            )
            .unwrap();
        let policy = updated.policy().unwrap();
        assert_eq!(policy.mode, SandboxMode::ReadOnly);
        assert!(!policy.network);
        assert_eq!(policy.readable_dirs, vec![logs]);
        // New agents may commit on their own and ask before pushing.
        assert_eq!(policy.rule_for("git commit -m x"), Some(RuleAction::Allow));
        assert_eq!(policy.rule_for("git push"), Some(RuleAction::Ask));
    }

    #[test]
    fn an_agent_is_listed_in_a_workspace_that_can_change() {
        let (agents, work) = agents("workspace");
        let old = agents
            .create("old", "Old", &work, "role", &Value::Null)
            .unwrap();
        assert_eq!(old.workspace, None);
        assert!(old.to_json().get("workspace").is_none());
        let ops = agents
            .create("ops", "Ops", &work, "role", &json!({ "workspace": "ws-1" }))
            .unwrap();
        assert_eq!(ops.to_json()["workspace"], "ws-1");
        let moved = agents
            .update("ops", &json!({ "workspace": "ws-2" }))
            .unwrap();
        assert_eq!(moved.workspace.as_deref(), Some("ws-2"));
        // Other changes leave it; null hands it back to the default workspace.
        assert_eq!(
            agents
                .update("ops", &json!({ "name": "Ops 2" }))
                .unwrap()
                .workspace
                .as_deref(),
            Some("ws-2")
        );
        assert_eq!(
            agents
                .update("ops", &json!({ "workspace": null }))
                .unwrap()
                .workspace,
            None
        );
    }

    #[test]
    fn appearance_is_checked_saved_and_cleared() {
        let (agents, work) = agents("appearance");
        let created = agents
            .create("ops", "Ops", &work, "role", &Value::Null)
            .unwrap();
        assert_eq!(created.avatar_seed.as_ref().map(String::len), Some(16));
        let web = agents
            .create("web", "Web", &work, "role", &json!({ "avatar_seed": "s1" }))
            .unwrap();
        assert_eq!(web.avatar_seed.as_deref(), Some("s1"));
        assert!(agents
            .create("api", "Api", &work, "role", &json!({ "color": "red" }))
            .is_err());
        assert!(agents.get("api").is_none());

        let rocket = json!({ "kind": "builtin", "id": "rocket" });
        let updated = agents
            .update(
                "ops",
                &json!({ "icon": { "kind": "builtin", "id": "rocket", "extra": 1 }, "color": "#2563EB" }),
            )
            .unwrap();
        assert_eq!(updated.icon, Some(rocket.clone()));
        assert_eq!(updated.color.as_deref(), Some("#2563eb"));
        assert_eq!(updated.to_json()["icon"], rocket);
        assert_eq!(agents.get("ops"), Some(updated));
        for bad in [
            json!({ "icon": { "kind": "emoji", "value": "x" } }),
            json!({ "icon": { "kind": "slug", "value": "x".repeat(33) } }),
            json!({ "icon": { "kind": "svg", "markup": "x".repeat(8193) } }),
            json!({ "icon": "rocket" }),
            json!({ "color": "#abc" }),
            json!({ "avatar_seed": "" }),
        ] {
            assert!(agents.update("ops", &bad).is_err(), "{bad}");
        }
        let cleared = agents
            .update(
                "ops",
                &json!({ "icon": null, "color": null, "avatar_seed": "abc" }),
            )
            .unwrap();
        assert_eq!((cleared.icon.clone(), cleared.color.clone()), (None, None));
        assert_eq!(cleared.avatar_seed.as_deref(), Some("abc"));
        assert!(cleared.to_json().get("icon").is_none());
        assert_eq!(agents.get("ops"), Some(cleared));
    }

    #[test]
    fn brief_files_are_limited_to_the_brief_and_memory() {
        let (agents, work) = agents("brief");
        agents
            .create("ops", "Ops", &work, "role", &Value::Null)
            .unwrap();
        agents.write_brief("ops", "state.md", "halfway").unwrap();
        agents
            .write_brief("ops", "memory/deploy.md", "use make ship")
            .unwrap();
        assert_eq!(agents.read_brief("ops", "state.md").unwrap(), "halfway");
        assert_eq!(agents.memory_files("ops"), vec!["memory/deploy.md"]);
        for bad in [
            "agent.json",
            "../x.md",
            "memory/../../x.md",
            "memory/a b.md",
            "notes.txt",
        ] {
            assert!(agents.write_brief("ops", bad, "x").is_err(), "{bad}");
        }
    }

    #[test]
    fn prompt_carries_the_brief_memory_and_other_agents() {
        let (agents, work) = agents("prompt");
        agents
            .create("ops", "Ops", &work, "Keeps deploys green", &Value::Null)
            .unwrap();
        agents
            .create("web", "Web", &work, "Owns the site", &Value::Null)
            .unwrap();
        agents
            .write_brief("ops", "state.md", "waiting on CI")
            .unwrap();
        agents.write_brief("ops", "memory/ci.md", "x").unwrap();
        agents
            .save_handoff("ops", "s1", "Fix CI", "CI green again", 1)
            .unwrap();
        agents
            .save_handoff("ops", "s2", "Deploy", "deployed v2", 2)
            .unwrap();
        let prompt = agents.prompt("ops", "s2");
        assert!(prompt
            .contains("<handoff session=\"s1\" title=\"Fix CI\">\nCI green again\n</handoff>"));
        assert!(
            !prompt.contains("deployed v2"),
            "a session's own note stays out of its prompt"
        );
        assert_eq!(agents.handoff("ops", "s2").as_deref(), Some("deployed v2"));
        assert_eq!(agents.handoffs("ops")[0]["session"], "s2");
        assert!(prompt.contains("<role>\nKeeps deploys green\n</role>"));
        assert!(prompt.contains("<state>\nwaiting on CI\n</state>"));
        assert!(prompt.contains("memory/ci.md"));
        assert!(prompt.contains("- web (Web): Owns the site"));
        assert!(!prompt.contains("- ops"));
    }
}
