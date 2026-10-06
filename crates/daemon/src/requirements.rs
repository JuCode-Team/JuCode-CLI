//! Requirements: what the user means to get done, kept across the sessions
//! that work on it. A requirement is the user's own words (and screenshots),
//! the project it belongs to (or none), the sessions linked to it, and a
//! progress record the title model rewrites after each of their turns, so
//! the next session picks up where the last one stopped.
//!
//! Screenshots are uploads (see `uploads`) moved into
//! `~/.jucode/uploads/requirements/<id>/` when the requirement is noted.
//!
//! Its state is the user's: idea, open, done or parked, or proposed while an
//! agent's proposal to note it waits for the user. While it is open, what it
//! shows follows its sessions: one waits for an approval, the session at the
//! start gate (`gate`) waits for the user to confirm its understanding or
//! plan, the latest one failed, one is running, or else it is the user's
//! turn ("review": the work is done, or the agent asks something in its
//! reply). An agent's proposal to close it shows as "proposal". Turning to
//! the user's turn notifies the paired phones.
//!
//! Work starts behind the gate: the session first only reads and explains
//! what it understood (read-only), then, if the user asked for one, writes a
//! plan (still read-only); each step waits for `requirement_confirm`, and
//! only the last one switches to the permission mode the user picked.

use crate::{
    engines,
    hub::{lock, Hub},
    projects,
    store::{now, write_private},
    titles,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

const FILE: &str = "requirements.json";
const MAX_IMAGES: usize = 4;
/// Words up to this long are their own title; longer ones get a model title.
const TITLE_CHARS: usize = 40;
/// How much of the latest reply a client and a notification get.
const REPLY_LIMIT: usize = 600;
/// Bounds on the progress record the model writes.
const ITEMS: usize = 8;
const ITEM_CHARS: usize = 300;
const STATES: [&str; 5] = ["idea", "open", "done", "parked", "proposed"];
const SECTIONS: [&str; 6] = ["decided", "done", "doing", "blocked", "next", "files"];

pub const PROGRESS_SYSTEM: &str = "You keep the progress record of one requirement that a user \
works on with coding agents, across several sessions. Reply with JSON only, no code fence: \
{\"goal\": string, \"decided\": [string], \"done\": [string], \"doing\": [string], \
\"blocked\": [string], \"next\": [string], \"files\": [string]}. Write in the language of the \
requirement. goal: one sentence on what the requirement is for. decided: decisions the user made \
or accepted, including suggestions they turned down (say so); keep earlier decisions unless the \
conversation reverses them. done: finished work. doing: work in progress. blocked: what stops \
progress. next: what happens next and who acts, the user or the agent. files: relevant file \
paths. At most 6 short items per list. Update the previous record with the latest session \
excerpt; where they conflict, the excerpt wins.";

/// What the daemon saw a session do last.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
struct Live {
    running: bool,
    waiting: bool,
    failed: bool,
}

struct Data {
    next: u64,
    list: Vec<Value>,
}

pub struct Requirements {
    path: PathBuf,
    images: PathBuf,
    data: Mutex<Data>,
    live: Mutex<HashMap<String, Live>>,
    /// Requirements whose progress is being written, with a session whose
    /// turn ended meanwhile (written next).
    writing: Mutex<HashMap<String, Option<String>>>,
}

impl Requirements {
    /// `dir`: the daemon's state; `images`: where screenshots are kept;
    /// `workspaces`: the saved workspaces, to migrate requirements from
    /// before projects had ids.
    pub fn load(dir: &Path, images: PathBuf, workspaces: &Value) -> Self {
        let path = dir.join(FILE);
        let saved = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or_default();
        let mut list = saved["requirements"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let migrated = migrate(&mut list, workspaces);
        let next = saved["next"].as_u64().unwrap_or(1);
        let reqs = Self {
            path,
            images,
            data: Mutex::new(Data { next, list }),
            live: Mutex::new(HashMap::new()),
            writing: Mutex::new(HashMap::new()),
        };
        if migrated {
            if let Err(error) = reqs.save(&lock(&reqs.data)) {
                jucode_agent_core::log_warn!("daemon", "requirements migration", error = error);
            }
        }
        reqs
    }

    /// The session's turn starts now (a message is on its way to it).
    fn mark_running(&self, session: &str) {
        lock(&self.live).insert(
            session.to_string(),
            Live {
                running: true,
                ..Live::default()
            },
        );
    }

    /// The list for clients, each with its shown `status`, its sessions'
    /// states and, on the user's turn, the end of the latest reply.
    pub fn json(&self, hub: &Hub) -> Value {
        let list = lock(&self.data).list.clone();
        let live = lock(&self.live).clone();
        let list: Vec<Value> = list
            .into_iter()
            .map(|mut r| {
                let status = status(&r, &live);
                let states: serde_json::Map<String, Value> = sessions(&r)
                    .map(|s| (s.to_string(), json!(session_state(live.get(s)))))
                    .collect();
                if matches!(status, "review" | "failed" | "confirm") {
                    if let Some(reply) = sessions(&r).last().and_then(|s| hub.last_reply(s)) {
                        r["last_reply"] = json!(tail(&reply, REPLY_LIMIT));
                    }
                }
                r["status"] = json!(status);
                r["session_states"] = Value::Object(states);
                r
            })
            .collect();
        json!({ "type": "requirements", "requirements": list })
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        lock(&self.data)
            .list
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
    }

    /// Applies `change` to requirement `id` and saves.
    fn update<T>(
        &self,
        id: &str,
        change: impl FnOnce(&mut Value) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut data = lock(&self.data);
        let r = data
            .list
            .iter_mut()
            .find(|r| r["id"] == id)
            .ok_or_else(|| format!("unknown requirement {id}"))?;
        let result = change(r)?;
        r["updated_at"] = json!(now());
        self.save(&data)?;
        Ok(result)
    }

    fn save(&self, data: &Data) -> Result<(), String> {
        let saved = json!({ "next": data.next, "requirements": data.list });
        write_private(&self.path, format!("{saved:#}\n").as_bytes())
            .map_err(|error| error.to_string())
    }
}

/// `requirement_create`: the user's words, the project it belongs to (one
/// noted in a session belongs to that session's project; `null`: none) and
/// screenshots (paths of finished uploads).
pub fn create(hub: &Arc<Hub>, op: &Value) -> Result<Value, String> {
    let words = text(&op["text"]).trim().to_string();
    if words.is_empty() {
        return Err("requirement_create requires text".to_string());
    }
    let from = op["session"].as_str();
    let project = match op.get("project") {
        Some(project) => project_id(hub, project)?,
        None => from
            .and_then(|s| record(hub, s))
            .and_then(|record| projects::project_of_cwd(hub, &record.cwd))
            .map_or(Value::Null, |project| project["id"].clone()),
    };
    let images = paths(&op["images"])
        .iter()
        .map(|path| {
            let upload = hub.uploads.finished(path)?;
            if !crate::uploads::is_image(&upload) {
                return Err(format!("not an image: {path}"));
            }
            Ok(upload)
        })
        .collect::<Result<Vec<_>, String>>()?;
    if images.len() > MAX_IMAGES {
        return Err(format!("at most {MAX_IMAGES} images"));
    }
    let source = match from {
        Some(_) => "session",
        None if op["source"] == "phone" => "phone",
        None => "desktop",
    };
    let r = add(
        hub,
        &words,
        &images,
        json!({ "project": project, "source": source, "source_session": from }),
    )?;
    Ok(json!({ "type": "requirement_created", "requirement": r }))
}

/// Notes a new requirement of `words` and `images`, with `fields` (project,
/// source, …) over the defaults, and broadcasts the list.
fn add(hub: &Arc<Hub>, words: &str, images: &[PathBuf], fields: Value) -> Result<Value, String> {
    let reqs = &hub.requirements;
    let r = {
        let mut data = lock(&reqs.data);
        let id = format!("R-{}", data.next);
        let saved = keep_images(&reqs.images.join(&id), images)?;
        let (title, long) = first_line(words);
        let mut r = json!({
            "id": id,
            "text": words,
            "title": title,
            "title_auto": long,
            "images": saved,
            "project": null,
            "state": "idea",
            "sessions": [],
            "progress": null,
            "created_at": now(),
            "updated_at": now(),
        });
        for (key, value) in fields.as_object().into_iter().flatten() {
            r[key] = value.clone();
        }
        data.next += 1;
        data.list.insert(0, r.clone());
        reqs.save(&data)?;
        r
    };
    if r["title_auto"] == true {
        retitle(hub, text(&r["id"]), words);
    }
    hub.broadcast(&reqs.json(hub));
    Ok(r)
}

/// A project id a client gave: a known project's, or null for none.
fn project_id(hub: &Hub, value: &Value) -> Result<Value, String> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::String(id) if projects::project(hub, id).is_some() => Ok(json!(id)),
        Value::String(id) => Err(format!("unknown project {id}")),
        _ => Err("project must be a project id or null".to_string()),
    }
}

