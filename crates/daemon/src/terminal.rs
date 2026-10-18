//! Terminals on a pty of this computer. A session's own TUI (claude, codex,
//! jucode, a built-in ACP agent; see `open_command`) belongs to its session;
//! clients are its viewers. Every viewer gets its output and may type into
//! it; a viewer that leaves is only detached, and one that attaches gets the
//! latest output replayed. A shell (`term_open`: the user's login shell in a
//! known directory) belongs to the client that opened it: that client is its
//! only viewer, and it is killed once that client stops viewing it. Paired
//! devices acknowledge what they wrote: while one is a window behind, the pty
//! is not read, and one that stays behind is detached.

use crate::{
    files,
    hub::{lock, Hub},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    io::{ErrorKind, Read, Write},
    path::Path,
    sync::{
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// Largest `term_output` before base64 (about 88 KiB encoded), well below
/// the relay's 1 MiB frame limit.
const MAX_CHUNK: usize = 64 * 1024;
/// Output is sent once the TUI has been quiet this long.
const QUIET: Duration = Duration::from_millis(10);
/// How often an idle terminal checks whether its program has exited while
/// the pty stays open (a background job holds it, or ConPTY on Windows).
const IDLE: Duration = Duration::from_millis(250);
/// Output still awaited after the program exited.
const GRACE: Duration = Duration::from_millis(500);
/// The latest output, replayed to a viewer that attaches.
const SCROLLBACK: usize = 256 * 1024;
/// Output a paired device may have unacknowledged: within the relay's
/// queue for one client (64 messages, 4 MiB).
const WINDOW: u64 = 512 * 1024;
/// A paired device whose window stays full this long is detached.
#[cfg(not(test))]
const SLOW: Duration = Duration::from_secs(15);
#[cfg(test)]
const SLOW: Duration = Duration::from_millis(300);
/// Shells one client may have open.
const MAX_SHELLS: usize = 4;

#[derive(Default)]
pub struct Terminals {
    open: Mutex<HashMap<String, Terminal>>,
}

struct Terminal {
    /// A shell: the client that opened it, its only viewer.
    owner: Option<u64>,
    master: Box<dyn MasterPty + Send>,
    /// Bytes for the TUI, written on their own thread so a program that
    /// does not read never holds up a client's connection.
    input: mpsc::Sender<Vec<u8>>,
    control: mpsc::Sender<Control>,
    viewers: Vec<Viewer>,
    scrollback: VecDeque<u8>,
    size: PtySize,
}

struct Viewer {
    client: u64,
    /// A paired device: its output is flow-controlled.
    device: bool,
    /// Bytes sent and acknowledged since it attached.
    sent: u64,
    acked: u64,
    full_since: Option<Instant>,
}

impl Viewer {
    fn full(&self, next: usize) -> bool {
        self.device && self.sent - self.acked + next as u64 > WINDOW
    }
}

enum Control {
    /// Kill the program (`term_close`, or its session stopped).
    Close,
    /// A viewer acknowledged, attached or left: a held pump looks again.
    Wake,
}

impl Terminal {
    /// Whether a viewer's window has no room for `next` more bytes. A
    /// device full for `SLOW` is detached instead.
    fn held(&mut self, hub: &Hub, id: &str, next: usize) -> bool {
        let now = Instant::now();
        let mut held = false;
        let owner = self.owner;
        let mut orphaned = false;
        self.viewers.retain_mut(|viewer| {
            if !viewer.full(next) {
                viewer.full_since = None;
                return true;
            }
            if now.duration_since(*viewer.full_since.get_or_insert(now)) >= SLOW {
                hub.send_to(
                    viewer.client,
                    &json!({ "type": "term_detached", "term": id, "reason": "slow" }),
                );
                orphaned |= owner == Some(viewer.client);
                return false;
            }
            held = true;
            true
        });
        if orphaned {
            let _ = self.control.send(Control::Close);
        }
        held
    }

    /// Stops `client`'s output; a shell it owns is killed.
    fn detach(&mut self, client: u64) {
        self.viewers.retain(|viewer| viewer.client != client);
        let next = if self.owner == Some(client) {
            Control::Close
        } else {
            Control::Wake
        };
        let _ = self.control.send(next);
    }
}

impl Terminals {
    /// Detaches `client` from every terminal and kills its shells (it
    /// disconnected).
    pub fn detach_client(&self, client: u64) {
        for terminal in lock(&self.open).values_mut() {
            terminal.detach(client);
        }
    }
}

/// `term_open`, `term_attach`, `term_detach`, `term_input`, `term_resize`,
/// `term_ack`, `term_close`. Only `term_open` and `term_attach` reply
/// (themselves, so `term_opened` or `term_attached` comes before output).
pub fn handle(hub: &Arc<Hub>, client: u64, name: &str, op: &Value) -> Result<Value, String> {
    if name == "term_open" {
        return open_shell(hub, client, op);
    }
    let id = op["term"].as_str().unwrap_or_default();
    let mut open = lock(&hub.terminals.open);
    if name == "term_attach" {
        let terminal = open
            .get_mut(id)
            .filter(|terminal| terminal.owner.is_none())
            .ok_or_else(|| format!("unknown terminal {id}"))?;
        attach(hub, terminal, client, id, op);
        return Ok(Value::Null);
    }
    let not_attached = || format!("not attached to terminal {id}");
    let terminal = open.get_mut(id).ok_or_else(not_attached)?;
    let viewer = terminal
        .viewers
        .iter()
        .position(|viewer| viewer.client == client)
        .ok_or_else(not_attached)?;
    match name {
        "term_input" => {
            let data = STANDARD
                .decode(op["data"].as_str().unwrap_or_default())
                .map_err(|error| format!("term_input data: {error}"))?;
            let _ = terminal.input.send(data);
        }
        "term_resize" => {
            let size = size(op);
            terminal
                .master
                .resize(size)
                .map_err(|error| error.to_string())?;
            terminal.size = size;
        }
        "term_detach" => terminal.detach(client),
        "term_ack" => {
            let viewer = &mut terminal.viewers[viewer];
            let bytes = op["bytes"].as_u64().unwrap_or(0).min(viewer.sent);
            if viewer.device && bytes > viewer.acked {
                viewer.acked = bytes;
                viewer.full_since = None;
                let _ = terminal.control.send(Control::Wake);
            }
        }
        _ => {
            let _ = terminal.control.send(Control::Close);
        }
    }
    Ok(Value::Null)
}

/// Makes `client` a viewer (again): `term_attached`, the scrollback as
/// `replay` output, then the pty at its size, changed at least briefly so
/// the TUI repaints (SIGWINCH comes only with a change).
fn attach(hub: &Hub, terminal: &mut Terminal, client: u64, id: &str, op: &Value) {
    terminal.viewers.retain(|viewer| viewer.client != client);
    let size = size(op);
    let mut reply =
        json!({ "type": "term_attached", "term": id, "cols": size.cols, "rows": size.rows });
    if !op["id"].is_null() {
        reply["id"] = op["id"].clone();
    }
    hub.send_to(client, &reply);
    let replay = terminal.scrollback.make_contiguous();
    for chunk in replay.chunks(MAX_CHUNK) {
        hub.send_to(
            client,
            &json!({ "type": "term_output", "term": id, "data": STANDARD.encode(chunk), "replay": true }),
        );
    }
    terminal.viewers.push(Viewer {
        client,
        device: hub.device_of(client).is_some(),
        sent: replay.len() as u64,
        acked: 0,
        full_since: None,
    });
    if size == terminal.size {
        let _ = terminal.master.resize(PtySize {
            rows: size.rows - 1,
            ..size
        });
    }
    let _ = terminal.master.resize(size);
    terminal.size = size;
    let _ = terminal.control.send(Control::Wake);
}

/// The user's login shell in `op`'s `cwd`, a known directory, owned by
/// `client`.
fn open_shell(hub: &Arc<Hub>, client: u64, op: &Value) -> Result<Value, String> {
    let cwd = op["cwd"].as_str().ok_or("term_open requires cwd")?;
    let cwd = files::known_dir(hub, cwd)?;
    if !cwd.is_dir() {
        return Err(format!("not a directory: {}", cwd.display()));
    }
    spawn(hub, client, op, shell(&cwd), None, true).map(|_| Value::Null)
}

/// A terminal running `command` (a session's TUI), with `client` as its
/// first viewer, replying `term_opened` (with `request`'s id) before any
/// output; `on_exit` runs once it has exited and every viewer got
/// `term_exit`. Its id.
pub fn open_command(
    hub: &Arc<Hub>,
    client: u64,
    request: &Value,
    command: CommandBuilder,
    on_exit: Option<Box<dyn FnOnce() + Send>>,
) -> Result<String, String> {
    spawn(hub, client, request, command, on_exit, false)
}

/// `open_command`; `shell`: owned by `client`.
fn spawn(
    hub: &Arc<Hub>,
    client: u64,
    request: &Value,
    command: CommandBuilder,
    on_exit: Option<Box<dyn FnOnce() + Send>>,
    shell: bool,
) -> Result<String, String> {
    // Held until the terminal is listed: keeps the cap exact, and its pump
    // cannot look for it before.
    let mut open = lock(&hub.terminals.open);
    let owner = shell.then_some(client);
    if shell
        && open
            .values()
            .filter(|terminal| terminal.owner == owner)
            .count()
            >= MAX_SHELLS
    {
        return Err(format!("at most {MAX_SHELLS} terminals may be open"));
    }
    let size = size(request);
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|error| error.to_string())?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| error.to_string())?;
    // The pty reads end of file once the program's side is closed.
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| error.to_string())?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| error.to_string())?;

    let id = hub.new_id("t");
    let mut reply = json!({ "type": "term_opened", "term": id });
    if !request["id"].is_null() {
        reply["id"] = request["id"].clone();
    }
    hub.send_to(client, &reply);

    let (input, inputs) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        for data in inputs {
            if writer
                .write_all(&data)
                .and_then(|()| writer.flush())
                .is_err()
            {
                break;
            }
        }
    });
    // Bounded: a pump held by a full window stops reading the pty, and the
    // program then waits on its writes as on a slow terminal.
    let (output, outputs) = mpsc::sync_channel(8);
    thread::spawn(move || read(reader, output));
    let (control, controls) = mpsc::channel();
    let (pumping, term) = (Arc::clone(hub), id.clone());
    thread::spawn(move || {
        pump(&pumping, &term, child, &outputs, &controls);
        if let Some(on_exit) = on_exit {
            on_exit();
        }
    });
    open.insert(
        id.clone(),
        Terminal {
            owner,
            master: pair.master,
            input,
            control,
            viewers: vec![Viewer {
                client,
                device: hub.device_of(client).is_some(),
                sent: 0,
                acked: 0,
                full_since: None,
            }],
            scrollback: VecDeque::new(),
            size,
        },
    );
    Ok(id)
}

