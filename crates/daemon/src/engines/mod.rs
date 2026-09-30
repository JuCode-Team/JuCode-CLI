//! Sessions run by another agent CLI (Claude Code, Codex, ACP agents) as a
//! child process. An adapter translates the engine's own wire format into the
//! jucode event dialect and client ops into engine frames, so every client
//! sees the same events it gets from a jucode session. The daemon keeps a
//! snapshot of each session (state events, transcript, pending approvals) for
//! clients that start watching mid-session.

pub mod claude;

use crate::hub::Hub;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread,
    time::Duration,
};

/// How long a retired engine gets to exit on its own after its stdin closes.
const EXIT_GRACE: Duration = Duration::from_millis(1500);
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Claude,
}

impl Kind {
    pub fn parse(name: &str) -> Result<Option<Self>, String> {
        match name {
            "" | "jucode" => Ok(None),
            "claude" => Ok(Some(Kind::Claude)),
            other => Err(format!("unknown engine {other}")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
        }
    }
}

/// How an engine is started. `resume` names the engine's own conversation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    /// The client's approval mode (jucode or desktop names).
    pub approval_mode: Option<String>,
    pub model: Option<String>,
    pub resume: Option<String>,
    /// Claude: resume the conversation as it was at this message.
    pub resume_at: Option<String>,
}

impl Options {
    pub fn from_json(value: &Value) -> Self {
        let text = |key: &str| {
            value[key]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        Self {
            approval_mode: text("approval_mode"),
            model: text("model"),
            resume: None,
            resume_at: text("resume_at"),
        }
    }
}

/// One line from the engine.
#[derive(Debug)]
pub enum Line {
    Frame(Value),
    Stderr(String),
}

/// What an adapter makes of a line or an op: events for clients and frames
/// for the engine's stdin.
#[derive(Debug, Default)]
pub struct Output {
    pub events: Vec<Value>,
    pub frames: Vec<String>,
}

impl Output {
    fn events(events: Vec<Value>) -> Self {
        Self {
            events,
            frames: Vec::new(),
        }
    }
}

pub trait Adapter: Send {
    /// Frames to send once the engine started.
    fn start(&mut self) -> Vec<String>;
    fn translate(&mut self, line: Line) -> Output;
    /// Frames for a client op; Err when the engine cannot do it.
    fn encode(&mut self, op: &Value) -> Result<Output, String>;
    /// A turn is running.
    fn busy(&self) -> bool;
    /// Options for restarting the engine to apply `op`, when the op needs a
    /// new process (Claude's full-access mode is a start flag).
    fn restart_for(&self, op: &Value) -> Option<Options>;
    /// The engine's conversation id, for resuming it.
    fn conversation(&self) -> Option<String>;
}

fn adapter(kind: Kind, options: &Options) -> Box<dyn Adapter> {
    match kind {
        Kind::Claude => Box::new(claude::Claude::new(options)),
    }
}

fn command(kind: Kind, id: &str, options: &Options) -> Command {
    match kind {
        Kind::Claude => claude::command(id, options),
    }
}

/// A running engine process: its stdin writer and merged output.
struct Process {
    child: Child,
    stdin: Option<Sender<String>>,
    lines: Receiver<Result<Line, String>>,
}

impl Process {
    fn spawn(mut command: Command, cwd: &Path) -> Result<Self, String> {
        let program = command.get_program().to_string_lossy().to_string();
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start {program}: {error}"))?;
        let (lines_tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;
        let mut stdin_pipe = child.stdin.take().ok_or("no stdin")?;
        let (stdin, stdin_rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            for frame in stdin_rx {
                if writeln!(stdin_pipe, "{frame}")
                    .and_then(|()| stdin_pipe.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        let errors = lines_tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if errors.send(Ok(Line::Stderr(line))).is_err() {
                    break;
                }
            }
        });
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // Anything that is not a JSON object is engine noise.
                if let Ok(frame @ Value::Object(_)) = serde_json::from_str::<Value>(&line) {
                    if lines_tx.send(Ok(Line::Frame(frame))).is_err() {
                        return;
                    }
                }
            }
            let _ = lines_tx.send(Err("the engine closed its output".to_string()));
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            lines,
        })
    }

    fn write(&self, frames: Vec<String>) {
        if let Some(stdin) = &self.stdin {
            for frame in frames {
                let _ = stdin.send(frame);
            }
        }
    }

    /// Closes stdin and kills the process if it has not exited after a grace
    /// period. Returns how it ended.
    fn stop(mut self) {
        self.stdin = None;
        thread::spawn(move || {
            let deadline = std::time::Instant::now() + EXIT_GRACE;
            while std::time::Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        });
    }

    fn exit_reason(&mut self) -> String {
        match self.child.wait() {
            Ok(status) => match status.code() {
                Some(code) => format!("exit code {code}"),
                None => "killed by a signal".to_string(),
            },
            Err(error) => error.to_string(),
        }
    }
}

