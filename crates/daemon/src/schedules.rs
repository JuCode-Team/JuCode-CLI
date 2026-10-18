//! Scheduled tasks: a prompt sent at set local times to a new session in a
//! directory, on an engine and in a permission mode, saved in
//! `~/.jucode/daemon/schedules.json`. Each run is an ordinary session that
//! records the schedule it ran for; what it did is that session.
//!
//! A due task fires once and its `next_run_at` moves past now, so times
//! missed while the daemon (or the computer) was off fire once on start,
//! never as a burst. A time that comes while the last run is still going (a
//! turn running, an approval waiting) is skipped, not queued. A new run
//! archives the earlier runs' sessions that have nothing left to do.
//!
//! The user hears of a run only when it needs them: it failed, it waits for
//! an approval, it ran past `RUN_TIMEOUT_MS`, or a time was skipped (once
//! in a row). Paired phones get a notification; clients a `schedule_event`.

use crate::{
    engines,
    hub::{lock, Hub},
    store::{now, random_hex, write_private},
};
use chrono::{
    DateTime, Datelike, Days, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone,
    Timelike,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const FILE: &str = "schedules.json";
/// A run working longer than this is interrupted and counted as failed.
pub const RUN_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1000;
/// Permission modes in the clients' names (see `requirements::engine_mode`).
const MODES: [&str; 4] = ["ask", "edits", "auto", "all"];

#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    pub prompt: String,
    /// Where its runs start; None until the user picks one (a task migrated
    /// from an agent whose own folder is gone).
    pub cwd: Option<PathBuf>,
    /// `claude`, `codex` or `acp` (a built-in agent, `options.agent`);
    /// None: jucode.
    pub engine: Option<String>,
    /// `ask`, `edits`, `auto` or `all`.
    pub mode: String,
    /// The engine's start options the desktop saved with it (gateway,
    /// model, …), as `engines::Options::from_json` reads them.
    pub options: Value,
    pub enabled: bool,
    /// `once`, `hourly`, `daily`, `weekdays` or `weekly`.
    pub repeat: String,
    /// Local `HH:MM`; `hourly` uses only the minute.
    pub time: String,
    /// `weekly`: 0 = Sunday … 6 = Saturday.
    pub days: Vec<u8>,
    /// `once`: local `YYYY-MM-DD`.
    pub date: Option<String>,
    pub created_at: u64,
    /// ms.
    pub next_run_at: Option<u64>,
    pub last_run: Option<LastRun>,
    /// Times skipped in a row since the last run started.
    pub skipped: u32,
    /// The agent proposal it was created from (`propose_schedule`), which
    /// the proposal's card looks for.
    pub origin: Option<String>,
}

/// The latest run: when it started, its session and how it is going.
#[derive(Debug, Clone, PartialEq)]
pub struct LastRun {
    pub at: u64,
    pub session: Option<String>,
    /// `running`, `waiting` (an approval), `done` or `failed`.
    pub status: String,
    /// Why it failed.
    pub error: Option<String>,
}