/// `requirement_update`: the user's words, project or state.
pub fn update(hub: &Arc<Hub>, op: &Value) -> Result<Value, String> {
    let id = text(&op["requirement"]);
    let project = op.get("project").map(|p| project_id(hub, p)).transpose()?;
    let r = hub.requirements.update(id, |r| {
        if let Some(words) = op["text"].as_str() {
            let words = words.trim();
            if words.is_empty() {
                return Err("a requirement needs text".to_string());
            }
            if words != text(&r["text"]) {
                let (title, _) = first_line(words);
                r["text"] = json!(words);
                r["title"] = json!(title);
                r["title_auto"] = json!(false);
            }
        }
        if let Some(project) = project {
            r["project"] = project;
        }
        if let Some(state) = op["state"].as_str() {
            if !STATES.contains(&state) {
                return Err(format!("unknown state {state}"));
            }
            r["state"] = json!(state);
        }
        Ok(r.clone())
    })?;
    hub.broadcast(&hub.requirements.json(hub));
    Ok(json!({ "type": "requirement_saved", "requirement": r }))
}

/// `requirement_delete`: the requirement and its screenshots; its sessions
/// stay.
pub fn delete(hub: &Arc<Hub>, id: &str) -> Result<Value, String> {
    let reqs = &hub.requirements;
    {
        let mut data = lock(&reqs.data);
        let before = data.list.len();
        data.list.retain(|r| r["id"] != id);
        if data.list.len() == before {
            return Err(format!("unknown requirement {id}"));
        }
        reqs.save(&data)?;
    }
    let _ = fs::remove_dir_all(reqs.images.join(id));
    hub.broadcast(&reqs.json(hub));
    Ok(json!({ "type": "requirement_deleted", "requirement": id }))
}

/// `requirement_link`: `session` works on requirement `id` from now on (and
/// on no other); a requirement that was not open is again.
pub fn link(hub: &Arc<Hub>, id: &str, session: &str) -> Result<(), String> {
    if record(hub, session).is_none() {
        return Err(format!("unknown session {session}"));
    }
    let reqs = &hub.requirements;
    {
        let mut data = lock(&reqs.data);
        if !data.list.iter().any(|r| r["id"] == id) {
            return Err(format!("unknown requirement {id}"));
        }
        for r in data.list.iter_mut() {
            let mut list: Vec<Value> = sessions(r)
                .filter(|s| *s != session)
                .map(|s| json!(s))
                .collect();
            if r["id"] == id {
                list.push(json!(session));
                r["state"] = json!("open");
                r["updated_at"] = json!(now());
            } else if r["gate"]["session"] == session {
                remove(r, "gate");
            }
            r["sessions"] = Value::Array(list);
        }
        reqs.save(&data)?;
    }
    hub.broadcast(&reqs.json(hub));
    Ok(())
}

/// `requirement_unlink`: the session no longer works on the requirement.
pub fn unlink(hub: &Arc<Hub>, id: &str, session: &str) -> Result<(), String> {
    hub.requirements.update(id, |r| {
        let list: Vec<Value> = sessions(r)
            .filter(|s| *s != session)
            .map(|s| json!(s))
            .collect();
        r["sessions"] = Value::Array(list);
        if r["gate"]["session"] == session {
            remove(r, "gate");
        }
        Ok(())
    })?;
    hub.broadcast(&hub.requirements.json(hub));
    Ok(())
}

/// `requirement_image`: screenshot `index` as a data URL.
pub fn image(hub: &Hub, id: &str, index: usize) -> Result<Value, String> {
    let r = hub
        .requirements
        .get(id)
        .ok_or_else(|| format!("unknown requirement {id}"))?;
    let path = r["images"][index]
        .as_str()
        .ok_or_else(|| format!("{id} has no image {index}"))?;
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let mime = match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/png",
    };
    Ok(json!({
        "type": "requirement_image",
        "requirement": id,
        "index": index,
        "data": format!("data:{mime};base64,{}", STANDARD.encode(bytes)),
    }))
}

