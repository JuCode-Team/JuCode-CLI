//! Long-lived agents: `~/.jucode/agents/<id>/` holds an agent's brief
//! (`role.md`, `capabilities.md`, `policy.md`, `state.md`), its `memory/`
//! notes and `agent.json` (name, working directory, settings). The daemon
//! reads the brief into every turn of the agent's sessions; the agent keeps
//! it current with the `brief` tool.

use jucode_agent_core::sandbox::{
    default_rules_json, directories_from_json, rules_from_json, rules_to_json, CommandRule,
    SandboxMode, SandboxPolicy,
};
use serde_json::{json, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub const BRIEF_FILES: [&str; 4] = ["role.md", "capabilities.md", "policy.md", "state.md"];

pub struct Agents {
    dir: PathBuf,
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct Directory {
    pub path: PathBuf,
    /// `ro` or `rw`.
    pub mode: String,
}

impl Agent {
    pub fn to_json(&self) -> Value {
        json!({
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
        })
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
        Ok(Self { dir })
    }

    pub fn list(&self) -> Vec<Agent> {
        let mut agents: Vec<Agent> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| self.get(&entry.file_name().to_string_lossy()))
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
        })
    }

    /// Creates an agent working in `cwd`, with `role` as its first brief.
    pub fn create(&self, id: &str, name: &str, cwd: &Path, role: &str) -> Result<Agent, String> {
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
        };
        let write = || -> io::Result<()> {
            fs::create_dir_all(dir.join("memory"))?;
            let mut settings = agent.to_json();
            settings.as_object_mut().map(|map| map.remove("id"));
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
        Ok(agent)
    }

    /// Changes the settings present in `changes` (`name`, `enabled`,
    /// `approval_mode`, `sandbox`, `network`, `directories`,
    /// `command_rules`), keeping the rest of `agent.json`.
    pub fn update(&self, id: &str, changes: &Value) -> Result<Agent, String> {
        if !valid_id(id) {
            return Err(format!("unknown agent {id}"));
        }
        let path = self.dir.join(id).join("agent.json");
        let text = fs::read_to_string(&path).map_err(|_| format!("unknown agent {id}"))?;
        let mut settings: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if let Some(name) = changes["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
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
        let text = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
        fs::write(&path, text + "\n").map_err(|error| error.to_string())?;
        self.get(id).ok_or_else(|| format!("unknown agent {id}"))
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
    pub fn prompt(&self, id: &str) -> String {
        let Some(agent) = self.get(id) else {
            return String::new();
        };
        let mut prompt = format!(
            "<agent id=\"{}\" name=\"{}\">\n\
             You are a long-lived agent: your sessions come and go, but your brief below persists and is \
             yours to keep current with the `brief` tool. Record what the next session needs in state.md \
             (progress, open threads) and durable knowledge in memory/<topic>.md. Nobody may be watching: \
             work on without waiting. When only the user can decide, `question` them and continue under \
             your assumption; when something is done or blocked, `report` it. Use `timer` to come back to \
             something later and `message_agent` to hand work to another agent.\n",
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
            .create("ops", "Ops", &work, "# Keeps the deploys green")
            .unwrap();
        assert_eq!(agents.get("ops"), Some(agent.clone()));
        assert_eq!(agent.approval_mode, "auto");
        assert_eq!(agents.summary("ops"), "Keeps the deploys green");
        assert!(agents.create("ops", "Ops", &work, "").is_err());
        assert!(agents.create("Bad Id", "x", &work, "").is_err());
        assert!(agents
            .create("nowhere", "x", &work.join("missing"), "")
            .is_err());
    }

    use jucode_agent_core::sandbox::RuleAction;

    #[test]
    fn update_changes_only_the_given_settings() {
        let (agents, work) = agents("update");
        agents.create("ops", "Ops", &work, "role").unwrap();
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
    fn brief_files_are_limited_to_the_brief_and_memory() {
        let (agents, work) = agents("brief");
        agents.create("ops", "Ops", &work, "role").unwrap();
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
            .create("ops", "Ops", &work, "Keeps deploys green")
            .unwrap();
        agents.create("web", "Web", &work, "Owns the site").unwrap();
        agents
            .write_brief("ops", "state.md", "waiting on CI")
            .unwrap();
        agents.write_brief("ops", "memory/ci.md", "x").unwrap();
        let prompt = agents.prompt("ops");
        assert!(prompt.contains("<role>\nKeeps deploys green\n</role>"));
        assert!(prompt.contains("<state>\nwaiting on CI\n</state>"));
        assert!(prompt.contains("memory/ci.md"));
        assert!(prompt.contains("- web (Web): Owns the site"));
        assert!(!prompt.contains("- ops"));
    }
}