impl Schedule {
    fn new(id: String, created_at: u64) -> Self {
        Self {
            id,
            name: String::new(),
            prompt: String::new(),
            cwd: None,
            engine: None,
            mode: "auto".to_string(),
            options: json!({}),
            enabled: true,
            repeat: String::new(),
            time: String::new(),
            days: Vec::new(),
            date: None,
            created_at,
            next_run_at: None,
            last_run: None,
            skipped: 0,
            origin: None,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "prompt": self.prompt,
            "cwd": self.cwd.as_ref().map(|cwd| cwd.display().to_string()),
            "engine": self.engine.as_deref().unwrap_or("jucode"),
            "mode": self.mode,
            "options": self.options,
            "enabled": self.enabled,
            "repeat": self.repeat,
            "time": self.time,
            "days": self.days,
            "date": self.date,
            "created_at": self.created_at,
            "next_run_at": self.next_run_at,
            "last_run": self.last_run.as_ref().map(|run| json!({
                "at": run.at, "session": run.session, "status": run.status, "error": run.error,
            })),
            "skipped": self.skipped,
            "origin": self.origin,
        })
    }

    /// A schedule as saved; the saved `next_run_at` stays, so one that came
    /// due while the daemon was down still fires.
    pub fn from_json(value: &Value) -> Option<Self> {
        let mut schedule = Self {
            next_run_at: value["next_run_at"].as_u64(),
            last_run: value["last_run"].as_object().map(|run| LastRun {
                at: run.get("at").and_then(Value::as_u64).unwrap_or_default(),
                session: run
                    .get("session")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                status: run
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("done")
                    .to_string(),
                error: run.get("error").and_then(Value::as_str).map(str::to_string),
            }),
            skipped: value["skipped"].as_u64().unwrap_or_default() as u32,
            ..Self::new(
                value["id"].as_str()?.to_string(),
                value["created_at"].as_u64().unwrap_or_default(),
            )
        };
        schedule.apply(value).ok()?;
        Some(schedule)
    }

    /// Takes the editable fields present in `changes`, then checks the
    /// whole schedule.
    fn apply(&mut self, changes: &Value) -> Result<(), String> {
        let text = |key: &str| changes[key].as_str().map(|text| text.trim().to_string());
        if let Some(name) = text("name") {
            self.name = name;
        }
        if let Some(prompt) = text("prompt") {
            self.prompt = prompt;
        }
        match &changes["cwd"] {
            Value::String(cwd) if !cwd.trim().is_empty() => self.cwd = Some(cwd.trim().into()),
            Value::Null => {}
            _ => self.cwd = None,
        }
        if let Some(engine) = text("engine") {
            engines::Kind::parse(&engine)?;
            self.engine = Some(engine).filter(|engine| engine != "jucode" && !engine.is_empty());
        }
        if let Some(mode) = text("mode") {
            self.mode = mode;
        }
        if changes["options"].is_object() {
            self.options = changes["options"].clone();
        }
        if let Some(enabled) = changes["enabled"].as_bool() {
            self.enabled = enabled;
        }
        if let Some(repeat) = text("repeat") {
            self.repeat = repeat;
        }
        if let Some(time) = text("time") {
            self.time = time;
        }
        if let Some(date) = text("date") {
            self.date = Some(date);
        }
        if let Some(origin) = text("origin").filter(|origin| !origin.is_empty()) {
            self.origin = Some(origin);
        }
        if let Some(days) = changes["days"].as_array() {
            let mut parsed = days
                .iter()
                .map(|day| day.as_u64().filter(|day| *day <= 6).map(|day| day as u8))
                .collect::<Option<Vec<u8>>>()
                .ok_or("days must be numbers 0-6 (0 is Sunday)")?;
            parsed.sort_unstable();
            parsed.dedup();
            self.days = parsed;
        }
        if self.engine.as_deref() == Some("acp") {
            let options = engines::Options::from_json(&self.options);
            if options.runs_programs() {
                return Err("scheduled ACP tasks run built-in agents only".to_string());
            }
            let agent = options
                .agent
                .ok_or("a scheduled ACP task needs a built-in agent")?;
            if engines::acp_presets::find(&agent).is_none() {
                return Err(format!("unknown ACP agent {agent}"));
            }
        }
        if self.name.is_empty() {
            return Err("schedule name must not be empty".to_string());
        }
        if self.prompt.is_empty() {
            return Err("schedule prompt must not be empty".to_string());
        }
        if !MODES.contains(&self.mode.as_str()) {
            return Err(format!(
                "unknown mode '{}': use {}",
                self.mode,
                MODES.join(", ")
            ));
        }
        if !matches!(
            self.repeat.as_str(),
            "once" | "hourly" | "daily" | "weekdays" | "weekly"
        ) {
            return Err(format!(
                "unknown repeat '{}': use once, hourly, daily, weekdays or weekly",
                self.repeat
            ));
        }
        let time = NaiveTime::parse_from_str(&self.time, "%H:%M")
            .map_err(|_| format!("invalid time '{}': use HH:MM (24-hour)", self.time))?;
        self.time = time.format("%H:%M").to_string();
        if self.repeat == "weekly" && self.days.is_empty() {
            return Err("a weekly schedule needs days (0 is Sunday, 6 is Saturday)".to_string());
        }
        if self.repeat == "once" {
            let date = self
                .date
                .as_deref()
                .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
                .ok_or("a one-off schedule needs date as YYYY-MM-DD")?;
            self.date = Some(date.format("%Y-%m-%d").to_string());
        }
        Ok(())
    }

    fn running(&self) -> bool {
        self.last_run
            .as_ref()
            .is_some_and(|run| matches!(run.status.as_str(), "running" | "waiting"))
    }
}