/// `requirement_reply`: sends `text` to the requirement's latest session, or
/// starts a new one (asked for, or none yet) in `cwd` on `engine` (default:
/// the latest session's) behind the start gate (`plan`, `mode`: see
/// `begin`).
pub fn reply(hub: &Arc<Hub>, op: &Value) -> Result<Value, String> {
    let id = text(&op["requirement"]);
    let r = hub
        .requirements
        .get(id)
        .ok_or_else(|| format!("unknown requirement {id}"))?;
    let feedback = text(&op["text"]).trim().to_string();
    let latest = sessions(&r).last().and_then(|s| record(hub, s));
    let target = if let Some(latest) = latest.as_ref().filter(|_| op["new_session"] != true) {
        if feedback.is_empty() {
            return Err("requirement_reply requires text".to_string());
        }
        let engine = engines::Kind::parse(latest.engine.as_deref().unwrap_or_default())?;
        hub.open_engine_session(&latest.id, None, engine, engines::Options::default())?;
        hub.send_to_session(&latest.id, &feedback)?;
        if r["state"] != "open" {
            hub.requirements.update(id, |r| {
                r["state"] = json!("open");
                Ok(())
            })?;
            hub.broadcast(&hub.requirements.json(hub));
        }
        latest.id.clone()
    } else {
        let mode = op["mode"].as_str().unwrap_or("auto");
        engine_mode(None, mode)?;
        let cwd = op["cwd"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| latest.as_ref().map(|l| l.cwd.clone()))
            .or_else(|| {
                let project = projects::project(hub, r["project"].as_str()?)?;
                project["path"].as_str().map(PathBuf::from)
            })
            .ok_or("需求未归属项目，请先选择")?;
        let engine = match op["engine"].as_str() {
            Some(name) => engines::Kind::parse(name)?,
            None => engines::Kind::parse(
                latest
                    .as_ref()
                    .and_then(|l| l.engine.as_deref())
                    .unwrap_or_default(),
            )?,
        };
        let session =
            hub.create_engine_session(Some(cwd), None, false, engine, engines::Options::default())?;
        let gate = Gate {
            plan: op["plan"] == true,
            mode,
            text: &feedback,
            lang: text(&op["lang"]),
        };
        start_gate(hub, id, &session, &gate)?;
        session
    };
    Ok(json!({ "type": "requirement_replied", "requirement": id, "session": target }))
}

/// How work on a requirement starts: with a plan step or not, the
/// permission mode once confirmed, and the user's words for the session.
struct Gate<'a> {
    plan: bool,
    mode: &'a str,
    text: &'a str,
    lang: &'a str,
}

/// `requirement_begin`: `session` starts on the requirement behind the gate.
pub fn begin(hub: &Arc<Hub>, op: &Value) -> Result<Value, String> {
    let id = text(&op["requirement"]);
    let session = op["session"]
        .as_str()
        .ok_or("requirement_begin requires session")?;
    let gate = Gate {
        plan: op["plan"] == true,
        mode: op["mode"]
            .as_str()
            .ok_or("requirement_begin requires mode")?,
        text: text(&op["text"]).trim(),
        lang: text(&op["lang"]),
    };
    start_gate(hub, id, session, &gate)?;
    Ok(json!({ "type": "requirement_begun", "requirement": id, "session": session }))
}

/// Links `session` to requirement `id`, switches it to read-only and asks
/// it to explain what it understood.
fn start_gate(hub: &Arc<Hub>, id: &str, session: &str, gate: &Gate) -> Result<(), String> {
    engine_mode(None, gate.mode)?;
    let engine = open(hub, session)?;
    let reqs = &hub.requirements;
    let r = reqs.update(id, |r| {
        r["gate"] = json!({
            "session": session,
            "stage": "understand",
            "plan": gate.plan,
            "mode": gate.mode,
            "answered": false,
        });
        Ok(r.clone())
    })?;
    reqs.mark_running(session);
    link(hub, id, session)?;
    let read_only = engine_mode(engine, "ask")?;
    hub.forward(
        session,
        json!({ "op": "set_approval_mode", "mode": read_only }),
    )?;
    hub.send_to_session(session, &understand_prompt(&r, gate.text, gate.lang))
}

/// `requirement_confirm`: the user accepts what the gate's session
/// understood (it plans next when a plan was asked for) or its plan; the
/// last step switches it to the chosen mode and starts the work.
pub fn confirm(hub: &Arc<Hub>, op: &Value) -> Result<Value, String> {
    let id = text(&op["requirement"]);
    let reqs = &hub.requirements;
    let r = reqs
        .get(id)
        .ok_or_else(|| format!("unknown requirement {id}"))?;
    let gate = &r["gate"];
    let session = gate["session"]
        .as_str()
        .ok_or_else(|| format!("{id} waits for no confirmation"))?;
    if hub.is_busy(session) {
        return Err(format!("session {session} is still running a turn"));
    }
    let engine = open(hub, session)?;
    let (extra, lang) = (text(&op["text"]).trim(), text(&op["lang"]));
    let planned = gate["stage"] == "plan";
    let stage = if !planned && gate["plan"] == true {
        reqs.update(id, |r| {
            r["gate"]["stage"] = json!("plan");
            r["gate"]["answered"] = json!(false);
            Ok(())
        })?;
        reqs.mark_running(session);
        hub.send_to_session(session, &plan_prompt(extra, lang))?;
        "plan"
    } else {
        let mode = engine_mode(engine, text(&gate["mode"]))?;
        hub.forward(session, json!({ "op": "set_approval_mode", "mode": mode }))?;
        reqs.update(id, |r| {
            remove(r, "gate");
            Ok(())
        })?;
        reqs.mark_running(session);
        hub.send_to_session(session, &go_prompt(planned, extra, lang))?;
        "go"
    };
    hub.broadcast(&reqs.json(hub));
    Ok(json!({ "type": "requirement_confirmed", "requirement": id, "stage": stage }))
}

