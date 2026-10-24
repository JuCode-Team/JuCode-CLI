//! Sessions run by another agent CLI (Claude Code, Codex, ACP agents) as a
//! child process. An adapter translates the engine's own wire format into the
//! jucode event dialect and client ops into engine frames, so every client
//! sees the same events it gets from a jucode session. The daemon keeps a
//! snapshot of each session (state events, transcript, pending approvals) for
//! clients that start watching mid-session.

pub mod acp;
pub mod acp_presets;
pub mod claude;
pub mod codex;

use crate::hub::Hub;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

/// How long a retired engine gets to exit on its own after its stdin closes.
const EXIT_GRACE: Duration = Duration::from_millis(1500);
const POLL: Duration = Duration::from_millis(20);
/// How long an engine may take to open its conversation.
const START_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Claude,
    Codex,
    Acp,
}

impl Kind {
    pub fn parse(name: &str) -> Result<Option<Self>, String> {
        match name {
            "" | "jucode" => Ok(None),
            "claude" => Ok(Some(Kind::Claude)),
            "codex" => Ok(Some(Kind::Codex)),
            "acp" => Ok(Some(Kind::Acp)),
            other => Err(format!("unknown engine {other}")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
            Kind::Acp => "acp",
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
    /// A new session that forks this conversation (the engine's id) instead
    /// of starting empty; it goes on apart from it. Only the start uses it.
    pub fork_from: Option<String>,
    /// Codex / jucode: how many of `fork_from`'s user turns the fork keeps
    /// (None: all). Claude forks at `resume_at` instead.
    pub fork_turns: Option<u64>,
    /// jucode: the directory `fork_from` was saved for, when the fork works
    /// elsewhere (its own worktree). None: the fork's own.
    pub fork_cwd: Option<PathBuf>,
    /// ACP: a built-in agent (`acp_presets`), by id.
    pub agent: Option<String>,
    /// ACP: the agent's command line.
    pub command: Option<String>,
    pub args: Vec<String>,
    /// Claude / Codex: the engine binary instead of the one on PATH.
    pub bin: Option<String>,
    /// Extra environment for the engine process.
    pub env: Vec<(String, String)>,
    /// Claude / Codex / gateway ACP presets: talk to the JuCode gateway on
    /// the user's JuCode login instead of the provider in their own config
    /// (which stays untouched). None: as the session last ran.
    pub gateway: Option<bool>,
    /// Claude / Codex off the JuCode gateway: the custom provider
    /// (`tool_providers`) the session runs on, by id; "" the tool's own
    /// configuration; None the tool's current default (a new session). A
    /// session keeps the one it started on (see `pin_tool_provider`).
    pub tool_provider: Option<String>,
    /// Claude: start with ultracode on (standing Workflow orchestration).
    pub ultracode: bool,
    /// Claude and ACP agents: the thinking effort to start with.
    pub effort: Option<String>,
    /// Claude: start in fast mode.
    pub fast: bool,
    /// Claude: show thinking summaries (Some(false) hides them).
    pub thinking: Option<bool>,
    /// Claude / Codex: directories besides `cwd` the engine may work in (the
    /// session's project's; set by the hub, never by a client).
    pub dirs: Vec<PathBuf>,
    /// The app tools (`user_tools::NAMES`) the user turned off (set by
    /// `spawn` from the daemon's settings, never by a client).
    pub app_tools_off: Vec<&'static str>,
    /// Claude: the config directory the session's CLI keeps its state in,
    /// once found (`claude::config_with`; set by the hub, never by a client).
    pub claude_config: Option<PathBuf>,
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
            fork_from: text("fork_from"),
            fork_turns: value["fork_turns"].as_u64(),
            fork_cwd: text("fork_cwd").map(PathBuf::from),
            agent: text("agent"),
            command: text("command"),
            bin: text("bin"),
            args: value["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|arg| arg.as_str().map(str::to_string))
                .collect(),
            env: value["env"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
                .collect(),
            gateway: value["jucode_gateway"].as_bool(),
            tool_provider: value["tool_provider"]
                .as_str()
                .map(|id| id.trim().to_string()),
            ultracode: value["ultracode"] == true,
            effort: text("effort"),
            fast: value["fast"] == true,
            thinking: value["thinking"].as_bool(),
            dirs: Vec::new(),
            app_tools_off: Vec::new(),
            claude_config: None,
        }
    }

    /// Whether these options name a program or environment to run, which
    /// only the desktop may do.
    pub fn runs_programs(&self) -> bool {
        self.command.is_some()
            || self.bin.is_some()
            || !self.args.is_empty()
            || !self.env.is_empty()
    }

    /// Environment names an agent may be given: plain names, never ones that
    /// change how programs load.
    pub fn check_env(&self) -> Result<(), String> {
        for (name, _) in &self.env {
            let plain =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !plain || name.starts_with("DYLD_") || name.starts_with("LD_") {
                return Err(format!("environment variable {name} is not allowed"));
            }
        }
        Ok(())
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

/// The engine called one of the tools the daemon gives it
/// (`jucode_agent_core::user_tools`); `token` is how the adapter finds the
/// request to answer.
#[derive(Debug)]
pub struct HostCall {
    pub token: Value,
    pub name: String,
    pub args: Value,
}

/// Runs a daemon tool for a session in `cwd`: its JSON output, or what the
/// model should fix. The session tools are not among them (see
/// `Session::host_calls`).
fn host_call(cwd: &Path, call: &HostCall) -> Result<Value, String> {
    jucode_agent_core::user_tools::run(&call.name, &call.args, cwd)
        .unwrap_or_else(|| Err(format!("unknown tool: {}", call.name)))
}

/// The daemon setting holding the app tools' switches (`user_tools::GROUPS`
/// keys; a switch not there is on).
pub const APP_TOOLS_SETTING: &str = "app_tools";

/// The app tools the user turned off, for engines starting now.
pub fn app_tools_off(store: &crate::store::Store) -> Vec<&'static str> {
    let setting = store.setting(APP_TOOLS_SETTING);
    jucode_agent_core::user_tools::GROUPS
        .iter()
        .filter(|(key, _)| setting[*key] == false)
        .flat_map(|(_, tools)| tools.iter().copied())
        .collect()
}

/// The switches, as `app_tools` and `app_tools_set` answer.
pub fn app_tools_json(store: &crate::store::Store) -> Value {
    let setting = store.setting(APP_TOOLS_SETTING);
    let mut out = json!({ "type": "app_tools" });
    for (key, _) in jucode_agent_core::user_tools::GROUPS {
        out[key] = json!(setting[key] != false);
    }
    out
}

/// Sets the switches `op` names; the others stay.
pub fn set_app_tools(store: &crate::store::Store, op: &Value) -> Result<Value, String> {
    let mut setting = store.setting(APP_TOOLS_SETTING);
    if !setting.is_object() {
        setting = json!({});
    }
    for (key, _) in jucode_agent_core::user_tools::GROUPS {
        if let Some(on) = op[key].as_bool() {
            setting[key] = json!(on);
        }
    }
    store
        .set_setting(APP_TOOLS_SETTING, setting)
        .map_err(|error| error.to_string())?;
    Ok(app_tools_json(store))
}

/// A session tool's result, for the adapter to answer with.
type Answer = (Value, Result<Value, String>);

pub trait Adapter: Send {
    /// Frames to send once the engine started.
    fn start(&mut self) -> Vec<String>;
    /// A probe's run (`probe`): nothing that waits on the user (a sign-in
    /// in the browser) starts from it.
    fn probing(&mut self) {}
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
    /// The session settings a restart keeps (Claude: effort, ultracode,
    /// fast mode, thinking display).
    fn keep(&self, options: Options) -> Options {
        options
    }
    /// The approval mode it runs in now (client names), kept across a restart.
    fn approval_mode(&self) -> Option<String> {
        None
    }
    /// The model it runs now (a switch mid-session included).
    fn model(&self) -> Option<String> {
        None
    }
    /// A mode switch reaches the running turn (else it applies to the next).
    fn mode_applies_live(&self) -> bool {
        true
    }
    /// The daemon-tool calls the engine made since last asked.
    fn take_calls(&mut self) -> Vec<HostCall> {
        Vec::new()
    }
    /// Frames answering a `HostCall` with the tool's result.
    fn answer(&mut self, _token: &Value, _result: Result<Value, String>) -> Vec<String> {
        Vec::new()
    }
    /// It can move to the JuCode gateway (`set_gateway`).
    fn has_gateway(&self) -> bool {
        true
    }
}

fn adapter(kind: Kind, cwd: &Path, options: &Options) -> Box<dyn Adapter> {
    match kind {
        Kind::Claude => Box::new(claude::Claude::new(options).at(cwd)),
        Kind::Codex => Box::new(codex::Codex::new(cwd, options)),
        Kind::Acp => Box::new(acp::Acp::new(cwd, options).with_mcp(jucode_agent_core::acp_mcp_servers())),
    }
}

/// The engine's command, and the local gateway key it holds when it runs
/// on the JuCode gateway (see crate::gateway).
fn command(
    hub: &Hub,
    kind: Kind,
    id: &str,
    cwd: &Path,
    options: &Options,
) -> Result<(Command, Option<String>), String> {
    let command = match kind {
        Kind::Claude => claude::command(id, options),
        Kind::Codex => codex::command(options),
        Kind::Acp => acp::command(options)?,
    };
    with_gateway(hub, kind, id, cwd, options, command)
}

/// The engine's own TUI resuming conversation `id` (Claude Code, Codex, a
/// built-in ACP agent), configured as its GUI process is: environment and
/// gateway, with a gateway key for `session`.
fn tui_command(
    hub: &Hub,
    kind: Kind,
    id: &str,
    session: &str,
    cwd: &Path,
    options: &Options,
    saved: bool,
) -> Result<(Command, Option<String>), String> {
    let command = match kind {
        Kind::Claude => claude::tui(id, options, saved),
        Kind::Codex => codex::tui(saved.then_some(id), options),
        Kind::Acp => match acp::preset(options)? {
            Some(preset) => acp_presets::tui(preset, options, id)?,
            None => return Err("a registered ACP agent has no TUI here".to_string()),
        },
    };
    with_gateway(hub, kind, session, cwd, options, command)
}

/// `command` with the session's environment, and on the JuCode gateway when
/// it runs there: with the local gateway key it holds.
fn with_gateway(
    hub: &Hub,
    kind: Kind,
    id: &str,
    cwd: &Path,
    options: &Options,
    mut command: Command,
) -> Result<(Command, Option<String>), String> {
    command.envs(options.env.iter().map(|(name, value)| (name, value)));
    let custom = custom_provider(&hub.store, kind, options)?;
    if options.gateway != Some(true) && custom.is_none() {
        return Ok((command, None));
    }
    let preset = match kind {
        Kind::Acp => match acp::preset(options)? {
            Some(preset) if preset.gateway => Some(preset),
            _ => return Err("this agent has no JuCode gateway mode".to_string()),
        },
        _ => None,
    };
    // Not signed in fails the start, not the first request.
    if custom.is_none() {
        jucode_agent_core::jucode_gateway_credentials()?;
    }
    let base = crate::gateway::base_url()?;
    let key = crate::gateway::issue(id)?;
    let model = custom
        .as_ref()
        .and_then(|p| p["model"].as_str().map(str::to_string));
    let name = custom
        .as_ref()
        .and_then(|p| p["name"].as_str().map(str::to_string));
    if let Some(provider) = custom {
        let owner = hub
            .handle()
            .as_ref()
            .map(Arc::downgrade)
            .unwrap_or_default();
        crate::gateway::set_custom(&key, kind.name(), provider, owner);
    }
    let configured = match (kind, preset) {
        (Kind::Claude, _) => claude::use_gateway(&mut command, id, &base, &key, model.as_deref()),
        (_, Some(preset)) => acp_presets::use_gateway(
            &mut command,
            preset,
            cwd,
            &base,
            &key,
            options.model.as_deref(),
        ),
        _ => codex::use_gateway(&mut command, &base, &key, name.as_deref()),
    };
    if let Err(error) = configured {
        crate::gateway::revoke(&key);
        return Err(error);
    }
    Ok((command, Some(key)))
}

/// Settles which custom provider a Claude / Codex session off the gateway
/// runs on, so it keeps that one when it reopens or restarts: one upstream
/// rejects another's conversation state (Claude's thinking signatures).
/// None (a new session, or one leaving the gateway) takes the tool's
/// current default.
pub fn pin_tool_provider(store: &crate::store::Store, kind: Kind, options: &mut Options) {
    if matches!(kind, Kind::Claude | Kind::Codex)
        && options.gateway != Some(true)
        && options.tool_provider.is_none()
    {
        let selected = crate::tool_providers::selected(store, kind.name());
        options.tool_provider = Some(
            selected
                .and_then(|p| p["id"].as_str().map(str::to_string))
                .unwrap_or_default(),
        );
    }
}

/// The custom provider a session runs on (None: the gateway or the tool's
/// own configuration). One that was removed fails the start, rather than
/// moving the conversation somewhere else.
fn custom_provider(
    store: &crate::store::Store,
    kind: Kind,
    options: &Options,
) -> Result<Option<Value>, String> {
    if !matches!(kind, Kind::Claude | Kind::Codex) || options.gateway == Some(true) {
        return Ok(None);
    }
    match options.tool_provider.as_deref() {
        None | Some("") => Ok(None),
        Some(id) => crate::tool_providers::find(store, kind.name(), id)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "这个会话使用的自定义服务商「{id}」已被删除，无法继续。请把会话切换到 JuCode 网关或 {} 自身的配置后再打开。",
                    match kind {
                        Kind::Claude => "Claude Code",
                        _ => "Codex",
                    }
                )
            }),
    }
}

/// The `tool_provider` state event: the custom provider the session runs
/// on (id, name, model), or null.
fn tool_provider_event(store: &crate::store::Store, kind: Kind, options: &Options) -> Value {
    let provider = custom_provider(store, kind, options)
        .ok()
        .flatten()
        .map(|p| crate::tool_providers::view(&p));
    json!({ "type": "tool_provider", "provider": provider })
}

/// One window of an official plan's usage for the `plan_usage` event: its
/// share used (0-100), when it resets (unix ms) and its length.
pub(crate) fn plan_window(
    key: &str,
    used_percent: f64,
    resets_secs: &Value,
    minutes: Option<u64>,
) -> Value {
    json!({
        "key": key,
        "used": (used_percent * 10.0).round() / 10.0,
        "resets_at": resets_secs.as_f64().map(|s| (s * 1000.0) as u64),
        "minutes": minutes,
    })
}

/// A local gateway key is done with: no request may use it again.
/// The events a probe keeps (the last of each kind): what a client shows of
/// an engine before any conversation of it.
const PROBE_KEEPS: &[&str] = &["model_status", "model_view", "engine_caps", "command_list", "session_options"];
/// A probe gives up after this long, and waits this long for the model list
/// once the engine has said something.
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const PROBE_SETTLE: Duration = Duration::from_secs(4);

/// What an engine says of itself before any conversation (as t3code probes
/// its providers): started in `cwd` as a session of it would be, out of every
/// list, asked for its models, then stopped. Returns the last event of each
/// PROBE_KEEPS kind it sent.
pub fn probe(hub: &Hub, kind: Kind, cwd: &Path, options: &Options) -> Result<Vec<Value>, String> {
    let id = new_uuid()?;
    let (command, key) = command(hub, kind, &id, cwd, options)?;
    let started = Process::spawn(command, cwd);
    let process = match started {
        Ok(process) => process,
        Err(error) => {
            if let Some(key) = &key {
                release_key(kind, key);
            }
            return Err(error);
        }
    };
    let mut engine = adapter(kind, cwd, options);
    engine.probing();
    process.write(engine.start());
    let mut kept: HashMap<String, Value> = HashMap::new();
    let keep = |events: Vec<Value>, kept: &mut HashMap<String, Value>| {
        for event in events {
            if let Some(kind) = event["type"].as_str().filter(|t| PROBE_KEEPS.contains(t)) {
                kept.insert(kind.to_string(), event);
            }
        }
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut settle: Option<Instant> = None;
    let mut asked = false;
    loop {
        let until = settle.map_or(deadline, |at| at.min(deadline));
        let Some(wait) = until.checked_duration_since(Instant::now()) else { break };
        match process.lines.recv_timeout(wait) {
            Ok(Ok(line)) => {
                let out = engine.translate(line);
                process.write(out.frames);
                keep(out.events, &mut kept);
            }
            Ok(Err(_)) | Err(_) => break,
        }
        // Talking: it has a little longer to say the rest. Its conversation
        // open (it said which model it is on): its models, asked for.
        if settle.is_none() && !kept.is_empty() {
            settle = Some(Instant::now() + PROBE_SETTLE);
        }
        if !asked && kept.contains_key("model_status") {
            asked = true;
            settle = Some(Instant::now() + PROBE_SETTLE);
            if let Ok(out) = engine.encode(&json!({ "op": "command", "input": "/model" })) {
                process.write(out.frames);
                keep(out.events, &mut kept);
            }
        }
        if kept.contains_key("model_view") {
            break;
        }
    }
    process.kill();
    if let Some(key) = &key {
        release_key(kind, key);
    }
    if kept.is_empty() {
        return Err(format!("{} said nothing about itself", kind.name()));
    }
    Ok(kept.into_values().collect())
}

fn release_key(kind: Kind, key: &str) {
    crate::gateway::revoke(key);
    if kind == Kind::Claude {
        claude::forget_gateway(key);
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
        // Its own process group: stopping the engine ends what it started
        // (MCP servers, shells) with it, and the daemon ends the group when
        // it exits (`terminate_tool_processes`).
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start {program}: {error}"))?;
        #[cfg(unix)]
        jucode_agent_core::track_process_group(child.id());
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
    /// period; what it started goes with it (`end_group`).
    fn stop(mut self) {
        self.stdin = None;
        thread::spawn(move || {
            let deadline = std::time::Instant::now() + EXIT_GRACE;
            while std::time::Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    self.end_group(false);
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            self.end_group(true);
            let _ = self.child.kill();
            let _ = self.child.wait();
        });
    }

    /// Ends the engine now, without the grace `stop` gives it.
    fn kill(mut self) {
        self.end_group(true);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Signals the engine's process group: what it started and left running
    /// (MCP servers, shells) ends with it. `now`: SIGKILL, else SIGTERM.
    fn end_group(&self, now: bool) {
        jucode_agent_core::signal_process_group(self.child.id(), now);
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

/// How an engine closed for idleness opens again: as it runs now (model and
/// approval mode switched mid-session, the settings `keep` carries), on its
/// conversation as a whole (no rewind point from an earlier open).
fn asleep_options(adapter: &dyn Adapter, current: &Options) -> Options {
    Options {
        model: adapter.model().or(current.model.clone()),
        approval_mode: adapter.approval_mode().or(current.approval_mode.clone()),
        resume: None,
        resume_at: None,
        ..adapter.keep(current.clone())
    }
}

/// Whether a subagent or workflow in this state is working or about to.
fn running(state: &str) -> bool {
    matches!(
        state,
        "running" | "pending" | "queued" | "start" | "started" | "async_launched" | "in_progress"
    )
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
    "approval_mode_pending",
    "mcp_servers",
    "background_tasks",
    "agent_runs",
    "plan",
    "rate_limit",
    "plan_usage",
    "engine_caps",
    "session_options",
    "tool_provider",
    // Messages waiting for the turn: a client that attaches (another
    // device, a desktop back from another workspace) shows them too.
    "pending_messages",
];

/// Now, ms since the epoch: when a snapshot item happened.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

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
            "transcript" => {
                self.in_reply = false;
                self.transcript = event["items"].as_array().cloned().unwrap_or_default();
            }
            "user_message" => {
                self.in_reply = false;
                let mut item =
                    json!({ "role": "user", "content": event["content"], "at": now_ms() });
                if event["images"].is_array() {
                    item["images"] = event["images"].clone();
                }
                self.transcript.push(item);
            }
            "assistant_start" => {
                self.in_reply = true;
                self.transcript
                    .push(json!({ "role": "assistant", "content": "", "at": now_ms() }));
            }
            "assistant_delta" => {
                if !self.in_reply {
                    self.in_reply = true;
                    self.transcript
                        .push(json!({ "role": "assistant", "content": "", "at": now_ms() }));
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
                let mut item = json!({
                    "role": "tool", "name": event["name"], "output": "", "call_id": event["call_id"], "at": now_ms(),
                });
                // A subagent's call: the client keeps it out of this conversation.
                if event["subagent"].is_string() {
                    item["subagent"] = event["subagent"].clone();
                }
                self.transcript.push(item);
            }
            "tool_output" => {
                if let Some(item) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|item| item["call_id"] == event["call_id"])
                {
                    item["output"] = event["output"].clone();
                    if event["is_error"] == true {
                        item["is_error"] = json!(true);
                    }
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

    /// Work outside a turn keeps the engine open (`Hub::set_held`):
    /// background work, approvals waiting.
    fn holds(&self, kind: Kind) -> bool {
        self.background(kind).0 || !self.approvals.is_empty()
    }

    /// Background work the turn left going (`Hub::set_background`): any
    /// (background commands, agents, workflows), and whether some of it will
    /// come back to the conversation (agents, workflows; a dev server never
    /// does). Claude Code's run list is rebuilt from its transcript (a run
    /// that never ended stays "running" there); its live background work is
    /// in `background_tasks`, so only other engines' runs count.
    fn background(&self, kind: Kind) -> (bool, bool) {
        let list = self
            .state
            .get("background_tasks")
            .and_then(|event| event["tasks"].as_array());
        let tasks = list.is_some_and(|tasks| !tasks.is_empty());
        let agents = list.into_iter().flatten().any(|task| {
            let kind = task["kind"].as_str().unwrap_or_default();
            kind.starts_with("local_agent") || kind.starts_with("local_workflow")
        });
        let runs = kind != Kind::Claude
            && self.state.get("agent_runs").is_some_and(|event| {
                let workflows = event["workflows"].as_array().into_iter().flatten();
                let nested = workflows
                    .clone()
                    .flat_map(|workflow| workflow["agents"].as_array().into_iter().flatten());
                event["agents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(workflows)
                    .chain(nested)
                    .any(|run| {
                        // Runs say `status`; a workflow's agents say `state`.
                        let state = run["status"].as_str().or(run["state"].as_str());
                        running(state.unwrap_or_default())
                    })
            });
        (tasks || runs, agents || runs)
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

/// Starts a `kind` engine session in `cwd` on its own thread: a new one
/// named `id` when the daemon picks the id (Claude Code), or `options.resume`,
/// or a new one the engine names (Codex). Returns the session id once the
/// engine has opened its conversation, with the thread's ops channel and
/// generation, or the start error.
pub fn spawn(
    hub: Arc<Hub>,
    kind: Kind,
    id: Option<String>,
    cwd: PathBuf,
    options: Options,
    transcript: Vec<Value>,
) -> Result<(String, Sender<Value>, u64), String> {
    let custom = custom_provider(&hub.store, kind, &options)?;
    let options = Options {
        model: options
            .model
            .clone()
            .or_else(|| custom.and_then(|p| p["model"].as_str().map(str::to_string))),
        app_tools_off: app_tools_off(&hub.store),
        ..options
    };
    let (command, gateway_key) = command(
        &hub,
        kind,
        id.as_deref().unwrap_or_default(),
        &cwd,
        &options,
    )?;
    let process = match Process::spawn(command, &cwd) {
        Ok(process) => process,
        Err(error) => {
            if let Some(key) = &gateway_key {
                release_key(kind, key);
            }
            return Err(error);
        }
    };
    let (ops_tx, ops) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let generation = hub.next_generation();
    thread::spawn(move || {
        let mut session = Session {
            hub: &hub,
            id: None,
            ready: Some(ready_tx),
            early: Vec::new(),
            last_error: None,
            stderr: Default::default(),
            kind,
            cwd,
            snapshot: Snapshot::default(),
            restart: None,
            pending_mode: None,
            gateway_key,
            tui_request: None,
        };
        session.snapshot.seed(transcript);
        // A resumed conversation is ready once the engine has opened it.
        if let Some(id) = id {
            session.named(id);
        }
        session.run(process, options, ops);
        match (session.id.clone(), session.ready.take()) {
            (Some(id), _) => hub.session_ended(&id, generation),
            (None, Some(ready)) => {
                let reason = session.last_error.take().unwrap_or_else(|| {
                    session.stopped_message(&format!(
                        "{} stopped before opening a conversation",
                        kind.name()
                    ))
                });
                let _ = ready.send(Err(reason));
            }
            (None, None) => {}
        }
    });
    // A timeout drops the ops channel, which stops the engine.
    let id = ready_rx
        .recv_timeout(START_TIMEOUT)
        .map_err(|_| format!("{} did not open a conversation in time", kind.name()))??;
    Ok((id, ops_tx, generation))
}

struct Session<'a> {
    hub: &'a Hub,
    /// The session id: the engine's conversation id, known once it opened.
    id: Option<String>,
    ready: Option<Sender<Result<String, String>>>,
    /// Events from before the id was known.
    early: Vec<Value>,
    last_error: Option<String>,
    /// The engine's latest stderr lines. Stderr is diagnostics (codex echoes
    /// code it is working on there), so it stays out of the conversation and
    /// only explains an exit (`stopped_message`).
    stderr: std::collections::VecDeque<String>,
    kind: Kind,
    cwd: PathBuf,
    snapshot: Snapshot,
    /// Options to restart with once the running turn ends.
    restart: Option<Options>,
    /// A mode the user picked that waits for the running turn to end (the
    /// engine cannot switch it mid-turn); clients say so, and can interrupt.
    pending_mode: Option<String>,
    /// The engine's local gateway key (gateway sessions).
    gateway_key: Option<String>,
    /// A client asked for the conversation in the engine's own TUI.
    tui_request: Option<Value>,
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.gateway_key.take() {
            release_key(self.kind, &key);
        }
    }
}

impl Session<'_> {
    const STDERR_KEPT: usize = 8;

    fn note_stderr(&mut self, line: &str) {
        let line = strip_ansi(line);
        let line = line.trim();
        if line.is_empty() || matches!(log_level(line), Some("INFO" | "DEBUG" | "TRACE")) {
            return;
        }
        if self.stderr.len() == Self::STDERR_KEPT {
            self.stderr.pop_front();
        }
        self.stderr.push_back(line.to_string());
    }

    /// `head`, then the engine's last stderr lines, which usually say why it
    /// stopped.
    fn stopped_message(&self, head: &str) -> String {
        if self.stderr.is_empty() {
            return head.to_string();
        }
        let tail: Vec<&str> = self.stderr.iter().map(String::as_str).collect();
        format!("{head}\n{}", tail.join("\n"))
    }

    /// Runs the daemon tools the engine called: the session tools on their
    /// own threads (answered through `answers`), the others here.
    fn host_calls(
        &self,
        process: &Process,
        adapter: &mut dyn Adapter,
        current: &Options,
        answers: &Sender<Answer>,
    ) {
        for call in adapter.take_calls() {
            // A resumed Codex thread keeps the tools it started with.
            if current.app_tools_off.contains(&call.name.as_str()) {
                let off = jucode_agent_core::user_tools::turned_off(&call.name);
                process.write(adapter.answer(&call.token, Err(off)));
                continue;
            }
            if !jucode_agent_core::user_tools::SESSION_TOOLS.contains(&call.name.as_str()) {
                let result = host_call(&self.cwd, &call);
                process.write(adapter.answer(&call.token, result));
                continue;
            }
            let (Some(hub), Some(session)) = (self.hub.handle(), self.id.clone()) else {
                process.write(
                    adapter.answer(&call.token, Err("the session is not open yet".to_string())),
                );
                continue;
            };
            let parent = crate::subsessions::Parent {
                session,
                cwd: self.cwd.clone(),
                mode: adapter
                    .approval_mode()
                    .or_else(|| current.approval_mode.clone())
                    .unwrap_or_default(),
                gateway: current.gateway == Some(true),
            };
            let answers = answers.clone();
            thread::spawn(move || {
                let result = crate::subsessions::call(&hub, &parent, &call.name, &call.args);
                let _ = answers.send((call.token, result));
            });
        }
    }

    fn run(&mut self, mut process: Process, options: Options, ops: Receiver<Value>) {
        let mut adapter = adapter(self.kind, &self.cwd, &options);
        process.write(adapter.start());
        // A new process runs no background task yet (the last one's went
        // with it); Claude Code reports none at startup.
        self.publish(vec![
            tool_provider_event(&self.hub.store, self.kind, &options),
            json!({ "type": "background_tasks", "tasks": [] }),
        ]);
        // What the process runs as now (a restart changes it). A fork is made
        // once: a restart goes on with the forked conversation.
        let mut current = Options {
            fork_from: None,
            fork_turns: None,
            fork_cwd: None,
            ..options.clone()
        };
        // Session tools answer from their own threads (`wait_session` blocks).
        let (answers_tx, answers) = mpsc::channel::<Answer>();
        // ACP: the agent's conversation id as last recorded.
        let mut agent_session: Option<String> = None;
        loop {
            while let Ok((token, result)) = answers.try_recv() {
                process.write(adapter.answer(&token, result));
            }
            loop {
                match ops.try_recv() {
                    Ok(op) => {
                        if self.apply(&mut process, adapter.as_mut(), &op) {
                            // Closed for idleness: the next open starts it
                            // as it runs now (model, effort, mode).
                            if op["keep"] == true {
                                if let Some(id) = &self.id {
                                    self.hub.keep_options(
                                        id,
                                        asleep_options(adapter.as_ref(), &current),
                                    );
                                }
                            }
                            process.stop();
                            return;
                        }
                        // The ops after a mode or gateway switch go to the
                        // new process (a message right after "full access"
                        // must not run in the old mode).
                        if self.restart.is_some() && !adapter.busy() {
                            break;
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
                        if let Line::Stderr(text) = &line {
                            self.note_stderr(text);
                        }
                        let output = adapter.translate(line);
                        process.write(output.frames);
                        self.host_calls(&process, adapter.as_mut(), &current, &answers_tx);
                        if self.id.is_none() {
                            if let Some(id) = adapter.conversation() {
                                self.named(id);
                            }
                        }
                        // An ACP session reopens the agent's conversation,
                        // which the agent names.
                        if self.kind == Kind::Acp && adapter.conversation() != agent_session {
                            agent_session = adapter.conversation();
                            if let (Some(id), Some(conversation)) = (&self.id, &agent_session) {
                                let _ = self.hub.store.record_session_meta(
                                    id,
                                    &json!({ "agent_session": conversation }),
                                );
                            }
                        }
                        let events =
                            self.allow_while_opening(&mut process, adapter.as_mut(), output.events);
                        self.publish(events);
                    }
                    Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                        let reason = process.exit_reason();
                        // What it started does not outlive it.
                        process.end_group(false);
                        let message = self
                            .stopped_message(&format!("{} stopped ({reason})", self.kind.name()));
                        self.publish(vec![json!({ "type": "error", "message": message })]);
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                }
            }
            let busy = adapter.busy();
            if let Some(id) = &self.id {
                self.hub.set_busy(id, busy);
                // A switch or a TUI waiting for the turn to end is work too.
                let held = self.snapshot.holds(self.kind)
                    || self.restart.is_some()
                    || self.tui_request.is_some();
                self.hub.set_held(id, held);
                let (any, awaited) = self.snapshot.background(self.kind);
                self.hub.set_background(id, any, awaited);
            }
            if !busy && self.pending_mode.take().is_some() {
                self.publish(vec![
                    json!({ "type": "approval_mode_pending", "mode": null }),
                ]);
            }
            if !busy {
                if let Some(mut next) = self.restart.take() {
                    next.resume = adapter.conversation().or(next.resume);
                    // What the process runs as stays: its binary, environment
                    // and gateway.
                    next.bin = options.bin.clone();
                    next.env = options.env.clone();
                    next.claude_config = options.claude_config.clone();
                    next.agent = options.agent.clone();
                    next.command = options.command.clone();
                    next.args = options.args.clone();
                    next.gateway = next.gateway.or(options.gateway);
                    next.tool_provider = next.tool_provider.or(options.tool_provider.clone());
                    // Off the gateway for the first time: the tool's default.
                    pin_tool_provider(&self.hub.store, self.kind, &mut next);
                    if let (Some(id), Some(provider)) = (&self.id, &next.tool_provider) {
                        let _ = self
                            .hub
                            .store
                            .record_session_meta(id, &json!({ "tool_provider": provider }));
                    }
                    next.approval_mode = next.approval_mode.or(options.approval_mode.clone());
                    next.dirs = crate::projects::extra_dirs(
                        self.hub,
                        &self.cwd,
                        &crate::projects::of_session(
                            self.hub,
                            self.id.as_deref().unwrap_or_default(),
                        ),
                    );
                    next.app_tools_off = app_tools_off(&self.hub.store);
                    current = next.clone();
                    process.stop();
                    let id = self.id.clone().unwrap_or_default();
                    if let Some(key) = self.gateway_key.take() {
                        release_key(self.kind, &key);
                    }
                    match command(self.hub, self.kind, &id, &self.cwd, &next).and_then(
                        |(command, key)| {
                            self.gateway_key = key;
                            Process::spawn(command, &self.cwd)
                        },
                    ) {
                        Ok(started) => {
                            process = started;
                            adapter = self::adapter(self.kind, &self.cwd, &next);
                            process.write(adapter.start());
                            // The new engine waits on nothing and runs nothing.
                            self.publish(vec![
                                tool_provider_event(&self.hub.store, self.kind, &next),
                                json!({ "type": "pending_messages", "messages": [] }),
                                json!({ "type": "background_tasks", "tasks": [] }),
                            ]);
                        }
                        Err(error) => {
                            self.publish(vec![json!({ "type": "error", "message": error })]);
                            return;
                        }
                    }
                }
            }
            if let Some(request) = self.tui_request.take() {
                // `force`: the user agreed to cut the running turn short.
                if busy && request["force"] != true {
                    self.refuse(&request, "the running turn must end first".to_string());
                } else {
                    let keep = adapter.keep(current.clone());
                    if let Some(tui) = self.tui_for(adapter.as_ref(), &keep, &request) {
                        // The TUI resumes the conversation file at once: no
                        // grace period in which a cut-off turn still writes it.
                        process.kill();
                        // Its background tasks went with the process.
                        let mut ended = vec![
                            json!({ "type": "background_tasks", "tasks": [] }),
                            json!({ "type": "pending_messages", "messages": [] }),
                        ];
                        if busy {
                            ended.push(json!({ "type": "status", "message": "interrupted" }));
                        }
                        self.publish(ended);
                        match self.terminal(tui, keep, &request, &ops) {
                            Some((started, next, options)) => {
                                process = started;
                                adapter = next;
                                current = options;
                            }
                            None => return,
                        }
                    }
                }
            }
        }
    }

    /// Whether the engine run with `options` has saved conversation `id`
    /// (only then can its TUI resume it).
    fn saved(&self, id: &str, options: &Options) -> bool {
        match self.kind {
            Kind::Claude => !claude::transcript(
                &claude::config_with(options, &self.cwd, id),
                &self.cwd,
                id,
                self.hub.uploads.dir(),
            )
            .is_empty(),
            Kind::Codex => codex::saved(&self.cwd)
                .iter()
                .any(|(saved, ..)| saved == id),
            Kind::Acp => true,
        }
    }

    /// The id a gateway key is issued for: the conversation's, except that
    /// an ACP agent's is not the session's.
    fn key_owner(&self, conversation: &str) -> String {
        match (self.kind, &self.id) {
            (Kind::Acp, Some(session)) => session.clone(),
            _ => conversation.to_string(),
        }
    }

    /// Answers a TUI request with an error, to the client that made it.
    fn refuse(&self, request: &Value, message: String) {
        let mut error = json!({ "type": "error", "message": message });
        if !request["id"].is_null() {
            error["id"] = request["id"].clone();
        }
        self.hub
            .send_to(request["client"].as_u64().unwrap_or(0), &error);
    }

    /// The TUI command for a request: the conversation and the engine's own
    /// TUI resuming it, with the local gateway key it holds. None (the
    /// client told why) leaves the engine running.
    fn tui_for(
        &self,
        adapter: &dyn Adapter,
        options: &Options,
        request: &Value,
    ) -> Option<(String, Command, Option<String>)> {
        // An ACP agent's conversation is its own, never the session id.
        let conversation = match self.kind {
            Kind::Acp => adapter.conversation(),
            _ => adapter.conversation().or(self.id.clone()),
        };
        let Some(id) = conversation else {
            self.refuse(request, "the conversation has not started yet".to_string());
            return None;
        };
        let session = self.key_owner(&id);
        match tui_command(
            self.hub,
            self.kind,
            &id,
            &session,
            &self.cwd,
            options,
            self.saved(&id, options),
        ) {
            Ok((tui, key)) => Some((id, tui, key)),
            Err(error) => {
                self.refuse(request, error);
                None
            }
        }
    }

    /// The conversation in its engine's own TUI, on a session terminal the
    /// client that asked views first, its engine stopped (one process per
    /// conversation); the engine starts again, resuming, once the TUI exits,
    /// and clients see `surface` change and the conversation as the TUI left
    /// it. None when the session should stop.
    fn terminal(
        &mut self,
        (id, tui, tui_key): (String, Command, Option<String>),
        options: Options,
        request: &Value,
        ops: &Receiver<Value>,
    ) -> Option<(Process, Box<dyn Adapter>, Options)> {
        let client = request["client"].as_u64().unwrap_or(0);
        let hub = self.hub.handle()?;
        if let Some(key) = self.gateway_key.take() {
            release_key(self.kind, &key);
        }
        let session = self.id.clone().unwrap_or_default();
        // A new Codex conversation gets its id from the TUI: the one saved
        // for this directory while it ran.
        let earlier: Vec<String> = match self.kind {
            Kind::Codex => codex::saved(&self.cwd)
                .into_iter()
                .map(|(id, ..)| id)
                .collect(),
            _ => Vec::new(),
        };
        let (exit_hub, exit_session) = (Arc::clone(&hub), session.clone());
        let on_exit: Box<dyn FnOnce() + Send> = Box::new(move || {
            let _ = exit_hub.forward(&exit_session, json!({ "op": "tui_exit" }));
        });
        let opened = crate::terminal::open_command(
            &hub,
            client,
            request,
            crate::terminal::tui(&tui, &self.cwd),
            Some(on_exit),
        );
        match opened {
            Ok(term) => {
                self.hub.set_busy(&session, false);
                // The conversation is in the TUI until it exits: never idle.
                self.hub.set_held(&session, true);
                self.publish(vec![
                    json!({ "type": "surface", "surface": "tui", "term": term, "client": client }),
                ]);
                loop {
                    let Ok(op) = ops.recv() else {
                        crate::terminal::kill(self.hub, &term);
                        return None;
                    };
                    match op["op"].as_str().unwrap_or_default() {
                        "tui_exit" => break,
                        "shutdown" => {
                            crate::terminal::kill(self.hub, &term);
                            if let Some(key) = &tui_key {
                                release_key(self.kind, key);
                            }
                            return None;
                        }
                        "snapshot" => {
                            if let Some(watcher) = op["client"].as_u64() {
                                let mut events = self.snapshot.events(false);
                                events.push(json!({ "type": "surface", "surface": "tui", "term": term, "client": client }));
                                for event in events {
                                    self.hub.send_to(watcher, &self.tagged(event));
                                }
                            }
                        }
                        "set_attended" => {}
                        _ => self.publish(vec![json!({ "type": "error", "message": "the conversation is open in its terminal: exit the TUI to continue here" })]),
                    }
                }
            }
            Err(error) => self.refuse(request, error),
        }
        if let Some(key) = tui_key {
            release_key(self.kind, &key);
        }
        // Back to the engine, on the conversation as the TUI left it (none
        // when the TUI saved nothing: the engine starts it afresh).
        let id = match self.kind {
            Kind::Codex if !self.saved(&id, &options) => codex::saved(&self.cwd)
                .into_iter()
                .map(|(id, ..)| id)
                .find(|saved| !earlier.contains(saved))
                .unwrap_or(id),
            _ => id,
        };
        let next = Options {
            resume: self.saved(&id, &options).then(|| id.clone()),
            resume_at: None,
            ..options
        };
        let owner = self.key_owner(&id);
        let started = self::command(self.hub, self.kind, &owner, &self.cwd, &next).and_then(
            |(command, key)| {
                self.gateway_key = key;
                Process::spawn(command, &self.cwd)
            },
        );
        match started {
            Ok(started) => {
                let mut engine = self::adapter(self.kind, &self.cwd, &next);
                started.write(engine.start());
                let mut events = vec![json!({ "type": "surface", "surface": "gui" })];
                // Codex replays its thread when it resumes; Claude Code's
                // conversation is read from its file.
                if self.kind == Kind::Claude {
                    events.push(json!({ "type": "transcript", "items": claude::transcript(&claude::config_with(&next, &self.cwd, &id), &self.cwd, &id, self.hub.uploads.dir()) }));
                }
                self.publish(events);
                Some((started, engine, next))
            }
            Err(error) => {
                self.publish(vec![json!({ "type": "error", "message": error })]);
                None
            }
        }
    }

    /// Applies one op; returns true when the session should stop.
    fn apply(&mut self, process: &mut Process, adapter: &mut dyn Adapter, op: &Value) -> bool {
        if op["claimed"] == true {
            if let Some(id) = &self.id {
                self.hub.release_claim(id);
            }
        }
        match op["op"].as_str().unwrap_or_default() {
            "tui" => {
                self.tui_request = Some(op.clone());
                false
            }
            "snapshot" => {
                if let Some(client) = op["client"].as_u64() {
                    for event in self.snapshot.events(adapter.busy()) {
                        let event = self.tagged(event);
                        self.hub.send_to(client, &event);
                    }
                }
                false
            }
            "shutdown" => true,
            // A client watching or not changes nothing: approvals wait for
            // whoever answers them next.
            "set_attended" => false,
            // Claude Code, Codex and the gateway ACP presets move between
            // this machine's own login and the JuCode gateway (a new
            // process; the conversation resumes), once the running turn ends.
            "set_gateway" => {
                let Some(gateway) = op["gateway"].as_bool() else {
                    self.publish(vec![
                        json!({ "type": "error", "message": "set_gateway requires gateway" }),
                    ]);
                    return false;
                };
                if !adapter.has_gateway() {
                    self.publish(vec![json!({ "type": "error", "message": "this agent has no JuCode gateway mode" })]);
                    return false;
                }
                self.restart = Some(
                    adapter.keep(Options {
                        gateway: Some(gateway),
                        model: op["model"]
                            .as_str()
                            .filter(|m| !m.is_empty())
                            .map(str::to_string),
                        approval_mode: adapter.approval_mode(),
                        ..Options::default()
                    }),
                );
                if let Some(id) = &self.id {
                    let _ = self
                        .hub
                        .store
                        .record_session_meta(id, &json!({ "gateway": gateway }));
                    self.hub.broadcast(&self.hub.sessions_json());
                }
                if adapter.busy() {
                    self.publish(vec![json!({
                        "type": "info",
                        "message": "the switch applies once the running turn ends",
                    })]);
                }
                false
            }
            name => {
                if let Some(options) = adapter.restart_for(op) {
                    self.restart = Some(options);
                    if adapter.busy() {
                        self.wait_for_turn(process, adapter, op);
                    }
                    return false;
                }
                if name == "set_approval_mode" && adapter.busy() && !adapter.mode_applies_live() {
                    self.wait_for_turn(process, adapter, op);
                }
                if name == "approve" {
                    if let Some(call) = op["call_id"].as_str() {
                        self.snapshot.answered(call);
                    }
                }
                if let Some(id) = &self.id {
                    self.hub.note_sent_images(id, op);
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

    /// The conversation is open under `id`: the session starts answering to
    /// it, and whatever it said before goes out.
    fn named(&mut self, id: String) {
        self.id = Some(id.clone());
        if let Some(key) = &self.gateway_key {
            crate::gateway::bind(key, &id);
        }
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(Ok(id));
        }
        let early = std::mem::take(&mut self.early);
        for event in early {
            self.hub.broadcast(&self.tagged(event));
        }
    }

    /// `op` (a mode switch) cannot reach the running turn: clients are told
    /// it waits for the turn to end. A switch to full access answers the
    /// turn's open approvals at once (and later ones, see
    /// `allow_while_opening`), since nothing would ask under it.
    fn wait_for_turn(&mut self, process: &mut Process, adapter: &mut dyn Adapter, op: &Value) {
        let mode = op["mode"].as_str().unwrap_or_default().to_string();
        self.pending_mode = Some(mode.clone());
        self.publish(vec![
            json!({ "type": "approval_mode_pending", "mode": mode }),
        ]);
        if full_access(&mode) {
            let open: Vec<String> = self
                .snapshot
                .approvals
                .iter()
                .map(|(call, _)| call.clone())
                .collect();
            for call in open {
                self.allow(process, adapter, &call);
            }
        }
    }

    /// While a switch to full access waits for the turn, the turn's approval
    /// requests are allowed rather than shown.
    fn allow_while_opening(
        &mut self,
        process: &mut Process,
        adapter: &mut dyn Adapter,
        events: Vec<Value>,
    ) -> Vec<Value> {
        if !self.pending_mode.as_deref().is_some_and(full_access) {
            return events;
        }
        let mut kept = Vec::with_capacity(events.len());
        for event in events {
            match event["call_id"].as_str() {
                Some(call) if event["type"] == "approval_request" => {
                    let call = call.to_string();
                    self.allow(process, adapter, &call);
                }
                _ => kept.push(event),
            }
        }
        kept
    }

    fn allow(&mut self, process: &mut Process, adapter: &mut dyn Adapter, call: &str) {
        let op = json!({ "op": "approve", "call_id": call, "decision": "allow" });
        match adapter.encode(&op) {
            Ok(output) => {
                self.snapshot.answered(call);
                process.write(output.frames);
                self.publish(output.events);
            }
            Err(message) => self.publish(vec![json!({ "type": "error", "message": message })]),
        }
    }

    fn publish(&mut self, events: Vec<Value>) {
        for mut event in events {
            if let Some(id) = &self.id {
                self.hub.attach_sent_images(id, &mut event);
            }
            if event["type"] == "error" {
                self.last_error = event["message"].as_str().map(str::to_string);
            }
            self.snapshot.apply(&event);
            if let Some(id) = &self.id {
                self.hub.observe(id, &event);
                self.hub.usage.tag_event(id, &mut event);
            }
            if self.id.is_some() {
                self.hub.broadcast(&self.tagged(event));
            } else {
                self.early.push(event);
            }
        }
    }
}

pub fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The level of a `2026-…T…Z LEVEL …` tracing line an engine logs.
pub fn log_level(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    let (stamp, level) = (words.next()?, words.next()?);
    let stamped = stamp.len() > 10 && stamp.as_bytes()[4] == b'-' && stamp.contains('T');
    (stamped && matches!(level, "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE")).then_some(level)
}

/// The engine binary: `env_override`, else `find_program`.
pub fn resolve(name: &str, env_override: &str, extra: &[PathBuf]) -> PathBuf {
    match std::env::var_os(env_override).filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => find_program(name, extra),
    }
}

/// `name` on PATH, then in the usual install directories and `extra`, else
/// the bare name.
pub fn find_program(name: &str, extra: &[PathBuf]) -> PathBuf {
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

/// The user's home where Claude Code and Codex keep their sessions. On
/// Windows that is USERPROFILE: a `HOME` from Git Bash or MSYS would point
/// the lookups at another directory.
pub fn home() -> PathBuf {
    let (first, second) = if cfg!(windows) {
        ("USERPROFILE", "HOME")
    } else {
        ("HOME", "USERPROFILE")
    };
    std::env::var_os(first)
        .or_else(|| std::env::var_os(second))
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

/// Full access, in any of the names clients and engines use.
fn full_access(mode: &str) -> bool {
    matches!(mode, "full-access" | "full-auto" | "bypassPermissions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_tools_are_switched_in_groups() {
        let dir = std::env::temp_dir().join(format!(
            "jucode-app-tools-{}-{}",
            std::process::id(),
            crate::store::now()
        ));
        let store = crate::store::Store::open(dir.clone()).unwrap();
        assert!(app_tools_off(&store).is_empty());
        assert_eq!(app_tools_json(&store)["sessions"], true);
        let set = set_app_tools(&store, &json!({ "sessions": false })).unwrap();
        assert_eq!(
            (set["sessions"].as_bool(), set["present_files"].as_bool()),
            (Some(false), Some(true))
        );
        set_app_tools(&store, &json!({ "present_files": false })).unwrap();
        assert_eq!(
            app_tools_off(&store),
            [
                "present_files",
                "start_session",
                "wait_session",
                "send_session"
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_idle_closed_engine_reopens_on_its_whole_conversation() {
        let current = Options {
            model: Some("started-with".into()),
            resume: Some("conv".into()),
            resume_at: Some("rewind-point".into()),
            bin: Some("/opt/claude".into()),
            ..Options::default()
        };
        let engine = adapter(Kind::Claude, Path::new("/tmp"), &current);
        let kept = asleep_options(engine.as_ref(), &current);
        assert_eq!(
            kept.resume_at, None,
            "a rewind applies to the open that asked"
        );
        assert_eq!(kept.resume, None);
        assert_eq!(kept.bin.as_deref(), Some("/opt/claude"));
        assert_eq!(kept.model.as_deref(), Some("started-with"));
    }

    #[test]
    fn background_work_and_waiting_approvals_hold_the_engine() {
        let mut snapshot = Snapshot::default();
        assert!(!snapshot.holds(Kind::Codex));
        snapshot.apply(&json!({ "type": "background_tasks", "tasks": [{ "id": "t1" }] }));
        assert!(snapshot.holds(Kind::Claude));
        snapshot.apply(&json!({ "type": "background_tasks", "tasks": [] }));
        assert!(!snapshot.holds(Kind::Claude));
        // As Codex reports a subagent still working after its turn.
        snapshot.apply(&json!({ "type": "agent_runs", "workflows": [], "agents": [{ "id": "a", "status": "running" }, { "id": "b", "status": "completed" }] }));
        assert!(snapshot.holds(Kind::Codex));
        // Claude's run list comes from its transcript: not trusted to hold.
        assert!(!snapshot.holds(Kind::Claude));
        snapshot.apply(&json!({ "type": "agent_runs", "workflows": [{ "status": "completed", "agents": [{ "state": "async_launched" }] }], "agents": [] }));
        assert!(
            snapshot.holds(Kind::Codex),
            "a workflow's agent still running"
        );
        snapshot.apply(&json!({ "type": "agent_runs", "workflows": [{ "status": "completed", "agents": [{ "state": "done" }] }], "agents": [{ "status": "failed" }, { "status": "" }] }));
        assert!(!snapshot.holds(Kind::Codex));
        snapshot.apply(&json!({ "type": "approval_request", "call_id": "c1" }));
        assert!(snapshot.holds(Kind::Codex));
        snapshot.apply(&json!({ "type": "status", "message": "ready" }));
        assert!(!snapshot.holds(Kind::Codex));
    }

    #[test]
    fn a_client_that_attaches_sees_the_messages_waiting_for_the_turn() {
        let mut snapshot = Snapshot::default();
        snapshot.apply(&json!({ "type": "pending_messages", "messages": ["one"], "fixed": 0 }));
        snapshot
            .apply(&json!({ "type": "pending_messages", "messages": ["one", "two"], "fixed": 1 }));
        let events = snapshot.events(true);
        let queued: Vec<&Value> = events
            .iter()
            .filter(|e| e["type"] == "pending_messages")
            .collect();
        assert_eq!(
            queued,
            [&json!({ "type": "pending_messages", "messages": ["one", "two"], "fixed": 1 })]
        );
    }

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
            json!({ "type": "tool_start", "call_id": "t2", "name": "read", "subagent": "Scan" }),
            json!({ "type": "tool_output", "call_id": "t2", "name": "read", "output": "no", "is_error": true }),
            json!({ "type": "approval_request", "call_id": "a1", "name": "bash" }),
            json!({ "type": "approval_request", "call_id": "a2", "name": "write" }),
        ] {
            snapshot.apply(&event);
        }
        snapshot.answered("a1");
        let events = snapshot.events(true);
        assert_eq!(events[0], json!({ "type": "startup", "model": "b" }));
        let transcript = events.iter().find(|e| e["type"] == "transcript").unwrap();
        // Each item says when it happened.
        let items: Vec<Value> = transcript["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                assert!(item["at"].as_u64().is_some_and(|at| at > 0), "{item}");
                let mut item = item.clone();
                item.as_object_mut().unwrap().remove("at");
                item
            })
            .collect();
        assert_eq!(
            json!(items),
            json!([
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "hello" },
                { "role": "tool", "name": "bash", "output": "ok" },
                { "role": "tool", "name": "read", "output": "no", "subagent": "Scan", "is_error": true },
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
