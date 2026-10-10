//! An agent's work as tasks and runs. Every message delivered to an agent
//! is a run of a task: a scheduled task (its schedule's id), or a task of
//! its own that one session carries (`TaskRecord`, `tasks.jsonl`). A run
//! goes queued → running when its session takes the message, and ends with
//! the turn; it concludes with an outcome the agent gives with `finish`, or
//! one inferred from how it went and its handoff note. A task's state is
//! read from its runs, questions and schedule, never stored.

use crate::{
    hub::{lock, title_from, Hub},
    store::{now, Message, Outcome, Question, Run, TaskRecord},
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

/// Runs of one agent working at once; more wait.
pub const MAX_PER_AGENT: usize = 2;
/// Questions one task may have open; the agent closes some before asking more.
pub const OPEN_QUESTIONS_PER_TASK: usize = 5;
/// How long a session's runs stay in the migration: older archived
/// sessions stay plain sessions.
const MIGRATE_SINCE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const MIGRATED: &str = "tasks_migrated";

const VERDICTS: [&str; 4] = ["quiet", "done", "needs_you", "failed"];
/// A run working longer than this is stopped and counted as failed.
pub const RUN_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1000;
/// An agent session nobody watches has its engine closed this long after
/// its last run ended; the next message to it opens it again.
pub const IDLE_CLOSE_MS: u64 = 30 * 60 * 1000;
/// How often `sweep` looks for runs to time out and sessions to close.
const SWEEP_EVERY_MS: u64 = 60 * 1000;
static LAST_SWEEP: AtomicU64 = AtomicU64::new(0);
/// How many earlier runs' notes a prompt carries.
const NOTES_IN_PROMPT: usize = 3;

/// What started a run, from its message's origin.
fn trigger(from: &str, task_exists: bool) -> &'static str {
    match from.split_once(':').map_or(from, |(kind, _)| kind) {
        "user" if task_exists => "reply",
        "user" => "user",
        "schedule" => "schedule",
        "question" => "answer",
        "timer" => "timer",
        "agent" => "agent",
        _ => "other",
    }
}

impl Hub {
    /// The task `session` belongs to: its latest run's, or the task it was
    /// opened for.
    pub fn task_of_session(&self, session: &str) -> Option<String> {
        self.store
            .runs()
            .into_iter()
            .rev()
            .find(|run| run.session.as_deref() == Some(session))
            .map(|run| run.task)
            .or_else(|| {
                self.store
                    .tasks()
                    .into_iter()
                    .find(|task| task.session.as_deref() == Some(session))
                    .map(|task| task.id)
            })
    }

    /// The task a question belongs to: the one it was asked for, or (asked
    /// before tasks existed) its session's.
    pub fn task_of_question(&self, question: &Question) -> Option<String> {
        question
            .task
            .clone()
            .or_else(|| self.task_of_session(&question.session))
    }

    fn new_task(&self, agent: &str, origin: &str, body: &str, session: &str) -> String {
        let origin = match origin.split_once(':') {
            Some(("agent", _)) => origin.to_string(),
            _ => "user".to_string(),
        };
        let task = TaskRecord {
            id: self.new_id("task"),
            agent: agent.to_string(),
            title: title_from(body).unwrap_or_default(),
            instruction: body.to_string(),
            origin,
            session: Some(session.to_string()),
            created_at: now(),
            closed: None,
        };
        if let Err(error) = self.store.record_task(&task) {
            jucode_agent_core::log_warn!("daemon", "task not saved", error = error.to_string());
        }
        task.id
    }

    /// `message` is about to go into `session` of its agent: it is a run of
    /// the scheduled task that sent it, or of the session's task (a new one
    /// for a new session). Recorded before the message goes, so the session
    /// taking it finds the run waiting.
    pub(crate) fn queue_run(&self, message: &Message, session: &str) -> Option<String> {
        if message.to == crate::dispatch::AGENT {
            return None;
        }
        let existing = self.task_of_session(session);
        let task = match message.from.strip_prefix("schedule:") {
            Some(schedule) => schedule.to_string(),
            None => existing.clone().unwrap_or_else(|| {
                self.new_task(&message.to, &message.from, &message.body, session)
            }),
        };
        let run = Run {
            id: self.new_id("run"),
            agent: message.to.clone(),
            task,
            session: Some(session.to_string()),
            trigger: trigger(&message.from, existing.is_some()).to_string(),
            message: Some(message.id.clone()),
            status: "queued".to_string(),
            started_at: now(),
            ended_at: None,
            outcome: None,
        };
        match self.store.record_run(&run) {
            Ok(()) => {
                self.broadcast_tasks(&run.agent);
                Some(run.id)
            }
            Err(error) => {
                jucode_agent_core::log_warn!("daemon", "run not saved", error = error.to_string());
                None
            }
        }
    }

    /// The message of run `id` never reached its session.
    pub(crate) fn discard_run(&self, id: &str) {
        let _ = self.store.discard_run(id);
    }