/// When `schedule` next runs strictly after `now`, in `now`'s time zone;
/// None when it is disabled or a one-off whose time has passed. A local time
/// skipped by a DST change runs an hour later; one that occurs twice runs at
/// the first.
pub fn next_run<Tz: TimeZone>(schedule: &Schedule, now: &DateTime<Tz>) -> Option<DateTime<Tz>> {
    if !schedule.enabled {
        return None;
    }
    let time = NaiveTime::parse_from_str(&schedule.time, "%H:%M").ok()?;
    let zone = now.timezone();
    let after_now = |local: NaiveDateTime| {
        zone.from_local_datetime(&local)
            .earliest()
            .or_else(|| {
                zone.from_local_datetime(&(local + TimeDelta::hours(1)))
                    .earliest()
            })
            .filter(|at| at > now)
    };
    let today = now.naive_local().date();
    match schedule.repeat.as_str() {
        "once" => after_now(
            NaiveDate::parse_from_str(schedule.date.as_deref()?, "%Y-%m-%d")
                .ok()?
                .and_time(time),
        ),
        "hourly" => {
            let first = today.and_hms_opt(now.naive_local().hour(), time.minute(), 0)?;
            (0..=26).find_map(|hours| after_now(first + TimeDelta::hours(hours)))
        }
        repeat => (0..=8).find_map(|days| {
            let date = today.checked_add_days(Days::new(days))?;
            let weekday = date.weekday().num_days_from_sunday() as u8;
            let runs = match repeat {
                "daily" => true,
                "weekdays" => (1..=5).contains(&weekday),
                "weekly" => schedule.days.contains(&weekday),
                _ => false,
            };
            runs.then(|| after_now(date.and_time(time))).flatten()
        }),
    }
}

/// `next_run` after `now` (ms) in this machine's time zone.
fn next_run_at(schedule: &Schedule, now: u64) -> Option<u64> {
    let now = Local.timestamp_millis_opt(now as i64).single()?;
    next_run(schedule, &now).map(|at| at.timestamp_millis() as u64)
}

/// The saved schedules. One saved without a next run (new, or migrated)
/// gets one from now.
pub fn load(dir: &Path) -> Vec<Schedule> {
    let saved: Value = fs::read_to_string(dir.join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    saved
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Schedule::from_json)
        .map(|mut schedule| {
            if schedule.next_run_at.is_none() {
                schedule.next_run_at = next_run_at(&schedule, now());
            }
            schedule
        })
        .collect()
}

impl Hub {
    pub fn schedules_json(&self) -> Value {
        let list: Vec<Value> = lock(&self.schedules)
            .iter()
            .map(Schedule::to_json)
            .collect();
        json!({ "type": "schedules", "schedules": list })
    }

    /// Creates a schedule (no `id`) or changes one. `local`: the desktop,
    /// which may set anything; a paired device only switches one on or off.
    pub fn save_schedule(&self, changes: &Value, local: bool) -> Result<Schedule, String> {
        let mut list = lock(&self.schedules);
        let mut schedule = match changes["id"].as_str() {
            Some(id) => list
                .iter()
                .find(|schedule| schedule.id == id)
                .cloned()
                .ok_or_else(|| format!("unknown schedule {id}"))?,
            None if local => {
                let id = random_hex(4).map_err(|error| error.to_string())?;
                Schedule::new(format!("sch-{id}"), now())
            }
            None => return Err("only the desktop creates scheduled tasks".to_string()),
        };
        if local {
            schedule.apply(changes)?;
        } else {
            // What a run starts (a directory, an engine, its options) is the
            // desktop's to choose.
            let enabled = changes["enabled"]
                .as_bool()
                .ok_or("a phone only switches a scheduled task on or off")?;
            schedule.apply(&json!({ "enabled": enabled }))?;
        }
        if schedule.enabled {
            match &schedule.cwd {
                Some(cwd) if cwd.is_dir() => {}
                Some(cwd) => return Err(format!("not a directory: {}", cwd.display())),
                None => return Err("请先选择定时任务运行的目录".to_string()),
            }
        }
        schedule.next_run_at = next_run_at(&schedule, now());
        let mut updated = list.clone();
        match updated.iter_mut().find(|other| other.id == schedule.id) {
            Some(slot) => *slot = schedule.clone(),
            None => updated.push(schedule.clone()),
        }
        self.persist_schedules(&updated)?;
        *list = updated;
        drop(list);
        self.broadcast(&self.schedules_json());
        Ok(schedule)
    }

