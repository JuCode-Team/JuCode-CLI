mod acp;
mod daemon_admin;

use jucode_agent_core::{
    protocol::{self, event_json},
    AgentCore, AgentEvent, ApprovalMode,
};
use jucode_tui::{TuiApp, TuiRuntime};
use serde_json::{json, Value};
use std::{
    env, io,
    io::{BufRead, Read, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Runtime(AgentCore);

#[derive(Default)]
struct HeadlessStats {
    status: String,
    approval_mode: String,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    context_tokens: u64,
    context_tokenizer: Option<String>,
    cost: f64,
    tool_calls: u64,
    denied_approvals: u64,
    subagent_events: u64,
    assistant_chars: usize,
    last_error: Option<String>,
    last_context_state: Option<String>,
    event_counts: std::collections::BTreeMap<String, u64>,
}

impl TuiRuntime for Runtime {
    fn startup_events(&self) -> Vec<AgentEvent> {
        self.0.startup_events()
    }

    fn model_status_event(&self) -> AgentEvent {
        self.0.model_status_event()
    }

    fn submit_user_message(&mut self, message: String) -> Vec<AgentEvent> {
        self.0.submit_user_message(message)
    }

    fn interrupt(&mut self) -> Vec<AgentEvent> {
        self.0.interrupt()
    }

    fn handle_command(&mut self, input: &str) -> (bool, Vec<AgentEvent>) {
        self.0.handle_command(input)
    }

    fn poll_events(&mut self) -> Vec<AgentEvent> {
        self.0.poll_events()
    }
}

/// SIGTERM, SIGHUP or SIGINT ends the tool commands before exiting. The
/// signals are blocked in every thread (call this before any is spawned) and
/// taken by one waiting thread; spawned commands start with a clean mask.
#[cfg(unix)]
fn end_tool_processes_on_signal() {
    // SAFETY: plain sigset manipulation; the set outlives the waiting thread
    // (moved into it).
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            libc::sigaddset(&mut set, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        std::thread::spawn(move || {
            let mut signal = 0;
            if libc::sigwait(&set, &mut signal) == 0 {
                jucode_agent_core::terminate_tool_processes();
                std::process::exit(128 + signal);
            }
        });
    }
}

fn main() -> io::Result<()> {
    jucode_agent_core::logging::init_global();
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    // Before anything that may touch the terminal: `--version` must work in
    // TTY-less contexts (CI release guard, the desktop's check_backend probe).
    if args.iter().any(|a| a == "--version" || a == "-V")
        || args.first().map(String::as_str) == Some("version")
    {
        println!("jucode {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h")
        || args.first().map(String::as_str) == Some("help")
    {
        print!("{}", help_text());
        return Ok(());
    }
    let approval_mode = match take_approval_mode_flag(&mut args) {
        Ok(mode) => mode,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    // The engine modes run tool commands in their own process groups; none of
    // them may outlive the engine, however it ends (input closed, a signal,
    // an error).
    let engine_mode = matches!(
        args.first().map(String::as_str),
        Some("--headless" | "serve" | "daemon" | "acp")
    );
    if engine_mode {
        #[cfg(unix)]
        end_tool_processes_on_signal();
    }
    let exit = |code: io::Result<i32>| -> io::Result<()> {
        jucode_agent_core::terminate_tool_processes();
        std::process::exit(code?);
    };
    if args.first().map(String::as_str) == Some("--headless") {
        args.remove(0);
        return exit(run_headless(args, approval_mode));
    }
    if args.first().map(String::as_str) == Some("serve") {
        let chat = args.iter().skip(1).any(|arg| arg == "--chat");
        return exit(run_serve(approval_mode, chat));
    }
    if args.first().map(String::as_str) == Some("daemon") {
        return exit(run_daemon(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("acp") {
        return exit(acp::run_acp(approval_mode));
    }
    if args.first().map(String::as_str) == Some("update") {
        std::process::exit(run_update());
    }
    if args.first().map(String::as_str) == Some("token") {
        // The desktop's own gateway calls take the token from here, so one
        // implementation refreshes it.
        match jucode_agent_core::jucode_session() {
            Ok((api_url, access_token, expires_at)) => {
                println!(
                    "{}",
                    json!({ "api_url": api_url, "access_token": access_token, "expires_at": expires_at })
                );
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
    if args.first().map(String::as_str) == Some("logout") {
        match jucode_agent_core::jucode_logout() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
    if args.first().map(String::as_str) == Some("providers") {
        // JuCode lists the models the user chose to show, once they have.
        let jucode_models = jucode_agent_core::jucode_visible_models();
        let list = jucode_agent_core::builtin_providers()
            .into_iter()
            .map(|(id, base_url, protocol)| {
                let models = if id == "jucode" && !jucode_models.is_empty() {
                    jucode_models.clone()
                } else {
                    jucode_agent_core::models_for_provider(&id)
                };
                let models = models
                    .into_iter()
                    .map(|m| {
                        json!({
                            "name": m.name,
                            "display_name": m.display_name,
                            "context_window": m.context_window,
                            "max_context_window": m.max_context_window,
                            "max_output_tokens": m.max_output_tokens,
                            "reasoning_efforts": m.reasoning_efforts,
                        })
                    })
                    .collect::<Vec<_>>();
                json!({ "id": id, "base_url": base_url, "protocol": protocol, "models": models })
            })
            .collect::<Vec<_>>();
        println!("{}", json!(list));
        std::process::exit(0);
    }
    let mut core = AgentCore::new()?.with_version(env!("CARGO_PKG_VERSION"));
    if let Some(mode) = approval_mode {
        // Startup events (emitted by the TUI) will reflect the switched mode.
        let _ = core.set_approval_mode(mode);
    }
    core.start_update_check();
    let app = TuiApp::new(Runtime(core));
    match args.iter().position(|arg| arg == "--resume") {
        Some(index) => match args.get(index + 1) {
            Some(id) => app.with_command(&format!("/resume {id}")).run(),
            None => {
                eprintln!("jucode: --resume requires a session id");
                std::process::exit(2);
            }
        },
        None => app.run(),
    }
}

fn help_text() -> String {
    format!(
        "jucode {} — a lightweight terminal coding agent

USAGE:
    jucode [OPTIONS]                     start the interactive TUI
    jucode --headless [PROMPT]           run one prompt non-interactively
                                         (reads stdin when PROMPT is omitted;
                                         emits JSONL events + final_result;
                                         defaults to manual, approvals are
                                         auto-denied — pass --approval-mode
                                         full-access for unattended writes)
    jucode serve                         newline-JSON protocol for GUI/IDE
                                         front-ends (jucode's native schema)
    jucode daemon [--listen <addr>]      host many sessions for Desktop and
                                         remote clients over a WebSocket
                                         (--relay <url> | --no-relay: reach
                                         it through the JuCode relay once
                                         Desktop turns that on)
    jucode daemon install|uninstall      run the daemon at login (launchd /
                                         systemd user service)
    jucode acp                           Agent Client Protocol (ACP v1)
                                         JSON-RPC adapter over stdio, for
                                         ACP-capable editors like Zed
    jucode providers                     print built-in providers as JSON
    jucode token                         print a JuCode access token as JSON (refreshed when needed)
    jucode update                        update jucode to the latest release
    jucode version                       print the version

OPTIONS:
    --resume <id>                        open the TUI on a saved conversation
    --approval-mode <manual|auto-edit|auto|full-access>
                                         tool approval mode for this run
                                         (auto runs a safety classifier on
                                         shell commands; full-access runs
                                         everything without prompts and is
                                         not confined to the workspace)
    -h, --help                           show this help
    -V, --version                        print the version

The TUI also supports: ! <cmd> (run a local shell command), @file mentions,
/image <path> (attach an image), and custom commands from ~/.jucode/commands.
",
        env!("CARGO_PKG_VERSION")
    )
}

/// Extracts `--approval-mode <mode>` (or `--approval-mode=<mode>`) from `args`.
fn take_approval_mode_flag(args: &mut Vec<String>) -> Result<Option<ApprovalMode>, String> {
    let Some(index) = args
        .iter()
        .position(|arg| arg == "--approval-mode" || arg.starts_with("--approval-mode="))
    else {
        return Ok(None);
    };
    let arg = args.remove(index);
    let value = match arg.strip_prefix("--approval-mode=") {
        Some(value) => value.to_string(),
        None => {
            if index >= args.len() {
                return Err(
                    "--approval-mode requires a value: manual, auto-edit, auto, or full-access"
                        .to_string(),
                );
            }
            args.remove(index)
        }
    };
    ApprovalMode::parse(&value).map(Some)
}

/// The approval mode a headless run uses. Headless reads no further stdin, so
/// approval prompts can never be answered interactively; instead of silently
/// running everything, headless defaults to the safest mode
/// and auto-denies gated tool calls. Loosen explicitly with
/// `--approval-mode auto-edit`, `auto`, or `full-access`.
fn headless_approval_mode(flag: Option<ApprovalMode>) -> ApprovalMode {
    flag.unwrap_or(ApprovalMode::Manual)
}

fn run_headless(args: Vec<String>, approval_mode: Option<ApprovalMode>) -> io::Result<i32> {
    let mut prompt = args.join(" ");
    if prompt.trim().is_empty() {
        io::stdin().read_to_string(&mut prompt)?;
    }
    let mut core = AgentCore::new()?.with_version(env!("CARGO_PKG_VERSION"));
    let mut stdout = io::stdout();
    let mode = headless_approval_mode(approval_mode);
    for event in core.set_approval_mode(mode) {
        write_event(&mut stdout, event)?;
    }
    let mut done = false;
    let mut stats = HeadlessStats {
        approval_mode: mode.as_str().to_string(),
        ..Default::default()
    };
    let started = Instant::now();
    let mut pending_denials: Vec<(String, String)> = Vec::new();
    for event in core.submit_user_message(prompt) {
        if matches!(event, AgentEvent::Error(_)) {
            done = true;
        }
        record_headless_event(&event, &mut stats);
        queue_headless_denial(&event, &mut pending_denials);
        write_event(&mut stdout, event)?;
    }
    auto_deny_approvals(&mut core, &mut stdout, &mut stats, &mut pending_denials)?;
    while !done {
        let events = core.poll_events();
        for event in events {
            if matches!(event, AgentEvent::Status(ref value) if value == "ready")
                || matches!(event, AgentEvent::Error(_))
            {
                done = true;
            }
            record_headless_event(&event, &mut stats);
            queue_headless_denial(&event, &mut pending_denials);
            write_event(&mut stdout, event)?;
        }
        auto_deny_approvals(&mut core, &mut stdout, &mut stats, &mut pending_denials)?;
        thread::sleep(Duration::from_millis(50));
    }
    stats.status = if stats.last_error.is_some() {
        "error".to_string()
    } else {
        "ready".to_string()
    };
    write_json_value(
        &mut stdout,
        final_result_json(&stats, started.elapsed().as_millis() as u64),
    )?;
    Ok(if stats.last_error.is_some() { 1 } else { 0 })
}

/// Denies every approval request surfaced by a headless run: nobody can
/// answer them, so blocking would hang the turn. The model receives the
/// denial as the tool result and can adapt or finish.
fn auto_deny_approvals(
    core: &mut AgentCore,
    stdout: &mut impl Write,
    stats: &mut HeadlessStats,
    pending: &mut Vec<(String, String)>,
) -> io::Result<()> {
    for (call_id, name) in pending.drain(..) {
        stats.denied_approvals += 1;
        let info = AgentEvent::Info(format!(
            "auto-denying {name} ({call_id}): approvals cannot be answered in --headless mode; rerun with --approval-mode auto, auto-edit, or full-access to allow this class of tools"
        ));
        record_headless_event(&info, stats);
        write_event(stdout, info)?;
        for event in core.approve(&call_id, false, false, None) {
            record_headless_event(&event, stats);
            write_event(stdout, event)?;
        }
    }
    Ok(())
}

/// Queues an approval request for auto-denial so headless can never hang on one.
fn queue_headless_denial(event: &AgentEvent, pending_denials: &mut Vec<(String, String)>) {
    if let AgentEvent::ApprovalRequest { call_id, name, .. } = event {
        pending_denials.push((call_id.clone(), name.clone()));
    }
}

/// `jucode update`: npm installs update through npm, the copy JuCode Desktop
/// manages updates with the app, and a release binary replaces itself.
fn run_update() -> i32 {
    use jucode_agent_core::update;
    let current = env!("CARGO_PKG_VERSION");
    let channel = update::install_channel();
    if channel == update::InstallChannel::Desktop {
        println!("this jucode is managed by JuCode Desktop and updates with the app ({current})");
        return 0;
    }
    let release = match update::latest_release(std::time::Duration::from_secs(10)) {
        Ok(release) => Some(release),
        Err(error) => {
            eprintln!("version check failed ({error})");
            None
        }
    };
    if let Some(release) = &release {
        if !update::is_newer_version(current, &release.version) {
            println!("already up to date ({current}; latest {})", release.version);
            return 0;
        }
        println!("updating to {}...", release.version);
    }
    let result = match (channel, &release) {
        (update::InstallChannel::Npm, _) => update::run_npm_update(),
        (_, Some(release)) => update::self_update(release),
        (_, None) => Err("cannot update without the release information".to_string()),
    };
    match result {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

/// `jucode daemon [--listen <addr>]`: host sessions for Desktop and remote
/// clients until killed. Listens on loopback unless told otherwise. The
/// relay connection (`--relay`, default `wss://app.jucode.net/relay/v1`) is
/// made only once a local client turns it on; `--no-relay` rules it out.
fn run_daemon(args: &[String]) -> io::Result<i32> {
    let (action, args) = match args.first().map(String::as_str) {
        Some(action @ ("install" | "uninstall" | "pair")) => (action, &args[1..]),
        // `relay on|off|status`: the state word comes first.
        Some("relay") => {
            let state = args.get(1).filter(|arg| !arg.starts_with("--"));
            let rest = &args[1 + usize::from(state.is_some())..];
            let listen = match rest {
                [] => jucode_daemon::DEFAULT_LISTEN.to_string(),
                [flag, address] if flag == "--listen" => address.clone(),
                _ => {
                    eprintln!("usage: jucode daemon relay [on|off|status] [--listen <host:port>]");
                    return Ok(2);
                }
            };
            return daemon_admin::relay(&listen, state.map(String::as_str));
        }
        _ => ("run", args),
    };
    let mut listen = jucode_daemon::DEFAULT_LISTEN.to_string();
    let mut web = None;
    let mut relay = Some(jucode_daemon::DEFAULT_RELAY.to_string());
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--no-relay" {
            relay = None;
            continue;
        }
        match (arg.as_str(), rest.next()) {
            ("--listen", Some(address)) => listen = address.clone(),
            ("--web", Some(dir)) => web = Some(std::path::PathBuf::from(dir)),
            ("--relay", Some(url)) => relay = Some(url.clone()),
            _ => {
                eprintln!(
                    "usage: jucode daemon [install|uninstall|pair] [--listen <host:port>] [--web <dir>] [--relay <wss url> | --no-relay]\n       jucode daemon relay [on|off|status] [--listen <host:port>]"
                );
                return Ok(2);
            }
        }
    }
    // A release ships the remote page next to the binary.
    let web = web.or_else(|| {
        env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("web")))
            .filter(|dir| dir.join("index.html").is_file())
    });
    let outcome = match action {
        // The service runs with the same relay choice as this command line.
        "install" => Some(jucode_daemon::install::install(
            &listen,
            match &relay {
                None => vec!["--no-relay".to_string()],
                Some(url) if url != jucode_daemon::DEFAULT_RELAY => {
                    vec!["--relay".to_string(), url.clone()]
                }
                Some(_) => Vec::new(),
            },
        )),
        "uninstall" => Some(jucode_daemon::install::uninstall()),
        "pair" => return daemon_admin::pair(&listen),
        _ => None,
    };
    if let Some(outcome) = outcome {
        return Ok(match outcome {
            Ok(message) => {
                println!("{message}");
                0
            }
            Err(error) => {
                eprintln!("{error}");
                1
            }
        });
    }
    // A login started from a daemon session is the desktop's.
    jucode_agent_core::set_login_client_label("JuCode Desktop");
    let store = match jucode_daemon::Store::open(jucode_daemon::state_dir()?) {
        Ok(store) => store,
        // Another daemon holds the state directory: exit successfully, as for
        // a taken port below.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            eprintln!("jucode daemon: {error}");
            return Ok(0);
        }
        Err(error) => return Err(error),
    };
    // The token exists before the port answers: a client that connects as
    // soon as it can (Desktop starting the daemon) reads it right away.
    store.token()?;
    let listener = match std::net::TcpListener::bind(&listen) {
        Ok(listener) => listener,
        // Another daemon has the port. Exit successfully so a service manager
        // (launchd SuccessfulExit=false, systemd Restart=on-failure) does not
        // retry forever.
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            eprintln!("jucode daemon: {listen} is already in use; another daemon is running");
            return Ok(0);
        }
        Err(error) => return Err(error),
    };
    eprintln!(
        "jucode daemon listening on ws://{} (token in {})",
        listener.local_addr()?,
        jucode_daemon::state_dir()?.join("token").display()
    );
    let agents = jucode_daemon::Agents::open(jucode_daemon::agents_dir()?)?;
    if let Some(dir) = &web {
        eprintln!(
            "remote page: http://{}/remote (from {})",
            listener.local_addr()?,
            dir.display()
        );
    }
    jucode_daemon::serve(
        listener,
        store,
        agents,
        web,
        env!("CARGO_PKG_VERSION"),
        relay,
    )?;
    Ok(0)
}

/// Persistent bidirectional protocol mode for GUI/IDE front-ends.
///
/// Reads newline-delimited JSON commands on stdin and emits the engine's
/// `AgentEvent` stream as newline-delimited JSON on stdout (same schema as
/// `--headless`). Runs until stdin closes or a `shutdown`/`/quit` command.
///
/// `--chat` starts a chat session in `~/.jucode/chats`.
fn run_serve(approval_mode: Option<ApprovalMode>, chat: bool) -> io::Result<i32> {
    let core = if chat {
        AgentCore::open(jucode_agent_core::chat::ensure_chats_dir()?)?
    } else {
        AgentCore::new()?
    };
    let mut core = core.with_version(env!("CARGO_PKG_VERSION"));
    if let Some(mode) = approval_mode {
        // Set before startup_events so the startup approval_mode event reflects it.
        let _ = core.set_approval_mode(mode);
    }
    core.start_update_check();
    let mut stdout = io::stdout();

    write_json_value(&mut stdout, protocol::hello_json(env!("CARGO_PKG_VERSION")))?;
    for event in core.startup_events() {
        write_session_event(&mut stdout, &core, event)?;
    }
    // Seed dedup so the first poll loop doesn't immediately re-emit model_status.
    let mut last_status = Some(protocol::session_event_json(
        core.session_id(),
        core.model_status_event(),
    ));

    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    loop {
        loop {
            match rx.try_recv() {
                Ok(line) => {
                    if handle_serve_line(&mut core, &mut stdout, &line)? {
                        return Ok(0);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(0),
            }
        }

        for event in core.poll_events() {
            write_session_event(&mut stdout, &core, event)?;
        }

        let status = protocol::session_event_json(core.session_id(), core.model_status_event());
        if last_status.as_ref() != Some(&status) {
            write_json_value(&mut stdout, status.clone())?;
            last_status = Some(status);
        }

        thread::sleep(Duration::from_millis(30));
    }
}

/// Dispatch one stdin command line. Returns `Ok(true)` to terminate serve mode.
fn handle_serve_line(
    core: &mut AgentCore,
    stdout: &mut impl Write,
    line: &str,
) -> io::Result<bool> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(false);
    }
    let value = match serde_json::from_str::<Value>(line) {
        Ok(value) => value,
        Err(error) => {
            jucode_agent_core::log_warn!(
                "serve",
                "failed to parse command line",
                error = error.to_string()
            );
            write_session_event(
                stdout,
                core,
                AgentEvent::Error(format!("invalid command: {error}")),
            )?;
            return Ok(false);
        }
    };
    let (quit, events) = protocol::apply_op(core, &value);
    for event in events {
        write_session_event(stdout, core, event)?;
    }
    Ok(quit)
}

fn write_event(stdout: &mut impl Write, event: AgentEvent) -> io::Result<()> {
    write_json_value(stdout, event_json(event))
}

fn write_session_event(
    stdout: &mut impl Write,
    core: &AgentCore,
    event: AgentEvent,
) -> io::Result<()> {
    write_json_value(
        stdout,
        protocol::session_event_json(core.session_id(), event),
    )
}

fn write_json_value(stdout: &mut impl Write, value: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, &value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn record_headless_event(event: &AgentEvent, stats: &mut HeadlessStats) {
    let key = match event {
        AgentEvent::Startup { .. } => "startup",
        AgentEvent::ModelStatus { .. } => "model_status",
        AgentEvent::PendingMessages(_) => "pending_messages",
        AgentEvent::UserMessage(_) => "user_message",
        AgentEvent::FillInput(_) => "fill_input",
        AgentEvent::Connecting => "connecting",
        AgentEvent::CompactionStart => "compaction_start",
        AgentEvent::CompactionProgress { .. } => "compaction_progress",
        AgentEvent::CompactionEnd => "compaction_end",
        AgentEvent::CompactionFailed(_) => "compaction_failed",
        AgentEvent::ContextUsage {
            tokens,
            tokenizer,
            cost,
        } => {
            stats.context_tokens = *tokens;
            stats.context_tokenizer = Some(tokenizer.clone());
            stats.cost = *cost;
            "context_usage"
        }
        AgentEvent::ThinkingStart => "thinking_start",
        AgentEvent::ReasoningDelta(delta) => {
            stats.assistant_chars += delta.len();
            "reasoning_delta"
        }
        AgentEvent::AssistantStart => "assistant_start",
        AgentEvent::AssistantDelta(delta) => {
            stats.assistant_chars += delta.len();
            "assistant_delta"
        }
        AgentEvent::Retrying { .. } => "retrying",
        AgentEvent::ToolStart { .. } => {
            stats.tool_calls += 1;
            "tool_start"
        }
        AgentEvent::ToolUpdate { .. } => "tool_update",
        AgentEvent::ToolOutput { .. } => "tool_output",
        AgentEvent::SubagentLifecycle { .. } => {
            stats.subagent_events += 1;
            "subagent_lifecycle"
        }
        AgentEvent::AgentRuns(_) => "agent_runs",
        AgentEvent::SubagentTranscript { .. } => "subagent_transcript",
        AgentEvent::PlanDraft { .. } => "plan_draft",
        AgentEvent::ProposedPlan { .. } => "proposed_plan",
        AgentEvent::Usage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            reasoning_tokens,
        } => {
            stats.input_tokens += input_tokens;
            stats.cached_input_tokens += cached_input_tokens;
            stats.output_tokens += output_tokens;
            stats.reasoning_tokens += reasoning_tokens;
            "usage"
        }
        AgentEvent::TreeView(_) => "tree_view",
        AgentEvent::ResumeView(_) => "resume_view",
        AgentEvent::CheckpointView(_) => "checkpoint_view",
        AgentEvent::McpServers { .. } => "mcp_servers",
        AgentEvent::ApprovalRequest { .. } => "approval_request",
        AgentEvent::ActionDeferred(_) => "action_deferred",
        AgentEvent::ActionDecided { .. } => "action_decided",
        AgentEvent::Attended(_) => "attended",
        AgentEvent::ApprovalMode { .. } => "approval_mode",
        AgentEvent::TrustPrompt { .. } => "trust_prompt",
        AgentEvent::ModelView { .. } => "model_view",
        AgentEvent::LoginPicker(_) => "login_picker",
        AgentEvent::LoginPastePrompt { .. } => "login_paste_prompt",
        AgentEvent::CommandList(_) => "command_list",
        AgentEvent::Goal(_) => "goal",
        AgentEvent::Plan(_) => "plan",
        AgentEvent::Transcript(_) => "transcript",
        AgentEvent::Info(_) => "info",
        AgentEvent::Error(message) => {
            stats.last_error = Some(message.clone());
            "error"
        }
        AgentEvent::Status(message) => {
            stats.last_context_state = Some(message.clone());
            stats.status = message.clone();
            "status"
        }
    };
    *stats.event_counts.entry(key.to_string()).or_insert(0) += 1;
}

fn final_result_json(stats: &HeadlessStats, elapsed_ms: u64) -> Value {
    json!({
        "type": "final_result",
        "status": stats.status,
        "approval_mode": stats.approval_mode,
        "denied_approvals": stats.denied_approvals,
        "input_tokens": stats.input_tokens,
        "cached_input_tokens": stats.cached_input_tokens,
        "output_tokens": stats.output_tokens,
        "reasoning_tokens": stats.reasoning_tokens,
        "context_tokens": stats.context_tokens,
        "context_tokenizer": stats.context_tokenizer,
        "cost": stats.cost,
        "tool_calls": stats.tool_calls,
        "subagent_events": stats.subagent_events,
        "assistant_chars": stats.assistant_chars,
        "elapsed_ms": elapsed_ms,
        "last_error": stats.last_error,
        "last_context_state": stats.last_context_state,
        "event_counts": stats.event_counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_result_contains_status_and_usage() {
        let mut stats = HeadlessStats {
            status: "ready".to_string(),
            approval_mode: "manual".to_string(),
            ..Default::default()
        };
        stats.input_tokens = 12;
        stats.output_tokens = 8;
        stats.reasoning_tokens = 4;
        stats.context_tokens = 99;
        stats.tool_calls = 3;
        stats.denied_approvals = 1;
        stats.subagent_events = 2;

        let value = final_result_json(&stats, 123);
        assert_eq!(value["type"], "final_result");
        assert_eq!(value["status"], "ready");
        assert_eq!(value["approval_mode"], "manual");
        assert_eq!(value["denied_approvals"], 1);
        assert_eq!(value["input_tokens"], 12);
        assert_eq!(value["cached_input_tokens"], 0);
        assert_eq!(value["context_tokens"], 99);
        assert_eq!(value["tool_calls"], 3);
        assert_eq!(value["elapsed_ms"], 123);
    }

    #[test]
    fn headless_defaults_to_manual_and_honors_explicit_flag() {
        assert_eq!(headless_approval_mode(None), ApprovalMode::Manual);
        assert_eq!(
            headless_approval_mode(Some(ApprovalMode::AutoEdit)),
            ApprovalMode::AutoEdit
        );
        assert_eq!(
            headless_approval_mode(Some(ApprovalMode::FullAccess)),
            ApprovalMode::FullAccess
        );
    }

    #[test]
    fn approval_requests_are_queued_for_headless_denial() {
        let mut pending = Vec::new();
        queue_headless_denial(
            &AgentEvent::ApprovalRequest {
                call_id: "call_1".to_string(),
                name: "bash".to_string(),
                summary: "rm -rf".to_string(),
                subagent_id: None,
                hunks: None,
            },
            &mut pending,
        );
        queue_headless_denial(&AgentEvent::Connecting, &mut pending);
        assert_eq!(pending, vec![("call_1".to_string(), "bash".to_string())]);
    }
}
