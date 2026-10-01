//! `jucode daemon`: hosts many JuCode sessions in one long-running process
//! and serves them to clients (Desktop, the remote web page) over a
//! WebSocket. Frames are the `jucode serve` JSON protocol, version 2, with a
//! `session` field on every session op and event. See
//! `docs/agent-daemon-plan.md` and `docs/daemon-protocol.md`.

mod agent_tools;
mod agents;
mod engines;
mod files;
mod gateway;
mod http;
mod hub;
pub mod install;
pub mod noise;
mod projects;
mod relay;
mod schedules;
mod session;
mod skills;
mod store;
mod titles;

pub use agents::Agents;
pub use store::Store;

use hub::Hub;
use jucode_agent_core::protocol;
use serde_json::{json, Value};
use std::{
    io::{self, ErrorKind},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};
use store::now;
use tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    Message, WebSocket,
};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7788";
pub const DEFAULT_RELAY: &str = "wss://app.jucode.net/relay/v1";

/// Serves clients on `listener` until the process exits. The token guards
/// every WebSocket connection: local clients read it from
/// `<state dir>/token`, paired devices hold their own. `web` is the remote
/// page's build directory, served over plain HTTP on the same port. `relay`
/// is the relay base URL (None: never use the relay); the connection is made
/// only while the `relay` setting is on (`relay_set`).
pub fn serve(
    listener: TcpListener,
    store: Store,
    agents: Agents,
    web: Option<PathBuf>,
    version: &'static str,
    relay: Option<String>,
) -> io::Result<()> {
    let token = store.token()?;
    gateway::set_port(listener.local_addr()?.port());
    for record in store.sessions() {
        gateway::set_group(&record.id, record.group.as_deref());
    }
    let hub = Hub::new(store, agents, version, relay);
    if hub.relay.url().is_some() {
        let relay = Arc::clone(&hub);
        thread::spawn(move || relay::run(&relay));
    }
    // Fires due timers and retries messages waiting for a free run slot,
    // including ones left over from before a restart.
    let scheduler = Arc::clone(&hub);
    thread::spawn(move || loop {
        scheduler.tick();
        thread::sleep(Duration::from_secs(1));
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let hub = Arc::clone(&hub);
        let token = token.clone();
        let web = web.clone();
        thread::spawn(move || {
            if let Err(error) = connection(&hub, stream, &token, web.as_deref()) {
                jucode_agent_core::log_warn!("daemon", "connection ended", error = error);
            }
        });
    }
    Ok(())
}

/// State directory: `~/.jucode/daemon`.
pub fn state_dir() -> io::Result<PathBuf> {
    Ok(jucode_dir()?.join("daemon"))
}

/// Agents directory: `~/.jucode/agents`.
pub fn agents_dir() -> io::Result<PathBuf> {
    Ok(jucode_dir()?.join("agents"))
}

fn jucode_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "home directory not found"))?;
    Ok(PathBuf::from(home).join(".jucode"))
}

fn connection(
    hub: &Arc<Hub>,
    stream: TcpStream,
    token: &str,
    web: Option<&std::path::Path>,
) -> Result<(), String> {
    let head = http::peek_head(&stream)?;
    let path = head.split(' ').nth(1).unwrap_or_default();
    if gateway::is_gateway(path.split('?').next().unwrap_or_default()) {
        return gateway::serve(stream, &head);
    }
    if !http::is_websocket(&head) {
        return http::serve(hub, stream, &head, web);
    }
    // Some(None): the local token; Some(Some(id)): a paired device.
    let mut authorized: Option<Option<String>> = None;
    // The error type is fixed by tungstenite's handshake callback.
    #[allow(clippy::result_large_err)]
    let authorize = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let presented = request_token(request);
        authorized = match presented.as_deref() {
            Some(presented) if presented == token => Some(None),
            Some(presented) => hub
                .store
                .device_for_token(presented)
                .map(|device| Some(device.id)),
            None => None,
        };
        if authorized.is_some() {
            Ok(response)
        } else {
            let mut denied = ErrorResponse::new(Some("missing or wrong token".to_string()));
            *denied.status_mut() = tungstenite::http::StatusCode::UNAUTHORIZED;
            Err(denied)
        }
    };
    let mut socket =
        tungstenite::accept_hdr(stream, authorize).map_err(|error| error.to_string())?;
    // Short reads let one thread both receive ops and flush outgoing events.
    socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(20)))
        .map_err(|error| error.to_string())?;

    attach(hub, &mut socket, authorized.flatten())
}

