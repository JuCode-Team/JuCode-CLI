//! The Workflows and Task subagents a Claude Code session ran, for the
//! desktop's agent trace: live from `system/task_*` frames, and from what
//! Claude Code saved for the conversation (`<session>/workflows/*.json`, the
//! Agent tool's results) once the session is reopened. Each subagent's own
//! conversation is a file: `<session>/subagents/agent-<id>.jsonl`, or
//! `<session>/subagents/workflows/<run>/agent-<id>.jsonl` for a Workflow's.

use super::{rows, text, Claude, Options};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// The newest items of a subagent's conversation that are sent.
const TRANSCRIPT_ITEMS: usize = 600;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `agent_runs` entries keep these field names; the desktop reads them.
#[derive(Default)]
pub(super) struct Runs {
    workflows: Vec<Value>,
    agents: Vec<Value>,
    seeded: bool,
}

/// One agent of a `workflow_progress` list.
fn workflow_agent(item: &Value) -> Value {
    json!({
        "id": item["agentId"],
        "label": item["label"],
        "phase": item["phaseIndex"],
        "model": item["model"],
        "state": item["state"],
        "started_at": item["startedAt"],
        "duration_ms": item["durationMs"],
        "tokens": item["tokens"],
        "tool_calls": item["toolCalls"],
        "attempt": item["attempt"],
        "prompt": item["promptPreview"],
        "result": item["resultPreview"],
        "error": item["error"],
    })
}

fn phases_and_agents(progress: &Value) -> (Vec<Value>, Vec<Value>) {
    let items = progress.as_array().map_or(&[][..], Vec::as_slice);
    let phases = items
        .iter()
        .filter(|i| i["type"] == "workflow_phase")
        .map(|i| json!({ "index": i["index"], "title": i["title"] }))
        .collect();
    let agents = items
        .iter()
        .filter(|i| i["type"] == "workflow_agent")
        .map(workflow_agent)
        .collect();
    (phases, agents)
}

fn find<'a>(list: &'a mut [Value], id: &str) -> Option<&'a mut Value> {
    list.iter_mut().find(|e| e["id"] == id)
}

impl Runs {
    pub fn event(&self) -> Value {
        json!({ "type": "agent_runs", "workflows": self.workflows, "agents": self.agents })
    }

    /// `task_started` of a Workflow or a Task subagent.
    pub fn started(&mut self, frame: &Value) -> bool {
        let id = text(&frame["task_id"]);
        let entry = match text(&frame["task_type"]) {
            "local_workflow" => json!({
                "id": id,
                "tool_use_id": frame["tool_use_id"],
                "name": frame["workflow_name"],
                "description": frame["description"],
                "status": "running",
                "started_at": now_ms(),
                "phases": [],
                "agents": [],
            }),
            "local_agent" => json!({
                "id": id,
                "label": frame["description"],
                "type": frame["subagent_type"],
                "tool_use_id": frame["tool_use_id"],
                "status": "running",
                "started_at": now_ms(),
            }),
            _ => return false,
        };
        let list = if frame["task_type"] == "local_workflow" {
            &mut self.workflows
        } else {
            &mut self.agents
        };
        list.retain(|e| e["id"] != id);
        list.push(entry);
        true
    }

    /// `task_progress`: a Workflow's whole agent list, or a subagent's usage.
    pub fn progress(&mut self, frame: &Value) -> bool {
        let id = text(&frame["task_id"]);
        let usage = &frame["usage"];
        if let Some(run) = find(&mut self.workflows, id) {
            if !frame["workflow_progress"].is_null() {
                let (phases, agents) = phases_and_agents(&frame["workflow_progress"]);
                run["phases"] = json!(phases);
                run["agents"] = json!(agents);
            }
            run["tokens"] = usage["total_tokens"].clone();
            run["tool_calls"] = usage["tool_uses"].clone();
            run["duration_ms"] = usage["duration_ms"].clone();
            return true;
        }
        if let Some(agent) = find(&mut self.agents, id) {
            agent["tokens"] = usage["total_tokens"].clone();
            agent["tool_calls"] = usage["tool_uses"].clone();
            agent["duration_ms"] = usage["duration_ms"].clone();
            if !text(&frame["summary"]).is_empty() {
                agent["summary"] = frame["summary"].clone();
            }
            return true;
        }
        false
    }

    /// `task_notification`: the run ended.
    pub fn finished(&mut self, frame: &Value) -> bool {
        let id = text(&frame["task_id"]);
        let usage = &frame["usage"];
        let Some(run) = find(&mut self.workflows, id).or_else(|| find(&mut self.agents, id)) else {
            return false;
        };
        run["status"] = frame["status"].clone();
        if usage.is_object() {
            run["tokens"] = usage["total_tokens"].clone();
            run["tool_calls"] = usage["tool_uses"].clone();
            run["duration_ms"] = usage["duration_ms"].clone();
        }
        true
    }