    /// Deletes a schedule; its runs' sessions stay. Only the desktop
    /// (`local`) deletes one.
    pub fn delete_schedule(&self, id: &str, local: bool) -> Result<(), String> {
        if !local {
            return Err("only the desktop deletes scheduled tasks".to_string());
        }
        let mut list = lock(&self.schedules);
        let mut updated = list.clone();
        updated.retain(|schedule| schedule.id != id);
        if updated.len() == list.len() {
            return Err(format!("unknown schedule {id}"));
        }
        self.persist_schedules(&updated)?;
        *list = updated;
        drop(list);
        self.broadcast(&self.schedules_json());
        Ok(())
    }

    /// Runs a schedule now, enabled or not, without moving its next run.
    pub fn run_schedule(self: &Arc<Self>, id: &str) -> Result<String, String> {
        match self.claim_run(id)? {
            Some(schedule) => self.start_run(&schedule),
            None => Err("上一次运行还没结束".to_string()),
        }
    }

    /// Starts the schedules that are due. A time that comes while the last
    /// run is still going is skipped (told once in a row).
    pub(crate) fn fire_due_schedules(self: &Arc<Self>) {
        let at = now();
        let due: Vec<String> = {
            let mut list = lock(&self.schedules);
            let mut due = Vec::new();
            for schedule in list.iter_mut() {
                if !schedule.enabled || schedule.next_run_at.is_none_or(|next| next > at) {
                    continue;
                }
                schedule.next_run_at = next_run_at(schedule, at);
                if schedule.repeat == "once" {
                    schedule.enabled = false;
                }
                due.push(schedule.id.clone());
            }
            if due.is_empty() {
                return;
            }
            if let Err(error) = self.persist_schedules(&list) {
                jucode_agent_core::log_warn!("daemon", "schedules not saved", error = error);
            }
            due
        };
        for id in due {
            match self.claim_run(&id) {
                Ok(Some(schedule)) => {
                    if let Err(error) = self.start_run(&schedule) {
                        jucode_agent_core::log_warn!(
                            "daemon",
                            "scheduled run not started",
                            error = error
                        );
                    }
                }
                Ok(None) => {
                    let skipped = self.update_schedule(&id, |s| s.skipped += 1);
                    if let Some(schedule) = skipped.filter(|s| s.skipped == 1) {
                        self.tell(&schedule, "skipped", "上一次运行还没结束，这次跳过。");
                    }
                }
                Err(_) => {}
            }
        }
    }

    /// Makes a new run the latest of schedule `id`, unless the last one is
    /// still going (None). Checked and claimed under one lock, so two starts
    /// at once make one run.
    fn claim_run(&self, id: &str) -> Result<Option<Schedule>, String> {
        let mut list = lock(&self.schedules);
        let schedule = list
            .iter_mut()
            .find(|schedule| schedule.id == id)
            .ok_or_else(|| format!("unknown schedule {id}"))?;
        if schedule.running() {
            return Ok(None);
        }
        schedule.last_run = Some(LastRun {
            at: now(),
            session: None,
            status: "running".to_string(),
            error: None,
        });
        schedule.skipped = 0;
        let claimed = schedule.clone();
        if let Err(error) = self.persist_schedules(&list) {
            jucode_agent_core::log_warn!("daemon", "schedules not saved", error = error);
        }
        drop(list);
        self.broadcast(&self.schedules_json());
        Ok(Some(claimed))
    }

    /// The claimed run: a new session in the schedule's directory, on its
    /// engine and in its mode, named after it and given its prompt. The run
    /// names its session before the prompt goes, so the session's first
    /// events find it. Earlier runs with nothing left to do are archived. A
    /// run that cannot start (its directory is gone, say) fails.
    fn start_run(self: &Arc<Self>, schedule: &Schedule) -> Result<String, String> {
        let result = self.open_run(schedule);
        if let Err(error) = &result {
            self.fail_run(&schedule.id, None, error);
        }
        result
    }

    fn open_run(self: &Arc<Self>, schedule: &Schedule) -> Result<String, String> {
        let cwd = schedule.cwd.clone().ok_or("定时任务没有设置运行目录")?;
        let engine = engines::Kind::parse(schedule.engine.as_deref().unwrap_or_default())?;
        let mode = crate::requirements::engine_mode(engine, &schedule.mode)?;
        let options = engines::Options {
            approval_mode: Some(mode.to_string()),
            ..engines::Options::from_json(&schedule.options)
        };
        let session =
            self.create_engine_session(Some(cwd), Some(&schedule.id), false, engine, options)?;
        let title = format!("{} · {}", schedule.name, Local::now().format("%m-%d %H:%M"));
        let _ = self
            .store
            .record_session_meta(&session, &json!({ "title": title }));
        self.update_schedule(&schedule.id, |s| {
            if let Some(run) = s.last_run.as_mut() {
                run.session = Some(session.clone());
            }
        });
        // The jucode engine takes no start options. An ACP agent starts in
        // its mode (Pi, which never asks, would refuse the switch).
        if engine != Some(engines::Kind::Acp) {
            self.forward(&session, json!({ "op": "set_approval_mode", "mode": mode }))?;
        }
        self.send_to_session(&session, &schedule.prompt)?;
        self.retire_runs(&schedule.id, &session);
        self.broadcast(&self.sessions_json());
        Ok(session)
    }