/// `requirement_proposal`: the user accepts or turns down an agent's
/// proposal to note the requirement or to close it.
pub fn decide_proposal(hub: &Arc<Hub>, op: &Value) -> Result<Value, String> {
    let id = text(&op["requirement"]);
    let accept = op["accept"]
        .as_bool()
        .ok_or("requirement_proposal requires accept")?;
    let r = hub
        .requirements
        .get(id)
        .ok_or_else(|| format!("unknown requirement {id}"))?;
    let proposal = &r["proposal"];
    match (text(&proposal["kind"]), accept) {
        ("create", false) => {
            delete(hub, id)?;
        }
        ("create", true) | ("close", false) => {
            hub.requirements.update(id, |r| {
                if r["state"] == "proposed" {
                    r["state"] = json!("idea");
                }
                remove(r, "proposal");
                Ok(())
            })?;
        }
        ("close", true) => {
            let outcome = match text(&proposal["outcome"]) {
                "parked" => "parked",
                _ => "done",
            };
            hub.requirements.update(id, |r| {
                r["state"] = json!(outcome);
                remove(r, "proposal");
                remove(r, "gate");
                Ok(())
            })?;
        }
        _ => return Err(format!("{id} has no proposal")),
    }
    hub.broadcast(&hub.requirements.json(hub));
    Ok(json!({ "type": "requirement_proposal_decided", "requirement": id, "accept": accept }))
}

/// Hosts `session` if it is not (after a restart); returns its engine
/// (None: jucode).
fn open(hub: &Arc<Hub>, session: &str) -> Result<Option<engines::Kind>, String> {
    let record = record(hub, session).ok_or_else(|| format!("unknown session {session}"))?;
    let engine = engines::Kind::parse(record.engine.as_deref().unwrap_or_default())?;
    hub.open_engine_session(session, None, engine, engines::Options::default())?;
    Ok(engine)
}

/// A permission mode, in a client's names (`ask`, `plan`, `auto`, `edits`,
/// `all`) or an engine's, as `engine` (None: jucode) takes it.
fn engine_mode(engine: Option<engines::Kind>, mode: &str) -> Result<&'static str, String> {
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

fn remove(r: &mut Value, key: &str) {
    if let Some(map) = r.as_object_mut() {
        map.remove(key);
    }
}

/// A session event (see `Hub::observe`): the shown status of the open
/// requirements it works on may change, and a turn that `ended` updates
/// their progress.
pub fn observe(hub: &Arc<Hub>, session: &str, event: &Value, ended: bool) {
    // Checked first: this sees every event of every session (deltas too).
    let kind = text(&event["type"]);
    let ready = kind == "status" && event["message"] == "ready";
    let change: fn(&mut Live) = match kind {
        "user_message" => |l| {
            *l = Live {
                running: true,
                ..Live::default()
            }
        },
        // Past an approval, or an engine that announces no user message.
        "assistant_delta" | "tool_start" | "action_decided" => |l| {
            l.running = true;
            l.waiting = false;
        },
        "approval_request" | "action_deferred" => |l| l.waiting = true,
        "error" => |l| {
            l.failed = true;
            l.running = false;
            l.waiting = false;
        },
        _ if ready => |l| {
            l.running = false;
            l.waiting = false;
        },
        _ => return,
    };
    let reqs = &hub.requirements;
    // Most events (a running turn's deltas) change nothing: no list scan.
    let next = {
        let live = lock(&reqs.live);
        let was = live.get(session).copied().unwrap_or_default();
        let mut next = was;
        change(&mut next);
        if next == was && !ended {
            return;
        }
        next
    };
    if ended {
        answer_gates(reqs, session);
    }
    let open: Vec<Value> = lock(&reqs.data)
        .list
        .iter()
        .filter(|r| r["state"] == "open" && sessions(r).any(|s| s == session))
        .cloned()
        .collect();
    let moved: Vec<(String, &'static str)> = {
        let mut live = lock(&reqs.live);
        let before: Vec<&str> = open.iter().map(|r| status(r, &live)).collect();
        live.insert(session.to_string(), next);
        open.iter()
            .zip(before)
            .filter_map(|(r, before)| {
                let after = status(r, &live);
                (after != before).then(|| (text(&r["id"]).to_string(), after))
            })
            .collect()
    };
    if !moved.is_empty() {
        hub.broadcast(&reqs.json(hub));
    }
    for (id, status) in &moved {
        let Some(r) = reqs.get(id) else { continue };
        let title = format!("{id} {}", text(&r["title"]));
        match *status {
            "review" | "confirm" => {
                let body = hub.last_reply(session).unwrap_or_default();
                let label = if *status == "confirm" {
                    "待确认"
                } else {
                    "待查看"
                };
                hub.notify_at(
                    &format!("{title} · {label}"),
                    &tail(&body, REPLY_LIMIT),
                    id,
                    &format!("/remote?requirement={id}"),
                );
            }
            "approval" => hub.notify_at(
                &format!("{title} · 需要你批准"),
                text(&event["title"]),
                id,
                &format!("/remote?requirement={id}"),
            ),
            _ => {}
        }
    }
    if ended {
        for r in &open {
            write_progress(hub, text(&r["id"]), session);
        }
    }
}

/// A turn of `session` ended: the gate stage it was at has its answer.
fn answer_gates(reqs: &Requirements, session: &str) {
    let mut data = lock(&reqs.data);
    let mut changed = false;
    for r in data.list.iter_mut() {
        if r["gate"]["session"] == session && r["gate"]["answered"] != true {
            r["gate"]["answered"] = json!(true);
            changed = true;
        }
    }
    if changed {
        if let Err(error) = reqs.save(&data) {
            jucode_agent_core::log_warn!("daemon", "requirement gate not saved", error = error);
        }
    }
}

/// The title model rewrites requirement `id`'s progress from `session`'s
/// latest turn, in the background. One at a time per requirement; a turn
/// that ends meanwhile is written next.
fn write_progress(hub: &Arc<Hub>, id: &str, session: &str) {
    {
        let mut writing = lock(&hub.requirements.writing);
        if let Some(queued) = writing.get_mut(id) {
            *queued = Some(session.to_string());
            return;
        }
        writing.insert(id.to_string(), None);
    }
    let hub = Arc::clone(hub);
    let id = id.to_string();
    let mut session = session.to_string();
    thread::spawn(move || loop {
        rewrite_progress(&hub, &id, &session);
        let mut writing = lock(&hub.requirements.writing);
        match writing.get_mut(&id).and_then(Option::take) {
            Some(next) => session = next,
            None => {
                writing.remove(&id);
                return;
            }
        }
    });
}