    /// The Agent tool's result: the subagent's model and final totals.
    pub fn agent_result(&mut self, result: &Value) -> bool {
        let Some(agent) = find(&mut self.agents, text(&result["agentId"])) else {
            return false;
        };
        agent["model"] = result["resolvedModel"].clone();
        if result["totalTokens"].is_u64() {
            agent["tokens"] = result["totalTokens"].clone();
            agent["tool_calls"] = result["totalToolUseCount"].clone();
            agent["duration_ms"] = result["totalDurationMs"].clone();
            agent["status"] = json!("completed");
        }
        true
    }

    /// Runs saved for the conversation before this process (a reopened
    /// session), under the live ones. Once.
    pub fn seed(&mut self, session: &Path) {
        if std::mem::replace(&mut self.seeded, true) {
            return;
        }
        let mut saved: Vec<Value> = fs::read_dir(session.join("workflows"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| serde_json::from_slice::<Value>(&fs::read(p).ok()?).ok())
            .map(|w| {
                let (phases, agents) = phases_and_agents(&w["workflowProgress"]);
                json!({
                    "id": w["taskId"],
                    "name": w["workflowName"],
                    "description": w["summary"],
                    "status": w["status"],
                    "started_at": w["startTime"],
                    "duration_ms": w["durationMs"],
                    "tokens": w["totalTokens"],
                    "tool_calls": w["totalToolCalls"],
                    "phases": phases,
                    "agents": agents,
                })
            })
            .collect();
        // The Workflow calls that launched them, by task id.
        let file = session.with_extension("jsonl");
        for row in rows(&file, 64 * 1024 * 1024) {
            let launched = &row["toolUseResult"];
            if launched["taskType"] != "local_workflow" {
                continue;
            }
            let call = row["message"]["content"][0]["tool_use_id"].clone();
            if let Some(w) = saved.iter_mut().find(|w| w["id"] == launched["taskId"]) {
                w["tool_use_id"] = call;
            }
        }
        saved.retain(|w| !self.workflows.iter().any(|l| l["id"] == w["id"]));
        saved.sort_by_key(|w| w["started_at"].as_u64().unwrap_or(0));
        self.workflows.splice(0..0, saved);

        // Agent tool calls, then their results with the subagent's totals.
        let mut labels = std::collections::HashMap::new();
        let mut agents = Vec::new();
        for row in rows(&file, 64 * 1024 * 1024) {
            for block in row["message"]["content"].as_array().into_iter().flatten() {
                if block["type"] == "tool_use" && matches!(text(&block["name"]), "Agent" | "Task") {
                    labels.insert(
                        text(&block["id"]).to_string(),
                        (
                            block["input"]["description"].clone(),
                            block["input"]["subagent_type"].clone(),
                        ),
                    );
                }
                let result = &row["toolUseResult"];
                if block["type"] != "tool_result" || !result["totalTokens"].is_u64() {
                    continue;
                }
                let id = text(&result["agentId"]);
                if id.is_empty() || self.agents.iter().any(|a| a["id"] == id) {
                    continue;
                }
                let (label, kind) = labels
                    .get(text(&block["tool_use_id"]))
                    .cloned()
                    .unwrap_or_default();
                let duration = result["totalDurationMs"].as_u64().unwrap_or(0);
                let ended = chrono::DateTime::parse_from_rfc3339(text(&row["timestamp"]))
                    .map_or(0, |t| t.timestamp_millis().max(0) as u64);
                agents.push(json!({
                    "id": id,
                    "label": label,
                    "type": kind,
                    "tool_use_id": block["tool_use_id"],
                    "status": result["status"],
                    "model": result["resolvedModel"],
                    "started_at": ended.saturating_sub(duration),
                    "duration_ms": duration,
                    "tokens": result["totalTokens"],
                    "tool_calls": result["totalToolUseCount"],
                }));
            }
        }
        self.agents.splice(0..0, agents);
    }
}

/// A subagent's saved conversation file, by its agent id.
fn agent_file(session: &Path, id: &str) -> Option<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let name = format!("agent-{id}.jsonl");
    let direct = session.join("subagents").join(&name);
    if direct.is_file() {
        return Some(direct);
    }
    fs::read_dir(session.join("subagents").join("workflows"))
        .ok()?
        .flatten()
        .map(|run| run.path().join(&name))
        .find(|path| path.is_file())
}