    /// Archives the earlier runs of `schedule` that are not working and
    /// have no approval waiting.
    fn retire_runs(&self, schedule: &str, current: &str) {
        let waiting: Vec<String> = self
            .store
            .open_actions()
            .into_iter()
            .map(|action| action.session_id)
            .collect();
        for record in self.store.sessions() {
            if record.schedule.as_deref() == Some(schedule)
                && record.id != current
                && !record.archived
                && !self.is_busy(&record.id)
                && !waiting.contains(&record.id)
            {
                let _ = self
                    .store
                    .record_session_meta(&record.id, &json!({ "archived": true }));
            }
        }
    }

    /// A session event: how the scheduled run in `session`, if one is, is
    /// going. `over`: a turn ended, `failed` with this error. A failed run
    /// stays failed.
    pub(crate) fn observe_run(
        &self,
        session: &str,
        event: &Value,
        over: bool,
        failed: Option<&str>,
    ) {
        let kind = event["type"].as_str().unwrap_or_default();
        let waiting = matches!(kind, "approval_request" | "action_deferred");
        let going = matches!(kind, "action_decided" | "assistant_delta" | "tool_start");
        if !(waiting || going || over) {
            return;
        }
        let Some((id, before)) = lock(&self.schedules).iter().find_map(|s| {
            let run = s.last_run.as_ref()?;
            (run.session.as_deref() == Some(session) && run.status != "failed")
                .then(|| (s.id.clone(), run.status.clone()))
        }) else {
            return;
        };
        if let Some(error) = failed {
            return self.fail_run(&id, Some(session), error);
        }
        let deferred = || {
            self.store
                .open_actions()
                .iter()
                .any(|action| action.session_id == session)
        };
        let status = if waiting || (over && deferred()) {
            "waiting"
        } else if over {
            "done"
        } else {
            "running"
        };
        if status == before {
            return;
        }
        let changed = self.update_schedule(&id, |s| {
            if let Some(run) = s.last_run.as_mut() {
                run.status = status.to_string();
            }
        });
        if let (Some(schedule), "waiting") = (changed, status) {
            self.tell(&schedule, "waiting", "运行在等你审批。");
        }
    }

    /// The engine of `session` stopped (closed, or it died): a scheduled run
    /// still working there did not finish.
    pub(crate) fn run_session_ended(&self, session: &str) {
        let id = lock(&self.schedules).iter().find_map(|s| {
            let run = s.last_run.as_ref()?;
            (run.session.as_deref() == Some(session) && run.status == "running")
                .then(|| s.id.clone())
        });
        if let Some(id) = id {
            self.fail_run(&id, Some(session), "会话在运行中关闭了，这次运行没有完成。");
        }
    }

    /// Run of schedule `id` (in `session`, when it has one) failed with
    /// `error`: recorded and told.
    fn fail_run(&self, id: &str, session: Option<&str>, error: &str) {
        let mut changed = false;
        let failed = self.update_schedule(id, |s| {
            if let Some(run) = s
                .last_run
                .as_mut()
                .filter(|run| run.session.as_deref() == session && run.status != "failed")
            {
                run.status = "failed".to_string();
                run.error = Some(error.to_string());
                changed = true;
            }
        });
        if let (Some(schedule), true) = (failed, changed) {
            self.tell(&schedule, "failed", error);
        }
    }

    /// Once a minute (`sweep`): runs going past `RUN_TIMEOUT_MS`, working
    /// or waiting for an approval, are interrupted and fail, so later times
    /// run again.
    pub(crate) fn time_out_runs(&self, at: u64) {
        let over: Vec<(String, String)> = lock(&self.schedules)
            .iter()
            .filter_map(|schedule| {
                let run = schedule.last_run.as_ref()?;
                let session = run.session.clone()?;
                (schedule.running() && at.saturating_sub(run.at) > RUN_TIMEOUT_MS)
                    .then(|| (schedule.id.clone(), session))
            })
            .collect();
        for (id, session) in over {
            let _ = self.forward(&session, json!({ "op": "interrupt" }));
            let error = format!("运行超过 {} 小时，已停止。", RUN_TIMEOUT_MS / 3_600_000);
            self.fail_run(&id, Some(&session), &error);
        }
    }

