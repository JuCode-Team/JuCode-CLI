//! What each agent turn used: tokens per turn, whatever engine ran it.
//!
//! Records are kept in `usage.jsonl` with the project they ran in (that
//! never leaves this computer) and, while signed in to JuCode, uploaded
//! without it to the account (`POST /v1/oauth/agent-usage`) so every
//! computer on the account shows the same totals. JuCode gateway requests
//! carry the turn id (`X-JuCode-Turn`), so the gateway puts their cost and
//! group on the same turn.
//!
//! A turn opens at its first usage (or, for Claude Code and Codex, at the
//! user message, so the local gateway can tag its requests) and is written
//! when the session is ready again. The JuCode engine tags its own turns;
//! its usage events carry the tag as `turn`.

use crate::store::{now, random_hex, Store};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{Condvar, Mutex},
    time::Duration,
};

const FILE: &str = "usage.jsonl";
/// Byte offset in FILE up to which records have been uploaded.
const SENT_FILE: &str = "usage.sent";
/// Set once the desktop's older per-day counts were imported.
const LEGACY_SETTING: &str = "usage_legacy_imported";
const BATCH: usize = 200;
/// Upload at least this often while records wait (sooner after a turn).
const UPLOAD_EVERY: Duration = Duration::from_secs(60);
const TOKEN_FIELDS: [&str; 5] = [
    "input_tokens",
    "cached_input_tokens",
    "cache_write_tokens",
    "output_tokens",
    "reasoning_tokens",
];

/// The session facts a record needs, read from the session list.
pub struct SessionInfo {
    /// jucode, claude, codex or acp.
    pub engine: String,
    /// Claude / Codex through the JuCode gateway.
    pub gateway: bool,
    pub cwd: String,
}

#[derive(Default)]
struct State {
    /// Session → the provider and model its engine last reported.
    routes: HashMap<String, (String, String)>,
    /// Session → the turn the daemon opened for it (Claude, Codex, ACP).
    current: HashMap<String, String>,
    /// Turn id → its record so far.
    open: HashMap<String, Value>,
    /// A turn was written since the last upload.
    dirty: bool,
}

pub struct Usage {
    dir: PathBuf,
    state: Mutex<State>,
    /// Serializes appends to FILE.
    file: Mutex<()>,
    wake: Condvar,
}