/// A Workflow agent's task comes framed by the harness; the task is what
/// follows, indented by two spaces.
fn task_text(text: &str) -> String {
    const MARK: &str = "The computed task text follows:\n";
    match text.split_once(MARK) {
        Some((_, task)) => task
            .lines()
            .map(|line| line.strip_prefix("  ").unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n"),
        None => text.to_string(),
    }
}

/// A subagent's conversation as transcript items: `user`, `assistant` and
/// `reasoning` (`content`), and `tool` (`call_id`, `name`, `output` as the
/// tool card reads it, `is_error`, `running` until its result).
pub(super) fn transcript(session: &Path, id: &str) -> Result<Vec<Value>, String> {
    let file = agent_file(session, id)
        .ok_or_else(|| format!("no conversation saved for subagent {id}"))?;
    // Tool cards the way a live session builds them.
    let mut cards = Claude::new(&Options::default());
    let mut items: Vec<Value> = Vec::new();
    for row in rows(&file, 64 * 1024 * 1024) {
        let content = &row["message"]["content"];
        match text(&row["type"]) {
            "user" => {
                if let Some(prompt) = content.as_str() {
                    items.push(json!({ "role": "user", "content": task_text(prompt) }));
                    continue;
                }
                for block in content.as_array().into_iter().flatten() {
                    match text(&block["type"]) {
                        "text" => items.push(
                            json!({ "role": "user", "content": task_text(text(&block["text"])) }),
                        ),
                        "tool_result" => {
                            for event in cards.tool_result_events(block, &row["toolUseResult"]) {
                                if event["type"] != "tool_output" {
                                    continue;
                                }
                                if let Some(item) = items
                                    .iter_mut()
                                    .rev()
                                    .find(|i| i["call_id"] == event["call_id"])
                                {
                                    item["output"] = event["output"].clone();
                                    item["is_error"] = event["is_error"].clone();
                                    item["running"] = json!(false);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "assistant" => {
                for block in content.as_array().into_iter().flatten() {
                    match text(&block["type"]) {
                        "text" if !text(&block["text"]).trim().is_empty() => {
                            items.push(json!({ "role": "assistant", "content": block["text"] }))
                        }
                        "thinking" if !text(&block["thinking"]).trim().is_empty() => {
                            items.push(json!({ "role": "reasoning", "content": block["thinking"] }))
                        }
                        _ if super::is_tool_use(block) => {
                            let events = cards.tool_use_events(block, true);
                            let name = events
                                .iter()
                                .find(|e| e["type"] == "tool_start")
                                .map(|e| e["name"].clone());
                            let output = events
                                .iter()
                                .find(|e| e["type"] == "tool_update")
                                .map(|e| e["output"].clone());
                            if let Some(name) = name {
                                items.push(json!({
                                    "role": "tool",
                                    "call_id": block["id"],
                                    "name": name,
                                    "output": output.unwrap_or(json!("")),
                                    "is_error": false,
                                    "running": true,
                                }));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let excess = items.len().saturating_sub(TRANSCRIPT_ITEMS);
    items.drain(..excess);
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("jucode-runs-{}-{}", std::process::id(), now_ms()));
        fs::create_dir_all(dir.join("conv/workflows")).unwrap();
        fs::create_dir_all(dir.join("conv/subagents/workflows/wf_1")).unwrap();
        dir.join("conv")
    }

    #[test]
    fn a_workflow_is_traced_live_and_ends_with_its_totals() {
        let mut runs = Runs::default();
        assert!(runs.started(&json!({ "task_id": "w1", "task_type": "local_workflow", "workflow_name": "review", "description": "Review changes" })));
        assert!(runs.progress(&json!({ "task_id": "w1", "usage": { "total_tokens": 900, "tool_uses": 3, "duration_ms": 1200 },
            "workflow_progress": [
                { "type": "workflow_phase", "index": 1, "title": "Find" },
                { "type": "workflow_agent", "agentId": "a1", "label": "bugs-0", "phaseIndex": 1, "model": "claude-sonnet-5-5",
                  "state": "running", "startedAt": 10, "tokens": 900, "toolCalls": 3, "durationMs": 1200, "promptPreview": "Find bugs" }
            ] })));
        assert!(runs.finished(&json!({ "task_id": "w1", "status": "completed", "usage": { "total_tokens": 2000, "tool_uses": 5, "duration_ms": 4000 } })));
        let event = runs.event();
        let run = &event["workflows"][0];
        assert_eq!(run["phases"][0]["title"], "Find");
        assert_eq!(run["agents"][0]["label"], "bugs-0");
        assert_eq!(run["agents"][0]["prompt"], "Find bugs");
        assert_eq!(
            (run["status"].as_str(), run["tokens"].as_u64()),
            (Some("completed"), Some(2000))
        );
        // Other task kinds are not runs.
        assert!(!runs.started(&json!({ "task_id": "b1", "task_type": "local_bash" })));
    }

    #[test]
    fn a_task_subagent_takes_its_totals_from_the_agent_result() {
        let mut runs = Runs::default();
        runs.started(&json!({ "task_id": "a9", "task_type": "local_agent", "description": "Read a.txt", "subagent_type": "general-purpose" }));
        runs.agent_result(&json!({ "agentId": "a9", "resolvedModel": "claude-sonnet-5-5", "totalTokens": 30420, "totalToolUseCount": 1, "totalDurationMs": 4529 }));
        let agent = &runs.event()["agents"][0];
        assert_eq!(agent["label"], "Read a.txt");
        assert_eq!(agent["model"], "claude-sonnet-5-5");
        assert_eq!(agent["tokens"], 30420);
        assert_eq!(agent["status"], "completed");
    }

    #[test]
    fn a_reopened_session_lists_what_claude_saved() {
        let session = session();
        fs::write(
            session.join("workflows/wf_1.json"),
            json!({ "taskId": "w1", "workflowName": "two-hi", "summary": "Two agents", "status": "completed",
                "startTime": 100, "durationMs": 3586, "totalTokens": 56940, "totalToolCalls": 0,
                "workflowProgress": [{ "type": "workflow_agent", "agentId": "a1", "label": "hi-0", "phaseIndex": 1, "state": "done" }] })
                .to_string(),
        )
        .unwrap();
        let rows = [
            json!({ "type": "user", "toolUseResult": { "status": "async_launched", "taskId": "w1", "taskType": "local_workflow" },
                "message": { "content": [{ "type": "tool_result", "tool_use_id": "t0", "content": "launched" }] } }),
            json!({ "type": "assistant", "message": { "content": [{ "type": "tool_use", "id": "t1", "name": "Agent", "input": { "description": "Read a.txt", "subagent_type": "general-purpose" } }] } }),
            json!({ "type": "user", "timestamp": "2026-10-05T04:51:10.000Z", "toolUseResult": { "agentId": "a9", "status": "completed", "resolvedModel": "claude-sonnet-5-5", "totalTokens": 30420, "totalToolUseCount": 1, "totalDurationMs": 4000 },
                "message": { "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "alpha" }] } }),
        ];
        fs::write(
            session.with_extension("jsonl"),
            rows.iter()
                .map(|r| r.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
        let mut runs = Runs::default();
        runs.seed(&session);
        let event = runs.event();
        assert_eq!(event["workflows"][0]["name"], "two-hi");
        assert_eq!(event["workflows"][0]["agents"][0]["id"], "a1");
        assert_eq!(event["workflows"][0]["tool_use_id"], "t0");
        let agent = &event["agents"][0];
        assert_eq!(agent["label"], "Read a.txt");
        assert_eq!(agent["tokens"], 30420);
        assert_eq!(agent["started_at"], 1_791_175_870_000u64 - 4000);
    }

    #[test]
    fn a_subagents_conversation_reads_as_transcript_items() {
        let session = session();
        let rows = [
            json!({ "type": "user", "message": { "content": "[Workflow harness — computed task] framing.\nThe computed task text follows:\n  Read a.txt\n  then report" } }),
            json!({ "type": "assistant", "message": { "content": [
                { "type": "thinking", "thinking": "Look at the file." },
                { "type": "tool_use", "id": "t1", "name": "Read", "input": { "file_path": "/p/a.txt" } }
            ] } }),
            json!({ "type": "user", "message": { "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "alpha" }] } }),
            json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "It says alpha." }] } }),
        ];
        fs::write(
            session.join("subagents/workflows/wf_1/agent-a1.jsonl"),
            rows.iter()
                .map(|r| r.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
        let items = transcript(&session, "a1").unwrap();
        assert_eq!(
            items[0],
            json!({ "role": "user", "content": "Read a.txt\nthen report" })
        );
        assert_eq!(items[1]["role"], "reasoning");
        assert_eq!(items[2]["name"], "read");
        assert_eq!(items[2]["running"], false);
        assert!(items[2]["output"].as_str().unwrap().contains("/p/a.txt"));
        assert_eq!(items[2]["is_error"], false);
        assert_eq!(items[3]["content"], "It says alpha.");
        assert!(transcript(&session, "../x").is_err());
        assert!(transcript(&session, "nope").is_err());
    }
}