fn rewrite_progress(hub: &Arc<Hub>, id: &str, session: &str) {
    let (Some(r), Some(excerpt)) = (hub.requirements.get(id), hub.turn_excerpt(session)) else {
        return;
    };
    let previous = match &r["progress"] {
        Value::Null => "(none)".to_string(),
        progress => progress.to_string(),
    };
    let request = format!(
        "Requirement {id}, in the user's words:\n{}\n\nPrevious progress record:\n{previous}\n\n\
         Latest session excerpt:\n{excerpt}",
        text(&r["text"])
    );
    let progress = match jucode_agent_core::title_completion(PROGRESS_SYSTEM, &request) {
        Ok(reply) => parse_progress(&reply),
        Err(error) => {
            jucode_agent_core::log_warn!("daemon", "requirement progress failed", error = error);
            return;
        }
    };
    let Some(progress) = progress else {
        jucode_agent_core::log_warn!(
            "daemon",
            "requirement progress unreadable",
            requirement = id
        );
        return;
    };
    let saved = hub.requirements.update(id, |r| {
        r["progress"] = progress;
        r["progress_at"] = json!(now());
        Ok(())
    });
    if saved.is_ok() {
        hub.broadcast(&hub.requirements.json(hub));
    }
}

/// The model's reply as a progress record: the JSON object in it, with
/// every field a bounded string or list of strings.
fn parse_progress(reply: &str) -> Option<Value> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    let value: Value = serde_json::from_str(reply.get(start..=end)?).ok()?;
    let mut progress = json!({ "goal": head(text(&value["goal"]).trim(), ITEM_CHARS) });
    for key in SECTIONS {
        let items: Vec<String> = value[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|item| head(item.trim(), ITEM_CHARS))
            .filter(|item| !item.is_empty())
            .take(ITEMS)
            .collect();
        progress[key] = json!(items);
    }
    Some(progress)
}

/// A requirement too long to be its own title gets one from the title model.
fn retitle(hub: &Arc<Hub>, id: &str, words: &str) {
    let hub = Arc::clone(hub);
    let id = id.to_string();
    let request = format!("Project: \nCurrent title: (none)\n\nFirst request:\n{words}\n");
    thread::spawn(move || {
        let title = match jucode_agent_core::title_completion(titles::SYSTEM, &request) {
            Ok(reply) => titles::clean(&reply),
            Err(error) => {
                jucode_agent_core::log_warn!("daemon", "requirement title failed", error = error);
                None
            }
        };
        let Some(title) = title else { return };
        // Edited by hand meanwhile: the user's words name it.
        let saved = hub.requirements.update(&id, |r| {
            if r["title_auto"] == true {
                r["title"] = json!(title);
            }
            Ok(())
        });
        if saved.is_ok() {
            hub.broadcast(&hub.requirements.json(&hub));
        }
    });
}

/// The first message of a session on `r`: the requirement (see `brief`),
/// then the ask to only explain what it understood.
fn understand_prompt(r: &Value, extra: &str, lang: &str) -> String {
    let mut out = brief(r, extra, lang);
    out.push('\n');
    out.push_str(if lang == "en" {
        "In this step only understand the requirement; do not modify any files. You may read code and look things up. Reply with:\n\
         1. The goal as you understand it, in a sentence or two.\n\
         2. Scope: what to do and what not to do.\n\
         3. Your assumptions and the questions I need to confirm. Where the requirement is vague, list two or three readings and say which you lean towards.\n\
         4. If you think it is not worth doing, or there is a simpler way, say so and why.\n\
         Then stop and wait for my confirmation."
    } else {
        "这一步只理解需求，不要修改任何文件。可以读代码、查资料。请回复：\n\
         1. 你理解的目标，一两句话。\n\
         2. 范围：要做什么，不做什么。\n\
         3. 你的假设和需要我确认的问题。需求写得模糊的地方，列出两三种理解，说明你倾向哪种。\n\
         4. 如果你认为不值得做，或者有更简单的做法，直接说明理由。\n\
         回复后停下，等我确认。"
    });
    out
}

/// After the understanding is confirmed, with a plan asked for.
fn plan_prompt(extra: &str, lang: &str) -> String {
    let en = lang == "en";
    let mut out = String::from(if en {
        "Your understanding of the requirement is confirmed.\n"
    } else {
        "需求理解已确认。\n"
    });
    out.push_str(&extra_text(extra, en));
    out.push_str(if en {
        "Now make an implementation plan; still do not modify any files. Cover which files and modules change, the steps, how each step is verified, and the risks. Then stop and wait for my confirmation."
    } else {
        "现在制定实施计划，仍然不要修改文件。写明：要改哪些文件和模块、分几步、每步怎么验证、有哪些风险。写完后停下，等我确认。"
    });
    out
}

/// The last confirmation: of the plan when there was one (`planned`).
fn go_prompt(planned: bool, extra: &str, lang: &str) -> String {
    let en = lang == "en";
    let mut out = String::from(match (planned, en) {
        (true, true) => "Your implementation plan is confirmed.\n",
        (true, false) => "实施计划已确认。\n",
        (false, true) => "Your understanding of the requirement is confirmed.\n",
        (false, false) => "需求理解已确认。\n",
    });
    out.push_str(&extra_text(extra, en));
    out.push_str(if en {
        "Now implement it. When done, say what you changed, how you verified it, and what is left."
    } else {
        "现在开始实施。做完后说明：改了什么、怎么验证的、还有什么没做。"
    });
    out
}

/// The user's added words, as a section of their own.
fn extra_text(extra: &str, en: bool) -> String {
    if extra.is_empty() {
        return String::new();
    }
    let label = if en {
        "Additional notes:"
    } else {
        "补充说明："
    };
    format!("\n{label}\n{extra}\n\n")
}

/// Requirement `r` for a session: its words, screenshots, the progress so
/// far and the user's added words (`extra`).
fn brief(r: &Value, extra: &str, lang: &str) -> String {
    let en = lang == "en";
    let label = |zh: &'static str, english: &'static str| if en { english } else { zh };
    let mut out = format!(
        "{} {}\n\n{}\n{}\n",
        text(&r["id"]),
        text(&r["title"]),
        label("需求原文：", "The requirement, in my words:"),
        text(&r["text"]),
    );
    let images = paths(&r["images"]);
    if !images.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            label("截图（请打开查看）：", "Screenshots (open them):")
        ));
        for image in images {
            out.push_str(&format!("- {image}\n"));
        }
    }
    let progress = &r["progress"];
    if progress.is_object() {
        out.push_str(&format!("\n{}\n", label("目前进展：", "Progress so far:")));
        let goal = text(&progress["goal"]);
        if !goal.is_empty() {
            out.push_str(&format!("{}{goal}\n", label("目标：", "Goal: ")));
        }
        let names = [
            ("decided", "已定", "Decided"),
            ("done", "已完成", "Done"),
            ("doing", "进行中", "In progress"),
            ("blocked", "卡住", "Blocked"),
            ("next", "下一步", "Next"),
            ("files", "相关文件", "Files"),
        ];
        for (key, zh, english) in names {
            let items = paths(&progress[key]);
            if items.is_empty() {
                continue;
            }
            out.push_str(&format!("{}{}\n", label(zh, english), label("：", ":")));
            for item in items {
                out.push_str(&format!("- {item}\n"));
            }
        }
        let note = text(&progress["note"]);
        if !note.is_empty() {
            out.push_str(&format!("{}{note}\n", label("备注：", "Note: ")));
        }
    }
    if !extra.is_empty() {
        out.push_str(&format!(
            "\n{}\n{extra}\n",
            label("补充说明：", "Additional notes:")
        ));
    }
    out
}