    /// `session` took a message: its oldest queued run starts. A message
    /// the user typed into the session directly is a run of its own (of a
    /// new task, for a session that has none).
    pub(crate) fn run_took_message(&self, session: &str, content: &str) {
        let Some(agent) = self.session_agent(session) else {
            return;
        };
        let runs = self.store.runs();
        if let Some(run) = runs
            .iter()
            .find(|run| run.session.as_deref() == Some(session) && run.status == "queued")
        {
            let _ = self.store.record_run_running(&run.id);
            self.broadcast_tasks(&agent);
            return;
        }
        let existing = self.task_of_session(session);
        let task = existing
            .clone()
            .unwrap_or_else(|| self.new_task(&agent, "user", content, session));
        let run = Run {
            id: self.new_id("run"),
            agent: agent.clone(),
            task,
            session: Some(session.to_string()),
            trigger: trigger("user", existing.is_some()).to_string(),
            message: None,
            status: "running".to_string(),
            started_at: now(),
            ended_at: None,
            outcome: None,
        };
        if self.store.record_run(&run).is_ok() {
            self.broadcast_tasks(&agent);
        }
    }

    /// A turn of `session` ended: its oldest running run ends with it. Its
    /// outcome, when the agent gave none, is inferred now (the handoff note
    /// fills in what it says later). Returns the run and whether the agent
    /// concluded it itself.
    pub(crate) fn end_run(&self, session: &str, failed: bool) -> Option<(String, bool)> {
        let agent = self.session_agent(session)?;
        let run = self
            .store
            .runs()
            .into_iter()
            .find(|run| run.session.as_deref() == Some(session) && run.status == "running")?;
        let status = if failed { "failed" } else { "succeeded" };
        let _ = self.store.record_run_ended(&run.id, status);
        let by_agent = run.outcome.as_ref().is_some_and(|o| o.source == "agent");
        let verdict = if failed {
            let mut outcome = run
                .outcome
                .clone()
                .unwrap_or_else(|| inferred("failed", ""));
            outcome.verdict = "failed".to_string();
            let _ = self.store.record_outcome(&run.id, &outcome);
            "failed".to_string()
        } else if by_agent {
            run.outcome
                .as_ref()
                .map(|o| o.verdict.clone())
                .unwrap_or_default()
        } else {
            let verdict = if self.run_left_items(&run) {
                "needs_you"
            } else {
                "done"
            };
            let _ = self.store.record_outcome(&run.id, &inferred(verdict, ""));
            verdict.to_string()
        };
        // The user hears of a run only when it needs them or failed; a
        // turn that ended on an error is already told (`report_failed_turn`).
        if verdict == "needs_you" || (verdict == "failed" && !failed) {
            self.notify_run(&run, &verdict);
        }
        self.broadcast_tasks(&agent);
        Some((run.id, by_agent))
    }

    /// The handoff note written after run `id` says what it concluded.
    pub(crate) fn note_run_handoff(&self, id: &str, note: &str) {
        let Some(run) = self.store.runs().into_iter().find(|run| run.id == id) else {
            return;
        };
        let mut outcome = run.outcome.unwrap_or_else(|| inferred("done", ""));
        if outcome.source == "agent" {
            return;
        }
        outcome.summary = note.to_string();
        outcome.next = note.to_string();
        if self.store.record_outcome(id, &outcome).is_ok() {
            self.broadcast_tasks(&run.agent);
        }
    }

