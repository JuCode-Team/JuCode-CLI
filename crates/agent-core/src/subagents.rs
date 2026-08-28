use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const MAX_LIVE_SUBAGENTS: usize = 4;
pub(crate) const MAX_SUBAGENT_DEPTH: u64 = 2;
const MAX_HARVEST_FILES: usize = 200;

/// An isolated working directory for one subagent. Subagent file writes are
/// confined here so a child can never freely overwrite the parent's cwd files;
/// the parent harvests results explicitly (reads files, or applies a diff).
#[derive(Debug, Clone)]
pub(crate) struct SubagentWorkspace {
    pub root: PathBuf,
    /// True when the workspace is a detached git worktree of the parent repo
    /// (full file view, isolated writes); false for a fresh empty directory.
    pub from_git: bool,
}

/// Prepares the isolated workspace for a subagent under
/// `<parent_cwd>/.jucode/agents/<task>-<millis>`. Inside a git repository this
/// is a detached `git worktree` (the child sees the committed tree and its
/// writes stay in the worktree); outside a repository it is a fresh directory
/// (the child reads the parent tree via absolute paths but writes only here).
pub(crate) fn prepare_workspace(
    parent_cwd: &Path,
    task_name: &str,
) -> Result<SubagentWorkspace, String> {
    let root = parent_cwd
        .join(".jucode")
        .join("agents")
        .join(format!("{task_name}-{}", now_ms()));
    if root.exists() {
        return Err(format!(
            "subagent workspace already exists: {}",
            root.display()
        ));
    }
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    if in_git_repository(parent_cwd) {
        let output = Command::new("git")
            .arg("-C")
            .arg(parent_cwd)
            .args(["worktree", "add", "--detach"])
            .arg(&root)
            .output()
            .map_err(|error| format!("failed to run git worktree add: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git worktree add failed for subagent workspace {}: {}",
                root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(SubagentWorkspace {
            root,
            from_git: true,
        })
    } else {
        std::fs::create_dir_all(&root)
            .map_err(|error| format!("failed to create {}: {error}", root.display()))?;
        Ok(SubagentWorkspace {
            root,
            from_git: false,
        })
    }
}

fn in_git_repository(cwd: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Files the subagent changed inside its workspace, for the parent to harvest.
/// Worktrees ask git (modified + untracked, gitignore-aware); plain directories
/// list every file (the child started from an empty dir). Capped to keep the
/// tool result bounded.
pub(crate) fn changed_files(workspace: &SubagentWorkspace) -> Vec<String> {
    if workspace.from_git {
        let Ok(output) = Command::new("git")
            .arg("-C")
            .arg(&workspace.root)
            .args(["status", "--porcelain", "--no-renames"])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.get(3..).map(str::to_string))
            .take(MAX_HARVEST_FILES)
            .collect()
    } else {
        let mut files = Vec::new();
        collect_files(&workspace.root, &workspace.root, &mut files);
        files.truncate(MAX_HARVEST_FILES);
        files
    }
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<String>) {
    if files.len() >= MAX_HARVEST_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.display().to_string());
        }
        if files.len() >= MAX_HARVEST_FILES {
            return;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubagentStatus {
    Pending,
    Running,
    Completed,
    Errored,
    Interrupted,
    Closed,
}

impl SubagentStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Errored => "errored",
            Self::Interrupted => "interrupted",
            Self::Closed => "closed",
        }
    }

    fn is_live(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }

    fn is_final(&self) -> bool {
        !self.is_live()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentSpawn {
    pub parent_path: String,
    pub task_name: String,
    pub message: String,
    pub model: String,
    pub reasoning_effort: String,
    pub depth: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentSlot {
    pub path: String,
    pub interrupt_flag: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SubagentRunResult {
    pub summary: String,
    pub partial_output: String,
    pub tool_calls: u64,
    pub tools_used: Vec<String>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub model: String,
    /// Isolated workspace the agent wrote into (empty when spawn failed before
    /// workspace creation). The parent harvests changes from here.
    pub workdir: String,
    /// Workspace-relative paths the agent created or modified.
    pub files_changed: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentLifecycleEvent {
    pub path: String,
    pub status: String,
    pub message: String,
}

#[derive(Clone, Default)]
pub(crate) struct SubagentManager {
    inner: Arc<SubagentInner>,
}

#[derive(Default)]
struct SubagentInner {
    state: Mutex<SubagentRegistry>,
    changed: Condvar,
}

#[derive(Default)]
struct SubagentRegistry {
    agents: BTreeMap<String, SubagentRecord>,
    events: VecDeque<SubagentLifecycleEvent>,
    /// Token usage of subagents that reached a final state, awaiting fold-in to
    /// the parent's cumulative totals. Drained once via `drain_finished_usage`.
    finished_usage: Vec<SubagentRunResult>,
}

struct SubagentRecord {
    path: String,
    parent_path: String,
    task_name: String,
    message: String,
    model: String,
    reasoning_effort: String,
    depth: u64,
    status: SubagentStatus,
    interrupt_flag: Arc<AtomicBool>,
    queued_messages: VecDeque<String>,
    result: Option<SubagentRunResult>,
    error: Option<String>,
    started_at_ms: u64,
    completed_at_ms: Option<u64>,
    /// Isolated workspace path once prepared; None until then.
    workdir: Option<String>,
}

impl SubagentManager {
    pub(crate) fn reserve_spawn(&self, spawn: SubagentSpawn) -> Result<SubagentSlot, String> {
        validate_task_name(&spawn.task_name)?;
        if spawn.depth > MAX_SUBAGENT_DEPTH {
            return Err("agent depth limit reached. Solve the task yourself.".to_string());
        }
        let path = child_path(&spawn.parent_path, &spawn.task_name);
        let mut state = self.inner.state.lock().unwrap();
        if state.agents.contains_key(&path) {
            return Err(format!("agent already exists: {path}"));
        }
        let live = state
            .agents
            .values()
            .filter(|agent| agent.status.is_live())
            .count();
        if live >= MAX_LIVE_SUBAGENTS {
            return Err(format!(
                "too many live agents ({MAX_LIVE_SUBAGENTS}); wait for or close an agent first"
            ));
        }
        let interrupt_flag = Arc::new(AtomicBool::new(false));
        state.agents.insert(
            path.clone(),
            SubagentRecord {
                path: path.clone(),
                parent_path: spawn.parent_path,
                task_name: spawn.task_name,
                message: spawn.message,
                model: spawn.model,
                reasoning_effort: spawn.reasoning_effort,
                depth: spawn.depth,
                status: SubagentStatus::Pending,
                interrupt_flag: Arc::clone(&interrupt_flag),
                queued_messages: VecDeque::new(),
                result: None,
                error: None,
                started_at_ms: now_ms(),
                completed_at_ms: None,
                workdir: None,
            },
        );
        state.push_event(&path, "pending", "reserved");
        self.inner.changed.notify_all();
        Ok(SubagentSlot {
            path,
            interrupt_flag,
        })
    }

    pub(crate) fn set_workdir(&self, path: &str, workdir: &str) {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(agent) = state.agents.get_mut(path) {
            agent.workdir = Some(workdir.to_string());
        }
    }

    pub(crate) fn mark_running(&self, path: &str) {
        let mut state = self.inner.state.lock().unwrap();
        let mut event = None;
        if let Some(agent) = state.agents.get_mut(path) {
            if agent.status == SubagentStatus::Pending {
                agent.status = SubagentStatus::Running;
                event = Some(("running", "started"));
            }
        }
        if let Some((status, message)) = event {
            state.push_event(path, status, message);
        }
        self.inner.changed.notify_all();
    }

    pub(crate) fn finish_ok(&self, path: &str, result: SubagentRunResult) {
        let mut state = self.inner.state.lock().unwrap();
        let mut event = None;
        let mut finished = None;
        if let Some(agent) = state.agents.get_mut(path) {
            if agent.status == SubagentStatus::Closed {
                self.inner.changed.notify_all();
                return;
            }
            agent.status = SubagentStatus::Completed;
            agent.completed_at_ms = Some(now_ms());
            agent.result = Some(result.clone());
            agent.error = None;
            event = Some(("completed", "finished".to_string()));
            finished = Some(result);
        }
        if let Some(usage) = finished {
            state.finished_usage.push(usage);
        }
        if let Some((status, message)) = event {
            state.push_event(path, status, &message);
        }
        self.inner.changed.notify_all();
    }

    pub(crate) fn finish_err(&self, path: &str, error: String, partial: SubagentRunResult) {
        let mut state = self.inner.state.lock().unwrap();
        let mut event = None;
        let mut finished = None;
        if let Some(agent) = state.agents.get_mut(path) {
            if agent.status == SubagentStatus::Closed {
                self.inner.changed.notify_all();
                return;
            }
            agent.status = if agent.interrupt_flag.load(Ordering::SeqCst) || error == "interrupted"
            {
                SubagentStatus::Interrupted
            } else {
                SubagentStatus::Errored
            };
            agent.completed_at_ms = Some(now_ms());
            agent.result = Some(partial.clone());
            agent.error = Some(error.clone());
            let status = agent.status.as_str();
            event = Some((status, error));
            finished = Some(partial);
        }
        if let Some(usage) = finished {
            state.finished_usage.push(usage);
        }
        if let Some((status, message)) = event {
            state.push_event(path, status, &message);
        }
        self.inner.changed.notify_all();
    }

    pub(crate) fn send_message(
        &self,
        requester_path: &str,
        target: &str,
        message: &str,
    ) -> Result<Value, String> {
        let target = self.resolve_existing_target(requester_path, target)?;
        let mut state = self.inner.state.lock().unwrap();
        let agent = state
            .agents
            .get_mut(&target)
            .ok_or_else(|| format!("agent not found: {target}"))?;
        if agent.status.is_final() {
            return Err(format!("agent is not running: {target}"));
        }
        agent.queued_messages.push_back(message.to_string());
        state.push_event(&target, "message", "queued message");
        self.inner.changed.notify_all();
        Ok(json!({
            "target": target,
            "delivered": true,
            "status": "queued"
        }))
    }

    pub(crate) fn drain_messages(&self, path: &str) -> Vec<String> {
        let mut state = self.inner.state.lock().unwrap();
        let Some(agent) = state.agents.get_mut(path) else {
            return Vec::new();
        };
        agent.queued_messages.drain(..).collect()
    }

    pub(crate) fn close_agent(&self, requester_path: &str, target: &str) -> Result<Value, String> {
        let target = self.resolve_existing_target(requester_path, target)?;
        let mut state = self.inner.state.lock().unwrap();
        let mut should_emit = false;
        let previous = {
            let agent = state
                .agents
                .get_mut(&target)
                .ok_or_else(|| format!("agent not found: {target}"))?;
            let previous = status_json(agent);
            if agent.status.is_live() {
                agent.interrupt_flag.store(true, Ordering::SeqCst);
                agent.status = SubagentStatus::Closed;
                agent.completed_at_ms = Some(now_ms());
                should_emit = true;
            }
            previous
        };
        if should_emit {
            state.push_event(&target, "closed", "close requested");
        }
        self.inner.changed.notify_all();
        Ok(json!({
            "target": target,
            "previous_status": previous,
            "closed": true
        }))
    }

    pub(crate) fn close_all(&self) {
        self.close_all_with_message("parent interrupted");
    }

    pub(crate) fn close_all_with_message(&self, message: &str) {
        let mut state = self.inner.state.lock().unwrap();
        let mut closed = Vec::new();
        for agent in state.agents.values_mut() {
            if agent.status.is_live() {
                agent.interrupt_flag.store(true, Ordering::SeqCst);
                agent.status = SubagentStatus::Closed;
                agent.completed_at_ms = Some(now_ms());
                closed.push(agent.path.clone());
            }
        }
        for path in closed {
            state.push_event(&path, "closed", message);
        }
        self.inner.changed.notify_all();
    }

    pub(crate) fn list_agents(&self, requester_path: &str, path_prefix: Option<&str>) -> Value {
        let state = self.inner.state.lock().unwrap();
        let prefix = path_prefix
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| self.resolve_path_prefix(requester_path, value));
        let agents = state
            .agents
            .values()
            .filter(|agent| {
                prefix
                    .as_ref()
                    .map(|prefix| agent.path.starts_with(prefix))
                    .unwrap_or(true)
            })
            .map(agent_json)
            .collect::<Vec<_>>();
        json!({ "agents": agents })
    }

    pub(crate) fn wait_agents(
        &self,
        requester_path: &str,
        targets: Vec<String>,
        timeout_ms: u64,
    ) -> Result<Value, String> {
        let canonical_targets = {
            let state = self.inner.state.lock().unwrap();
            targets
                .iter()
                .map(|target| resolve_existing_target_in_state(&state, requester_path, target))
                .collect::<Result<Vec<_>, _>>()?
        };
        let deadline = Duration::from_millis(timeout_ms);
        let started = SystemTime::now();
        let mut state = self.inner.state.lock().unwrap();
        loop {
            let ready = wait_ready(&state, &canonical_targets);
            if ready {
                break;
            }
            let elapsed = started.elapsed().unwrap_or_default();
            if elapsed >= deadline {
                break;
            }
            let remaining = deadline.saturating_sub(elapsed);
            let (next_state, _) = self.inner.changed.wait_timeout(state, remaining).unwrap();
            state = next_state;
        }

        let ready = wait_ready(&state, &canonical_targets);
        let statuses = wait_statuses(&state, &canonical_targets);
        Ok(json!({
            "status": statuses,
            "timed_out": !ready,
        }))
    }

    pub(crate) fn drain_events(&self) -> Vec<SubagentLifecycleEvent> {
        let mut state = self.inner.state.lock().unwrap();
        state.events.drain(..).collect()
    }

    /// Drains the token usage of subagents that have reached a final state so the
    /// parent can fold it into its cumulative totals. Returns each result once.
    pub(crate) fn drain_finished_usage(&self) -> Vec<SubagentRunResult> {
        let mut state = self.inner.state.lock().unwrap();
        state.finished_usage.drain(..).collect()
    }

    fn resolve_existing_target(
        &self,
        requester_path: &str,
        target: &str,
    ) -> Result<String, String> {
        let state = self.inner.state.lock().unwrap();
        resolve_existing_target_in_state(&state, requester_path, target)
    }

    fn resolve_path_prefix(&self, requester_path: &str, value: &str) -> String {
        if value.starts_with('/') {
            value.to_string()
        } else {
            child_path(requester_path, value)
        }
    }
}

impl SubagentRegistry {
    fn push_event(&mut self, path: &str, status: &str, message: &str) {
        self.events.push_back(SubagentLifecycleEvent {
            path: path.to_string(),
            status: status.to_string(),
            message: message.to_string(),
        });
    }
}

fn resolve_existing_target_in_state(
    state: &SubagentRegistry,
    requester_path: &str,
    target: &str,
) -> Result<String, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("target is required".to_string());
    }
    let canonical = if target.starts_with('/') {
        target.to_string()
    } else {
        child_path(requester_path, target)
    };
    if state.agents.contains_key(&canonical) {
        Ok(canonical)
    } else {
        Err(format!("agent not found: {canonical}"))
    }
}

fn wait_ready(state: &SubagentRegistry, targets: &[String]) -> bool {
    if targets.is_empty() {
        state.agents.values().any(|agent| agent.status.is_final())
            || !state.agents.values().any(|agent| agent.status.is_live())
    } else {
        targets.iter().any(|target| {
            state
                .agents
                .get(target)
                .map(|agent| agent.status.is_final())
                .unwrap_or(true)
        })
    }
}

fn wait_statuses(state: &SubagentRegistry, targets: &[String]) -> Value {
    let mut statuses = serde_json::Map::new();
    let agents = if targets.is_empty() {
        state.agents.values().collect::<Vec<_>>()
    } else {
        targets
            .iter()
            .filter_map(|target| state.agents.get(target))
            .collect::<Vec<_>>()
    };
    for agent in agents {
        statuses.insert(agent.path.clone(), status_json(agent));
    }
    Value::Object(statuses)
}

fn agent_json(agent: &SubagentRecord) -> Value {
    json!({
        "task_name": agent.path,
        "name": agent.task_name,
        "parent": agent.parent_path,
        "depth": agent.depth,
        "status": status_json(agent),
        "model": agent.model,
        "reasoning_effort": agent.reasoning_effort,
        "message": agent.message,
        "started_at_ms": agent.started_at_ms,
        "completed_at_ms": agent.completed_at_ms,
        "workdir": agent.workdir,
        "result": agent.result.as_ref().map(result_json),
    })
}

fn status_json(agent: &SubagentRecord) -> Value {
    match agent.status {
        SubagentStatus::Completed => json!({
            "completed": agent.result.as_ref().map(|result| result.summary.clone()).unwrap_or_default()
        }),
        SubagentStatus::Errored => json!({
            "errored": agent.error.clone().unwrap_or_else(|| "agent failed".to_string()),
            "partial_output": agent.result.as_ref().map(|result| result.partial_output.clone()).unwrap_or_default()
        }),
        SubagentStatus::Pending
        | SubagentStatus::Running
        | SubagentStatus::Interrupted
        | SubagentStatus::Closed => json!(agent.status.as_str()),
    }
}

fn result_json(result: &SubagentRunResult) -> Value {
    json!({
        "summary": result.summary,
        "partial_output": result.partial_output,
        "tool_calls": result.tool_calls,
        "tools_used": result.tools_used,
        "input_tokens": result.input_tokens,
        "cached_input_tokens": result.cached_input_tokens,
        "output_tokens": result.output_tokens,
        "elapsed_ms": result.elapsed_ms,
        "model": result.model,
        "workdir": result.workdir,
        "files_changed": result.files_changed,
    })
}

fn child_path(parent: &str, task_name: &str) -> String {
    let parent = parent.trim_end_matches('/');
    format!("{parent}/{}", task_name.trim_matches('/'))
}

fn validate_task_name(task_name: &str) -> Result<(), String> {
    if task_name.is_empty() {
        return Err("task_name is required".to_string());
    }
    if task_name.len() > 64 {
        return Err("task_name is too long".to_string());
    }
    if task_name == "root"
        || !task_name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err("task_name must use lowercase letters, digits, and underscores".to_string());
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn(task_name: &str) -> SubagentSpawn {
        SubagentSpawn {
            parent_path: "/root".to_string(),
            task_name: task_name.to_string(),
            message: "inspect".to_string(),
            model: "gpt-test".to_string(),
            reasoning_effort: "medium".to_string(),
            depth: 1,
        }
    }

    #[test]
    fn subagent_spawn_reserves_and_lists_agent() {
        let manager = SubagentManager::default();
        let slot = manager.reserve_spawn(spawn("worker")).unwrap();
        assert_eq!(slot.path, "/root/worker");
        let listed = manager.list_agents("/root", None);
        assert_eq!(listed["agents"][0]["task_name"], "/root/worker");
        assert_eq!(listed["agents"][0]["status"], "pending");
    }

    #[test]
    fn subagent_rejects_duplicate_task_name() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("worker")).unwrap();
        let error = manager.reserve_spawn(spawn("worker")).unwrap_err();
        assert!(error.contains("already exists"));
    }

    #[test]
    fn subagent_enforces_live_limit() {
        let manager = SubagentManager::default();
        for index in 0..MAX_LIVE_SUBAGENTS {
            manager
                .reserve_spawn(spawn(&format!("worker_{index}")))
                .unwrap();
        }
        let error = manager.reserve_spawn(spawn("extra")).unwrap_err();
        assert!(error.contains("too many live agents"));
    }

    #[test]
    fn subagent_wait_returns_completed_summary() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("worker")).unwrap();
        manager.finish_ok(
            "/root/worker",
            SubagentRunResult {
                summary: "done".to_string(),
                partial_output: "done".to_string(),
                input_tokens: 1,
                output_tokens: 2,
                elapsed_ms: 3,
                model: "gpt-test".to_string(),
                ..Default::default()
            },
        );
        let result = manager
            .wait_agents("/root", vec!["worker".to_string()], 1)
            .unwrap();
        assert_eq!(result["timed_out"], false);
        assert_eq!(result["status"]["/root/worker"]["completed"], "done");
        let listed = manager.list_agents("/root", None);
        assert_eq!(listed["agents"][0]["result"]["summary"], "done");
        assert_eq!(listed["agents"][0]["result"]["tool_calls"], 0);
    }

    fn run_result(input: u64, output: u64) -> SubagentRunResult {
        SubagentRunResult {
            summary: "done".to_string(),
            partial_output: "done".to_string(),
            input_tokens: input,
            output_tokens: output,
            elapsed_ms: 1,
            model: "gpt-test".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn finished_usage_drains_once_and_skips_closed_agents() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("worker")).unwrap();
        manager.finish_ok("/root/worker", run_result(100, 7));

        let drained = manager.drain_finished_usage();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].input_tokens, 100);
        assert_eq!(drained[0].output_tokens, 7);
        assert_eq!(drained[0].model, "gpt-test");
        // Drained exactly once: a second drain is empty.
        assert!(manager.drain_finished_usage().is_empty());

        // A closed agent contributes no usage even if its thread later finishes.
        manager.reserve_spawn(spawn("closed_worker")).unwrap();
        manager.mark_running("/root/closed_worker");
        manager.close_agent("/root", "closed_worker").unwrap();
        manager.finish_ok("/root/closed_worker", run_result(999, 999));
        assert!(manager.drain_finished_usage().is_empty());
    }

    #[test]
    fn subagent_close_interrupts_running_agent() {
        let manager = SubagentManager::default();
        let slot = manager.reserve_spawn(spawn("worker")).unwrap();
        manager.mark_running("/root/worker");
        let result = manager.close_agent("/root", "worker").unwrap();
        assert_eq!(result["closed"], true);
        assert!(slot.interrupt_flag.load(Ordering::SeqCst));
        let listed = manager.list_agents("/root", None);
        assert_eq!(listed["agents"][0]["status"], "closed");
    }

    #[test]
    fn subagent_rejects_depth_limit() {
        let manager = SubagentManager::default();
        let mut spawn = spawn("too_deep");
        spawn.depth = MAX_SUBAGENT_DEPTH + 1;
        let error = manager.reserve_spawn(spawn).unwrap_err();
        assert!(error.contains("depth limit"));
    }

    #[test]
    fn subagent_target_not_found_is_clear() {
        let manager = SubagentManager::default();
        let error = manager
            .wait_agents("/root", vec!["missing".to_string()], 1)
            .unwrap_err();
        assert!(error.contains("agent not found: /root/missing"));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jucode-subagent-ws-{tag}-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    #[test]
    fn workspace_in_git_repo_is_worktree_and_child_writes_stay_out_of_parent() {
        let repo = temp_dir("git");
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("tracked.txt"), "committed content").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);

        let workspace = prepare_workspace(&repo, "worker").unwrap();
        assert!(workspace.from_git);
        assert!(workspace
            .root
            .starts_with(repo.join(".jucode").join("agents")));
        // The child sees the committed tree...
        assert!(workspace.root.join("tracked.txt").exists());

        // ...and its writes do not appear in the parent cwd.
        std::fs::write(workspace.root.join("child_output.txt"), "from child").unwrap();
        std::fs::write(workspace.root.join("tracked.txt"), "child edit").unwrap();
        assert!(!repo.join("child_output.txt").exists());
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "committed content"
        );

        let mut changed = changed_files(&workspace);
        changed.sort();
        assert_eq!(changed, vec!["child_output.txt", "tracked.txt"]);

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn workspace_outside_git_repo_is_fresh_restricted_directory() {
        let parent = temp_dir("plain");
        std::fs::write(parent.join("parent.txt"), "parent data").unwrap();

        let workspace = prepare_workspace(&parent, "worker").unwrap();
        assert!(!workspace.from_git);
        // Restricted cwd: the child starts from an empty directory.
        assert!(!workspace.root.join("parent.txt").exists());

        std::fs::write(workspace.root.join("report.md"), "findings").unwrap();
        assert!(!parent.join("report.md").exists());
        assert_eq!(changed_files(&workspace), vec!["report.md".to_string()]);

        let _ = std::fs::remove_dir_all(parent);
    }
}
