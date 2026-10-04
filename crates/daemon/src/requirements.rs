//! Requirements: what the user means to get done, kept across the sessions
//! that work on it. A requirement is the user's own words (and screenshots),
//! the projects it concerns, the sessions linked to it, and a progress record
//! the title model rewrites after each of their turns, so the next session
//! picks up where the last one stopped.
//!
//! Screenshots are uploads (see `uploads`) moved into
//! `~/.jucode/uploads/requirements/<id>/` when the requirement is noted.
//!
//! Its state is the user's: idea, open, done or parked. While it is open,
//! what it shows follows its sessions: one waits for an approval, the latest
//! one failed, one is running, or else it is the user's turn ("review": the
//! work is done, or the agent asks something in its reply). Turning to the
//! user's turn notifies the paired phones.

use crate::{
    engines,
    hub::{lock, Hub},
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
const STATES: [&str; 4] = ["idea", "open", "done", "parked"];
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
    /// `dir`: the daemon's state; `images`: where screenshots are kept.
    pub fn load(dir: &Path, images: PathBuf) -> Self {
        let path = dir.join(FILE);
        let saved = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or_default();
        let list = saved["requirements"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let next = saved["next"].as_u64().unwrap_or(1);
        Self {
            path,
            images,
            data: Mutex::new(Data { next, list }),
            live: Mutex::new(HashMap::new()),
            writing: Mutex::new(HashMap::new()),
        }
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
                if matches!(status, "review" | "failed") {
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

/// `requirement_create`: the user's words, the projects they concern (a
/// requirement noted in a session concerns that session's project) and
/// screenshots (paths of finished uploads).
pub fn create(hub: &Arc<Hub>, op: &Value) -> Result<Value, String> {
    let words = text(&op["text"]).trim().to_string();
    if words.is_empty() {
        return Err("requirement_create requires text".to_string());
    }
    let from = op["session"].as_str();
    let mut projects = paths(&op["projects"]);
    if projects.is_empty() {
        if let Some(record) = from.and_then(|s| record(hub, s)) {
            projects.push(record.cwd.to_string_lossy().into_owned());
        }
    }
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
    let reqs = &hub.requirements;
    let r = {
        let mut data = lock(&reqs.data);
        let id = format!("R-{}", data.next);
        let saved = keep_images(&reqs.images.join(&id), &images)?;
        let (title, long) = first_line(&words);
        let r = json!({
            "id": id,
            "text": words,
            "title": title,
            "title_auto": long,
            "images": saved,
            "projects": projects,
            "state": "idea",
            "sessions": [],
            "progress": null,
            "source": match from {
                Some(_) => "session",
                None if op["source"] == "phone" => "phone",
                None => "desktop",
            },
            "source_session": from,
            "created_at": now(),
            "updated_at": now(),
        });
        data.next += 1;
        data.list.insert(0, r.clone());
        reqs.save(&data)?;
        r
    };
    if r["title_auto"] == true {
        retitle(hub, text(&r["id"]), &words);
    }
    hub.broadcast(&reqs.json(hub));
    Ok(json!({ "type": "requirement_created", "requirement": r }))
}

/// `requirement_update`: the user's words, projects or state.
pub fn update(hub: &Arc<Hub>, op: &Value) -> Result<Value, String> {
    let id = text(&op["requirement"]);
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
        if op["projects"].is_array() {
            r["projects"] = json!(paths(&op["projects"]));
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

/// `requirement_prompt`: the first message of a session that starts on the
/// requirement (the client shows it in its composer to edit before sending).
pub fn prompt_json(hub: &Hub, id: &str, feedback: &str, lang: &str) -> Result<Value, String> {
    let r = hub
        .requirements
        .get(id)
        .ok_or_else(|| format!("unknown requirement {id}"))?;
    Ok(json!({
        "type": "requirement_prompt",
        "requirement": id,
        "text": prompt(&r, feedback, lang),
    }))
}

/// `requirement_reply`: sends `text` to the requirement's latest session, or
/// starts a new one (asked for, or none yet) in `cwd` on `engine` (default:
/// the latest session's) with the requirement, its progress and `text`.
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
        let cwd = op["cwd"]
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                latest
                    .as_ref()
                    .map(|l| l.cwd.to_string_lossy().into_owned())
            })
            .or_else(|| paths(&r["projects"]).into_iter().next())
            .ok_or("this requirement has no project; choose one")?;
        let engine = match op["engine"].as_str() {
            Some(name) => engines::Kind::parse(name)?,
            None => engines::Kind::parse(
                latest
                    .as_ref()
                    .and_then(|l| l.engine.as_deref())
                    .unwrap_or_default(),
            )?,
        };
        let session = hub.create_engine_session(
            Some(PathBuf::from(cwd)),
            None,
            false,
            engine,
            engines::Options::default(),
        )?;
        link(hub, id, &session)?;
        let lang = text(&op["lang"]);
        hub.send_to_session(&session, &prompt(&r, &feedback, lang))?;
        session
    };
    Ok(json!({ "type": "requirement_replied", "requirement": id, "session": target }))
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
            "review" => {
                let body = hub.last_reply(session).unwrap_or_default();
                hub.notify_at(
                    &format!("{title} · 待查看"),
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

/// The message that starts a session on `r`: its words, the progress so far,
/// the user's `feedback`, and how to work on it.
fn prompt(r: &Value, feedback: &str, lang: &str) -> String {
    let en = lang == "en";
    let label = |zh: &'static str, english: &'static str| if en { english } else { zh };
    let mut out = format!(
        "{}{}{}\n\n{}{}\n",
        text(&r["id"]),
        label("：", ": "),
        text(&r["title"]),
        label("原话：", "In my words: "),
        text(&r["text"]),
    );
    let progress = &r["progress"];
    if progress.is_object() {
        out.push_str(&format!("\n{}\n", label("当前进展：", "Progress so far:")));
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
            let items: Vec<&str> = progress[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if items.is_empty() {
                continue;
            }
            out.push_str(&format!("{}{}\n", label(zh, english), label("：", ":")));
            for item in items {
                out.push_str(&format!("- {item}\n"));
            }
        }
    }
    if !feedback.is_empty() {
        out.push_str(&format!(
            "\n{}\n{feedback}\n",
            label("这次的意见：", "This time:")
        ));
    }
    out.push('\n');
    out.push_str(label(
        "动手前先判断这件事是否值得做、有没有更简单或更好的做法。如果有，先说明理由和推荐做法，等我答复再动手。做完后用三行说明：改了什么、怎么验证的、还有什么没做。",
        "Before you start, judge whether this is worth doing and whether there is a simpler or better way. If there is, explain why and what you recommend, and wait for my answer. When done, say in three lines what you changed, how you verified it, and what is left.",
    ));
    out
}

/// The status a client shows (see the module doc).
fn status(r: &Value, live: &HashMap<String, Live>) -> &'static str {
    match text(&r["state"]) {
        "open" => {
            let list: Vec<&str> = sessions(r).collect();
            let Some(latest) = list.last() else {
                return "open";
            };
            let of = |s: &&str| live.get(*s).copied().unwrap_or_default();
            if list.iter().any(|s| of(s).waiting) {
                "approval"
            } else if of(latest).failed {
                "failed"
            } else if list.iter().any(|s| of(s).running) {
                "running"
            } else {
                "review"
            }
        }
        "done" => "done",
        "parked" => "parked",
        _ => "idea",
    }
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
    fn the_prompt_carries_words_progress_and_feedback() {
        let r = json!({
            "id": "R-3", "title": "导出", "text": "会话能导出成 markdown",
            "progress": { "goal": "导出会话", "decided": ["只导出正文"], "done": [], "next": [] },
        });
        let text = prompt(&r, "文件名别带冒号", "zh");
        assert!(text.starts_with("R-3：导出\n\n原话：会话能导出成 markdown\n"));
        assert!(text.contains("目标：导出会话\n已定：\n- 只导出正文\n"));
        assert!(!text.contains("已完成"));
        assert!(text.contains("这次的意见：\n文件名别带冒号\n"));
        assert!(text.ends_with("还有什么没做。"));
        let fresh = prompt(&json!({ "id": "R-4", "title": "t", "text": "w" }), "", "en");
        assert!(fresh.starts_with("R-4: t\n\nIn my words: w\n\nBefore you start"));
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