    /// The agent's `finish`: what the run working in `session` concluded.
    pub fn finish_run(
        &self,
        session: &str,
        verdict: &str,
        summary: &str,
        next: &str,
        details: &str,
    ) -> Result<String, String> {
        if !VERDICTS.contains(&verdict) {
            return Err(format!("verdict must be one of {}", VERDICTS.join(", ")));
        }
        let run = self
            .store
            .runs()
            .into_iter()
            .find(|run| run.session.as_deref() == Some(session) && run.status == "running")
            .ok_or("no run is working in this conversation")?;
        let outcome = Outcome {
            verdict: verdict.to_string(),
            summary: summary.trim().to_string(),
            next: next.trim().to_string(),
            source: "agent".to_string(),
            at: now(),
        };
        self.store
            .record_outcome(&run.id, &outcome)
            .map_err(|error| error.to_string())?;
        // A longer write-up stays readable as a report of the run.
        if !details.trim().is_empty() {
            let title = self
                .task_title(&run.task)
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| summary.lines().next().unwrap_or_default().to_string());
            self.post_report(&crate::store::Report {
                id: self.new_id("r"),
                agent: run.agent.clone(),
                session: session.to_string(),
                title,
                body: details.trim().to_string(),
                at: now(),
                read: false,
            })?;
        }
        self.broadcast_tasks(&run.agent);
        Ok(run.id)
    }

    /// Tells the user's devices that a run needs them or failed.
    fn notify_run(&self, run: &Run, verdict: &str) {
        let title = self.task_title(&run.task).unwrap_or_default();
        let summary = self
            .store
            .runs()
            .into_iter()
            .find(|r| r.id == run.id)
            .and_then(|r| r.outcome)
            .map(|o| o.summary)
            .filter(|summary| !summary.is_empty());
        // Inferred: what it asked is what the user needs to see.
        let body = summary.unwrap_or_else(|| {
            self.store
                .open_questions()
                .iter()
                .filter(|q| Some(q.session.as_str()) == run.session.as_deref())
                .map(|q| q.title.clone())
                .collect::<Vec<_>>()
                .join("\n")
        });
        let head = if verdict == "failed" {
            "失败"
        } else {
            "需要你"
        };
        self.notify(&format!("{head}：{title}"), &body, &run.id);
    }

    /// A task's title as the user sees it (its schedule's name, or its
    /// session's title).
    pub(crate) fn task_title(&self, id: &str) -> Option<String> {
        if let Some(name) = lock(&self.schedules)
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
        {
            return Some(name);
        }
        let task = self.store.tasks().into_iter().find(|task| task.id == id)?;
        let session_title = task.session.as_deref().and_then(|session| {
            self.store
                .sessions()
                .into_iter()
                .find(|record| record.id == session)
                .and_then(|record| record.title)
        });
        Some(session_title.unwrap_or(task.title))
    }

    /// Stops a working run: its turn is interrupted and it ends `cancelled`
    /// (by the user) or `failed` (it ran past `RUN_TIMEOUT_MS`).
    pub fn cancel_run(&self, id: &str, timed_out: bool) -> Result<(), String> {
        let run = self
            .store
            .runs()
            .into_iter()
            .find(|run| run.id == id)
            .ok_or_else(|| format!("unknown run {id}"))?;
        if run.status != "running" {
            return Err(format!("run {id} is not working"));
        }
        let session = run.session.clone().ok_or("the run has no session")?;
        let _ = self.forward(&session, json!({ "op": "interrupt" }));
        let status = if timed_out { "failed" } else { "cancelled" };
        self.store
            .record_run_ended(id, status)
            .map_err(|error| error.to_string())?;
        if timed_out {
            let summary = format!("运行超过 {} 小时，已停止。", RUN_TIMEOUT_MS / 3_600_000);
            let mut outcome = run
                .outcome
                .clone()
                .unwrap_or_else(|| inferred("failed", &summary));
            outcome.verdict = "failed".to_string();
            if outcome.summary.is_empty() {
                outcome.summary = summary;
            }
            let _ = self.store.record_outcome(id, &outcome);
            self.notify_run(&run, "failed");
        }
        self.broadcast_tasks(&run.agent);
        Ok(())
    }

    /// Once a minute: runs working past `RUN_TIMEOUT_MS` are stopped, and
    /// agent sessions idle (no watcher, nothing running) for `IDLE_CLOSE_MS`
    /// since their last run have their engines closed.
    pub(crate) fn sweep(&self) {
        let at = now();
        let last = LAST_SWEEP.load(Ordering::Relaxed);
        if at.saturating_sub(last) < SWEEP_EVERY_MS
            || LAST_SWEEP
                .compare_exchange(last, at, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let runs = self.store.runs();
        for run in &runs {
            if run.status == "running" && at.saturating_sub(run.started_at) > RUN_TIMEOUT_MS {
                let _ = self.cancel_run(&run.id, true);
            }
        }
        for session in self.idle_agent_sessions() {
            let mine: Vec<&Run> = runs
                .iter()
                .filter(|run| run.session.as_deref() == Some(session.as_str()))
                .collect();
            let settled = !mine.is_empty() && mine.iter().all(|run| run.ended());
            let ended = mine
                .iter()
                .filter_map(|run| run.ended_at)
                .max()
                .unwrap_or(0);
            if settled && at.saturating_sub(ended) > IDLE_CLOSE_MS {
                let _ = self.close_session(&session);
            }
        }
    }

    /// The runs of `session` still open when its engine stopped (closed,
    /// crashed, or the daemon restarted) were interrupted.
    pub(crate) fn interrupt_runs(&self, session: Option<&str>) {
        let mut agents: Vec<String> = Vec::new();
        for run in self.store.runs() {
            if run.ended() || run.status == "skipped" {
                continue;
            }
            if session.is_some_and(|session| run.session.as_deref() != Some(session)) {
                continue;
            }
            // Queued for a session that is gone: its message was delivered,
            // so it will not come again.
            let _ = self.store.record_run_ended(&run.id, "interrupted");
            // The daemon went down under it: nobody chose to stop it.
            if session.is_none() && run.status == "running" {
                let title = self.task_title(&run.task).unwrap_or_default();
                self.notify(
                    &format!("运行中断：{title}"),
                    "后台服务重启，这次运行没有完成。",
                    &run.id,
                );
            }
            if !agents.contains(&run.agent) {
                agents.push(run.agent.clone());
            }
        }
        for agent in &agents {
            self.broadcast_tasks(agent);
        }
    }

    /// A scheduled task came due while its last run was still going: this
    /// time is recorded as skipped. Called with the schedules locked, so the
    /// caller broadcasts.
    pub(crate) fn skip_run(&self, agent: &str, schedule: &str) {
        let at = now();
        let run = Run {
            id: self.new_id("run"),
            agent: agent.to_string(),
            task: schedule.to_string(),
            session: None,
            trigger: "schedule".to_string(),
            message: None,
            status: "skipped".to_string(),
            started_at: at,
            ended_at: Some(at),
            outcome: None,
        };
        if let Err(error) = self.store.record_run(&run) {
            jucode_agent_core::log_warn!("daemon", "run not saved", error = error.to_string());
        }
    }

    /// A run of scheduled task `id` is waiting or working.
    pub(crate) fn schedule_running(&self, id: &str) -> bool {
        let from = format!("schedule:{id}");
        self.store
            .pending_messages()
            .iter()
            .any(|message| message.from == from)
            || self
                .store
                .runs()
                .iter()
                .any(|run| run.task == id && !run.ended() && run.status != "skipped")
    }

    /// What a new run of scheduled task `id` is told: what the last run
    /// left for it, and the questions still waiting for the user.
    pub(crate) fn schedule_context(&self, id: &str) -> Option<(u64, String)> {
        let last = self
            .store
            .runs()
            .into_iter()
            .rev()
            .filter(|run| run.task == id)
            .find_map(|run| {
                let next = run.outcome?.next;
                (!next.trim().is_empty()).then_some((run.started_at, next))
            });
        let questions: Vec<String> = self
            .store
            .open_questions()
            .into_iter()
            .filter(|question| self.task_of_question(question).as_deref() == Some(id))
            .map(|question| format!("- [{}] {}", question.id, question.title))
            .collect();
        let mut text = String::new();
        let mut at = 0;
        if let Some((started, next)) = last {
            at = started;
            text.push_str(&next);
        }
        if !questions.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!(
                "仍在等用户回答的问题（不要重复提问；不再需要的用 open_items 关闭）：\n{}",
                questions.join("\n")
            ));
        }
        (!text.is_empty()).then_some((at, text))
    }

    /// The open question of `agent`'s `task` that `question` asks again:
    /// the same key, or (without one) the same title.
    pub(crate) fn same_question(
        &self,
        task: Option<&str>,
        question: &Question,
    ) -> Option<Question> {
        let normal = |text: &str| text.trim().to_lowercase();
        self.store.open_questions().into_iter().find(|open| {
            open.agent == question.agent
                && self.task_of_question(open).as_deref() == task
                && match (&question.key, &open.key) {
                    (Some(key), Some(other)) => key == other,
                    (Some(_), None) | (None, _) => normal(&open.title) == normal(&question.title),
                }
        })
    }

    /// Open questions of `agent`'s `task`.
    pub(crate) fn open_questions_of_task(&self, agent: &str, task: &str) -> usize {
        self.store
            .open_questions()
            .iter()
            .filter(|q| q.agent == agent && self.task_of_question(q).as_deref() == Some(task))
            .count()
    }

    /// Whether run `run` left something for the user: a question asked or
    /// an action deferred in its session while it worked, still open.
    fn run_left_items(&self, run: &Run) -> bool {
        let Some(session) = run.session.as_deref() else {
            return false;
        };
        self.store
            .open_questions()
            .iter()
            .any(|q| q.session == session && q.asked_at >= run.started_at)
            || self
                .store
                .open_actions()
                .iter()
                .any(|a| a.session_id == session && a.created_at >= run.started_at)
    }

    pub(crate) fn session_agent(&self, session: &str) -> Option<String> {
        self.store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
            .and_then(|record| record.agent)
            .filter(|agent| agent != crate::dispatch::AGENT)
    }

    /// Closes a task; a scheduled one is switched off too.
    pub fn close_task(&self, id: &str, by: &str, reason: &str) -> Result<(), String> {
        let agent = self
            .task_agent(id)
            .ok_or_else(|| format!("unknown task {id}"))?;
        if !self
            .store
            .close_task(id, by, reason.trim())
            .map_err(|error| error.to_string())?
        {
            return Err(format!("task {id} is already closed"));
        }
        if self.schedule_agent(id).is_some() {
            self.save_schedule(&json!({ "id": id, "enabled": false }))?;
        }
        self.broadcast_tasks(&agent);
        Ok(())
    }

    pub fn reopen_task(&self, id: &str) -> Result<(), String> {
        let agent = self
            .task_agent(id)
            .ok_or_else(|| format!("unknown task {id}"))?;
        if !self
            .store
            .reopen_task(id)
            .map_err(|error| error.to_string())?
        {
            return Err(format!("task {id} is not closed"));
        }
        self.broadcast_tasks(&agent);
        Ok(())
    }

    fn schedule_agent(&self, id: &str) -> Option<String> {
        lock(&self.schedules)
            .iter()
            .find(|schedule| schedule.id == id)
            .map(|schedule| schedule.agent.clone())
    }

    fn task_agent(&self, id: &str) -> Option<String> {
        self.schedule_agent(id).or_else(|| {
            self.store
                .tasks()
                .into_iter()
                .find(|task| task.id == id)
                .map(|task| task.agent)
        })
    }

    /// The session a message to task `id` continues: a task's own, or the
    /// latest run's of a scheduled one.
    pub fn task_session(&self, id: &str) -> Result<(String, String), String> {
        let agent = self
            .task_agent(id)
            .ok_or_else(|| format!("unknown task {id}"))?;
        let session = self
            .store
            .tasks()
            .into_iter()
            .find(|task| task.id == id)
            .and_then(|task| task.session)
            .or_else(|| {
                self.store
                    .runs()
                    .into_iter()
                    .rev()
                    .find(|run| run.task == id && run.session.is_some())
                    .and_then(|run| run.session)
            })
            .ok_or_else(|| format!("task {id} has not run yet"))?;
        Ok((agent, session))
    }

    /// `agent`'s tasks (every agent's when None), most recently active first.
    pub fn tasks_json(&self, agent: Option<&str>) -> Value {
        let runs = self.store.runs();
        let mut by_task: HashMap<&str, Vec<&Run>> = HashMap::new();
        for run in &runs {
            by_task.entry(run.task.as_str()).or_default().push(run);
        }
        let closures = self.store.task_closures();
        let questions = self.store.open_questions();
        let mut asked: HashMap<String, usize> = HashMap::new();
        for question in &questions {
            if let Some(task) = self.task_of_question(question) {
                *asked.entry(task).or_default() += 1;
            }
        }
        let actions = self.store.open_actions();
        let sessions = self.store.sessions();
        let timers = self.store.active_timers();
        let mut list: Vec<(u64, Value)> = Vec::new();
        let schedules = lock(&self.schedules).clone();
        for schedule in schedules
            .iter()
            .filter(|s| agent.is_none_or(|a| s.agent == a))
        {
            let runs = by_task
                .get(schedule.id.as_str())
                .cloned()
                .unwrap_or_default();
            let view = TaskView {
                id: &schedule.id,
                agent: &schedule.agent,
                title: &schedule.name,
                instruction: &schedule.prompt,
                origin: if schedule.by_agent { "agent" } else { "user" },
                created_at: schedule.created_at * 1000,
                session: None,
                runs: &runs,
                closed: closures.get(&schedule.id),
                open_questions: asked.get(&schedule.id).copied().unwrap_or_default(),
                open_actions: 0,
                waiting_until: schedule
                    .next_run_at
                    .filter(|_| schedule.enabled)
                    .map(|at| at * 1000),
                trigger: json!({
                    "kind": if schedule.repeat == "once" { "once" } else { "repeat" },
                    "repeat": schedule.repeat, "time": schedule.time, "days": schedule.days,
                    "date": schedule.date, "enabled": schedule.enabled,
                    "next_run_at": schedule.next_run_at.map(|at| at * 1000),
                }),
                paused: !schedule.enabled,
            };
            list.push(view.json());
        }
        for task in self
            .store
            .tasks()
            .iter()
            .filter(|t| agent.is_none_or(|a| t.agent == a))
        {
            let runs = by_task.get(task.id.as_str()).cloned().unwrap_or_default();
            let session = task.session.as_deref();
            // The session's title (the title model's, or the user's) once
            // it has one; the first line of the instruction until then.
            let title = session
                .and_then(|id| sessions.iter().find(|record| record.id == id))
                .and_then(|record| record.title.clone())
                .unwrap_or_else(|| task.title.clone());
            let timer = timers
                .iter()
                .filter(|timer| timer.session.is_some() && timer.session.as_deref() == session)
                .map(|timer| timer.fire_at)
                .min();
            let view = TaskView {
                id: &task.id,
                agent: &task.agent,
                title: &title,
                instruction: &task.instruction,
                origin: &task.origin,
                created_at: task.created_at,
                session,
                runs: &runs,
                closed: task.closed.as_ref(),
                open_questions: asked.get(&task.id).copied().unwrap_or_default(),
                open_actions: actions
                    .iter()
                    .filter(|action| Some(action.session_id.as_str()) == session)
                    .count(),
                waiting_until: timer,
                trigger: match timer {
                    Some(at) => json!({ "kind": "once", "at": at, "enabled": true }),
                    None => json!({ "kind": "manual" }),
                },
                paused: false,
            };
            list.push(view.json());
        }
        list.sort_by_key(|(latest, _)| std::cmp::Reverse(*latest));
        json!({
            "type": "tasks",
            "agent": agent,
            "tasks": list.into_iter().map(|(_, task)| task).collect::<Vec<_>>(),
        })
    }

    /// A task with its runs (newest first, at most `limit`) and its open
    /// questions.
    pub fn task_json(&self, id: &str, limit: usize) -> Result<Value, String> {
        let agent = self
            .task_agent(id)
            .ok_or_else(|| format!("unknown task {id}"))?;
        let task = self.tasks_json(Some(&agent))["tasks"]
            .as_array()
            .and_then(|tasks| tasks.iter().find(|task| task["id"] == id).cloned())
            .ok_or_else(|| format!("unknown task {id}"))?;
        let runs: Vec<Value> = self
            .store
            .runs()
            .iter()
            .rev()
            .filter(|run| run.task == id)
            .take(limit)
            .map(run_json)
            .collect();
        let questions: Vec<Value> = self
            .store
            .open_questions()
            .iter()
            .filter(|question| self.task_of_question(question).as_deref() == Some(id))
            .map(|question| json!({ "id": question.id, "title": question.title, "session": question.session, "asked_at": question.asked_at }))
            .collect();
        Ok(json!({ "type": "task", "task": task, "runs": runs, "questions": questions }))
    }

    /// What `agent`'s latest runs of other tasks than `session`'s left for
    /// the next run, newest first, one per task.
    pub fn recent_notes(&self, agent: &str, session: &str) -> Vec<crate::agents::RecentNote> {
        let current = self.task_of_session(session);
        let mut seen: Vec<String> = Vec::new();
        let mut notes = Vec::new();
        for run in self.store.runs().into_iter().rev() {
            if notes.len() == NOTES_IN_PROMPT {
                break;
            }
            if run.agent != agent || Some(&run.task) == current.as_ref() || seen.contains(&run.task)
            {
                continue;
            }
            let Some(next) = run
                .outcome
                .map(|o| o.next)
                .filter(|next| !next.trim().is_empty())
            else {
                continue;
            };
            seen.push(run.task.clone());
            notes.push(crate::agents::RecentNote {
                title: self.task_title(&run.task).unwrap_or_default(),
                task: run.task,
                text: next,
            });
        }
        notes
    }

    /// `handoff_list` for older clients: each session's latest note.
    pub fn handoffs_json(&self, agent: &str) -> Value {
        let mut seen: Vec<String> = Vec::new();
        let mut list = Vec::new();
        for run in self.store.runs().into_iter().rev() {
            let (Some(session), Some(outcome)) = (run.session.clone(), run.outcome.clone()) else {
                continue;
            };
            if run.agent != agent || outcome.next.is_empty() || seen.contains(&session) {
                continue;
            }
            seen.push(session.clone());
            list.push(json!({
                "session": session, "title": self.task_title(&run.task).unwrap_or_default(),
                "at": outcome.at, "text": outcome.next,
            }));
        }
        json!({ "type": "handoffs", "agent": agent, "handoffs": list })
    }

    pub(crate) fn broadcast_tasks(&self, agent: &str) {
        self.broadcast(&self.tasks_json(Some(agent)));
    }

    /// Once: the agent sessions before tasks existed become tasks and runs,
    /// those not archived and those from the last 30 days. A session from a
    /// schedule is a run of that schedule; any other is a task of its own.
    /// Each session is one run, concluded with its handoff note.
    pub fn migrate_tasks(&self) {
        if self.store.setting(MIGRATED) == true {
            return;
        }
        let since = now().saturating_sub(MIGRATE_SINCE_MS);
        let messages = self.store.message_log(None, usize::MAX);
        let schedules: Vec<String> = lock(&self.schedules).iter().map(|s| s.id.clone()).collect();
        let done: Vec<String> = self
            .store
            .runs()
            .into_iter()
            .filter_map(|run| run.session)
            .collect();
        for record in self.store.sessions() {
            let Some(agent) = record.agent.clone() else {
                continue;
            };
            if agent == crate::dispatch::AGENT
                || done.contains(&record.id)
                || (record.archived && record.created_at < since)
                || self.agents.get(&agent).is_none()
            {
                continue;
            }
            // message_log is newest first: the last one is the first message.
            let first = messages
                .iter()
                .rev()
                .find(|message| message["session"] == record.id.as_str());
            let from = first
                .and_then(|message| message["from"].as_str())
                .unwrap_or("user");
            let body = first
                .and_then(|message| message["body"].as_str())
                .unwrap_or_default();
            let task = match from.strip_prefix("schedule:") {
                Some(schedule) if schedules.iter().any(|id| id == schedule) => schedule.to_string(),
                _ => {
                    let task = TaskRecord {
                        id: self.new_id("task"),
                        agent: agent.clone(),
                        title: record
                            .title
                            .clone()
                            .or_else(|| title_from(body))
                            .unwrap_or_default(),
                        instruction: body.to_string(),
                        origin: if from.starts_with("agent:") {
                            from.to_string()
                        } else {
                            "user".to_string()
                        },
                        session: Some(record.id.clone()),
                        created_at: record.created_at,
                        closed: None,
                    };
                    if self.store.record_task(&task).is_err() {
                        continue;
                    }
                    task.id
                }
            };
            let note = self.agents.handoff(&agent, &record.id);
            let run = Run {
                id: self.new_id("run"),
                agent,
                task,
                session: Some(record.id.clone()),
                trigger: trigger(from, false).to_string(),
                message: first.and_then(|m| m["id"].as_str()).map(str::to_string),
                status: "succeeded".to_string(),
                started_at: record.created_at,
                ended_at: Some(record.created_at),
                outcome: note.map(|note| Outcome {
                    verdict: "done".to_string(),
                    summary: note.clone(),
                    next: note,
                    source: "inferred".to_string(),
                    at: record.created_at,
                }),
            };
            let _ = self.store.record_run(&run);
        }
        if let Err(error) = self.store.set_setting(MIGRATED, json!(true)) {
            jucode_agent_core::log_warn!(
                "daemon",
                "task migration not marked",
                error = error.to_string()
            );
        }
    }
}