/// Kills terminal `id` (its exit still follows).
pub fn kill(hub: &Hub, id: &str) {
    if let Some(terminal) = lock(&hub.terminals.open).get(id) {
        let _ = terminal.control.send(Control::Close);
    }
}

/// `command` (an engine's TUI) on a pty: its program, arguments and
/// environment, in `cwd`, with the terminal variables a shell gets.
pub fn tui(command: &std::process::Command, cwd: &Path) -> CommandBuilder {
    let mut builder = CommandBuilder::new(command.get_program());
    builder.args(command.get_args());
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => builder.env(name, value),
            None => builder.env_remove(name),
        }
    }
    builder.cwd(cwd);
    builder.env("TERM", "xterm-256color");
    builder.env("COLORTERM", "truecolor");
    if builder.get_env("LANG").is_none() {
        builder.env("LANG", "en_US.UTF-8");
    }
    builder
}

/// The user's login shell in `cwd`, with the daemon's environment.
fn shell(cwd: &Path) -> CommandBuilder {
    #[cfg(unix)]
    let mut command = {
        let fallback = if cfg!(target_os = "macos") {
            "/bin/zsh"
        } else {
            "/bin/bash"
        };
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| {
                let found = [fallback, "/bin/sh"]
                    .into_iter()
                    .find(|shell| Path::new(shell).exists());
                found.unwrap_or("/bin/sh").to_string()
            });
        let mut command = CommandBuilder::new(shell);
        command.arg("-l");
        command
    };
    #[cfg(windows)]
    let mut command = CommandBuilder::new(
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string()),
    );
    command.cwd(cwd);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    if command.get_env("LANG").is_none() {
        command.env("LANG", "en_US.UTF-8");
    }
    command
}