/// The status a client shows (see the module doc).
fn status(r: &Value, live: &HashMap<String, Live>) -> &'static str {
    let state = text(&r["state"]);
    if matches!(state, "open" | "idea") && r["proposal"]["kind"] == "close" {
        return "proposal";
    }
    match state {
        "open" => {
            let list: Vec<&str> = sessions(r).collect();
            let Some(latest) = list.last() else {
                return "open";
            };
            let of = |s: &&str| live.get(*s).copied().unwrap_or_default();
            // Only a turn that ended at this stage answers it: an engine
            // also reports ready when it starts or switches modes.
            let gate = r["gate"]["session"]
                .as_str()
                .filter(|s| session_state(live.get(*s)) == "idle");
            let at_gate = gate.is_some() && r["gate"]["answered"] == true;
            if list.iter().any(|s| of(s).waiting) {
                "approval"
            } else if at_gate {
                "confirm"
            } else if of(latest).failed {
                "failed"
            } else if gate.is_some() || list.iter().any(|s| of(s).running) {
                "running"
            } else {
                "review"
            }
        }
        "done" => "done",
        "parked" => "parked",
        "proposed" => "proposed",
        _ => "idea",
    }
}

/// The `requirements` tool of `agent` (of `project`, if any) in `session`.
/// Noting and closing a requirement only proposes it to the user.
pub fn tool(
    hub: &Arc<Hub>,
    agent: &str,
    project: Option<&str>,
    session: &str,
    args: &Value,
) -> Result<Value, String> {
    let reqs = &hub.requirements;
    let arg = |key: &str| args[key].as_str().map(str::trim).unwrap_or_default();
    match arg("action") {
        "list" => {
            let project = args["project"].as_str().or(project);
            let live = lock(&reqs.live).clone();
            let list: Vec<Value> = lock(&reqs.data)
                .list
                .iter()
                .filter(|r| match project {
                    Some("none") => r["project"].is_null(),
                    Some(id) => r["project"] == id,
                    None => true,
                })
                .filter(|r| args["state"].is_null() || r["state"] == args["state"])
                .map(|r| {
                    json!({
                        "id": r["id"],
                        "title": r["title"],
                        "state": r["state"],
                        "status": status(r, &live),
                        "project": r["project"],
                        "goal": r["progress"]["goal"],
                        "proposal": r["proposal"].is_object(),
                    })
                })
                .collect();
            Ok(json!(list))
        }
        "get" => {
            let id = arg("requirement");
            let mut r = reqs
                .get(id)
                .ok_or_else(|| format!("unknown requirement {id}"))?;
            r["status"] = json!(status(&r, &lock(&reqs.live)));
            Ok(r)
        }
        "propose_create" => {
            let words = arg("text");
            let reason = arg("reason");
            if words.is_empty() || reason.is_empty() {
                return Err("propose_create requires text and reason".to_string());
            }
            let r = add(
                hub,
                words,
                &[],
                json!({
                    "project": project,
                    "state": "proposed",
                    "source": "agent",
                    "source_session": session,
                    "proposal": proposal("create", agent, session, reason),
                }),
            )?;
            notify_proposal(hub, &r, reason);
            Ok(
                json!({ "proposed": r["id"], "note": "The user is asked; it is noted only once they accept." }),
            )
        }
        "propose_close" => {
            let id = arg("requirement");
            let reason = arg("reason");
            let outcome = arg("outcome");
            if reason.is_empty() || !matches!(outcome, "done" | "parked") {
                return Err(
                    "propose_close requires reason and outcome (done or parked)".to_string()
                );
            }
            let r = reqs.update(id, |r| {
                if !matches!(text(&r["state"]), "open" | "idea") {
                    return Err(format!("{id} is not open"));
                }
                let mut close = proposal("close", agent, session, reason);
                close["outcome"] = json!(outcome);
                r["proposal"] = close;
                Ok(r.clone())
            })?;
            hub.broadcast(&reqs.json(hub));
            notify_proposal(hub, &r, reason);
            Ok(
                json!({ "proposed": id, "note": "The user is asked; it closes only once they accept." }),
            )
        }
        "progress" => {
            let id = arg("requirement");
            reqs.update(id, |r| {
                r["progress"] = merge_progress(&r["progress"], args);
                r["progress_at"] = json!(now());
                Ok(())
            })?;
            hub.broadcast(&reqs.json(hub));
            Ok(json!({ "updated": id }))
        }
        _ => Err(
            "requirements action must be list, get, propose_create, propose_close or progress"
                .to_string(),
        ),
    }
}

fn proposal(kind: &str, agent: &str, session: &str, reason: &str) -> Value {
    json!({ "kind": kind, "agent": agent, "session": session, "reason": reason, "at": now() })
}

fn notify_proposal(hub: &Hub, r: &Value, reason: &str) {
    let id = text(&r["id"]);
    hub.notify_at(
        &format!("{id} {} · Agent 提议", text(&r["title"])),
        reason,
        id,
        &format!("/remote?requirement={id}"),
    );
}

/// `progress` with an agent's additions: each list item appended once, the
/// newest kept within bounds; `note` replaces the note.
fn merge_progress(progress: &Value, args: &Value) -> Value {
    let mut merged = if progress.is_object() {
        progress.clone()
    } else {
        json!({ "goal": "" })
    };
    for key in ["decided", "done", "doing", "blocked", "next"] {
        let mut items = paths(&merged[key]);
        let added = match &args[key] {
            Value::String(item) => vec![item.clone()],
            list => paths(list),
        };
        for item in added {
            let item = head(item.trim(), ITEM_CHARS);
            if !item.is_empty() && !items.contains(&item) {
                items.push(item);
            }
        }
        let skip = items.len().saturating_sub(ITEMS);
        merged[key] = json!(items[skip..]);
    }
    if let Some(note) = args["note"].as_str() {
        merged["note"] = json!(head(note.trim(), ITEM_CHARS));
    }
    merged
}

