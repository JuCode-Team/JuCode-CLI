use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const MAX_LIVE_SUBAGENTS: usize = 4;
pub(crate) const MAX_SUBAGENT_DEPTH: u64 = 2;

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

#[derive(Debug, Clone)]
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
            },
        );
        state.push_event(&path, "pending", "reserved");
        self.inner.changed.notify_all();
        Ok(SubagentSlot {
            path,
            interrupt_flag,
        })
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
        if let Some(agent) = state.agents.get_mut(path) {
            if agent.status == SubagentStatus::Closed {
                self.inner.changed.notify_all();
                return;
            }
            agent.status = SubagentStatus::Completed;
            agent.completed_at_ms = Some(now_ms());
            agent.result = Some(result);
            agent.error = None;
            event = Some(("completed", "finished".to_string()));
        }
        if let Some((status, message)) = event {
            state.push_event(path, status, &message);
        }
        self.inner.changed.notify_all();
    }

    pub(crate) fn finish_err(&self, path: &str, error: String, partial: SubagentRunResult) {
        let mut state = self.inner.state.lock().unwrap();
        let mut event = None;
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
            agent.result = Some(partial);
            agent.error = Some(error.clone());
            let status = agent.status.as_str();
            event = Some((status, error));
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
                tool_calls: 0,
                tools_used: Vec::new(),
                input_tokens: 1,
                cached_input_tokens: 0,
                output_tokens: 2,
                elapsed_ms: 3,
                model: "gpt-test".to_string(),
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
}