/// `cols` × `rows`, 80 × 24 by default.
fn size(op: &Value) -> PtySize {
    let dimension =
        |key: &str, default: u64| op[key].as_u64().unwrap_or(default).clamp(2, 1000) as u16;
    PtySize {
        rows: dimension("rows", 24),
        cols: dimension("cols", 80),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Reads the pty until it closes (the channel then disconnects).
fn read(mut reader: Box<dyn Read + Send>, output: mpsc::SyncSender<Vec<u8>>) {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if output.send(buffer[..read].to_vec()).is_err() {
                    return;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            // EIO once the program's side closed (Linux).
            Err(_) => break,
        }
    }
}

/// Sends the terminal's output in chunks as the viewers' windows allow,
/// then `term_exit` to every viewer once the program is gone and reaped.
fn pump(
    hub: &Hub,
    id: &str,
    mut child: Box<dyn Child + Send + Sync>,
    output: &mpsc::Receiver<Vec<u8>>,
    controls: &mpsc::Receiver<Control>,
) {
    let mut buffer = Vec::new();
    let mut exited: Option<Instant> = None;
    // Killed: what is left goes out whatever the windows.
    let mut closing = false;
    // The buffered output has been followed by `QUIET`.
    let mut quiet = false;
    loop {
        while let Ok(next) = controls.try_recv() {
            closing |= closes(next, child.as_mut());
        }
        if quiet || buffer.len() >= MAX_CHUNK {
            if !send(hub, id, &mut buffer, closing) {
                if let Ok(next) = controls.recv_timeout(IDLE) {
                    closing |= closes(next, child.as_mut());
                }
                continue;
            }
            quiet = false;
        }
        let wait = if buffer.is_empty() { IDLE } else { QUIET };
        match output.recv_timeout(wait) {
            Ok(bytes) => buffer.extend(bytes),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) if !buffer.is_empty() => quiet = true,
            Err(RecvTimeoutError::Timeout) => match exited {
                Some(at) if at.elapsed() >= GRACE => break,
                Some(_) => {}
                None => {
                    if let Ok(Some(_)) = child.try_wait() {
                        exited = Some(Instant::now());
                    }
                }
            },
        }
    }
    send(hub, id, &mut buffer, true);
    // Closes the pty (ending ConPTY's reader on Windows).
    let viewers = lock(&hub.terminals.open)
        .remove(id)
        .map(|terminal| terminal.viewers)
        .unwrap_or_default();
    let code = child
        .wait()
        .ok()
        .filter(|status| status.signal().is_none())
        .map(|status| status.exit_code());
    for viewer in viewers {
        hub.send_to(
            viewer.client,
            &json!({ "type": "term_exit", "term": id, "code": code }),
        );
    }
}