fn inferred(verdict: &str, note: &str) -> Outcome {
    Outcome {
        verdict: verdict.to_string(),
        summary: note.to_string(),
        next: note.to_string(),
        source: "inferred".to_string(),
        at: now(),
    }
}

pub fn run_json(run: &Run) -> Value {
    json!({
        "id": run.id,
        "agent": run.agent,
        "task": run.task,
        "session": run.session,
        "trigger": run.trigger,
        "message": run.message,
        "status": run.status,
        "started_at": run.started_at,
        "ended_at": run.ended_at,
        "outcome": run.outcome.as_ref().map(|o| json!({
            "verdict": o.verdict, "summary": o.summary, "next": o.next,
            "source": o.source, "at": o.at,
        })),
    })
}

struct TaskView<'a> {
    id: &'a str,
    agent: &'a str,
    title: &'a str,
    instruction: &'a str,
    origin: &'a str,
    created_at: u64,
    session: Option<&'a str>,
    /// Oldest first.
    runs: &'a [&'a Run],
    closed: Option<&'a crate::store::Closure>,
    open_questions: usize,
    open_actions: usize,
    /// The next time it runs by itself (ms).
    waiting_until: Option<u64>,
    trigger: Value,
    /// A scheduled task switched off.
    paused: bool,
}

