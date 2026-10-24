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
//! The user hears of a run only when it needs them: it failed (the first
//! time in a row), it waits for an approval, it ran past `RUN_TIMEOUT_MS`
//! or its `budget`, or a time was skipped (once in a row). After
//! `MAX_FAILURES` failures in a row the task switches itself off, and says
//! so. Paired phones get a notification; clients a `schedule_event`.
//!
//! A task may run in a worktree of its own (`worktree`), so it never mixes
//! its changes with the user's in the main checkout; an archived run's
//! worktree with nothing in it is removed. While a run is working the
//! computer is kept from idle sleep (`awake`).

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
/// Failures in a row after which a task switches itself off.
pub const MAX_FAILURES: u32 = 3;
/// Earlier runs kept with a task (`history`).
const HISTORY: usize = 20;
/// Permission modes in the clients' names (see `engine_mode`).
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
    /// Runs start in a new git worktree of `cwd`'s repository.
    pub worktree: bool,
    /// What one run may cost: gateway points for runs through the JuCode
    /// gateway, estimated US dollars for others. A run past it stops.
    pub budget: Option<f64>,
    /// Runs failed in a row.
    pub failures: u32,
    /// The runs before `last_run`, newest first (`HISTORY` of them).
    pub history: Vec<LastRun>,
}

/// A run: when it started, its session and how it is going.
#[derive(Debug, Clone, PartialEq)]
pub struct LastRun {
    pub at: u64,
    pub session: Option<String>,
    /// `running`, `waiting` (an approval), `done` or `failed`.
    pub status: String,
    /// Why it failed.
    pub error: Option<String>,
    /// The user sent a message in its session after it was done: what
    /// happens there since is theirs, not the run's.
    pub followed_up: bool,
}