    /// A run that was working when the daemon stopped did not finish, nor
    /// did an engine's approval; one waiting on a deferred action goes on.
    pub(crate) fn interrupt_schedule_runs(&self) {
        let deferred: Vec<String> = self
            .store
            .open_actions()
            .into_iter()
            .map(|action| action.session_id)
            .collect();
        let mut list = lock(&self.schedules);
        let mut changed = false;
        for run in list.iter_mut().filter_map(|s| s.last_run.as_mut()) {
            let waits_on_action = run.status == "waiting"
                && run.session.as_ref().is_some_and(|s| deferred.contains(s));
            if matches!(run.status.as_str(), "running" | "waiting") && !waits_on_action {
                run.status = "failed".to_string();
                run.error = Some("后台服务重启，这次运行没有完成。".to_string());
                changed = true;
            }
        }
        if changed {
            if let Err(error) = self.persist_schedules(&list) {
                jucode_agent_core::log_warn!("daemon", "schedules not saved", error = error);
            }
        }
    }

    /// Notifies the paired phones and the clients about a run of
    /// `schedule`: `failed`, `waiting` or `skipped`.
    fn tell(&self, schedule: &Schedule, kind: &str, detail: &str) {
        let head = match kind {
            "failed" => "定时任务失败",
            "waiting" => "定时任务等待审批",
            _ => "定时任务跳过",
        };
        let session = schedule
            .last_run
            .as_ref()
            .and_then(|run| run.session.clone())
            .unwrap_or_default();
        self.broadcast(&json!({
            "type": "schedule_event",
            "schedule": schedule.id,
            "name": schedule.name,
            "kind": kind,
            "detail": detail,
            "session": session,
        }));
        let tag = if session.is_empty() {
            &schedule.id
        } else {
            &session
        };
        self.notify(&format!("{head}：{}", schedule.name), detail, tag);
    }

    /// Applies `change` to schedule `id`, saves and broadcasts; returns it
    /// as changed.
    fn update_schedule(&self, id: &str, change: impl FnOnce(&mut Schedule)) -> Option<Schedule> {
        let mut list = lock(&self.schedules);
        let schedule = list.iter_mut().find(|schedule| schedule.id == id)?;
        change(schedule);
        let schedule = schedule.clone();
        if let Err(error) = self.persist_schedules(&list) {
            jucode_agent_core::log_warn!("daemon", "schedules not saved", error = error);
        }
        drop(list);
        self.broadcast(&self.schedules_json());
        Some(schedule)
    }

