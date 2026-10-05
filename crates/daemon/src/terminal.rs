//! Remote terminals: the user's login shell on a pty, in a known directory,
//! or a session's own TUI (see `open_command`). A terminal belongs to the
//! client that opened it: only that client may use it or sees its output,
//! and it is killed when that client goes.

use crate::{
    files,
    hub::{lock, Hub},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    path::Path,
    sync::{
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_PER_CLIENT: usize = 4;
/// Largest `term_output` before base64 (about 88 KiB encoded), well below
/// the relay's 1 MiB frame limit.
const MAX_CHUNK: usize = 64 * 1024;
/// Output is sent once the shell has been quiet this long.
const QUIET: Duration = Duration::from_millis(10);
/// How often an idle terminal checks whether its shell has exited while
/// the pty stays open (a background job holds it, or ConPTY on Windows).
const IDLE: Duration = Duration::from_millis(250);
/// Output still awaited after the shell exited.
const GRACE: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Terminals {
    open: Mutex<HashMap<String, Terminal>>,
}

struct Terminal {
    client: u64,
    master: Box<dyn MasterPty + Send>,
    /// Bytes for the shell, written on their own thread so a shell that
    /// does not read never holds up the client's connection.
    input: mpsc::Sender<Vec<u8>>,
    events: mpsc::Sender<Event>,
}

enum Event {
    Output(Vec<u8>),
    /// The pty closed.
    End,
    /// Kill the shell (`term_close`, or its client left).
    Close,
}

impl Terminals {
    /// Kills the terminals `client` opened; each still sends `term_exit`.
    pub fn close_client(&self, client: u64) {
        for terminal in lock(&self.open).values() {
            if terminal.client == client {
                let _ = terminal.events.send(Event::Close);
            }
        }
    }
}

/// `term_open`, `term_input`, `term_resize`, `term_close`. Only `term_open`
/// replies (itself, so `term_opened` comes before any output).
pub fn handle(hub: &Arc<Hub>, client: u64, name: &str, op: &Value) -> Result<Value, String> {
    if name == "term_open" {
        return open(hub, client, op);
    }
    let id = op["term"].as_str().unwrap_or_default();
    let open = lock(&hub.terminals.open);
    let terminal = open
        .get(id)
        .filter(|terminal| terminal.client == client)
        .ok_or_else(|| format!("unknown terminal {id}"))?;
    match name {
        "term_input" => {
            let data = STANDARD
                .decode(op["data"].as_str().unwrap_or_default())
                .map_err(|error| format!("term_input data: {error}"))?;
            let _ = terminal.input.send(data);
        }
        "term_resize" => terminal
            .master
            .resize(size(op))
            .map_err(|error| error.to_string())?,
        _ => {
            let _ = terminal.events.send(Event::Close);
        }
    }
    Ok(Value::Null)
}

fn open(hub: &Arc<Hub>, client: u64, op: &Value) -> Result<Value, String> {
    let cwd = op["cwd"].as_str().ok_or("term_open requires cwd")?;
    let cwd = files::known_dir(hub, cwd)?;
    if !cwd.is_dir() {
        return Err(format!("not a directory: {}", cwd.display()));
    }
    open_command(hub, client, op, shell(&cwd), None).map(|_| Value::Null)
}

/// A terminal running `command` for `client`, replying `term_opened` (with
/// `op`'s id) before any output; `on_exit` runs once it has exited. Its id.
pub fn open_command(
    hub: &Arc<Hub>,
    client: u64,
    op: &Value,
    command: CommandBuilder,
    on_exit: Option<Box<dyn FnOnce() + Send>>,
) -> Result<String, String> {
    // Held until the terminal is listed: keeps the cap exact, and its
    // thread cannot unlist it before that.
    let mut open = lock(&hub.terminals.open);
    if open
        .values()
        .filter(|terminal| terminal.client == client)
        .count()
        >= MAX_PER_CLIENT
    {
        return Err(format!("at most {MAX_PER_CLIENT} terminals may be open"));
    }
    let pair = native_pty_system()
        .openpty(size(op))
        .map_err(|error| error.to_string())?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| error.to_string())?;
    // The pty reads end of file once the shell's side is closed.
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
    if !op["id"].is_null() {
        reply["id"] = op["id"].clone();
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
    let (events, received) = mpsc::channel();
    let output = events.clone();
    thread::spawn(move || read(reader, output));
    let (pumping, term) = (Arc::clone(hub), id.clone());
    thread::spawn(move || {
        pump(&pumping, client, &term, child, received);
        if let Some(on_exit) = on_exit {
            on_exit();
        }
    });
    open.insert(
        id.clone(),
        Terminal {
            client,
            master: pair.master,
            input,
            events,
        },
    );
    Ok(id)
}

/// Types `data` into terminal `id` (a TUI's first command).
pub fn type_in(hub: &Hub, id: &str, data: &[u8]) {
    if let Some(terminal) = lock(&hub.terminals.open).get(id) {
        let _ = terminal.input.send(data.to_vec());
    }
}

/// Kills terminal `id` (its exit still follows).
pub fn kill(hub: &Hub, id: &str) {
    if let Some(terminal) = lock(&hub.terminals.open).get(id) {
        let _ = terminal.events.send(Event::Close);
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

fn read(mut reader: Box<dyn Read + Send>, events: mpsc::Sender<Event>) {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if events.send(Event::Output(buffer[..read].to_vec())).is_err() {
                    return;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            // EIO once the shell's side closed (Linux).
            Err(_) => break,
        }
    }
    let _ = events.send(Event::End);
}

/// Sends the terminal's output in chunks, then `term_exit` once the shell
/// is gone and reaped.
fn pump(
    hub: &Hub,
    client: u64,
    id: &str,
    mut child: Box<dyn Child + Send + Sync>,
    events: mpsc::Receiver<Event>,
) {
    let mut buffer = Vec::new();
    let mut exited: Option<Instant> = None;
    loop {
        let wait = if buffer.is_empty() { IDLE } else { QUIET };
        match events.recv_timeout(wait) {
            Ok(Event::Output(bytes)) => {
                buffer.extend(bytes);
                if buffer.len() >= MAX_CHUNK {
                    flush(hub, client, id, &mut buffer);
                }
            }
            // SIGHUP, then SIGKILL if the shell is still there shortly after.
            Ok(Event::Close) => {
                let _ = child.kill();
            }
            Ok(Event::End) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                flush(hub, client, id, &mut buffer);
                match exited {
                    Some(at) if at.elapsed() >= GRACE => break,
                    Some(_) => {}
                    None => {
                        if let Ok(Some(_)) = child.try_wait() {
                            exited = Some(Instant::now());
                        }
                    }
                }
            }
        }
    }
    flush(hub, client, id, &mut buffer);
    // Closes the pty (ending ConPTY's reader on Windows).
    lock(&hub.terminals.open).remove(id);
    let code = child
        .wait()
        .ok()
        .filter(|status| status.signal().is_none())
        .map(|status| status.exit_code());
    hub.send_to(
        client,
        &json!({ "type": "term_exit", "term": id, "code": code }),
    );
}

fn flush(hub: &Hub, client: u64, id: &str, buffer: &mut Vec<u8>) {
    for chunk in buffer.chunks(MAX_CHUNK) {
        hub.send_to(
            client,
            &json!({ "type": "term_output", "term": id, "data": STANDARD.encode(chunk) }),
        );
    }
    buffer.clear();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        store::{now, Store},
        Agents,
    };
    use std::{fs, path::PathBuf};

    fn hub(label: &str) -> (Arc<Hub>, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "jucode-terminal-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
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
        (hub, project, dir)
    }

    fn next(frames: &mpsc::Receiver<String>) -> Value {
        let frame = frames.recv_timeout(Duration::from_secs(30)).unwrap();
        serde_json::from_str(&frame).unwrap()
    }

    fn input(text: &str) -> String {
        STANDARD.encode(text)
    }

    #[test]
    fn a_shell_runs_in_the_project_and_reports_its_exit() {
        let (hub, project, dir) = hub("run");
        let (outbox, frames) = mpsc::channel();
        let client = hub.add_client(outbox, None);
        let op = json!({ "op": "term_open", "cwd": project, "cols": 100, "rows": 30, "id": 7 });
        assert_eq!(handle(&hub, client, "term_open", &op), Ok(Value::Null));
        let opened = next(&frames);
        assert_eq!(opened["type"], "term_opened");
        assert_eq!(opened["id"], 7);
        let term = opened["term"].as_str().unwrap().to_string();

        // Another client can neither type into it nor close it.
        let (other_outbox, _other) = mpsc::channel();
        let other = hub.add_client(other_outbox, None);
        let close = json!({ "term": term });
        assert!(handle(&hub, other, "term_close", &close).is_err());

        let resize = json!({ "term": term, "cols": 120, "rows": 40 });
        assert_eq!(
            handle(&hub, client, "term_resize", &resize),
            Ok(Value::Null)
        );
        let typed = json!({ "term": term, "data": input("echo $((40+2)); exit 3\n") });
        assert_eq!(handle(&hub, client, "term_input", &typed), Ok(Value::Null));
        let mut output = Vec::new();
        let exit = loop {
            let frame = next(&frames);
            assert_eq!(frame["term"], term.as_str());
            match frame["type"].as_str() {
                Some("term_output") => {
                    output.extend(STANDARD.decode(frame["data"].as_str().unwrap()).unwrap())
                }
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
    fn terminals_stay_in_known_directories_and_are_capped() {
        let (hub, project, dir) = hub("cap");
        let (outbox, frames) = mpsc::channel();
        let client = hub.add_client(outbox, None);
        let outside = json!({ "cwd": std::env::temp_dir() });
        assert!(handle(&hub, client, "term_open", &outside).is_err());
        let op = json!({ "cwd": project });
        for _ in 0..MAX_PER_CLIENT {
            assert_eq!(handle(&hub, client, "term_open", &op), Ok(Value::Null));
        }
        assert!(handle(&hub, client, "term_open", &op).is_err());

        // Leaving kills them all.
        hub.remove_client(client);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !lock(&hub.terminals.open).is_empty() {
            assert!(Instant::now() < deadline, "terminals still open");
            thread::sleep(Duration::from_millis(50));
        }
        drop(frames);
        let _ = fs::remove_dir_all(dir);
    }
}