/// Requirements from before projects had ids name theirs by path
/// (`projects`); each gets the id of the project at its first path, or
/// none. Waits for the workspaces to have projects. Returns whether any
/// changed.
fn migrate(list: &mut [Value], workspaces: &Value) -> bool {
    let projects: Vec<&Value> = workspaces["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|ws| ws["projects"].as_array().into_iter().flatten())
        .collect();
    if projects.is_empty() {
        return false;
    }
    let mut changed = false;
    for r in list.iter_mut() {
        let Some(map) = r.as_object_mut() else {
            continue;
        };
        let Some(old) = map.remove("projects") else {
            continue;
        };
        let id = old[0]
            .as_str()
            .and_then(|path| projects.iter().find(|p| p["path"] == path))
            .map_or(Value::Null, |p| p["id"].clone());
        map.insert("project".to_string(), id);
        changed = true;
    }
    changed
}

fn session_state(live: Option<&Live>) -> &'static str {
    match live.copied().unwrap_or_default() {
        Live { waiting: true, .. } => "waiting",
        Live { running: true, .. } => "running",
        Live { failed: true, .. } => "failed",
        _ => "idle",
    }
}

fn sessions(r: &Value) -> impl Iterator<Item = &str> {
    r["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

fn record(hub: &Hub, session: &str) -> Option<crate::store::SessionRecord> {
    hub.store
        .sessions()
        .into_iter()
        .find(|record| record.id == session)
}

/// Moves uploaded screenshots into `dir` as 1.png, 2.jpg, …; returns their
/// new paths.
fn keep_images(dir: &Path, uploads: &[PathBuf]) -> Result<Vec<String>, String> {
    if uploads.is_empty() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    uploads
        .iter()
        .enumerate()
        .map(|(n, upload)| {
            let ext = upload
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or("png")
                .to_ascii_lowercase();
            let path = dir.join(format!("{}.{ext}", n + 1));
            fs::rename(upload, &path).map_err(|error| error.to_string())?;
            Ok(path.to_string_lossy().into_owned())
        })
        .collect()
}

/// The title the words give themselves (their first line, clipped) and
/// whether they are too long for it to name them well.
fn first_line(words: &str) -> (String, bool) {
    let line = words.lines().next().unwrap_or_default().trim();
    let long = words.chars().count() > TITLE_CHARS;
    (head(line, TITLE_CHARS), long)
}

fn paths(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn head(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

fn tail(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    format!("…{}", text.chars().skip(count - limit).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(sessions: &[&str]) -> Value {
        json!({ "id": "R-1", "state": "open", "sessions": sessions })
    }

    #[test]
    fn status_follows_the_sessions() {
        let mut live = HashMap::new();
        assert_eq!(status(&open(&[]), &live), "open");
        assert_eq!(status(&open(&["a"]), &live), "review");
        live.insert(
            "a".to_string(),
            Live {
                running: true,
                ..Live::default()
            },
        );
        assert_eq!(status(&open(&["a", "b"]), &live), "running");
        live.insert(
            "b".to_string(),
            Live {
                failed: true,
                ..Live::default()
            },
        );
        assert_eq!(status(&open(&["a", "b"]), &live), "failed");
        live.insert(
            "b".to_string(),
            Live {
                waiting: true,
                ..Live::default()
            },
        );
        assert_eq!(status(&open(&["a", "b"]), &live), "approval");
        // Only the latest session's failure shows; an older one is history.
        live.insert(
            "a".to_string(),
            Live {
                failed: true,
                ..Live::default()
            },
        );
        live.insert("b".to_string(), Live::default());
        assert_eq!(status(&open(&["a", "b"]), &live), "review");
        assert_eq!(status(&open(&["b", "a"]), &live), "failed");
        let parked = json!({ "state": "parked", "sessions": ["a"] });
        assert_eq!(status(&parked, &live), "parked");
    }

    #[test]
    fn progress_is_read_from_the_reply_and_bounded() {
        let reply =
            "Here:\n```json\n{\"goal\": \"导出会话\", \"decided\": [\"只导出正文\", \"\", 3], \
                     \"done\": [], \"next\": [\"你查看效果\"], \"extra\": 1}\n```";
        let progress = parse_progress(reply).unwrap();
        assert_eq!(progress["goal"], "导出会话");
        assert_eq!(progress["decided"], json!(["只导出正文"]));
        assert_eq!(progress["blocked"], json!([]));
        assert_eq!(progress["next"], json!(["你查看效果"]));
        assert!(progress.get("extra").is_none());
        assert!(parse_progress("no record").is_none());
    }

    #[test]
    fn the_prompts_carry_words_progress_and_the_gate_steps() {
        let r = json!({
            "id": "R-3", "title": "导出", "text": "会话能导出成 markdown",
            "images": ["/u/R-3/1.png"],
            "progress": { "goal": "导出会话", "decided": ["只导出正文"], "done": [], "next": [] },
        });
        let text = understand_prompt(&r, "文件名别带冒号", "zh");
        assert!(text.starts_with("R-3 导出\n\n需求原文：\n会话能导出成 markdown\n"));
        assert!(text.contains("截图（请打开查看）：\n- /u/R-3/1.png\n"));
        assert!(text.contains("目标：导出会话\n已定：\n- 只导出正文\n"));
        assert!(!text.contains("已完成"));
        assert!(text.contains("补充说明：\n文件名别带冒号\n"));
        assert!(text.contains("不要修改任何文件"));
        assert!(text.ends_with("回复后停下，等我确认。"));
        assert!(!text.contains("动手前先判断"));
        let fresh = understand_prompt(&json!({ "id": "R-4", "title": "t", "text": "w" }), "", "en");
        assert!(fresh.starts_with("R-4 t\n\nThe requirement, in my words:\nw\n\nIn this step"));
        assert!(plan_prompt("", "zh").starts_with("需求理解已确认。\n现在制定实施计划"));
        let go = go_prompt(true, "先做导出", "zh");
        assert!(go.starts_with("实施计划已确认。\n\n补充说明：\n先做导出\n\n现在开始实施。"));
        assert!(go_prompt(false, "", "en").starts_with("Your understanding"));
    }

    #[test]
    fn status_shows_the_gate_and_proposals() {
        let mut live = HashMap::new();
        let mut r = open(&["a"]);
        r["gate"] = json!({ "session": "a", "stage": "understand", "plan": false, "mode": "auto" });
        // Not answered yet: the engine only started.
        assert_eq!(status(&r, &live), "running");
        r["gate"]["answered"] = json!(true);
        assert_eq!(status(&r, &live), "confirm");
        live.insert(
            "a".to_string(),
            Live {
                running: true,
                ..Live::default()
            },
        );
        assert_eq!(status(&r, &live), "running");
        live.insert(
            "a".to_string(),
            Live {
                waiting: true,
                ..Live::default()
            },
        );
        assert_eq!(status(&r, &live), "approval");
        r["proposal"] = json!({ "kind": "close", "outcome": "done" });
        assert_eq!(status(&r, &live), "proposal");
        let idea = json!({ "state": "idea", "proposal": { "kind": "close" } });
        assert_eq!(status(&idea, &live), "proposal");
        let proposed = json!({ "state": "proposed", "proposal": { "kind": "create" } });
        assert_eq!(status(&proposed, &live), "proposed");
    }

    #[test]
    fn old_project_paths_become_project_ids() {
        let workspaces = json!({ "workspaces": [{ "id": "w", "projects": [
            { "id": "p1", "path": "/work/app" },
        ] }] });
        let mut list = vec![
            json!({ "id": "R-1", "projects": ["/work/app", "/work/x"] }),
            json!({ "id": "R-2", "projects": ["/gone"] }),
            json!({ "id": "R-3", "projects": [] }),
            json!({ "id": "R-4", "project": "p1" }),
        ];
        assert!(!migrate(&mut list, &json!({ "workspaces": [] })));
        assert!(list[0].get("projects").is_some());
        assert!(migrate(&mut list, &workspaces));
        assert_eq!(list[0]["project"], "p1");
        assert!(list[1]["project"].is_null());
        assert!(list[2]["project"].is_null());
        assert!(list.iter().all(|r| r.get("projects").is_none()));
        assert!(!migrate(&mut list, &workspaces));
    }

    #[test]
    fn an_agents_progress_is_merged_once_and_bounded() {
        let before = json!({ "goal": "导出", "done": ["读取会话"], "files": ["a.rs"] });
        let args = json!({ "done": ["读取会话", " 写文件 ", ""], "next": "你查看效果", "note": "先做 md" });
        let merged = merge_progress(&before, &args);
        assert_eq!(merged["goal"], "导出");
        assert_eq!(merged["done"], json!(["读取会话", "写文件"]));
        assert_eq!(merged["next"], json!(["你查看效果"]));
        assert_eq!(merged["files"], json!(["a.rs"]));
        assert_eq!(merged["note"], "先做 md");
        let many: Vec<String> = (0..12).map(|n| n.to_string()).collect();
        let bounded = merge_progress(&Value::Null, &json!({ "doing": many }));
        assert_eq!(bounded["doing"].as_array().unwrap().len(), ITEMS);
        assert_eq!(bounded["doing"][ITEMS - 1], "11");
    }

    #[test]
    fn the_gate_reads_first_then_plans_then_switches_to_the_mode() {
        let dir =
            std::env::temp_dir().join(format!("jucode-req-gate-{}-{}", std::process::id(), now()));
        let hub = Hub::new(
            crate::store::Store::open(dir.join("daemon")).unwrap(),
            crate::Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        let cwd = dir.join("app");
        fs::create_dir_all(&cwd).unwrap();
        hub.store
            .record_engine_session("s", &cwd, None, None, false)
            .unwrap();
        let (ops, rx) = std::sync::mpsc::channel();
        hub.host("s".to_string(), ops, cwd, hub.next_generation());
        let r = add(&hub, "导出会话", &[], json!({})).unwrap();
        let id = text(&r["id"]).to_string();
        // The engine took the message and ended its turn.
        let idle = || {
            hub.release_claim("s");
            hub.set_busy("s", false);
        };
        let op = json!({ "requirement": id, "session": "s", "plan": true, "mode": "edits" });
        begin(&hub, &op).unwrap();
        let sent: Vec<Value> = rx.try_iter().collect();
        assert_eq!(
            sent[0],
            json!({ "op": "set_approval_mode", "mode": "manual" })
        );
        assert!(text(&sent[1]["content"]).contains("这一步只理解需求"));
        assert_eq!(
            hub.requirements.get(&id).unwrap()["gate"]["stage"],
            "understand"
        );
        let confirm_op = json!({ "requirement": id, "text": "" });
        assert!(confirm(&hub, &confirm_op).is_err(), "the turn still runs");

        idle();
        assert_eq!(confirm(&hub, &confirm_op).unwrap()["stage"], "plan");
        let sent: Vec<Value> = rx.try_iter().collect();
        assert_eq!(sent.len(), 1);
        assert!(text(&sent[0]["content"]).contains("现在制定实施计划"));

        idle();
        assert_eq!(confirm(&hub, &confirm_op).unwrap()["stage"], "go");
        let sent: Vec<Value> = rx.try_iter().collect();
        assert_eq!(
            sent[0],
            json!({ "op": "set_approval_mode", "mode": "auto-edit" })
        );
        assert!(text(&sent[1]["content"]).starts_with("实施计划已确认。"));
        assert!(hub.requirements.get(&id).unwrap().get("gate").is_none());
        idle();
        assert!(confirm(&hub, &confirm_op).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn long_words_get_a_model_title() {
        assert_eq!(first_line("短需求\n细节"), ("短需求".to_string(), false));
        let long = "字".repeat(50);
        let (title, auto) = first_line(&long);
        assert!(auto);
        assert_eq!(title.chars().count(), TITLE_CHARS + 1);
    }

    #[test]
    fn screenshots_move_in_numbered() {
        let dir = std::env::temp_dir().join(format!("jucode-req-img-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let upload = dir.join("u-1-shot.JPG");
        fs::write(&upload, b"jpg").unwrap();
        let kept = keep_images(&dir.join("R-1"), std::slice::from_ref(&upload)).unwrap();
        assert!(kept[0].ends_with("R-1/1.jpg"));
        assert_eq!(fs::read(&kept[0]).unwrap(), b"jpg");
        assert!(!upload.exists());
        let _ = fs::remove_dir_all(dir);
    }
}