/// The state a client needs when it starts watching.
#[derive(Default)]
pub struct Snapshot {
    /// Latest event of each state type, by type.
    state: BTreeMap<&'static str, Value>,
    transcript: Vec<Value>,
    /// Open approval requests, by call id, oldest first.
    approvals: Vec<(String, Value)>,
    /// The transcript's last item is an assistant reply still streaming.
    in_reply: bool,
}

const STATE_EVENTS: &[&str] = &[
    "startup",
    "model_status",
    "command_list",
    "approval_mode",
    "mcp_servers",
    "plan",
    "rate_limit",
];

impl Snapshot {
    pub fn seed(&mut self, transcript: Vec<Value>) {
        self.transcript = transcript;
    }

    fn apply(&mut self, event: &Value) {
        let kind = event["type"].as_str().unwrap_or_default();
        if let Some(name) = STATE_EVENTS.iter().find(|name| **name == kind) {
            self.state.insert(name, event.clone());
            return;
        }
        match kind {
            "user_message" => {
                self.in_reply = false;
                self.transcript
                    .push(json!({ "role": "user", "content": event["content"] }));
            }
            "assistant_start" => {
                self.in_reply = true;
                self.transcript
                    .push(json!({ "role": "assistant", "content": "" }));
            }
            "assistant_delta" => {
                if !self.in_reply {
                    self.in_reply = true;
                    self.transcript
                        .push(json!({ "role": "assistant", "content": "" }));
                }
                if let Some(last) = self.transcript.last_mut() {
                    let text = format!(
                        "{}{}",
                        last["content"].as_str().unwrap_or_default(),
                        event["delta"].as_str().unwrap_or_default()
                    );
                    last["content"] = json!(text);
                }
            }
            "tool_start" => {
                self.in_reply = false;
                self.transcript.push(json!({
                    "role": "tool", "name": event["name"], "output": "", "call_id": event["call_id"],
                }));
            }
            "tool_output" => {
                if let Some(item) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|item| item["call_id"] == event["call_id"])
                {
                    item["output"] = event["output"].clone();
                }
            }
            "approval_request" => {
                if let Some(call) = event["call_id"].as_str() {
                    self.approvals.push((call.to_string(), event.clone()));
                }
            }
            "status" if event["message"] == "ready" => {
                self.in_reply = false;
                self.approvals.clear();
            }
            _ => {}
        }
    }

    fn answered(&mut self, call: &str) {
        self.approvals.retain(|(id, _)| id != call);
    }

    fn events(&self, busy: bool) -> Vec<Value> {
        let mut events: Vec<Value> = STATE_EVENTS
            .iter()
            .filter_map(|name| self.state.get(name).cloned())
            .collect();
        let items: Vec<Value> = self
            .transcript
            .iter()
            .map(|item| {
                let mut item = item.clone();
                if let Some(map) = item.as_object_mut() {
                    map.remove("call_id");
                }
                item
            })
            .collect();
        events.push(json!({ "type": "transcript", "items": items }));
        if busy {
            events.push(json!({ "type": "connecting" }));
        }
        events.extend(self.approvals.iter().map(|(_, event)| event.clone()));
        events.push(json!({ "type": "attended", "attended": true }));
        events
    }
}

/// Starts a `kind` engine session in `cwd` on its own thread: a new one with
/// id `id`, or `options.resume`. Returns the thread's ops channel and
/// generation, or the start error.
pub fn spawn(
    hub: Arc<Hub>,
    kind: Kind,
    id: String,
    cwd: PathBuf,
    options: Options,
    transcript: Vec<Value>,
) -> Result<(Sender<Value>, u64), String> {
    let process = Process::spawn(command(kind, &id, &options), &cwd)?;
    let (ops_tx, ops) = mpsc::channel();
    let generation = hub.next_generation();
    thread::spawn(move || {
        let mut session = Session {
            hub: &hub,
            id: &id,
            kind,
            cwd,
            snapshot: Snapshot::default(),
            restart: None,
        };
        session.snapshot.seed(transcript);
        session.run(process, options, ops);
        hub.session_ended(&id, generation);
    });
    Ok((ops_tx, generation))
}

struct Session<'a> {
    hub: &'a Hub,
    id: &'a str,
    kind: Kind,
    cwd: PathBuf,
    snapshot: Snapshot,
    /// Options to restart with once the running turn ends.
    restart: Option<Options>,
}

