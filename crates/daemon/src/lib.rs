//! `jucode daemon`: hosts many JuCode sessions in one long-running process
//! and serves them to clients (Desktop, the remote web page) over a
//! WebSocket. Frames are the `jucode serve` JSON protocol, version 2, with a
//! `session` field on every session op and event. See
//! `docs/agent-daemon-plan.md` and `docs/daemon-protocol.md`.

mod agent_tools;
mod agents;
mod hub;
pub mod install;
mod session;
mod store;

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

/// Serves clients on `listener` until the process exits. The token guards
/// every connection; local clients read it from `<state dir>/token`.
pub fn serve(
    listener: TcpListener,
    store: Store,
    agents: Agents,
    version: &'static str,
) -> io::Result<()> {
    let token = store.token()?;
    let hub = Hub::new(store, agents, version);
    // Fires due timers and retries messages waiting for a free run slot,
    // including ones left over from before a restart.
    let scheduler = Arc::clone(&hub);
    thread::spawn(move || loop {
        scheduler.fire_due_timers();
        thread::sleep(Duration::from_secs(1));
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let hub = Arc::clone(&hub);
        let token = token.clone();
        thread::spawn(move || {
            if let Err(error) = connection(&hub, stream, &token) {
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

fn connection(hub: &Arc<Hub>, stream: TcpStream, token: &str) -> Result<(), String> {
    // The error type is fixed by tungstenite's handshake callback.
    #[allow(clippy::result_large_err)]
    let authorize = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        if request_token(request).as_deref() == Some(token) {
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

    let (outbox, inbox) = mpsc::channel();
    let client = hub.add_client(outbox);
    let result = pump(hub, client, &mut socket, &inbox);
    hub.remove_client(client);
    result
}

fn pump(
    hub: &Arc<Hub>,
    client: u64,
    socket: &mut WebSocket<TcpStream>,
    inbox: &mpsc::Receiver<String>,
) -> Result<(), String> {
    send(socket, &protocol::hello_json(hub.version))?;
    send(socket, &hub.sessions_json())?;
    send(socket, &hub.agents_json())?;
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => handle(hub, client, text.as_str()),
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(())
            }
            Err(error) => return Err(error.to_string()),
        }
        while let Ok(frame) = inbox.try_recv() {
            socket
                .send(Message::text(frame))
                .map_err(|error| error.to_string())?;
        }
    }
}

fn send(socket: &mut WebSocket<TcpStream>, frame: &Value) -> Result<(), String> {
    socket
        .send(Message::text(frame.to_string()))
        .map_err(|error| error.to_string())
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
    let result = match (op["op"].as_str().unwrap_or_default(), session) {
        ("session_list", _) => Ok(hub.sessions_json()),
        ("session_create", _) => hub
            .create_session(op["cwd"].as_str().map(PathBuf::from), op["agent"].as_str())
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
        ("timer_list", _) => Ok(json!({
            "type": "timers",
            "timers": hub.store.active_timers().iter().map(|timer| json!({
                "timer": timer.id,
                "agent": timer.agent,
                "session": timer.session,
                "fire_at": timer.fire_at,
                "body": timer.body,
            })).collect::<Vec<_>>(),
        })),
        ("session_open", Some(session)) => hub
            .open_session(&session)
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
        (_, Some(session)) => hub.forward(&session, op.clone()).map(|()| Value::Null),
        (name, None) => Err(format!("{name} requires session")),
    };
    match result {
        Ok(Value::Null) => {}
        Ok(frame) => reply(frame),
        Err(message) => reply(json!({ "type": "error", "message": message })),
    }
}
