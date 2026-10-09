//! Scheduled tasks: a prompt an agent receives at set local times, saved in
//! `~/.jucode/agents/<id>/schedules.json`. A due task fires once and its
//! `next_run_at` moves past now, so times missed while the daemon (or the
//! computer) was off fire once on start, never as a burst. A task of a
//! disabled agent waits, neither firing nor moving on, and fires once when
//! the agent is enabled again. Times are unix seconds.

use crate::{
    agents::Agents,
    hub::{lock, Hub},
    store::{now, random_hex, Message},
};
use chrono::{
    DateTime, Datelike, Days, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone,
    Timelike,
};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub id: String,
    pub agent: String,
    pub name: String,
    pub prompt: String,
    pub enabled: bool,
    /// `once`, `hourly`, `daily`, `weekdays` or `weekly`.
    pub repeat: String,
    /// Local `HH:MM`; `hourly` uses only the minute.
    pub time: String,
    /// `weekly`: 0 = Sunday … 6 = Saturday.
    pub days: Vec<u8>,
    /// `once`: local `YYYY-MM-DD`.
    pub date: Option<String>,
    /// Each run starts a new session (carrying the last run's handoff
    /// note); false continues the last run's.
    pub new_session: bool,
    /// The agent proposed it (with its `schedule` tool); it starts switched
    /// off until the user turns it on.
    pub by_agent: bool,
    pub created_at: u64,
    pub last_run_at: Option<u64>,
    /// The session the last run was delivered to.
    pub last_session: Option<String>,
    pub next_run_at: Option<u64>,
}