/// Kills the program on `Close` (SIGHUP, then SIGKILL if it is still there
/// shortly after); whether it did.
fn closes(control: Control, child: &mut (dyn Child + Send + Sync)) -> bool {
    let close = matches!(control, Control::Close);
    if close {
        let _ = child.kill();
    }
    close
}

/// Sends `buffer` to every viewer and keeps it as scrollback; false when a
/// viewer's window is full (`force`: never), the rest left in `buffer`.
fn send(hub: &Hub, id: &str, buffer: &mut Vec<u8>, force: bool) -> bool {
    let mut open = lock(&hub.terminals.open);
    let Some(terminal) = open.get_mut(id) else {
        buffer.clear();
        return true;
    };
    while !buffer.is_empty() {
        let len = buffer.len().min(MAX_CHUNK);
        if !force && terminal.held(hub, id, len) {
            return false;
        }
        let chunk: Vec<u8> = buffer.drain(..len).collect();
        let frame = json!({ "type": "term_output", "term": id, "data": STANDARD.encode(&chunk) });
        for viewer in &mut terminal.viewers {
            viewer.sent += len as u64;
            hub.send_to(viewer.client, &frame);
        }
        terminal.scrollback.extend(chunk);
        let over = terminal.scrollback.len().saturating_sub(SCROLLBACK);
        terminal.scrollback.drain(..over);
    }
    true
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::store::{now, Store};
    use std::{fs, path::PathBuf};

    fn hub(label: &str) -> (Arc<Hub>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "jucode-terminal-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(Store::open(dir.join("daemon")).unwrap(), "test", None);
        (hub, dir)
    }

    fn client(hub: &Hub, device: bool) -> (u64, mpsc::Receiver<String>) {
        let (outbox, frames) = Hub::outbox();
        let device = device.then(|| "dev-1".to_string());
        (hub.add_client(outbox, device), frames)
    }

    fn next(frames: &mpsc::Receiver<String>) -> Value {
        let frame = frames.recv_timeout(Duration::from_secs(30)).unwrap();
        serde_json::from_str(&frame).unwrap()
    }

    fn data(frame: &Value) -> Vec<u8> {
        STANDARD.decode(frame["data"].as_str().unwrap()).unwrap()
    }

    /// `script` as the TUI, opened by `client`; its id.
    fn open(hub: &Arc<Hub>, client: u64, frames: &mpsc::Receiver<String>, script: &str) -> String {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        command.cwd(std::env::temp_dir());
        let request = json!({ "id": 7, "cols": 80, "rows": 24 });
        let term = open_command(hub, client, &request, command, None).unwrap();
        let opened = next(frames);
        assert_eq!(opened["type"], "term_opened");
        assert_eq!(opened["id"], 7);
        assert_eq!(opened["term"], term.as_str());
        term
    }

    /// Output (after `term_attached`) until it contains `text`.
    fn until(frames: &mpsc::Receiver<String>, text: &str) -> String {
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains(text) {
            let frame = next(frames);
            if frame["type"] == "term_attached" {
                continue;
            }
            assert_eq!(frame["type"], "term_output", "{frame}");
            output.extend(data(&frame));
        }
        String::from_utf8_lossy(&output).to_string()
    }

    fn op(hub: &Arc<Hub>, client: u64, name: &str, op: Value) -> Result<Value, String> {
        handle(hub, client, name, &op)
    }

    fn typed(term: &str, text: &str) -> Value {
        json!({ "term": term, "data": STANDARD.encode(text) })
    }

    /// A project at `dir`/app, a known directory.
    fn project(hub: &Hub, dir: &Path) -> PathBuf {
        let project = dir.join("app");
        fs::create_dir_all(&project).unwrap();
        hub.store
            .update_workspaces(|list| {
                list.push(json!({ "id": "w", "name": "W", "projects": [
                    { "id": "p", "name": "app", "path": project.display().to_string() }
                ] }));
                Ok(())
            })
            .unwrap();
        project
    }

    #[test]
    fn a_shell_runs_in_the_project_for_its_owner_only() {
        let (hub, dir) = hub("shell");
        let project = project(&hub, &dir);
        let (a, a_frames) = client(&hub, false);
        let (b, _b_frames) = client(&hub, false);
        let open = json!({ "cwd": project, "cols": 100, "rows": 30, "id": 7 });
        assert_eq!(op(&hub, a, "term_open", open), Ok(Value::Null));
        let opened = next(&a_frames);
        assert_eq!(opened["type"], "term_opened");
        assert_eq!(opened["id"], 7);
        let term = opened["term"].as_str().unwrap().to_string();

        // Nobody else may attach to it, type into it or close it.
        let unknown = Err(format!("unknown terminal {term}"));
        assert_eq!(op(&hub, b, "term_attach", json!({ "term": term })), unknown);
        let refused = Err(format!("not attached to terminal {term}"));
        assert_eq!(op(&hub, b, "term_close", json!({ "term": term })), refused);

        let resize = json!({ "term": term, "cols": 120, "rows": 40 });
        assert_eq!(op(&hub, a, "term_resize", resize), Ok(Value::Null));
        let input = typed(&term, "echo $((40+2)); exit 3\n");
        assert_eq!(op(&hub, a, "term_input", input), Ok(Value::Null));
        let mut output = Vec::new();
        let exit = loop {
            let frame = next(&a_frames);
            assert_eq!(frame["term"], term.as_str());
            match frame["type"].as_str() {
                Some("term_output") => output.extend(data(&frame)),
                _ => break frame,
            }
        };
        assert!(String::from_utf8_lossy(&output).contains("42"));
        assert_eq!(exit["type"], "term_exit");
        assert_eq!(exit["code"], 3);
        assert!(lock(&hub.terminals.open).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn shells_stay_in_known_directories_are_capped_and_die_with_their_owner() {
        let (hub, dir) = hub("shells");
        let project = project(&hub, &dir);
        let (a, a_frames) = client(&hub, false);
        let outside = json!({ "cwd": std::env::temp_dir() });
        assert!(op(&hub, a, "term_open", outside).is_err());
        let open = json!({ "cwd": project });
        for _ in 0..MAX_SHELLS {
            assert_eq!(op(&hub, a, "term_open", open.clone()), Ok(Value::Null));
        }
        assert!(op(&hub, a, "term_open", open).is_err());

        hub.remove_client(a);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !lock(&hub.terminals.open).is_empty() {
            assert!(Instant::now() < deadline, "shells still open");
            thread::sleep(Duration::from_millis(50));
        }
        drop(a_frames);
        let _ = fs::remove_dir_all(dir);
    }

    const ECHO: &str = "stty -echo; echo ready; while read l; do echo \"got:$l\"; done";

    #[test]
    fn every_viewer_sees_the_output_and_may_type() {
        let (hub, dir) = hub("viewers");
        let (a, a_frames) = client(&hub, false);
        let (b, b_frames) = client(&hub, false);
        let (c, _c_frames) = client(&hub, false);
        let term = open(&hub, a, &a_frames, ECHO);
        until(&a_frames, "ready");

        let attach = json!({ "term": term, "cols": 90, "rows": 30, "id": 8 });
        assert_eq!(op(&hub, b, "term_attach", attach), Ok(Value::Null));
        let attached = next(&b_frames);
        assert_eq!(
            attached,
            json!({ "type": "term_attached", "term": term, "cols": 90, "rows": 30, "id": 8 })
        );
        let replay = next(&b_frames);
        assert_eq!(replay["replay"], true);
        assert!(String::from_utf8_lossy(&data(&replay)).contains("ready"));

        for (who, text) in [(b, "from-b"), (a, "from-a")] {
            let input = typed(&term, &format!("{text}\n"));
            assert_eq!(op(&hub, who, "term_input", input), Ok(Value::Null));
            until(&a_frames, &format!("got:{text}"));
            until(&b_frames, &format!("got:{text}"));
        }
        let resize = json!({ "term": term, "cols": 120, "rows": 40 });
        assert_eq!(op(&hub, b, "term_resize", resize), Ok(Value::Null));

        let refused = Err(format!("not attached to terminal {term}"));
        assert_eq!(op(&hub, c, "term_input", typed(&term, "x\n")), refused);
        assert_eq!(op(&hub, c, "term_detach", json!({ "term": term })), refused);
        assert_eq!(op(&hub, c, "term_close", json!({ "term": term })), refused);
        assert_eq!(
            op(&hub, c, "term_attach", json!({ "term": "t-none" })),
            Err("unknown terminal t-none".to_string())
        );

        assert_eq!(
            op(&hub, a, "term_close", json!({ "term": term })),
            Ok(Value::Null)
        );
        for frames in [&a_frames, &b_frames] {
            let exit = loop {
                let frame = next(frames);
                if frame["type"] != "term_output" {
                    break frame;
                }
            };
            assert_eq!(exit["type"], "term_exit");
        }
        assert!(lock(&hub.terminals.open).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_attaching_viewer_gets_the_latest_output_and_its_size() {
        let (hub, dir) = hub("replay");
        let (a, a_frames) = client(&hub, false);
        let (b, b_frames) = client(&hub, false);
        let term = open(
            &hub,
            a,
            &a_frames,
            "yes 0123456789 | head -c 400000; printf END; exec sleep 30",
        );
        until(&a_frames, "END");

        let attach = json!({ "term": term, "cols": 100, "rows": 30 });
        assert_eq!(op(&hub, b, "term_attach", attach), Ok(Value::Null));
        assert_eq!(next(&b_frames)["type"], "term_attached");
        let mut replay = Vec::new();
        while let Ok(frame) = b_frames.try_recv() {
            let frame: Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(frame["replay"], true);
            replay.extend(data(&frame));
        }
        assert!(replay.len() > 200_000 && replay.len() <= SCROLLBACK);
        assert!(replay.ends_with(b"END"));
        let size = lock(&hub.terminals.open)[&term].master.get_size().unwrap();
        assert_eq!((size.cols, size.rows), (100, 30));

        kill(&hub, &term);
        assert_eq!(next(&b_frames)["type"], "term_exit");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_viewer_that_leaves_is_detached_and_the_tui_goes_on() {
        let (hub, dir) = hub("leave");
        let (a, a_frames) = client(&hub, false);
        let (b, b_frames) = client(&hub, false);
        let (c, _c_frames) = client(&hub, false);
        let term = open(&hub, a, &a_frames, ECHO);
        until(&a_frames, "ready");
        for viewer in [b, c] {
            let attach = json!({ "term": term });
            assert_eq!(op(&hub, viewer, "term_attach", attach), Ok(Value::Null));
        }
        hub.remove_client(c);
        assert_eq!(lock(&hub.terminals.open)[&term].viewers.len(), 2);

        assert_eq!(
            op(&hub, b, "term_input", typed(&term, "on\n")),
            Ok(Value::Null)
        );
        until(&a_frames, "got:on");
        until(&b_frames, "got:on");
        assert_eq!(
            op(&hub, b, "term_close", json!({ "term": term })),
            Ok(Value::Null)
        );
        for frames in [&a_frames, &b_frames] {
            let exit = loop {
                let frame = next(frames);
                if frame["type"] != "term_output" {
                    break frame;
                }
            };
            assert_eq!(exit["type"], "term_exit");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_paired_device_holds_the_output_until_it_acks_or_is_too_slow() {
        let (hub, dir) = hub("window");
        let (phone, phone_frames) = client(&hub, true);
        let (desk, desk_frames) = client(&hub, false);
        let term = open(
            &hub,
            phone,
            &phone_frames,
            "head -c 2097152 /dev/zero | tr '\\0' x; printf DONE",
        );
        // Everything the window allows, then nothing.
        let mut received = 0;
        while let Ok(frame) = phone_frames.recv_timeout(Duration::from_millis(60)) {
            let frame: Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(frame["type"], "term_output");
            received += data(&frame).len() as u64;
        }
        assert!(received > 0 && received <= WINDOW, "{received}");
        let ack = json!({ "term": term, "bytes": received });
        assert_eq!(op(&hub, phone, "term_ack", ack), Ok(Value::Null));
        assert_eq!(next(&phone_frames)["type"], "term_output");

        // The desktop is not held once the phone, no longer acking, is
        // detached.
        assert_eq!(
            op(&hub, desk, "term_attach", json!({ "term": term })),
            Ok(Value::Null)
        );
        let detached = loop {
            let frame = next(&phone_frames);
            if frame["type"] != "term_output" {
                break frame;
            }
        };
        assert_eq!(
            detached,
            json!({ "type": "term_detached", "term": term, "reason": "slow" })
        );
        let mut output = Vec::new();
        let exit = loop {
            let frame = next(&desk_frames);
            match frame["type"].as_str() {
                Some("term_attached") => {}
                Some("term_output") => output.extend(data(&frame)),
                _ => break frame,
            }
        };
        assert_eq!(exit["type"], "term_exit");
        assert!(output.ends_with(b"DONE"));
        let _ = fs::remove_dir_all(dir);
    }
}
