//! Tools the JuCode app gives every agent, beyond acting on the workspace:
//! `present_files` (the task's deliverables, shown as cards) and
//! `propose_schedule` (a scheduled task the user confirms before it exists),
//! which run here; and the session tools (`start_session`, `wait_session`,
//! `send_session`: one session handing work to another), which only the
//! daemon can run. jucode sessions offer them as built-in tools (the session
//! tools only under the daemon, see `ToolState::set_host`), Claude Code
//! through the daemon's in-process MCP server, Codex as dynamic tools. Their
//! output is JSON the clients render from, so a replayed conversation shows
//! the same cards.

use serde_json::{json, Value};
use std::path::Path;

pub const PRESENT_FILES: &str = "present_files";
pub const PROPOSE_SCHEDULE: &str = "propose_schedule";
pub const START_SESSION: &str = "start_session";
pub const WAIT_SESSION: &str = "wait_session";
pub const SEND_SESSION: &str = "send_session";
/// The tools `run` runs.
pub const LOCAL: [&str; 2] = [PRESENT_FILES, PROPOSE_SCHEDULE];
/// The tools only the daemon runs.
pub const SESSION_TOOLS: [&str; 3] = [START_SESSION, WAIT_SESSION, SEND_SESSION];
/// All of them.
pub const NAMES: [&str; 5] = [
    PRESENT_FILES,
    PROPOSE_SCHEDULE,
    START_SESSION,
    WAIT_SESSION,
    SEND_SESSION,
];

/// Files one call may present.
const MAX_FILES: usize = 12;

/// The tools `run` runs, as (name, description, JSON schema of the input).
pub fn specs() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            PRESENT_FILES,
            "Show the user the files this task produced for them: what they asked you to make, such as a report, document, spreadsheet, slide deck, image, video, audio, standalone HTML page, export or build artifact. The app shows each file as a card the user can open. Call it once, at the end, when the files are final. Put the most important file first. Give each file a one-line description of what it is (for media, include facts like length, resolution or page count). Do not present source files you edited as part of a code change, intermediate files or logs: the app lists edited files separately.",
            json!({
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "description": "The deliverables, most important first.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Absolute path, or relative to the working directory." },
                                "description": { "type": "string", "description": "One line on what the file is." }
                            },
                            "required": ["path"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["files"],
                "additionalProperties": false
            }),
        ),
        (
            PROPOSE_SCHEDULE,
            "Propose a scheduled task: a prompt the app sends to a new session in this project at set times, such as a daily test run or a weekly dependency check. Use it when the user asks for something to happen repeatedly or later. The user sees the proposal as a card and decides whether to create it; nothing is scheduled until they do. Runs use this project's folder and engine. Times are the user's local time. The prompt must stand on its own: the run starts without this conversation.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Short name for the task." },
                    "prompt": { "type": "string", "description": "What each run is asked to do, complete on its own." },
                    "repeat": { "type": "string", "enum": ["once", "hourly", "daily", "weekdays", "weekly"] },
                    "time": { "type": "string", "description": "Local time as HH:MM (24-hour); hourly uses only the minute." },
                    "days": { "type": "array", "items": { "type": "integer", "minimum": 0, "maximum": 6 }, "description": "weekly: days to run, 0 is Sunday." },
                    "date": { "type": "string", "description": "once: the local date as YYYY-MM-DD." }
                },
                "required": ["name", "prompt", "repeat", "time"],
                "additionalProperties": false
            }),
        ),
    ]
}

/// The session tools, as `specs` gives the others.
pub fn session_specs() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            START_SESSION,
            "Start another agent session in this project and give it a task. It works on its own in the background, as a session the user can see and open. Use it to hand off a separate piece of work, possibly to another engine (claude: Claude Code, codex: Codex, jucode: JuCode's own agent), or to get an independent second opinion. It returns at once with the session id; call wait_session for its reply. The new session knows nothing of this conversation, so the prompt must say everything it needs: the goal, the files involved, what to report back. It runs in this session's permission mode, and the user answers its approval requests. Sessions started this way cannot start sessions themselves.",
            json!({
                "type": "object",
                "properties": {
                    "engine": { "type": "string", "enum": ["claude", "codex", "jucode"] },
                    "prompt": { "type": "string", "description": "The task, complete on its own." },
                    "title": { "type": "string", "description": "A short title for the session list." },
                    "model": { "type": "string", "description": "Optional model for the engine; leave out for its default." }
                },
                "required": ["engine", "prompt"],
                "additionalProperties": false
            }),
        ),
        (
            WAIT_SESSION,
            "Wait for a session you started to finish its current task, and get its final reply. Returns early with status waiting_approval when it needs the user to approve something. When the wait ends with the session still working, status is running: call again to keep waiting.",
            json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "The id start_session returned." },
                    "timeout_seconds": { "type": "integer", "minimum": 10, "maximum": 1800, "description": "How long to wait; default 600." }
                },
                "required": ["session"],
                "additionalProperties": false
            }),
        ),
        (
            SEND_SESSION,
            "Send a follow-up message to a session you started, once its last task is done; then call wait_session for the reply.",
            json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "prompt": { "type": "string" }
                },
                "required": ["session", "prompt"],
                "additionalProperties": false
            }),
        ),
    ]
}

/// Runs one of the `LOCAL` tools: Ok with the JSON output, Err with what the
/// model should fix. None when `name` is not one of them.
pub fn run(name: &str, args: &Value, cwd: &Path) -> Option<Result<Value, String>> {
    match name {
        PRESENT_FILES => Some(present_files(args, cwd)),
        PROPOSE_SCHEDULE => Some(propose_schedule(args)),
        _ => None,
    }
}