impl Usage {
    pub fn new(store: &Store) -> Self {
        Self {
            dir: store.dir().to_path_buf(),
            state: Mutex::default(),
            file: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Takes one session event. `info` is read only for events that need it.
    pub fn observe(
        &self,
        session: &str,
        event: &Value,
        info: impl FnOnce() -> Option<SessionInfo>,
    ) {
        match event["type"].as_str() {
            Some("model_status") => {
                let text = |key: &str| event[key].as_str().unwrap_or_default().to_string();
                if !text("model").is_empty() || !text("provider").is_empty() {
                    self.lock()
                        .routes
                        .insert(session.to_string(), (text("provider"), text("model")));
                }
            }
            Some("user_message") => {
                let Some(info) = info() else { return };
                // The JuCode engine tags its own turns.
                if info.engine != "jucode" {
                    let mut state = self.lock();
                    if !state.current.contains_key(session) {
                        let turn = new_turn_id();
                        crate::gateway::set_turn(session, Some(&turn));
                        state.current.insert(session.to_string(), turn);
                    }
                }
            }
            Some("usage") => {
                let Some(info) = info() else { return };
                self.add(session, event, &info);
            }
            Some("status") if event["message"] == "ready" => self.close(session),
            _ => {}
        }
    }

    fn add(&self, session: &str, event: &Value, info: &SessionInfo) {
        let mut state = self.lock();
        let turn = match event["turn"].as_str().filter(|turn| !turn.is_empty()) {
            Some(turn) => turn.to_string(),
            None => match state.current.get(session) {
                Some(turn) => turn.clone(),
                None => {
                    let turn = new_turn_id();
                    if info.engine != "jucode" {
                        crate::gateway::set_turn(session, Some(&turn));
                    }
                    state.current.insert(session.to_string(), turn.clone());
                    turn
                }
            },
        };
        let (provider, model) = state.routes.get(session).cloned().unwrap_or_default();
        let (kind, channel) = channel(&info.engine, info.gateway, &provider);
        let record = state.open.entry(turn.clone()).or_insert_with(|| {
            json!({
                "turn_id": turn,
                "session": session,
                "engine": info.engine,
                "cwd": info.cwd,
                "started_at": now(),
                "requests": 0,
                "input_tokens": 0,
                "cached_input_tokens": 0,
                "cache_write_tokens": 0,
                "output_tokens": 0,
                "reasoning_tokens": 0,
            })
        });
        record["channel_kind"] = json!(kind);
        record["channel"] = json!(channel);
        record["model"] = json!(event["model"]
            .as_str()
            .filter(|m| !m.is_empty())
            .unwrap_or(&model));
        record["requests"] = json!(record["requests"].as_u64().unwrap_or(0) + 1);
        for field in TOKEN_FIELDS {
            let sum = record[field].as_u64().unwrap_or(0) + event[field].as_u64().unwrap_or(0);
            record[field] = json!(sum);
        }
    }

    /// The session is ready again: its turns are written.
    pub fn close(&self, session: &str) {
        let records: Vec<Value> = {
            let mut state = self.lock();
            if state.current.remove(session).is_some() {
                crate::gateway::set_turn(session, None);
            }
            let turns: Vec<String> = state
                .open
                .iter()
                .filter(|(_, record)| record["session"] == session)
                .map(|(turn, _)| turn.clone())
                .collect();
            turns
                .iter()
                .filter_map(|turn| state.open.remove(turn))
                .collect()
        };
        if records.is_empty() {
            return;
        }
        // Records made while signed in go to that account; the rest stay here.
        let upload = jucode_agent_core::jucode_signed_in();
        let ended = now();
        let lines: String = records
            .into_iter()
            .filter(|record| {
                TOKEN_FIELDS
                    .iter()
                    .any(|f| record[*f].as_u64().unwrap_or(0) > 0)
            })
            .map(|mut record| {
                record["ended_at"] = json!(ended);
                record["upload"] = json!(upload);
                format!("{record}\n")
            })
            .collect();
        if let Err(error) = self.append(&lines) {
            jucode_agent_core::log_warn!("usage", "cannot record usage", error = error.to_string());
            return;
        }
        if upload {
            self.lock().dirty = true;
            self.wake.notify_all();
        }
    }

    fn append(&self, lines: &str) -> std::io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let _guard = self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        fs::create_dir_all(&self.dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(FILE))?;
        file.write_all(lines.as_bytes())
    }

    fn records(&self) -> Vec<Value> {
        let _guard = self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Ok(file) = fs::File::open(self.dir.join(FILE)) else {
            return Vec::new();
        };
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect()
    }

    /// Imports the desktop's older per-day counts once (`days`: day →
    /// `{in, out, prov, models, agents}`). False when already imported.
    pub fn import_legacy(&self, store: &Store, days: &Value) -> Result<bool, String> {
        if store.setting(LEGACY_SETTING) == json!(true) {
            return Ok(false);
        }
        let lines: String = days
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(day, _)| chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok())
            .map(|(day, usage)| {
                let line = json!({
                    "legacy": true,
                    "day": day,
                    "input_tokens": usage["in"].as_u64().unwrap_or(0),
                    "output_tokens": usage["out"].as_u64().unwrap_or(0),
                    "prov": usage["prov"],
                    "models": usage["models"],
                    "agents": usage["agents"],
                });
                format!("{line}\n")
            })
            .collect();
        self.append(&lines).map_err(|error| error.to_string())?;
        store
            .set_setting(LEGACY_SETTING, json!(true))
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    /// This computer's usage over the last `days` days (local days at
    /// `tz_offset` minutes, as JavaScript's getTimezoneOffset), with the
    /// project each turn ran in.
    pub fn local_json(&self, days: u64, tz_offset: i64) -> Value {
        local_summary(&self.records(), days, tz_offset, now())
    }

    /// Uploads waiting records until the process exits.
    pub fn run_uploads(&self) {
        loop {
            {
                let state = self.lock();
                if !state.dirty {
                    let _ = self.wake.wait_timeout(state, UPLOAD_EVERY);
                }
            }
            self.lock().dirty = false;
            while let Ok(true) = self.upload_batch() {}
        }
    }

    /// Sends the next batch; Ok(true) when more may wait.
    fn upload_batch(&self) -> Result<bool, String> {
        let sent_path = self.dir.join(SENT_FILE);
        let offset: u64 = fs::read_to_string(&sent_path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        let (turns, end) = {
            let _guard = self
                .file
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let Ok(mut file) = fs::File::open(self.dir.join(FILE)) else {
                return Ok(false);
            };
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| error.to_string())?;
            let mut text = String::new();
            file.read_to_string(&mut text)
                .map_err(|error| error.to_string())?;
            pending(&text, offset)
        };
        if end == offset {
            return Ok(false);
        }
        if !turns.is_empty() {
            let (api, token) = jucode_agent_core::jucode_gateway_token()?;
            let url = format!("{}/v1/oauth/agent-usage", api.trim_end_matches('/'));
            match ureq::post(&url)
                .timeout(Duration::from_secs(30))
                .set("Authorization", &format!("Bearer {token}"))
                .send_json(json!({ "turns": turns }))
            {
                Ok(_) => {}
                // The gateway refused these records for good: skip them.
                Err(ureq::Error::Status(400, response)) => {
                    jucode_agent_core::log_warn!(
                        "usage",
                        "usage upload rejected",
                        error = response.into_string().unwrap_or_default()
                    );
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        fs::write(&sent_path, end.to_string()).map_err(|error| error.to_string())?;
        Ok(true)
    }
}

/// The next batch to upload from `text` (FILE from `offset`): the upload
/// bodies and the offset after the last line it covers. Records kept local
/// are passed over.
fn pending(text: &str, offset: u64) -> (Vec<Value>, u64) {
    let mut turns = Vec::new();
    let mut end = offset;
    for line in text.split_inclusive('\n') {
        if !line.ends_with('\n') || turns.len() >= BATCH {
            break;
        }
        end += line.len() as u64;
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record["upload"] != true {
            continue;
        }
        let mut turn = json!({
            "turn_id": record["turn_id"],
            "session_id": record["session"],
            "engine": record["engine"],
            "channel_kind": record["channel_kind"],
            "channel": record["channel"],
            "model": record["model"],
            "requests": record["requests"],
            "started_at": record["started_at"],
            "ended_at": record["ended_at"],
        });
        for field in TOKEN_FIELDS {
            turn[field] = json!(record[field].as_u64().unwrap_or(0));
        }
        turns.push(turn);
    }
    (turns, end)
}

/// The channel a turn ran on: (kind, id). See `provider_channel_kind`.
fn channel(engine: &str, gateway: bool, provider: &str) -> (&'static str, String) {
    match engine {
        "jucode" => (
            jucode_agent_core::provider_channel_kind(provider),
            provider.to_string(),
        ),
        "claude" if gateway => ("jucode", "jucode".to_string()),
        "claude" => ("local", "anthropic".to_string()),
        "codex" if provider == "jucode_gateway" || (gateway && provider.is_empty()) => {
            ("jucode", "jucode".to_string())
        }
        "codex" => (
            "local",
            if provider.is_empty() {
                "openai"
            } else {
                provider
            }
            .to_string(),
        ),
        _ => ("local", engine.to_string()),
    }
}

fn new_turn_id() -> String {
    format!("t-{}", random_hex(12).unwrap_or_default())
}

#[derive(Default)]
struct Sums {
    tokens: [u64; 5],
    turns: u64,
}

impl Sums {
    fn add(&mut self, record: &Value) {
        for (sum, field) in self.tokens.iter_mut().zip(TOKEN_FIELDS) {
            *sum += record[field].as_u64().unwrap_or(0);
        }
    }

    fn add_in_out(&mut self, usage: &Value) {
        self.tokens[0] += usage["in"].as_u64().unwrap_or(0);
        self.tokens[3] += usage["out"].as_u64().unwrap_or(0);
    }

    fn json(&self, mut row: Value) -> Value {
        for (sum, field) in self.tokens.iter().zip(TOKEN_FIELDS) {
            row[field] = json!(sum);
        }
        row["turns"] = json!(self.turns);
        row
    }
}

fn local_summary(records: &[Value], days: u64, tz_offset: i64, now_ms: u64) -> Value {
    let day_of = |ms: u64| {
        chrono::DateTime::from_timestamp_millis(ms as i64 - tz_offset * 60_000)
            .map(|time| time.date_naive())
    };
    let Some(today) = day_of(now_ms) else {
        return json!({ "type": "usage_local" });
    };
    let first = today - chrono::Days::new(days.clamp(1, 400) - 1);
    let mut totals = Sums::default();
    let mut by_day: BTreeMap<String, Sums> = BTreeMap::new();
    let mut by_project: BTreeMap<String, Sums> = BTreeMap::new();
    let mut by_channel: BTreeMap<(String, String), Sums> = BTreeMap::new();
    let mut by_model: BTreeMap<String, Sums> = BTreeMap::new();
    let mut by_engine: BTreeMap<String, Sums> = BTreeMap::new();
    for record in records {
        if record["legacy"] == true {
            let Some(day) = record["day"]
                .as_str()
                .and_then(|day| chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok())
                .filter(|day| *day >= first && *day <= today)
            else {
                continue;
            };
            let in_out = json!({ "in": record["input_tokens"], "out": record["output_tokens"] });
            totals.add_in_out(&in_out);
            by_day
                .entry(day.to_string())
                .or_default()
                .add_in_out(&in_out);
            by_project
                .entry(String::new())
                .or_default()
                .add_in_out(&in_out);
            for (provider, usage) in record["prov"].as_object().into_iter().flatten() {
                let key = match provider.as_str() {
                    "jucode" | "jucode_gateway" => ("jucode".to_string(), "jucode".to_string()),
                    // Claude through the gateway and on its own login alike.
                    "anthropic" => ("legacy".to_string(), "anthropic".to_string()),
                    other => (
                        jucode_agent_core::provider_channel_kind(other).to_string(),
                        other.to_string(),
                    ),
                };
                by_channel.entry(key).or_default().add_in_out(usage);
            }
            for (model, usage) in record["models"].as_object().into_iter().flatten() {
                by_model.entry(model.clone()).or_default().add_in_out(usage);
            }
            for (agent, usage) in record["agents"].as_object().into_iter().flatten() {
                let engine = agent.split(':').next().unwrap_or(agent).to_string();
                by_engine.entry(engine).or_default().add_in_out(usage);
            }
            continue;
        }
        let Some(day) = record["started_at"]
            .as_u64()
            .and_then(day_of)
            .filter(|day| *day >= first && *day <= today)
        else {
            continue;
        };
        let text = |key: &str| record[key].as_str().unwrap_or_default().to_string();
        let groups = [
            &mut totals,
            by_day.entry(day.to_string()).or_default(),
            by_project.entry(text("cwd")).or_default(),
            by_channel
                .entry((text("channel_kind"), text("channel")))
                .or_default(),
            by_model.entry(text("model")).or_default(),
            by_engine.entry(text("engine")).or_default(),
        ];
        for sums in groups {
            sums.add(record);
            sums.turns += 1;
        }
    }
    let rows = |map: BTreeMap<String, Sums>, key: &str| -> Vec<Value> {
        map.into_iter()
            .map(|(name, sums)| sums.json(json!({ key: name })))
            .collect()
    };
    json!({
        "type": "usage_local",
        "totals": totals.json(json!({})),
        "days": rows(by_day, "day"),
        "by_project": rows(by_project, "cwd"),
        "by_channel": by_channel
            .into_iter()
            .map(|((kind, channel), sums)| sums.json(json!({ "channel_kind": kind, "channel": channel })))
            .collect::<Vec<_>>(),
        "by_model": rows(by_model, "model"),
        "by_engine": rows(by_engine, "engine"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spend(input: u64, output: u64) -> Value {
        json!({ "type": "usage", "input_tokens": input, "cached_input_tokens": 1, "output_tokens": output })
    }

    fn info(engine: &str, gateway: bool) -> Option<SessionInfo> {
        Some(SessionInfo {
            engine: engine.to_string(),
            gateway,
            cwd: "/p".to_string(),
        })
    }

    fn usage_in(name: &str) -> (Usage, PathBuf) {
        let dir = std::env::temp_dir().join(format!("jucode-usage-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let usage = Usage {
            dir: dir.clone(),
            state: Mutex::default(),
            file: Mutex::new(()),
            wake: Condvar::new(),
        };
        (usage, dir)
    }

    #[test]
    fn a_turn_sums_its_requests_and_is_written_when_ready() {
        let (usage, _dir) = usage_in("turn");
        usage.observe(
            "s1",
            &json!({ "type": "model_status", "provider": "jucode_gateway", "model": "gpt-5.5" }),
            || None,
        );
        usage.observe(
            "s1",
            &json!({ "type": "user_message", "content": "hi" }),
            || info("codex", true),
        );
        usage.observe("s1", &spend(10, 2), || info("codex", true));
        usage.observe("s1", &spend(5, 3), || info("codex", true));
        assert!(usage.records().is_empty());
        usage.observe(
            "s1",
            &json!({ "type": "status", "message": "ready" }),
            || None,
        );
        let records = usage.records();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record["input_tokens"], 15);
        assert_eq!(record["cached_input_tokens"], 2);
        assert_eq!(record["output_tokens"], 5);
        assert_eq!(record["requests"], 2);
        assert_eq!(record["channel_kind"], "jucode");
        assert_eq!(record["model"], "gpt-5.5");
        assert_eq!(record["cwd"], "/p");
    }

    #[test]
    fn jucode_engine_turns_are_keyed_by_the_engine_tag() {
        let (usage, _dir) = usage_in("tag");
        let tagged = |turn: &str| json!({ "type": "usage", "turn": turn, "input_tokens": 4, "output_tokens": 1 });
        usage.observe("s", &tagged("t-a"), || info("jucode", false));
        usage.observe("s", &tagged("t-b"), || info("jucode", false));
        usage.observe(
            "s",
            &json!({ "type": "status", "message": "ready" }),
            || None,
        );
        let ids: Vec<Value> = usage
            .records()
            .iter()
            .map(|r| r["turn_id"].clone())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&json!("t-a")) && ids.contains(&json!("t-b")));
    }

    #[test]
    fn channels_follow_the_route() {
        assert_eq!(channel("claude", true, "anthropic").0, "jucode");
        assert_eq!(channel("claude", false, "anthropic").0, "local");
        assert_eq!(channel("codex", false, "openai").0, "local");
        assert_eq!(channel("codex", true, "jucode_gateway").0, "jucode");
        assert_eq!(channel("jucode", false, "jucode").0, "jucode");
        assert_eq!(channel("jucode", false, "kimi-code").0, "third_party");
        assert_eq!(channel("jucode", false, "my-relay").0, "local");
        assert_eq!(channel("acp", false, "").0, "local");
    }

    #[test]
    fn uploads_skip_local_records_and_stop_at_a_partial_line() {
        let text = concat!(
            "{\"turn_id\":\"t-1\",\"upload\":true,\"input_tokens\":3,\"cwd\":\"/secret\"}\n",
            "{\"turn_id\":\"t-2\",\"upload\":false}\n",
            "{\"legacy\":true}\n",
            "{\"turn_id\":\"t-3\",\"upl"
        );
        let (turns, end) = pending(text, 7);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["turn_id"], "t-1");
        assert_eq!(turns[0]["input_tokens"], 3);
        assert!(turns[0].get("cwd").is_none());
        let complete = text.rfind('\n').unwrap() + 1;
        assert_eq!(end, 7 + complete as u64);
    }

    #[test]
    fn the_local_summary_splits_by_day_project_and_channel() {
        let day_ms = 86_400_000;
        let now_ms = 20_000 * day_ms + 3_600_000;
        let yesterday = chrono::DateTime::from_timestamp_millis((now_ms - day_ms) as i64)
            .unwrap()
            .date_naive()
            .to_string();
        let records = vec![
            json!({ "started_at": now_ms, "cwd": "/a", "channel_kind": "jucode", "channel": "jucode", "model": "m", "engine": "jucode", "input_tokens": 10, "output_tokens": 1 }),
            json!({ "started_at": now_ms - day_ms, "cwd": "/b", "channel_kind": "local", "channel": "anthropic", "model": "c", "engine": "claude", "input_tokens": 5, "output_tokens": 2 }),
            json!({ "started_at": now_ms - 40 * day_ms, "cwd": "/old", "input_tokens": 99 }),
            json!({ "legacy": true, "day": yesterday, "input_tokens": 7, "output_tokens": 0, "prov": { "jucode_gateway": { "in": 4, "out": 0 }, "anthropic": { "in": 3, "out": 0 } } }),
        ];
        let summary = local_summary(&records, 30, 0, now_ms);
        assert_eq!(summary["totals"]["input_tokens"], 15 + 7);
        assert_eq!(summary["totals"]["turns"], 2);
        assert_eq!(summary["by_project"].as_array().unwrap().len(), 3);
        let channels = summary["by_channel"].as_array().unwrap();
        assert!(channels
            .iter()
            .any(|c| c["channel_kind"] == "legacy" && c["input_tokens"] == 3));
        let jucode = channels
            .iter()
            .find(|c| c["channel_kind"] == "jucode")
            .unwrap();
        assert_eq!(jucode["input_tokens"], 14);
    }
}