impl LastRun {
    fn to_json(&self) -> Value {
        json!({
            "at": self.at, "session": self.session, "status": self.status, "error": self.error,
            "followed_up": self.followed_up,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let run = value.as_object()?;
        let text = |key: &str| run.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            at: run.get("at").and_then(Value::as_u64).unwrap_or_default(),
            session: text("session"),
            status: text("status").unwrap_or_else(|| "done".to_string()),
            error: text("error"),
            followed_up: run.get("followed_up").and_then(Value::as_bool).unwrap_or(false),
        })
    }
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
            worktree: false,
            budget: None,
            failures: 0,
            history: Vec::new(),
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
            "last_run": self.last_run.as_ref().map(LastRun::to_json),
            "skipped": self.skipped,
            "origin": self.origin,
            "worktree": self.worktree,
            "budget": self.budget,
            "failures": self.failures,
            "history": self.history.iter().map(LastRun::to_json).collect::<Vec<_>>(),
        })
    }

    /// A schedule as saved; the saved `next_run_at` stays, so one that came
    /// due while the daemon was down still fires.
    pub fn from_json(value: &Value) -> Option<Self> {
        let mut schedule = Self {
            next_run_at: value["next_run_at"].as_u64(),
            last_run: LastRun::from_json(&value["last_run"]),
            failures: value["failures"].as_u64().unwrap_or_default() as u32,
            history: value["history"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(LastRun::from_json)
                .take(HISTORY)
                .collect(),
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
        if let Some(worktree) = changes["worktree"].as_bool() {
            self.worktree = worktree;
        }
        // Absent: as it was; null: no budget.
        match changes.get("budget") {
            None => {}
            Some(Value::Null) => self.budget = None,
            Some(budget) => match budget.as_f64() {
                Some(budget) if budget > 0.0 && budget.is_finite() => self.budget = Some(budget),
                _ => return Err("budget must be a number above 0".to_string()),
            },
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

    /// Its latest run is not over: working, waiting for an approval, or on
    /// background work its turn left going.
    fn running(&self) -> bool {
        self.last_run
            .as_ref()
            .is_some_and(|run| matches!(run.status.as_str(), "running" | "waiting" | "background"))
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

/// `id` as a worktree name: lowercase letters, digits and hyphens.
fn slug(id: &str) -> String {
    let slug: String = id
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    match slug.trim_matches('-') {
        "" => "schedule".to_string(),
        slug => slug.to_string(),
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

/// A permission mode, in a client's names (`ask`, `plan`, `auto`, `edits`,
/// `all`) or an engine's, as `engine` (None: jucode) takes it.
pub(crate) fn engine_mode(
    engine: Option<engines::Kind>,
    mode: &str,
) -> Result<&'static str, String> {
    let jucode = engine.is_none();
    Ok(match mode {
        "ask" | "manual" | "read-only" => {
            if jucode {
                "manual"
            } else {
                "read-only"
            }
        }
        // jucode has no plan mode: it asks before every change.
        "plan" if jucode => "manual",
        "plan" => "plan",
        "edits" | "auto-edit" => "auto-edit",
        "auto" => "auto",
        "all" | "full-auto" | "full-access" => {
            if jucode {
                "full-access"
            } else {
                "full-auto"
            }
        }
        other => return Err(format!("unknown permission mode {other}")),
    })
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
        // Switched on again: its failures so far no longer count.
        if schedule.enabled && !list.iter().any(|s| s.id == schedule.id && s.enabled) {
            schedule.failures = 0;
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
                // Off the scheduler: a run may make a worktree first.
                Ok(Some(schedule)) => {
                    let hub = Arc::clone(self);
                    std::thread::spawn(move || {
                        if let Err(error) = hub.start_run(&schedule) {
                            jucode_agent_core::log_warn!(
                                "daemon",
                                "scheduled run not started",
                                error = error
                            );
                        }
                    });
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
        if let Some(previous) = schedule.last_run.take() {
            schedule.history.insert(0, previous);
            schedule.history.truncate(HISTORY);
        }
        schedule.last_run = Some(LastRun {
            at: now(),
            session: None,
            status: "running".to_string(),
            error: None,
            followed_up: false,
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
        let mode = engine_mode(engine, &schedule.mode)?;
        let options = engines::Options {
            approval_mode: Some(mode.to_string()),
            ..engines::Options::from_json(&schedule.options)
        };
        // A run in a worktree of its own still belongs to the project of the
        // task's folder.
        let (run_cwd, tree, project) = if schedule.worktree {
            let project = crate::projects::for_new_session(self, &cwd, None)?;
            let slug = format!("{}-{}", slug(&schedule.id), Local::now().format("%m%d-%H%M"));
            let (tree, inside) = crate::worktrees::create(&cwd, &slug)?;
            (inside, Some(tree), Some(project))
        } else {
            (cwd, None, None)
        };
        let session = self
            .create_engine_session(
                Some(run_cwd),
                Some(&schedule.id),
                false,
                engine,
                options,
                project.as_deref(),
            )
            .inspect_err(|_| {
                if let Some(tree) = &tree {
                    crate::worktrees::remove_if_unused(tree);
                }
            })?;
        if let Some(tree) = &tree {
            let _ = self
                .store
                .record_session_meta(&session, &json!({ "worktree": tree }));
        }
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
    /// have no approval waiting; their worktrees go if nothing is in them.
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
                && !self.is_awaited(&record.id)
                && !waiting.contains(&record.id)
            {
                let _ = self
                    .store
                    .record_session_meta(&record.id, &json!({ "archived": true }));
                // Not from under an engine still open there (idle, not closed yet).
                if let Some(tree) = record.worktree.filter(|_| !self.is_hosted(&record.id)) {
                    self.drop_worktree(&record.id, tree);
                }
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
        self.observe_run_event(session, event, over, failed);
        // The engine is ready: a run waiting on background work may be over,
        // whatever the background state did meanwhile.
        if event["type"] == "status" && event["message"] == "ready" {
            self.run_activity(session);
        }
    }

    fn observe_run_event(&self, session: &str, event: &Value, over: bool, failed: Option<&str>) {
        let kind = event["type"].as_str().unwrap_or_default();
        // A message the user sends in a run whose turn ended is theirs: the
        // run is done, and from then on no timeout, budget or alert applies
        // there. Turns the engine starts itself, when background work the
        // run left going ends, are still the run's.
        if kind == "user_message" {
            let ended = lock(&self.schedules).iter().find_map(|s| {
                let run = s.last_run.as_ref()?;
                (run.session.as_deref() == Some(session)
                    && matches!(run.status.as_str(), "done" | "background")
                    && !run.followed_up)
                    .then(|| s.id.clone())
            });
            if let Some(id) = ended {
                self.update_schedule(&id, |s| {
                    if let Some(run) = s.last_run.as_mut() {
                        run.followed_up = true;
                        run.status = "done".to_string();
                    }
                });
            }
            return;
        }
        let waiting = matches!(kind, "approval_request" | "action_deferred");
        let going = matches!(kind, "action_decided" | "assistant_delta" | "tool_start");
        if !(waiting || going || over) {
            return;
        }
        let Some((id, before)) = lock(&self.schedules).iter().find_map(|s| {
            let run = s.last_run.as_ref()?;
            (run.session.as_deref() == Some(session) && run.status != "failed" && !run.followed_up)
                .then(|| (s.id.clone(), run.status.clone()))
        }) else {
            return;
        };
        if let Some(error) = failed {
            return self.fail_run(&id, Some(session), error);
        }
        // A subagent's output (`nested`) is not the run working again (a
        // background one goes on after the turn), unless it answers the
        // approval the run waited on (no `action_decided` comes then).
        if going && event["nested"] == true && before != "waiting" {
            return;
        }
        let deferred = || {
            self.store
                .open_actions()
                .iter()
                .any(|action| action.session_id == session)
        };
        let status = if waiting || (over && deferred()) {
            "waiting"
        } else if over && self.run_waits(session) {
            "background"
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
            if status == "done" {
                s.failures = 0;
            }
        });
        if let (Some(schedule), "waiting") = (changed, status) {
            self.tell(&schedule, "waiting", "运行在等你审批。");
        }
    }

    /// Background work in `session` started or ended (`set_background`): a
    /// run whose turn ended waits on it (`background`), and is done once it
    /// ends with no turn going.
    pub(crate) fn run_activity(&self, session: &str) {
        let Some((id, before)) = lock(&self.schedules).iter().find_map(|s| {
            let run = s.last_run.as_ref()?;
            (run.session.as_deref() == Some(session)
                && matches!(run.status.as_str(), "done" | "background")
                && !run.followed_up)
                .then(|| (s.id.clone(), run.status.clone()))
        }) else {
            return;
        };
        let status = match (before.as_str(), self.run_waits(session)) {
            ("done", true) => "background",
            ("background", false) if !self.is_busy(session) => "done",
            _ => return,
        };
        // Only from the status read above: a session thread may have moved it
        // meanwhile (this runs from the sweep too).
        self.update_schedule(&id, |s| {
            if let Some(run) = s.last_run.as_mut().filter(|run| run.status == before) {
                run.status = status.to_string();
                if status == "done" {
                    s.failures = 0;
                }
            }
        });
    }

    /// Work the run in `session` left going comes back to it: background
    /// agents or workflows, or a session it started still working.
    fn run_waits(&self, session: &str) -> bool {
        let children: Vec<String> = lock(&self.children)
            .iter()
            .filter(|(_, child)| child.parent == session)
            .map(|(id, _)| id.clone())
            .collect();
        self.is_awaited(session)
            || children
                .iter()
                .any(|child| self.is_busy(child) || self.is_awaited(child))
    }

    /// Once a minute (`sweep`): runs waiting on background work settle,
    /// should a change (a child's turn ending) have come by no other way.
    pub(crate) fn settle_runs(&self) {
        let waiting: Vec<String> = lock(&self.schedules)
            .iter()
            .filter_map(|s| s.last_run.as_ref())
            .filter(|run| run.status == "background")
            .filter_map(|run| run.session.clone())
            .collect();
        for session in waiting {
            self.run_activity(&session);
        }
    }

    /// Stops what `session` is doing: its turn, and a Claude Code
    /// session's background tasks.
    fn stop_work(&self, session: &str) {
        if self.is_busy(session) {
            let _ = self.forward(session, json!({ "op": "interrupt" }));
        }
        let claude = || {
            self.store
                .sessions()
                .iter()
                .any(|r| r.id == session && r.engine.as_deref() == Some("claude"))
        };
        if self.is_background(session) && claude() {
            let _ = self.forward(session, json!({ "op": "stop_background" }));
        }
    }

    /// The engine of `session` stopped (closed, or it died): a scheduled run
    /// still working there did not finish, and an archived run's worktree,
    /// kept while the engine was open (`retire_runs`), goes if unused.
    pub(crate) fn run_session_ended(&self, session: &str) {
        let tree = self
            .store
            .sessions()
            .into_iter()
            .find(|r| r.id == session && r.archived && r.schedule.is_some())
            .and_then(|r| r.worktree);
        if let Some(tree) = tree {
            self.drop_worktree(session, tree);
        }
        let id = lock(&self.schedules).iter().find_map(|s| {
            let run = s.last_run.as_ref()?;
            (run.session.as_deref() == Some(session)
                && matches!(run.status.as_str(), "running" | "background"))
                .then(|| s.id.clone())
        });
        if let Some(id) = id {
            self.fail_run(&id, Some(session), "会话在运行中关闭了，这次运行没有完成。");
        }
    }

    /// Removes `session`'s worktree `tree`, off this thread, if nothing is
    /// in it; the session then lists it with `gone: true`.
    fn drop_worktree(&self, session: &str, mut tree: Value) {
        let Some(hub) = self.arc() else { return };
        let session = session.to_string();
        std::thread::spawn(move || {
            if crate::worktrees::remove_if_unused(&tree) {
                tree["gone"] = json!(true);
                let _ = hub
                    .store
                    .record_session_meta(&session, &json!({ "worktree": tree }));
                hub.broadcast(&hub.sessions_json());
            }
        });
    }

    /// Run of schedule `id` (in `session`, when it has one) failed with
    /// `error`: recorded, and told the first time in a row. The
    /// `MAX_FAILURES`th in a row switches the task off, which is told too.
    fn fail_run(&self, id: &str, session: Option<&str>, error: &str) {
        let mut changed = false;
        let mut stopped = false;
        let failed = self.update_schedule(id, |s| {
            if let Some(run) = s
                .last_run
                .as_mut()
                .filter(|run| run.session.as_deref() == session && run.status != "failed")
            {
                run.status = "failed".to_string();
                run.error = Some(error.to_string());
                changed = true;
                // A switched-off task run by hand counts no failures.
                if s.enabled {
                    s.failures += 1;
                }
                if s.failures >= MAX_FAILURES && s.enabled {
                    s.enabled = false;
                    s.next_run_at = None;
                    stopped = true;
                }
            }
        });
        match failed {
            Some(schedule) if stopped => {
                let detail = format!(
                    "连续失败 {} 次，已自动停用。最后一次：{error}",
                    schedule.failures
                );
                self.tell(&schedule, "stopped", &detail);
            }
            Some(schedule) if changed && schedule.failures == 1 => {
                self.tell(&schedule, "failed", error)
            }
            _ => {}
        }
    }

    /// `sessions` and the sessions they started (`start_session`): what
    /// their work costs.
    pub(crate) fn with_children(&self, sessions: Vec<String>) -> Vec<String> {
        let children: Vec<String> = self
            .store
            .sessions()
            .into_iter()
            .filter(|r| r.parent.as_ref().is_some_and(|p| sessions.contains(p)))
            .map(|r| r.id)
            .collect();
        sessions.into_iter().chain(children).collect()
    }

    /// Once a minute (`sweep`): a run with a budget that has cost more,
    /// with the sessions it started, is interrupted and fails. Costs come
    /// from the usage records (gateway charges from the account's ledger,
    /// so they show a little late).
    pub(crate) fn check_budgets(self: &Arc<Self>) {
        let runs: Vec<(String, String, f64)> = lock(&self.schedules)
            .iter()
            .filter(|schedule| schedule.running())
            .filter_map(|schedule| {
                let session = schedule.last_run.as_ref()?.session.clone()?;
                Some((schedule.id.clone(), session, schedule.budget?))
            })
            .collect();
        if runs.is_empty() {
            return;
        }
        let hub = Arc::clone(self);
        std::thread::spawn(move || {
            for (id, session, budget) in runs {
                let sessions = hub.with_children(vec![session.clone()]);
                let usage = hub.usage.sessions_json(&sessions);
                // In the run's own unit: points when it went through the
                // gateway, else estimated dollars (what children spent in
                // the other unit is left out).
                let gateway = usage["sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|row| row["session"] == session.as_str() && row["gateway_requests"].as_u64() > Some(0));
                let (unit, cost) = if gateway {
                    ("点", usage["totals"]["gateway_cost"].as_f64())
                } else {
                    ("美元（估算）", usage["totals"]["estimated_cost_usd"].as_f64())
                };
                if let Some(cost) = cost.filter(|cost| *cost > budget) {
                    for session in &sessions {
                        hub.stop_work(session);
                    }
                    let error = format!("花费 {cost:.4} {unit}，超过上限 {budget}，已停止。");
                    hub.fail_run(&id, Some(&session), &error);
                }
            }
        });
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
            for session in self.with_children(vec![session.clone()]) {
                self.stop_work(&session);
            }
            let error = format!("运行超过 {} 小时，已停止。", RUN_TIMEOUT_MS / 3_600_000);
            self.fail_run(&id, Some(&session), &error);
        }
    }

    /// At start: the worktrees of archived runs that waited for their
    /// engines to close (`retire_runs`) go, if unused; the last daemon may
    /// have stopped before the engines did.
    pub(crate) fn clean_run_worktrees(&self) {
        for record in self.store.sessions() {
            let tree = record.worktree.filter(|tree| tree["gone"] != true);
            if let (true, Some(_), Some(tree)) = (record.archived, &record.schedule, tree) {
                self.drop_worktree(&record.id, tree);
            }
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
            if matches!(run.status.as_str(), "running" | "waiting" | "background") && !waits_on_action {
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
    /// `schedule`: `failed`, `stopped` (failed too often, switched off),
    /// `waiting` or `skipped`.
    fn tell(&self, schedule: &Schedule, kind: &str, detail: &str) {
        let head = match kind {
            "failed" => "定时任务失败",
            "stopped" => "定时任务已停用",
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

    /// Saves the schedules; the computer stays awake while a run works.
    fn persist_schedules(&self, list: &[Schedule]) -> Result<(), String> {
        self.awake.sync(
            "schedule",
            list.iter()
                .filter(|s| {
                    s.last_run
                        .as_ref()
                        .is_some_and(|run| matches!(run.status.as_str(), "running" | "background"))
                })
                .map(|s| s.id.clone()),
        );
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
        // What the user asks there afterwards is not the run.
        hub.observe("s1", &json!({ "type": "user_message", "content": "and?" }));
        hub.observe("s1", &json!({ "type": "assistant_delta", "delta": "x" }));
        assert_eq!(status(&hub, &id), "done");
        assert!(hub.awake.keys().is_empty());

        assert!(hub.claim_run(&id).unwrap().is_some());
        hub.update_schedule(&id, |s| {
            s.last_run.as_mut().unwrap().session = Some("s2".into())
        });
        hub.observe("s2", &json!({ "type": "user_message", "content": "check" }));
        hub.observe("s2", &json!({ "type": "status", "message": "ready" }));
        assert_eq!(status(&hub, &id), "done");
        // Work its turn left going (a background task) is still the run.
        hub.observe("s2", &json!({ "type": "tool_start", "call_id": "t" }));
        assert_eq!(status(&hub, &id), "running");
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
            followed_up: false,
        });
        saved.skipped = 2;
        saved.worktree = true;
        saved.budget = Some(1.5);
        saved.failures = 1;
        saved.history = vec![LastRun {
            at: 0,
            session: Some("s0".into()),
            status: "failed".into(),
            error: Some("boom".into()),
            followed_up: true,
        }];
        assert_eq!(Schedule::from_json(&saved.to_json()), Some(saved.clone()));
        // jucode is no engine of its own.
        let jucode = schedule(json!({ "repeat": "daily", "time": "08:00", "engine": "jucode" }));
        assert_eq!(jucode.engine, None);
        assert_eq!(jucode.to_json()["engine"], "jucode");
    }

    #[test]
    fn a_budget_is_above_zero_and_null_clears_it() {
        let mut task = schedule(json!({ "repeat": "daily", "time": "08:00", "budget": 2 }));
        assert_eq!(task.budget, Some(2.0));
        // Changes without it keep it.
        task.apply(&json!({ "name": "m" })).unwrap();
        assert_eq!(task.budget, Some(2.0));
        for bad in [json!(0), json!(-1), json!("2"), json!(true)] {
            assert!(task.clone().apply(&json!({ "budget": bad })).is_err());
        }
        task.apply(&json!({ "budget": null })).unwrap();
        assert_eq!(task.budget, None);
    }

    #[test]
    fn three_failures_in_a_row_switch_a_task_off_and_a_good_run_resets_them() {
        let (hub, dir, id) = hub_with_run("failures");
        hub.save_schedule(&json!({ "id": id, "enabled": true }), true)
            .unwrap();
        let task = |hub: &Hub| lock(&hub.schedules).iter().find(|s| s.id == id).cloned().unwrap();
        let fail = |n: usize| {
            if n > 0 {
                assert!(hub.claim_run(&id).unwrap().is_some());
            }
            let session = format!("s{}", n + 1);
            hub.update_schedule(&id, |s| s.last_run.as_mut().unwrap().session = Some(session.clone()));
            hub.run_session_ended(&session);
        };
        fail(0);
        fail(1);
        assert_eq!(task(&hub).failures, 2);
        assert!(task(&hub).enabled);
        // A good run starts the count again.
        assert!(hub.claim_run(&id).unwrap().is_some());
        hub.update_schedule(&id, |s| s.last_run.as_mut().unwrap().session = Some("ok".into()));
        hub.observe("ok", &json!({ "type": "user_message", "content": "check" }));
        hub.observe("ok", &json!({ "type": "status", "message": "ready" }));
        assert_eq!(task(&hub).failures, 0);
        for n in 3..6 {
            fail(n);
        }
        let stopped = task(&hub);
        assert_eq!(stopped.failures, 3);
        assert!(!stopped.enabled);
        assert_eq!(stopped.next_run_at, None);
        // Earlier runs are kept, newest first.
        let sessions: Vec<_> = stopped.history.iter().map(|r| r.session.clone().unwrap()).collect();
        assert_eq!(sessions, ["s5", "s4", "ok", "s2", "s1"]);
        // Switching it on again forgives them.
        hub.save_schedule(&json!({ "id": id, "enabled": true }), false)
            .unwrap();
        assert_eq!(task(&hub).failures, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_computer_stays_awake_only_while_a_run_works() {
        let (hub, dir, id) = hub_with_run("awake");
        assert_eq!(hub.awake.keys(), [format!("schedule:{id}")]);
        hub.observe("s1", &json!({ "type": "user_message", "content": "check" }));
        hub.observe("s1", &json!({ "type": "approval_request", "call_id": "c" }));
        assert!(hub.awake.keys().is_empty(), "waiting on the user is no work");
        hub.observe("s1", &json!({ "type": "action_decided" }));
        assert_eq!(hub.awake.keys().len(), 1);
        hub.observe("s1", &json!({ "type": "status", "message": "ready" }));
        assert!(hub.awake.keys().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_run_waits_on_its_background_work_apart_from_working() {
        let (hub, dir, id) = hub_with_run("background");
        hub.observe("s1", &json!({ "type": "user_message", "content": "check" }));
        hub.set_background("s1", true, true);
        hub.observe("s1", &json!({ "type": "status", "message": "ready" }));
        assert_eq!(status(&hub, &id), "background");
        assert_eq!(hub.awake.keys().len(), 1, "background work keeps it awake");
        assert!(hub.claim_run(&id).unwrap().is_none(), "the next time is skipped");
        // A background subagent at work is not the run working.
        hub.observe("s1", &json!({ "type": "tool_start", "call_id": "s", "nested": true }));
        assert_eq!(status(&hub, &id), "background");
        // The work ends; the engine reports on it in a turn of its own.
        hub.set_background("s1", false, false);
        assert_eq!(status(&hub, &id), "done");
        hub.observe("s1", &json!({ "type": "tool_start", "call_id": "t" }));
        assert_eq!(status(&hub, &id), "running");
        hub.observe("s1", &json!({ "type": "status", "message": "ready" }));
        assert_eq!(status(&hub, &id), "done");
        assert!(hub.awake.keys().is_empty());
        // Background work heard of after the turn's end still holds it.
        hub.set_background("s1", true, true);
        assert_eq!(status(&hub, &id), "background");
        // A message from the user ends the run there.
        hub.observe("s1", &json!({ "type": "user_message", "content": "and?" }));
        assert_eq!(status(&hub, &id), "done");
        hub.set_background("s1", false, false);
        hub.set_background("s1", true, true);
        assert_eq!(status(&hub, &id), "done");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_run_costs_what_the_sessions_it_started_cost() {
        let (hub, dir, _) = hub_with_run("children");
        for (session, parent) in [("s1", None), ("c1", Some("s1")), ("other", None)] {
            hub.store.record_session(session, &dir).unwrap();
            if let Some(parent) = parent {
                hub.store
                    .record_session_meta(session, &json!({ "parent": parent }))
                    .unwrap();
            }
        }
        assert_eq!(hub.with_children(vec!["s1".into()]), ["s1", "c1"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_dev_server_left_running_does_not_hold_a_run_but_a_child_at_work_does() {
        let (hub, dir, id) = hub_with_run("left-running");
        hub.observe("s1", &json!({ "type": "user_message", "content": "check" }));
        // A background command never comes back: the run is done.
        hub.set_background("s1", true, false);
        hub.observe("s1", &json!({ "type": "status", "message": "ready" }));
        assert_eq!(status(&hub, &id), "done");
        assert!(hub.awake.keys().is_empty());

        assert!(hub.claim_run(&id).unwrap().is_some());
        hub.update_schedule(&id, |s| s.last_run.as_mut().unwrap().session = Some("s2".into()));
        lock(&hub.children).insert(
            "c1".into(),
            crate::subsessions::Child { parent: "s2".into(), status: "running".into(), ..Default::default() },
        );
        hub.set_busy("c1", true);
        hub.observe("s2", &json!({ "type": "user_message", "content": "check" }));
        hub.observe("s2", &json!({ "type": "status", "message": "ready" }));
        assert_eq!(status(&hub, &id), "background", "a session it started still works");
        // The child's turn ends; the sweep settles the run.
        hub.set_busy("c1", false);
        hub.settle_runs();
        assert_eq!(status(&hub, &id), "done");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_subagent_going_on_after_its_approval_is_the_run_working() {
        let (hub, dir, id) = hub_with_run("subagent-approval");
        hub.observe("s1", &json!({ "type": "user_message", "content": "check" }));
        hub.observe("s1", &json!({ "type": "approval_request", "call_id": "c" }));
        assert_eq!(status(&hub, &id), "waiting");
        hub.observe("s1", &json!({ "type": "tool_start", "call_id": "s", "nested": true }));
        assert_eq!(status(&hub, &id), "running");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_run_done_after_its_background_work_forgives_failures() {
        let (hub, dir, id) = hub_with_run("bg-forgives");
        hub.update_schedule(&id, |s| s.failures = 2);
        hub.observe("s1", &json!({ "type": "user_message", "content": "check" }));
        hub.set_background("s1", true, true);
        hub.observe("s1", &json!({ "type": "status", "message": "ready" }));
        assert_eq!(status(&hub, &id), "background");
        hub.set_background("s1", false, false);
        assert_eq!(status(&hub, &id), "done");
        assert_eq!(lock(&hub.schedules).iter().find(|s| s.id == id).unwrap().failures, 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