fn text<'a>(args: &'a Value, key: &str) -> &'a str {
    args[key].as_str().map(str::trim).unwrap_or_default()
}

fn present_files(args: &Value, cwd: &Path) -> Result<Value, String> {
    let entries = args["files"]
        .as_array()
        .filter(|files| !files.is_empty())
        .ok_or("files must list at least one file")?;
    if entries.len() > MAX_FILES {
        return Err(format!(
            "present at most {MAX_FILES} files: the deliverables, not everything you touched"
        ));
    }
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for entry in entries {
        // A bare path is accepted too.
        let (path, description) = match entry {
            Value::String(path) => (path.trim(), ""),
            entry => (text(entry, "path"), text(entry, "description")),
        };
        if path.is_empty() {
            continue;
        }
        let resolved = crate::tools::resolve_path(cwd, path);
        match std::fs::metadata(&resolved) {
            Ok(meta) if meta.is_file() => files.push(json!({
                "path": resolved.display().to_string(),
                "description": description,
                "size": meta.len(),
            })),
            _ => missing.push(path.to_string()),
        }
    }
    if files.is_empty() {
        return Err(format!("no such file: {}", missing.join(", ")));
    }
    let mut output = json!({ "files": files });
    if !missing.is_empty() {
        output["missing"] = json!(missing);
    }
    Ok(output)
}

fn propose_schedule(args: &Value) -> Result<Value, String> {
    let name = text(args, "name");
    let prompt = text(args, "prompt");
    let repeat = text(args, "repeat");
    let time = text(args, "time");
    if name.is_empty() || prompt.is_empty() {
        return Err("name and prompt must not be empty".to_string());
    }
    if !matches!(repeat, "once" | "hourly" | "daily" | "weekdays" | "weekly") {
        return Err("repeat must be once, hourly, daily, weekdays or weekly".to_string());
    }
    let clock = time.split_once(':').and_then(|(h, m)| {
        let (h, m) = (h.parse::<u8>().ok()?, m.parse::<u8>().ok()?);
        (h < 24 && m < 60 && time.len() == 5).then_some(())
    });
    if clock.is_none() {
        return Err(format!("time must be HH:MM (24-hour), got '{time}'"));
    }
    let mut proposal = json!({
        "id": proposal_id(),
        "name": name,
        "prompt": prompt,
        "repeat": repeat,
        "time": time,
    });
    if repeat == "weekly" {
        let days: Vec<u64> = args["days"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .filter(|day| *day <= 6)
            .collect();
        if days.is_empty() {
            return Err("a weekly task needs days (0 is Sunday … 6 is Saturday)".to_string());
        }
        proposal["days"] = json!(days);
    }
    if repeat == "once" {
        let date = text(args, "date");
        let valid = date.len() == 10
            && date.char_indices().all(|(i, c)| {
                if i == 4 || i == 7 {
                    c == '-'
                } else {
                    c.is_ascii_digit()
                }
            });
        if !valid {
            return Err("a one-off task needs date as YYYY-MM-DD".to_string());
        }
        proposal["date"] = json!(date);
    }
    proposal["note"] =
        json!("Shown to the user as a card; nothing is scheduled until they create it.");
    Ok(proposal)
}

/// Ties the card to the task created from it.
fn proposal_id() -> String {
    let mut bytes = [0u8; 6];
    let _ = getrandom::getrandom(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("prop-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_files_keeps_existing_files_and_names_missing_ones() {
        let dir = std::env::temp_dir().join(format!("jucode-present-{}", proposal_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("out.html"), "<p>").unwrap();
        let output = run(
            PRESENT_FILES,
            &json!({ "files": [{ "path": "out.html", "description": "page" }, { "path": "gone.mp4" }] }),
            &dir,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            output["files"][0]["path"],
            dir.join("out.html").display().to_string()
        );
        assert_eq!(output["files"][0]["description"], "page");
        assert_eq!(output["files"][0]["size"], 3);
        assert_eq!(output["missing"], json!(["gone.mp4"]));
        let none = run(PRESENT_FILES, &json!({ "files": ["gone.mp4"] }), &dir).unwrap();
        assert!(none.unwrap_err().contains("gone.mp4"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn propose_schedule_checks_when_it_runs() {
        let ok = propose_schedule(&json!({ "name": "Tests", "prompt": "run tests", "repeat": "weekly", "time": "09:30", "days": [1, 5] })).unwrap();
        assert!(ok["id"].as_str().unwrap().starts_with("prop-"));
        assert_eq!(ok["days"], json!([1, 5]));
        for bad in [
            json!({ "name": "a", "prompt": "b", "repeat": "daily", "time": "9:30" }),
            json!({ "name": "a", "prompt": "b", "repeat": "daily", "time": "24:00" }),
            json!({ "name": "a", "prompt": "b", "repeat": "monthly", "time": "09:00" }),
            json!({ "name": "a", "prompt": "b", "repeat": "weekly", "time": "09:00" }),
            json!({ "name": "a", "prompt": "b", "repeat": "once", "time": "09:00", "date": "tomorrow" }),
            json!({ "name": "", "prompt": "b", "repeat": "daily", "time": "09:00" }),
        ] {
            assert!(propose_schedule(&bad).is_err(), "{bad}");
        }
    }
}