/// One client connection as the hub sees it: JSON text frames both ways.
/// A local WebSocket and a relay stream (`relay.rs`) are both links.
trait Link {
    fn send(&mut self, frame: &str) -> Result<(), String>;
    /// The next frame, waiting briefly: None when nothing arrived in time.
    /// `Closed` ends the connection without an error.
    fn receive(&mut self) -> Result<Option<String>, Received>;
    /// Drops the connection from the daemon's side.
    fn close(&mut self);
}

enum Received {
    Closed,
    Failed(String),
}

impl Link for WebSocket<TcpStream> {
    fn send(&mut self, frame: &str) -> Result<(), String> {
        WebSocket::send(self, Message::text(frame)).map_err(|error| error.to_string())
    }

    fn receive(&mut self) -> Result<Option<String>, Received> {
        match self.read() {
            Ok(Message::Text(text)) => Ok(Some(text.as_str().to_string())),
            Ok(Message::Close(_)) => Err(Received::Closed),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                Ok(None)
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                Err(Received::Closed)
            }
            Err(error) => Err(Received::Failed(error.to_string())),
        }
    }

    fn close(&mut self) {
        let _ = WebSocket::close(self, None);
        let _ = self.flush();
    }
}

/// Serves one authenticated client over `link` until either side closes.
/// `device` is the paired device it authenticated as; None for a local
/// client.
fn attach(hub: &Arc<Hub>, link: &mut impl Link, device: Option<String>) -> Result<(), String> {
    let (outbox, inbox) = mpsc::channel();
    let client = hub.add_client(outbox, device);
    let result = pump(hub, client, link, &inbox);
    hub.remove_client(client);
    result
}

fn pump(
    hub: &Arc<Hub>,
    client: u64,
    link: &mut impl Link,
    inbox: &mpsc::Receiver<String>,
) -> Result<(), String> {
    for frame in [
        protocol::hello_json(hub.version),
        hub.sessions_json(),
        projects::workspaces_json(hub),
        hub.agents_json(),
        hub.schedules_json(None),
        hub.questions_json(),
        hub.actions_json(),
    ] {
        link.send(&frame.to_string())?;
    }
    loop {
        match link.receive() {
            Ok(Some(text)) => handle(hub, client, &text),
            Ok(None) => {}
            Err(Received::Closed) => return Ok(()),
            Err(Received::Failed(error)) => return Err(error),
        }
        loop {
            match inbox.try_recv() {
                Ok(frame) => link.send(&frame)?,
                Err(mpsc::TryRecvError::Empty) => break,
                // The hub dropped this client (its device was revoked).
                Err(mpsc::TryRecvError::Disconnected) => {
                    link.close();
                    return Ok(());
                }
            }
        }
    }
}

/// The token from `?token=` (browsers cannot set headers on a WebSocket) or
/// an `Authorization: Bearer` header.
fn request_token(request: &Request) -> Option<String> {
    if let Some(query) = request.uri().query() {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("token=") {
                return Some(value.to_string());
            }
        }
    }
    request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// Handles one client frame. Replies to daemon ops go to this client only