impl TaskView<'_> {
    /// (when it was last active, its JSON).
    fn json(&self) -> (u64, Value) {
        let latest = self
            .runs
            .iter()
            .rev()
            .find(|run| run.status != "skipped")
            .copied();
        let working = self
            .runs
            .iter()
            .any(|run| !run.ended() && run.status != "skipped");
        let verdict = latest
            .and_then(|run| run.outcome.as_ref())
            .map(|o| o.verdict.as_str());
        let state = if self.closed.is_some() {
            "closed"
        } else if working {
            "running"
        } else if self.open_questions > 0
            || self.open_actions > 0
            || matches!(verdict, Some("needs_you" | "failed"))
            || latest.is_some_and(|run| run.status == "interrupted")
        {
            "needs_you"
        } else if self.paused {
            "paused"
        } else if self.waiting_until.is_some() {
            "waiting"
        } else {
            "done"
        };
        let active = latest
            .map(|run| run.ended_at.unwrap_or(run.started_at))
            .unwrap_or(self.created_at);
        // Runs since the last one that had something to say.
        let quiet = self
            .runs
            .iter()
            .rev()
            .filter(|run| run.status != "skipped")
            .take_while(|run| run.outcome.as_ref().is_some_and(|o| o.verdict == "quiet"))
            .count();
        let mut value = json!({
            "id": self.id,
            "agent": self.agent,
            "title": self.title,
            "instruction": self.instruction,
            "origin": self.origin,
            "created_at": self.created_at,
            "session": self.session,
            "trigger": self.trigger,
            "state": state,
            "runs": self.runs.iter().filter(|run| run.status != "skipped").count(),
            "quiet_runs": quiet,
            "open_questions": self.open_questions,
            "open_actions": self.open_actions,
            "next_at": self.waiting_until,
            "latest_run": latest.map(run_json),
            "updated_at": active,
        });
        if let Some(closure) = self.closed {
            value["closed_by"] = json!(closure.by);
            value["closed_reason"] = json!(closure.reason);
            value["closed_at"] = json!(closure.at);
        }
        (active, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{store::Store, Agents};
    use std::{fs, path::PathBuf, sync::Arc};

    fn hub(label: &str) -> (Arc<Hub>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "jucode-tasks-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let agents = Agents::open(dir.join("agents")).unwrap();
        agents
            .create("ops", "ops", &dir, "Keeps things running", &Value::Null)
            .unwrap();
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            agents,
            "test",
            None,
        );
        (hub, dir)
    }

    fn message(hub: &Hub, id: &str, from: &str, body: &str, session: &str) {
        hub.store
            .record_message(&Message {
                id: id.into(),
                to: "ops".into(),
                from: from.into(),
                body: body.into(),
                session: None,
                reply_to: None,
                dedupe_key: None,
                at: now(),
            })
            .unwrap();
        hub.store.record_delivered(id, session).unwrap();
    }

    fn question(id: &str, session: &str, title: &str, key: Option<&str>) -> Question {
        Question {
            id: id.into(),
            agent: "ops".into(),
            session: session.into(),
            title: title.into(),
            body: String::new(),
            assumption: String::new(),
            default_action: String::new(),
            importance: "normal".into(),
            due_at: None,
            asked_at: now(),
            task: None,
            key: key.map(str::to_string),
        }
    }

    #[test]
    fn a_run_goes_queued_running_ended_and_infers_what_it_left() {
        let (hub, dir) = hub("lifecycle");
        hub.store
            .record_engine_session("s1", &dir, Some("ops"), None, false)
            .unwrap();
        let sent = Message {
            id: "m1".into(),
            to: "ops".into(),
            from: "user".into(),
            body: "修复登录\n细节".into(),
            session: None,
            reply_to: None,
            dedupe_key: None,
            at: now(),
        };
        let run = hub.queue_run(&sent, "s1").unwrap();
        let task = hub.task_of_session("s1").unwrap();
        assert_eq!(hub.store.tasks()[0].title, "修复登录");
        hub.run_took_message("s1", "修复登录");
        assert_eq!(hub.store.runs()[0].status, "running");
        hub.ask(&question("q1", "s1", "Which env?", None)).unwrap();
        assert_eq!(hub.end_run("s1", false), Some((run.clone(), false)));
        let ended = &hub.store.runs()[0];
        assert_eq!(ended.status, "succeeded");
        assert_eq!(ended.outcome.as_ref().unwrap().verdict, "needs_you");
        // The handoff note fills in what it says, keeping the verdict.
        hub.note_run_handoff(&run, "asked which env");
        let outcome = hub.store.runs()[0].outcome.clone().unwrap();
        assert_eq!(
            (outcome.verdict.as_str(), outcome.summary.as_str()),
            ("needs_you", "asked which env")
        );

        // Typed into the session directly: a run of the same task.
        hub.run_took_message("s1", "also check logout");
        let runs = hub.store.runs();
        assert_eq!(
            (runs.len(), runs[1].task.as_str(), runs[1].trigger.as_str()),
            (2, task.as_str(), "reply")
        );
        hub.finish_run("s1", "done", "fixed", "", "root cause: a stale cookie")
            .unwrap();
        let reports = hub.store.reports(5);
        assert_eq!(
            (reports[0].title.as_str(), reports[0].body.as_str()),
            ("修复登录", "root cause: a stale cookie")
        );
        assert_eq!(
            hub.end_run("s1", false).map(|(_, by_agent)| by_agent),
            Some(true)
        );
        assert!(
            hub.finish_run("s1", "done", "x", "", "").is_err(),
            "nothing is running"
        );
        assert!(hub.finish_run("s1", "great", "x", "", "").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn runs_left_open_are_interrupted_and_a_due_schedule_skips_a_busy_one() {
        let (hub, dir) = hub("interrupt");
        hub.store
            .record_engine_session("s1", &dir, Some("ops"), None, false)
            .unwrap();
        let sent = Message {
            id: "m1".into(),
            to: "ops".into(),
            from: "schedule:sch-1".into(),
            body: "定时任务「巡检」：\ncheck".into(),
            session: None,
            reply_to: None,
            dedupe_key: None,
            at: now(),
        };
        hub.queue_run(&sent, "s1").unwrap();
        hub.run_took_message("s1", "check");
        assert!(hub.schedule_running("sch-1"));
        hub.skip_run("ops", "sch-1");
        // The daemon restarted: what was working is interrupted.
        hub.interrupt_runs(None);
        let runs = hub.store.runs();
        assert_eq!(runs[0].status, "interrupted");
        assert_eq!(runs[1].status, "skipped");
        assert!(!hub.schedule_running("sch-1"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_scheduled_task_carries_its_open_questions_into_the_next_run() {
        let (hub, dir) = hub("carry");
        for (id, session) in [("m1", "run1"), ("m2", "run2")] {
            hub.store
                .record_engine_session(session, &dir, Some("ops"), None, false)
                .unwrap();
            let sent = Message {
                id: id.into(),
                to: "ops".into(),
                from: "schedule:sch-1".into(),
                body: "check".into(),
                session: None,
                reply_to: None,
                dedupe_key: None,
                at: now(),
            };
            hub.queue_run(&sent, session).unwrap();
            hub.run_took_message(session, "check");
        }
        let mut asked = question("q1", "run1", "Refund order 12?", Some("refund"));
        asked.task = Some("sch-1".into());
        hub.ask(&asked).unwrap();
        hub.finish_run("run1", "needs_you", "asked", "look at order 12 again", "")
            .unwrap();
        hub.end_run("run1", false);

        // The second run asks it again by key: still one question, now its.
        let again = question("q2", "run2", "Refund order 12 (still)?", Some("refund"));
        let open = hub.same_question(Some("sch-1"), &again).unwrap();
        assert_eq!(open.id, "q1");
        let by_title = question("q3", "run2", " refund order 12? ", None);
        assert_eq!(
            hub.same_question(Some("sch-1"), &by_title)
                .map(|q| q.id)
                .as_deref(),
            Some("q1")
        );
        assert_eq!(hub.open_questions_of_task("ops", "sch-1"), 1);

        let (_, text) = hub.schedule_context("sch-1").unwrap();
        assert!(text.contains("look at order 12 again"), "{text}");
        assert!(text.contains("[q1] Refund order 12?"), "{text}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_working_run_is_cancelled_or_times_out_and_notes_cross_tasks() {
        let (hub, dir) = hub("cancel");
        for session in ["a", "b"] {
            hub.store
                .record_engine_session(session, &dir, Some("ops"), None, false)
                .unwrap();
        }
        let send = |id: &str, session: &str, body: &str| {
            let sent = Message {
                id: id.into(),
                to: "ops".into(),
                from: "user".into(),
                body: body.into(),
                session: None,
                reply_to: None,
                dedupe_key: None,
                at: now(),
            };
            let run = hub.queue_run(&sent, session).unwrap();
            hub.run_took_message(session, body);
            run
        };
        let first = send("m1", "a", "部署");
        assert!(hub.cancel_run(&first, false).is_ok());
        assert_eq!(hub.store.runs()[0].status, "cancelled");
        assert!(hub.cancel_run(&first, false).is_err(), "no longer working");

        let second = send("m2", "a", "部署");
        hub.cancel_run(&second, true).unwrap();
        let timed_out = hub
            .store
            .runs()
            .into_iter()
            .find(|r| r.id == second)
            .unwrap();
        assert_eq!(timed_out.status, "failed");
        assert!(timed_out.outcome.unwrap().summary.contains("小时"));

        // What task a's runs left reaches task b's prompt, not a's own.
        let third = send("m3", "a", "部署");
        hub.finish_run("a", "done", "deployed", "check the canary tomorrow", "")
            .unwrap();
        hub.end_run("a", false);
        let _ = third;
        send("m4", "b", "对账");
        let notes = hub.recent_notes("ops", "b");
        assert_eq!(notes.len(), 1);
        assert_eq!(
            (notes[0].title.as_str(), notes[0].text.as_str()),
            ("部署", "check the canary tomorrow")
        );
        assert!(hub.recent_notes("ops", "a").is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sessions_from_before_tasks_become_tasks_once() {
        let (hub, dir) = hub("migrate");
        for session in ["old", "mine", "sched", "gone"] {
            hub.store
                .record_engine_session(session, &dir, Some("ops"), None, false)
                .unwrap();
        }
        hub.save_schedule(&json!({
            "agent": "ops", "name": "巡检", "prompt": "check", "repeat": "daily", "time": "09:00",
        }))
        .unwrap();
        let schedule = lock(&hub.schedules)[0].id.clone();
        message(&hub, "m1", "user", "修复登录", "mine");
        message(
            &hub,
            "m2",
            &format!("schedule:{schedule}"),
            "check",
            "sched",
        );
        fs::write(
            dir.join("agents").join("ops").join("handoffs.json"),
            json!([{ "session": "mine", "title": "修复登录", "at": 1, "text": "已修复" }])
                .to_string(),
        )
        .unwrap();
        hub.store
            .record_session_meta("gone", &json!({ "archived": true }))
            .unwrap();

        hub.migrate_tasks();
        hub.migrate_tasks();
        let runs = hub.store.runs();
        // "gone" is archived but recent, so it is kept too.
        assert_eq!(runs.len(), 4, "{runs:?}");
        let sched = runs
            .iter()
            .find(|r| r.session.as_deref() == Some("sched"))
            .unwrap();
        assert_eq!(
            (sched.task.as_str(), sched.trigger.as_str()),
            (schedule.as_str(), "schedule")
        );
        let mine = runs
            .iter()
            .find(|r| r.session.as_deref() == Some("mine"))
            .unwrap();
        assert_eq!(mine.outcome.as_ref().unwrap().summary, "已修复");
        let tasks = hub.store.tasks();
        assert_eq!(
            tasks.len(),
            3,
            "one task per session that is not a schedule's"
        );
        let task = tasks
            .iter()
            .find(|t| t.session.as_deref() == Some("mine"))
            .unwrap();
        assert_eq!(task.instruction, "修复登录");
        let _ = fs::remove_dir_all(dir);
    }
}