    fn persist_schedules(&self, list: &[Schedule]) -> Result<(), String> {
        let list: Vec<Value> = list.iter().map(Schedule::to_json).collect();
        write_private(
            &self.store.dir().join(FILE),
            format!("{:#}\n", json!(list)).as_bytes(),
        )
        .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::Hub;
    use chrono::FixedOffset;

    /// Beijing time; 2026-10-01 is a Thursday.
    fn at(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_str(&format!("{text} +0800"), "%Y-%m-%d %H:%M %z").unwrap()
    }

    fn schedule(changes: Value) -> Schedule {
        let mut schedule = Schedule::new("sch-1".to_string(), 0);
        let mut fields = json!({ "name": "n", "prompt": "p" });
        fields
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        schedule.apply(&fields).unwrap();
        schedule
    }

    fn next(changes: Value, now: &str) -> Option<DateTime<FixedOffset>> {
        next_run(&schedule(changes), &at(now))
    }

    #[test]
    fn once_runs_at_its_date_and_never_after() {
        let once = json!({ "repeat": "once", "date": "2026-10-02", "time": "11:00" });
        assert_eq!(
            next(once.clone(), "2026-10-01 12:00"),
            Some(at("2026-10-02 11:00"))
        );
        assert_eq!(next(once.clone(), "2026-10-02 11:00"), None);
        assert_eq!(next(once, "2026-10-03 09:00"), None);
    }

    #[test]
    fn hourly_runs_at_the_minute_of_the_next_hour() {
        let hourly = json!({ "repeat": "hourly", "time": "00:15" });
        assert_eq!(
            next(hourly.clone(), "2026-10-01 10:05"),
            Some(at("2026-10-01 10:15"))
        );
        assert_eq!(
            next(hourly.clone(), "2026-10-01 10:15"),
            Some(at("2026-10-01 11:15"))
        );
        assert_eq!(
            next(hourly, "2026-10-01 23:30"),
            Some(at("2026-10-02 00:15"))
        );
    }

    #[test]
    fn daily_runs_today_or_tomorrow() {
        let daily = json!({ "repeat": "daily", "time": "11:00" });
        assert_eq!(
            next(daily.clone(), "2026-10-01 09:00"),
            Some(at("2026-10-01 11:00"))
        );
        assert_eq!(
            next(daily, "2026-10-01 11:00"),
            Some(at("2026-10-02 11:00"))
        );
    }

    #[test]
    fn weekdays_skip_the_weekend() {
        let weekdays = json!({ "repeat": "weekdays", "time": "09:30" });
        // Friday after the run: next Monday.
        assert_eq!(
            next(weekdays.clone(), "2026-10-02 10:00"),
            Some(at("2026-10-05 09:30"))
        );
        assert_eq!(
            next(weekdays, "2026-10-04 08:00"),
            Some(at("2026-10-05 09:30"))
        );
    }

    #[test]
    fn weekly_wraps_to_next_week() {
        let weekly = json!({ "repeat": "weekly", "time": "08:00", "days": [1, 3] });
        // Thursday: next Monday.
        assert_eq!(
            next(weekly.clone(), "2026-10-01 07:00"),
            Some(at("2026-10-05 08:00"))
        );
        // Wednesday after the run: the next Monday.
        assert_eq!(
            next(weekly.clone(), "2026-10-07 08:00"),
            Some(at("2026-10-12 08:00"))
        );
        let sunday = json!({ "repeat": "weekly", "time": "08:00", "days": [0] });
        assert_eq!(
            next(sunday.clone(), "2026-10-04 07:59"),
            Some(at("2026-10-04 08:00"))
        );
        assert_eq!(
            next(sunday, "2026-10-04 08:00"),
            Some(at("2026-10-11 08:00"))
        );
    }

    #[test]
    fn a_disabled_schedule_has_no_next_run() {
        assert_eq!(
            next(
                json!({ "repeat": "daily", "time": "11:00", "enabled": false }),
                "2026-10-01 09:00"
            ),
            None
        );
    }

    #[test]
    fn invalid_schedules_are_refused() {
        for bad in [
            json!({ "name": "", "prompt": "p", "repeat": "daily", "time": "11:00" }),
            json!({ "name": "n", "prompt": " ", "repeat": "daily", "time": "11:00" }),
            json!({ "name": "n", "prompt": "p", "repeat": "monthly", "time": "11:00" }),
            json!({ "name": "n", "prompt": "p", "repeat": "daily", "time": "25:00" }),
            json!({ "name": "n", "prompt": "p", "repeat": "weekly", "time": "11:00" }),
            json!({ "name": "n", "prompt": "p", "repeat": "weekly", "time": "11:00", "days": [7] }),
            json!({ "name": "n", "prompt": "p", "repeat": "once", "time": "11:00" }),
            json!({ "name": "n", "prompt": "p", "repeat": "once", "time": "11:00", "date": "2026-02-30" }),
            json!({ "name": "n", "prompt": "p", "repeat": "daily", "time": "11:00", "mode": "yolo" }),
            json!({ "name": "n", "prompt": "p", "repeat": "daily", "time": "11:00", "engine": "acp" }),
            json!({ "name": "n", "prompt": "p", "repeat": "daily", "time": "11:00", "engine": "acp",
                    "options": { "agent": "gemini" } }),
            json!({ "name": "n", "prompt": "p", "repeat": "daily", "time": "11:00", "engine": "acp",
                    "options": { "agent": "omp", "command": "/bin/sh" } }),
            json!({ "name": "n", "prompt": "p", "repeat": "daily", "time": "11:00", "engine": "acp",
                    "options": { "command": "/bin/sh", "args": ["-c", "x"] } }),
            json!({ "name": "n", "prompt": "p", "repeat": "daily", "time": "11:00", "engine": "gemini" }),
        ] {
            let mut schedule = Schedule::new("sch-1".to_string(), 0);
            assert!(schedule.apply(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_scheduled_acp_task_runs_a_built_in_agent() {
        let mut schedule = Schedule::new("sch-1".to_string(), 0);
        let fields = json!({ "name": "n", "prompt": "p", "repeat": "daily", "time": "09:00",
            "engine": "acp", "mode": "edits", "options": { "agent": "omp", "jucode_gateway": false } });
        schedule.apply(&fields).unwrap();
        assert_eq!(schedule.engine.as_deref(), Some("acp"));
        assert_eq!(schedule.to_json()["options"]["agent"], "omp");
        let message = |changes: Value| {
            let mut schedule = schedule.clone();
            schedule.apply(&changes).unwrap_err()
        };
        assert_eq!(
            message(json!({ "options": {} })),
            "a scheduled ACP task needs a built-in agent"
        );
        assert_eq!(
            message(json!({ "options": { "agent": "x" } })),
            "unknown ACP agent x"
        );
        assert_eq!(
            message(json!({ "options": { "agent": "omp", "bin": "/bin/sh" } })),
            "scheduled ACP tasks run built-in agents only"
        );
        // Back on jucode, the agent no longer matters.
        schedule.apply(&json!({ "engine": "jucode" })).unwrap();
        assert_eq!(schedule.engine, None);
    }

    fn hub_with_run(label: &str) -> (Arc<Hub>, std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "jucode-schedule-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            crate::store::Store::open(dir.join("daemon")).unwrap(),
            "test",
            None,
        );
        let mut fields = json!({
            "name": "巡检", "prompt": "check", "repeat": "daily", "time": "09:00", "cwd": dir,
        });
        fields["enabled"] = json!(false);
        let saved = hub.save_schedule(&fields, true).unwrap();
        // The run claims, then names its session (as `open_run` does).
        assert!(hub.claim_run(&saved.id).unwrap().is_some());
        assert!(
            hub.claim_run(&saved.id).unwrap().is_none(),
            "one run at a time"
        );
        hub.update_schedule(&saved.id, |s| {
            s.last_run.as_mut().unwrap().session = Some("s1".into());
        });
        (hub, dir, saved.id)
    }

    fn status(hub: &Hub, id: &str) -> String {
        lock(&hub.schedules)
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.last_run.clone())
            .map(|run| run.status)
            .unwrap_or_default()
    }

    #[test]
    fn a_turn_that_fails_before_any_reply_fails_the_run_for_good() {
        let (hub, dir, id) = hub_with_run("failed");
        for event in [
            json!({ "type": "user_message", "content": "check" }),
            json!({ "type": "error", "message": "HTTP 502" }),
            json!({ "type": "status", "message": "ready" }),
        ] {
            hub.observe("s1", &event);
        }
        assert_eq!(status(&hub, &id), "failed");
        // A late event does not bring it back.
        hub.observe("s1", &json!({ "type": "assistant_delta", "delta": "x" }));
        hub.observe("s1", &json!({ "type": "status", "message": "ready" }));
        assert_eq!(status(&hub, &id), "failed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_run_waits_then_ends_and_fails_when_its_session_closes_midway() {
        let (hub, dir, id) = hub_with_run("ends");
        hub.observe("s1", &json!({ "type": "user_message", "content": "check" }));
        hub.observe("s1", &json!({ "type": "approval_request", "call_id": "c" }));
        assert_eq!(status(&hub, &id), "waiting");
        hub.observe("s1", &json!({ "type": "action_decided" }));
        assert_eq!(status(&hub, &id), "running");
        // A turn that ends without any text still ends the run.
        hub.observe("s1", &json!({ "type": "status", "message": "ready" }));
        assert_eq!(status(&hub, &id), "done");

        assert!(hub.claim_run(&id).unwrap().is_some());
        hub.update_schedule(&id, |s| {
            s.last_run.as_mut().unwrap().session = Some("s2".into())
        });
        hub.run_session_ended("s2");
        assert_eq!(status(&hub, &id), "failed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_schedule_reads_back_as_saved() {
        let mut saved = schedule(json!({
            "repeat": "weekly", "time": "08:00", "days": [1], "cwd": "/p", "engine": "claude",
            "mode": "edits", "options": { "gateway": true },
        }));
        saved.next_run_at = Some(5);
        saved.last_run = Some(LastRun {
            at: 1,
            session: Some("s1".into()),
            status: "waiting".into(),
            error: None,
        });
        saved.skipped = 2;
        assert_eq!(Schedule::from_json(&saved.to_json()), Some(saved.clone()));
        // jucode is no engine of its own.
        let jucode = schedule(json!({ "repeat": "daily", "time": "08:00", "engine": "jucode" }));
        assert_eq!(jucode.engine, None);
        assert_eq!(jucode.to_json()["engine"], "jucode");
    }
}