/// and echo its `id`; session events reach every client through the hub.
fn handle(hub: &Arc<Hub>, client: u64, text: &str) {
    let op: Value = match serde_json::from_str(text) {
        Ok(op) => op,
        Err(error) => {
            hub.send_to(
                client,
                &json!({ "type": "error", "message": format!("invalid frame: {error}") }),
            );
            return;
        }
    };
    let request = op.get("id").cloned().unwrap_or(Value::Null);
    let reply = |mut frame: Value| {
        if !request.is_null() {
            frame["id"] = request.clone();
        }
        hub.send_to(client, &frame);
    };
    let session = op["session"].as_str().map(str::to_string);
    let name = op["op"].as_str().unwrap_or_default();
    if matches!(
        name,
        "pair_start"
            | "pair_link"
            | "device_list"
            | "device_revoke"
            | "relay_status"
            | "relay_set"
            | "restart_when_idle"
    ) && !hub.is_local(client)
    {
        reply(json!({ "type": "error", "message": "only the desktop can manage devices" }));
        return;
    }
    // An MCP server is a command to run or a place to send credentials.
    if matches!(name, "mcp_set" | "mcp_remove" | "mcp_toggle") && !hub.is_local(client) {
        reply(json!({ "type": "error", "message": "only the desktop can change MCP servers" }));
        return;
    }
    // Installed skills are instructions and scripts every later session may
    // run, so only the desktop installs them. Both ops wait on the network.
    if matches!(name, "skills_catalog" | "skill_install") {
        if !hub.is_local(client) {
            reply(json!({ "type": "error", "message": "only the desktop can install skills" }));
            return;
        }
        let hub = Arc::clone(hub);
        let name = name.to_string();
        thread::spawn(move || {
            let result = skills::handle(&name, &op);
            respond(&hub, client, &request, result);
        });
        return;
    }
    // The gateway's groups come from the network.
    if name == "gateway_catalog" {
        let hub = Arc::clone(hub);
        thread::spawn(move || respond(&hub, client, &request, Ok(gateway::catalog_json())));
        return;
    }
    // File and git reads can take a while; they must not hold up this
    // client's other frames.
    if matches!(name, "fs_list" | "fs_read" | "git_status" | "git_diff") {
        let hub = Arc::clone(hub);
        let name = name.to_string();
        thread::spawn(move || {
            let result = files::handle(&hub, &name, &op);
            respond(&hub, client, &request, result);
        });
        return;
    }
    // A program to run (an ACP agent, an engine binary, its environment):
    // only the desktop names one.
    if matches!(name, "session_create" | "session_open") {
        let options = engines::Options::from_json(&op["options"]);
        let acp = op["engine"] == "acp";
        if (acp || options.runs_programs()) && !hub.is_local(client) {
            reply(
                json!({ "type": "error", "message": "only the desktop can choose what an engine runs" }),
            );
            return;
        }
        let checked = options
            .check_env()
            .and_then(|()| match acp && options.command.is_none() {
                true => Err("an ACP session needs options.command".to_string()),
                false => Ok(()),
            });
        if let Err(message) = checked {
            reply(json!({ "type": "error", "message": message }));
            return;
        }
    }
    let result = match (name, session) {
        ("ping", _) => Ok(json!({ "type": "pong" })),
        ("restart_when_idle", _) => {
            hub.restart_when_idle();
            Ok(Value::Null)
        }
        ("workspaces", _) => Ok(projects::workspaces_json(hub)),
        ("workspaces_set" | "project_add" | "project_create" | "project_remove", _) => {
            projects::handle(hub, name, &op)
        }
        ("session_history", _) => match op["cwd"].as_str() {
            Some(cwd) => hub.session_history(std::path::Path::new(cwd)),
            None => Err("session_history requires cwd".to_string()),
        },
        // A client's title is its own: never one the daemon may rewrite.
        ("session_meta", Some(session)) => match hub.store.record_session_meta(
            &session,
            &json!({ "title": op["title"], "archived": op["archived"], "hidden": op["hidden"], "group": op["group"] }),
        ) {
            Ok(true) => {
                // The local gateway routes this session's next request there.
                if let Some(group) = op["group"].as_str() {
                    gateway::set_group(&session, Some(group));
                }
                hub.broadcast(&hub.sessions_json());
                Ok(Value::Null)
            }
            Ok(false) => Err("session_meta requires title, archived, hidden or group".to_string()),
            Err(error) => Err(error.to_string()),
        },
        ("pair_link", _) => hub.relay.pair_link(hub),
        ("relay_status", _) => Ok(hub.relay.status_json()),
        ("relay_set", _) => match op["enabled"].as_bool() {
            Some(enabled) => hub
                .relay
                .set_enabled(&hub.store, enabled)
                .map(|()| hub.relay.status_json()),
            None => Err("relay_set requires enabled".to_string()),
        },
        ("pair_start", _) => hub
            .start_pairing()
            .map(|(code, expires_at)| json!({ "type": "pairing", "code": code, "expires_at": expires_at })),
        ("device_list", _) => Ok(hub.devices_json()),
        ("device_revoke", _) => match op["device"].as_str() {
            Some(device) => hub
                .revoke_device(device)
                .map(|()| json!({ "type": "device_revoked", "device": device })),
            None => Err("device_revoke requires device".to_string()),
        },
        ("session_list", _) => Ok(hub.sessions_json()),
        ("session_create", _) => engines::Kind::parse(op["engine"].as_str().unwrap_or_default())
            .and_then(|engine| {
                hub.create_engine_session(
                    op["cwd"].as_str().map(PathBuf::from),
                    op["agent"].as_str(),
                    op["chat"].as_bool().unwrap_or(false),
                    engine,
                    engines::Options::from_json(&op["options"]),
                )
            })
            .map(|session| json!({ "type": "session_created", "session": session })),
        ("agent_list", _) => Ok(hub.agents_json()),
        ("agent_create", _) => {
            let text = |key: &str| op[key].as_str().unwrap_or_default();
            hub.agents
                .create(
                    text("agent"),
                    text("name"),
                    &PathBuf::from(text("cwd")),
                    text("role"),
                    &op,
                )
                .map(|agent| {
                    hub.broadcast(&hub.agents_json());
                    json!({ "type": "agent_created", "agent": agent.to_json() })
                })
        }
        ("message_send", _) => {
            let text = |key: &str| op[key].as_str().map(str::to_string);
            match (text("agent"), text("body")) {
                (Some(to), Some(body)) => {
                    let id = hub.new_id("m");
                    hub.send_message(store::Message {
                        id: id.clone(),
                        to,
                        from: "user".to_string(),
                        body,
                        session: text("session"),
                        reply_to: text("reply_to"),
                        dedupe_key: text("dedupe_key"),
                        at: now(),
                    })
                    .map(|fresh| json!({ "type": "message_accepted", "message": id, "duplicate": !fresh }))
                }
                _ => Err("message_send requires agent and body".to_string()),
            }
        }
        ("question_list", _) => Ok(hub.questions_json()),
        ("question_answer", _) => match (op["question"].as_str(), op["answer"].as_str()) {
            (Some(question), Some(answer)) if !answer.trim().is_empty() => hub
                .answer_question(question, answer.trim(), "user")
                .map(|()| json!({ "type": "question_answered", "question": question })),
            _ => Err("question_answer requires question and a non-empty answer".to_string()),
        },
        ("report_list", _) => Ok(hub.reports_json(op["limit"].as_u64().unwrap_or(50) as usize)),
        ("report_read", _) => match op["report"].as_str() {
            Some(report) => hub
                .store
                .record_report_read(report)
                .map(|()| json!({ "type": "report_read", "report": report }))
                .map_err(|error| error.to_string()),
            None => Err("report_read requires report".to_string()),
        },
        ("agent_get", _) => match op["agent"].as_str().and_then(|id| hub.agents.get(id)) {
            Some(agent) => {
                let brief: serde_json::Map<String, Value> = agents::BRIEF_FILES
                    .iter()
                    .map(|file| {
                        let text = hub.agents.read_brief(&agent.id, file).unwrap_or_default();
                        (file.to_string(), json!(text))
                    })
                    .collect();
                let sessions: Vec<Value> = hub.sessions_json()["sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|session| session["agent"] == agent.id.as_str())
                    .cloned()
                    .collect();
                Ok(json!({
                    "type": "agent",
                    "agent": agent.to_json(),
                    "brief": brief,
                    "memory": hub.agents.memory_files(&agent.id),
                    "sessions": sessions,
                }))
            }
            None => Err("agent_get requires a known agent".to_string()),
        },
        ("agent_update", _) => match op["agent"].as_str() {
            Some(id) => hub.agents.update(id, &op).map(|agent| {
                hub.broadcast(&hub.agents_json());
                json!({ "type": "agent_updated", "agent": agent.to_json() })
            }),
            None => Err("agent_update requires agent".to_string()),
        },
        ("agent_delete", _) => match op["agent"].as_str() {
            Some(id) => hub
                .delete_agent(id)
                .map(|()| json!({ "type": "agent_deleted", "agent": id })),
            None => Err("agent_delete requires agent".to_string()),
        },
        ("agent_memory_read", _) => match (op["agent"].as_str(), op["file"].as_str()) {
            (Some(agent), Some(file)) => hub.agents.read_memory(agent, file).map(|content| {
                json!({ "type": "agent_memory", "agent": agent, "file": file, "content": content })
            }),
            _ => Err("agent_memory_read requires agent and file".to_string()),
        },
        ("schedule_list", _) => Ok(hub.schedules_json(op["agent"].as_str())),
        ("schedule_save", _) => hub
            .save_schedule(&op["schedule"])
            .map(|schedule| json!({ "type": "schedule_saved", "schedule": schedule.to_json() })),
        // The schedule's id comes as `schedule`, or as `id`, which then also
        // serves as the request id.
        ("schedule_delete" | "schedule_run", _) => {
            match op["schedule"].as_str().or(op["id"].as_str()) {
                Some(id) if name == "schedule_delete" => hub
                    .delete_schedule(id)
                    .map(|()| json!({ "type": "schedule_deleted", "schedule": id, "id": id })),
                Some(id) => hub
                    .run_schedule(id)
                    .map(|()| json!({ "type": "schedule_started", "schedule": id, "id": id })),
                None => Err(format!("{name} requires schedule")),
            }
        }
        // An action can be decided after its session closed or the daemon
        // restarted: reopen the session so its engine can run the action.
        ("decide_action", Some(session)) => hub
            .open_session(&session, None)
            .and_then(|()| hub.forward(&session, op.clone()))
            .map(|()| Value::Null),
        ("timer_list", _) => Ok(json!({
            "type": "timers",
            "timers": hub.store.active_timers().iter()
                .filter(|timer| op["agent"].as_str().is_none_or(|agent| timer.agent == agent))
                .map(|timer| json!({
                "timer": timer.id,
                "agent": timer.agent,
                "session": timer.session,
                "fire_at": timer.fire_at,
                "body": timer.body,
            })).collect::<Vec<_>>(),
        })),
        ("session_open", Some(session)) => engines::Kind::parse(op["engine"].as_str().unwrap_or_default())
            .and_then(|engine| {
                hub.open_engine_session(
                    &session,
                    op["cwd"].as_str().map(PathBuf::from),
                    engine,
                    engines::Options::from_json(&op["options"]),
                )
            })
            .map(|()| json!({ "type": "session_opened", "session": session })),
        // Confirmed by the `session_closed` broadcast once the engine has
        // stopped and released the session.
        ("session_close", Some(session)) => hub.close_session(&session).map(|()| Value::Null),
        ("watch", Some(session)) => {
            hub.set_watch(client, &session, true);
            Ok(json!({ "type": "watching", "session": session, "watching": true }))
        }
        ("unwatch", Some(session)) => {
            hub.set_watch(client, &session, false);
            Ok(json!({ "type": "watching", "session": session, "watching": false }))
        }
        ("actions_list", _) => Ok(json!({
            "type": "actions",
            "actions": hub.store.open_actions().iter().map(|action| action.to_json()).collect::<Vec<_>>(),
        })),
        ("set_attended", Some(_)) => Err("the daemon sets attended from watch/unwatch".to_string()),
        // MCP servers are saved for every session: the config first, then the
        // open JuCode sessions apply the same op.
        ("mcp_set" | "mcp_remove" | "mcp_toggle", None) => {
            jucode_agent_core::change_mcp_config(&op).map(|()| {
                hub.forward_to_jucode_sessions(&op);
                json!({ "type": "mcp_saved" })
            })
        }
        (_, Some(session)) => hub.forward(&session, op.clone()).map(|()| Value::Null),
        (name, None) => Err(format!("{name} requires session")),
    };
    respond(hub, client, &request, result);
}

/// Replies to an op: `Null` means no reply; the frame echoes the op's `id`.
fn respond(hub: &Hub, client: u64, request: &Value, result: Result<Value, String>) {
    let mut frame = match result {
        Ok(Value::Null) => return,
        Ok(frame) => frame,
        Err(message) => json!({ "type": "error", "message": message }),
    };
    if !request.is_null() {
        frame["id"] = request.clone();
    }
    hub.send_to(client, &frame);
}