impl Session<'_> {
    fn run(&mut self, mut process: Process, options: Options, ops: Receiver<Value>) {
        let mut adapter = adapter(self.kind, &options);
        process.write(adapter.start());
        loop {
            loop {
                match ops.try_recv() {
                    Ok(op) => {
                        if self.apply(&mut process, adapter.as_mut(), &op) {
                            process.stop();
                            return;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        process.stop();
                        return;
                    }
                }
            }
            let mut first = true;
            loop {
                let next = if first {
                    process.lines.recv_timeout(POLL)
                } else {
                    process.lines.try_recv().map_err(|error| match error {
                        mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                        mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                    })
                };
                first = false;
                match next {
                    Ok(Ok(line)) => {
                        let output = adapter.translate(line);
                        process.write(output.frames);
                        self.publish(output.events);
                    }
                    Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                        let reason = process.exit_reason();
                        self.publish(vec![json!({
                            "type": "error",
                            "message": format!("{} stopped ({reason})", self.kind.name()),
                        })]);
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                }
            }
            let busy = adapter.busy();
            self.hub.set_busy(self.id, busy);
            if !busy {
                if let Some(mut next) = self.restart.take() {
                    next.resume = adapter.conversation().or(next.resume);
                    process.stop();
                    match Process::spawn(command(self.kind, self.id, &next), &self.cwd) {
                        Ok(started) => {
                            process = started;
                            adapter = self::adapter(self.kind, &next);
                            process.write(adapter.start());
                        }
                        Err(error) => {
                            self.publish(vec![json!({ "type": "error", "message": error })]);
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Applies one op; returns true when the session should stop.
    fn apply(&mut self, process: &mut Process, adapter: &mut dyn Adapter, op: &Value) -> bool {
        if op["claimed"] == true {
            self.hub.release_claim(self.id);
        }
        match op["op"].as_str().unwrap_or_default() {
            "snapshot" => {
                if let Some(client) = op["client"].as_u64() {
                    for event in self.snapshot.events(adapter.busy()) {
                        self.hub.send_to(client, &self.tagged(event));
                    }
                }
                false
            }
            "shutdown" => true,
            // A client watching or not changes nothing: approvals wait for
            // whoever answers them next.
            "set_attended" => false,
            name => {
                if let Some(options) = adapter.restart_for(op) {
                    self.restart = Some(options);
                    if adapter.busy() {
                        self.publish(vec![json!({
                            "type": "info",
                            "message": "the new mode applies once the running turn ends",
                        })]);
                    }
                    return false;
                }
                if name == "approve" {
                    if let Some(call) = op["call_id"].as_str() {
                        self.snapshot.answered(call);
                    }
                }
                match adapter.encode(op) {
                    Ok(output) => {
                        process.write(output.frames);
                        self.publish(output.events);
                    }
                    Err(message) => {
                        self.publish(vec![json!({ "type": "error", "message": message })])
                    }
                }
                false
            }
        }
    }

    fn tagged(&self, mut event: Value) -> Value {
        event["session"] = json!(self.id);
        event
    }

    fn publish(&mut self, events: Vec<Value>) {
        for event in events {
            self.snapshot.apply(&event);
            self.hub.broadcast(&self.tagged(event));
        }
    }
}

/// The engine binary: `<NAME>_BIN`, then PATH, then the usual install
/// directories, else the bare name.
pub fn resolve(name: &str, env_override: &str, extra: &[PathBuf]) -> PathBuf {
    if let Some(path) = std::env::var_os(env_override).filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let path_dirs = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default();
    let home = home();
    let known = [
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".cargo/bin"),
        home.join(".local/bin"),
    ];
    path_dirs
        .iter()
        .chain(known.iter())
        .map(|dir| dir.join(&exe))
        .chain(extra.iter().cloned())
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(exe))
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// A random UUID v4, for engines that take the conversation id from us.
pub fn new_uuid() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| error.to_string())?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_rebuilds_the_conversation_and_open_approvals() {
        let mut snapshot = Snapshot::default();
        for event in [
            json!({ "type": "startup", "model": "a" }),
            json!({ "type": "startup", "model": "b" }),
            json!({ "type": "user_message", "content": "hi" }),
            json!({ "type": "assistant_start" }),
            json!({ "type": "assistant_delta", "delta": "hel" }),
            json!({ "type": "assistant_delta", "delta": "lo" }),
            json!({ "type": "tool_start", "call_id": "t1", "name": "bash" }),
            json!({ "type": "tool_output", "call_id": "t1", "name": "bash", "output": "ok", "is_error": false }),
            json!({ "type": "approval_request", "call_id": "a1", "name": "bash" }),
            json!({ "type": "approval_request", "call_id": "a2", "name": "write" }),
        ] {
            snapshot.apply(&event);
        }
        snapshot.answered("a1");
        let events = snapshot.events(true);
        assert_eq!(events[0], json!({ "type": "startup", "model": "b" }));
        let transcript = events.iter().find(|e| e["type"] == "transcript").unwrap();
        assert_eq!(
            transcript["items"],
            json!([
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "hello" },
                { "role": "tool", "name": "bash", "output": "ok" },
            ])
        );
        let open: Vec<&Value> = events
            .iter()
            .filter(|e| e["type"] == "approval_request")
            .collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["call_id"], "a2");
        assert!(events.iter().any(|e| e["type"] == "connecting"));

        snapshot.apply(&json!({ "type": "status", "message": "ready" }));
        assert!(!snapshot
            .events(false)
            .iter()
            .any(|e| e["type"] == "approval_request"));
    }

    #[test]
    fn uuids_are_version_4() {
        let id = new_uuid().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_ne!(new_uuid().unwrap(), id);
    }
}