impl Schedule {
    fn new(id: String, agent: String, created_at: u64) -> Self {
        Self {
            id,
            agent,
            name: String::new(),
            prompt: String::new(),
            enabled: true,
            repeat: String::new(),
            time: String::new(),
            days: Vec::new(),
            date: None,
            new_session: true,
            by_agent: false,
            created_at,
            last_run_at: None,
            last_session: None,
            next_run_at: None,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "agent": self.agent,
            "name": self.name,
            "prompt": self.prompt,
            "enabled": self.enabled,
            "repeat": self.repeat,
            "time": self.time,
            "days": self.days,
            "date": self.date,
            "new_session": self.new_session,
            "by_agent": self.by_agent,
            "created_at": self.created_at,
            "last_run_at": self.last_run_at,
            "last_session": self.last_session,
            "next_run_at": self.next_run_at,
        })
    }

    /// A schedule as saved; the saved `next_run_at` stays, so one that came
    /// due while the daemon was down still fires.
    fn from_json(value: &Value) -> Option<Self> {
        let mut schedule = Self {
            by_agent: value["by_agent"] == true,
            last_run_at: value["last_run_at"].as_u64(),
            last_session: value["last_session"].as_str().map(str::to_string),
            next_run_at: value["next_run_at"].as_u64(),
            ..Self::new(
                value["id"].as_str()?.to_string(),
                value["agent"].as_str()?.to_string(),
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
        if let Some(enabled) = changes["enabled"].as_bool() {
            self.enabled = enabled;
        }
        if let Some(new_session) = changes["new_session"].as_bool() {
            self.new_session = new_session;
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
        if self.name.is_empty() {
            return Err("schedule name must not be empty".to_string());
        }
        if self.prompt.is_empty() {
            return Err("schedule prompt must not be empty".to_string());
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

/// `next_run` after `now` (unix seconds) in this machine's time zone.
fn next_run_at(schedule: &Schedule, now: u64) -> Option<u64> {
    let now = Local.timestamp_opt(now as i64, 0).single()?;
    next_run(schedule, &now).map(|at| at.timestamp() as u64)
}

/// Every agent's saved schedules, read at startup.
pub fn load(agents: &Agents) -> Vec<Schedule> {
    agents
        .list()
        .iter()
        .flat_map(|agent| agents.schedules(&agent.id))
        .filter_map(|value| Schedule::from_json(&value))
        .collect()
}

fn seconds() -> u64 {
    now() / 1000
}

impl Hub {
    /// All schedules, or `agent`'s.
    pub fn schedules_json(&self, agent: Option<&str>) -> Value {
        let list: Vec<Value> = lock(&self.schedules)
            .iter()
            .filter(|schedule| agent.is_none_or(|agent| schedule.agent == agent))
            .map(Schedule::to_json)
            .collect();
        json!({ "type": "schedules", "schedules": list })
    }

    /// Creates a schedule (no `id`) or changes one; its agent stays.
    pub fn save_schedule(&self, changes: &Value) -> Result<Schedule, String> {
        self.save_schedule_by(changes, None)
    }

    /// An agent's own `schedule` tool: it may create and change only its
    /// own tasks, and whatever it saves is switched off until the user
    /// turns it on.
    pub fn propose_schedule(&self, agent: &str, fields: &Value) -> Result<Schedule, String> {
        let mut changes = json!({ "agent": agent, "enabled": false });
        for key in ["id", "name", "prompt", "repeat", "time", "days", "date"] {
            if !fields[key].is_null() {
                changes[key] = fields[key].clone();
            }
        }
        self.save_schedule_by(&changes, Some(agent))
    }

    fn save_schedule_by(
        &self,
        changes: &Value,
        by_agent: Option<&str>,
    ) -> Result<Schedule, String> {
        let mut list = lock(&self.schedules);
        let mut schedule = match changes["id"].as_str() {
            Some(id) => list
                .iter()
                .find(|schedule| schedule.id == id)
                .filter(|schedule| by_agent.is_none_or(|agent| schedule.agent == agent))
                .cloned()
                .ok_or_else(|| format!("unknown schedule {id}"))?,
            None => {
                let agent = changes["agent"].as_str().unwrap_or_default();
                if self.agents.get(agent).is_none() {
                    return Err(format!("unknown agent '{agent}'"));
                }
                let id = random_hex(8).map_err(|error| error.to_string())?;
                let mut schedule = Schedule::new(format!("sch-{id}"), agent.to_string(), seconds());
                schedule.by_agent = by_agent.is_some();
                schedule
            }
        };
        schedule.apply(changes)?;
        schedule.next_run_at = next_run_at(&schedule, seconds());
        let mut updated = list.clone();
        match updated.iter_mut().find(|other| other.id == schedule.id) {
            Some(slot) => *slot = schedule.clone(),
            None => updated.push(schedule.clone()),
        }
        self.persist_schedules(&updated, &schedule.agent)?;
        *list = updated;
        drop(list);
        self.broadcast(&self.schedules_json(None));
        Ok(schedule)
    }

    pub fn delete_schedule(&self, id: &str) -> Result<(), String> {
        self.delete_schedule_by(id, None)
    }

    /// Deletes a schedule; `by_agent` may delete only its own.
    pub fn delete_schedule_by(&self, id: &str, by_agent: Option<&str>) -> Result<(), String> {
        let mut list = lock(&self.schedules);
        let agent = list
            .iter()
            .find(|schedule| schedule.id == id)
            .filter(|schedule| by_agent.is_none_or(|agent| schedule.agent == agent))
            .map(|schedule| schedule.agent.clone())
            .ok_or_else(|| format!("unknown schedule {id}"))?;
        let mut updated = list.clone();
        updated.retain(|schedule| schedule.id != id);
        self.persist_schedules(&updated, &agent)?;
        *list = updated;
        drop(list);
        self.broadcast(&self.schedules_json(None));
        Ok(())
    }

    /// Runs a schedule now, enabled or not, without moving its next run.
    pub fn run_schedule(self: &Arc<Self>, id: &str) -> Result<(), String> {
        let at = seconds();
        {
            let mut list = lock(&self.schedules);
            let schedule = list
                .iter_mut()
                .find(|schedule| schedule.id == id)
                .ok_or_else(|| format!("unknown schedule {id}"))?;
            match self.agents.get(&schedule.agent) {
                None => return Err(format!("unknown agent {}", schedule.agent)),
                Some(agent) if !agent.enabled => {
                    return Err(format!("agent {} is disabled", agent.id))
                }
                Some(_) => {}
            }
            self.record_schedule_run(schedule, format!("schedule:{id}:run:{at}"))?;
            schedule.last_run_at = Some(at);
            let agent = schedule.agent.clone();
            self.persist_schedules(&list, &agent)?;
        }
        self.broadcast(&self.schedules_json(None));
        self.deliver_pending();
        Ok(())
    }

    /// Turns due schedules of enabled agents into messages. The message's
    /// dedupe key names the due time, so a schedule that fired just before a
    /// crash fires only once.
    pub(crate) fn fire_due_schedules(&self) {
        let at = seconds();
        let mut list = lock(&self.schedules);
        let mut fired: Vec<String> = Vec::new();
        for schedule in list.iter_mut() {
            let Some(due) = schedule.next_run_at.filter(|due| *due <= at) else {
                continue;
            };
            if !schedule.enabled
                || !self
                    .agents
                    .get(&schedule.agent)
                    .is_some_and(|agent| agent.enabled)
            {
                continue;
            }
            let key = format!("schedule:{}:{due}", schedule.id);
            if let Err(error) = self.record_schedule_run(schedule, key) {
                jucode_agent_core::log_warn!("daemon", "schedule not fired", error = error);
                continue;
            }
            schedule.last_run_at = Some(at);
            schedule.next_run_at = next_run_at(schedule, at);
            if !fired.contains(&schedule.agent) {
                fired.push(schedule.agent.clone());
            }
        }
        if fired.is_empty() {
            return;
        }
        for agent in &fired {
            if let Err(error) = self.persist_schedules(&list, agent) {
                jucode_agent_core::log_warn!("daemon", "schedules not saved", error = error);
            }
        }
        drop(list);
        self.broadcast(&self.schedules_json(None));
    }

    /// A run of schedule `id` reached `session`: later runs that continue
    /// the last session go there, and a run in a new session retires the
    /// earlier ones (see `retire_schedule_runs`).
    pub(crate) fn schedule_delivered(&self, id: &str, session: &str) {
        let mut list = lock(&self.schedules);
        let Some(schedule) = list.iter_mut().find(|schedule| schedule.id == id) else {
            return;
        };
        if schedule.last_session.as_deref() == Some(session) {
            return;
        }
        schedule.last_session = Some(session.to_string());
        let (agent, name) = (schedule.agent.clone(), schedule.name.clone());
        if let Err(error) = self.persist_schedules(&list, &agent) {
            jucode_agent_core::log_warn!("daemon", "schedules not saved", error = error);
        }
        drop(list);
        self.broadcast(&self.schedules_json(None));
        self.retire_schedule_runs(id, &name, session);
    }

    /// Records the message for one run: into the last run's session when
    /// the schedule continues it and the agent still owns it, else a new one
    /// told what the last run concluded.
    fn record_schedule_run(&self, schedule: &Schedule, dedupe_key: String) -> Result<(), String> {
        let session = schedule
            .last_session
            .clone()
            .filter(|_| !schedule.new_session)
            .filter(|session| {
                self.store.sessions().iter().any(|record| {
                    record.id == *session && record.agent.as_deref() == Some(&schedule.agent)
                })
            });
        let mut body = format!("定时任务「{}」：\n{}", schedule.name, schedule.prompt);
        if session.is_none() {
            let last = schedule
                .last_session
                .as_deref()
                .and_then(|last| self.agents.handoff(&schedule.agent, last));
            if let (Some(note), Some(at)) = (last, schedule.last_run_at) {
                let at = Local
                    .timestamp_opt(at as i64, 0)
                    .single()
                    .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                body.push_str(&format!("\n\n上次运行（{at}）的交接：\n{note}"));
            }
        }
        self.store
            .record_message(&Message {
                id: self.new_id("m"),
                to: schedule.agent.clone(),
                from: format!("schedule:{}", schedule.id),
                body,
                session,
                reply_to: None,
                dedupe_key: Some(dedupe_key),
                at: now(),
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Writes `agent`'s schedules from `list`.
    fn persist_schedules(&self, list: &[Schedule], agent: &str) -> Result<(), String> {
        let own: Vec<Value> = list
            .iter()
            .filter(|schedule| schedule.agent == agent)
            .map(Schedule::to_json)
            .collect();
        self.agents.save_schedules(agent, &own)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    /// Beijing time; 2026-10-01 is a Thursday.
    fn at(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_str(&format!("{text} +0800"), "%Y-%m-%d %H:%M %z").unwrap()
    }

    fn schedule(changes: Value) -> Schedule {
        let mut schedule = Schedule::new("sch-1".to_string(), "ops".to_string(), 0);
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
        ] {
            let mut schedule = Schedule::new("sch-1".to_string(), "ops".to_string(), 0);
            assert!(schedule.apply(&bad).is_err(), "{bad}");
        }
    }
}
