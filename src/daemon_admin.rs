//! `jucode daemon pair` / `jucode daemon relay …`: set up a daemon from a
//! terminal, for machines without Desktop (a server, a cloud container).
//! Both talk to the running daemon as a local client (its token is in the
//! state directory); `relay on|off` also works while it is stopped.

use serde_json::{json, Value};
use std::io;
use tungstenite::{connect, Message};

type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// Prints a pairing link (and its code) for a phone or browser to scan.
pub fn pair(listen: &str) -> io::Result<i32> {
    let mut socket = match open(listen) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("jucode daemon pair: {error}");
            return Ok(1);
        }
    };
    let reply = request(&mut socket, json!({ "op": "pair_link" }))?;
    let _ = socket.close(None);
    if reply["type"] == "pair_link" {
        println!("{}", reply["link"].as_str().unwrap_or_default());
        eprintln!(
            "code {} — open the link on the device within 5 minutes",
            reply["code"].as_str().unwrap_or_default()
        );
        Ok(0)
    } else {
        eprintln!(
            "jucode daemon pair: {}",
            reply["message"].as_str().unwrap_or("the daemon refused")
        );
        eprintln!("the relay must be on: jucode daemon relay on");
        Ok(1)
    }
}

/// `relay on|off|status`. A running daemon switches live (and saves the
/// choice); a stopped one finds it saved when it starts.
pub fn relay(listen: &str, action: Option<&str>) -> io::Result<i32> {
    let enabled = match action {
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some("status") | None => None,
        Some(_) => {
            eprintln!("usage: jucode daemon relay [on|off|status] [--listen <host:port>]");
            return Ok(2);
        }
    };
    match open(listen) {
        Ok(mut socket) => {
            let op = match enabled {
                Some(enabled) => json!({ "op": "relay_set", "enabled": enabled }),
                None => json!({ "op": "relay_status" }),
            };
            let reply = request(&mut socket, op)?;
            let _ = socket.close(None);
            print_status(&reply);
            Ok(if reply["type"] == "relay_status" {
                0
            } else {
                1
            })
        }
        // Not running: the setting is read at start.
        Err(_) => {
            let store = jucode_daemon::Store::open(jucode_daemon::state_dir()?)?;
            if let Some(enabled) = enabled {
                store.set_setting("relay", json!(enabled))?;
            }
            let on = store.setting("relay").as_bool().unwrap_or(false);
            println!(
                "relay {} (daemon not running; applies when it starts)",
                if on { "on" } else { "off" }
            );
            Ok(0)
        }
    }
}

fn print_status(reply: &Value) {
    if reply["type"] != "relay_status" {
        eprintln!(
            "jucode daemon relay: {}",
            reply["message"].as_str().unwrap_or("unexpected reply")
        );
        return;
    }
    let on = reply["enabled"].as_bool().unwrap_or(false);
    let connected = reply["connected"].as_bool().unwrap_or(false);
    println!(
        "relay {}{} host {} ({})",
        if on { "on" } else { "off" },
        if on && !connected {
            ", not connected yet,"
        } else {
            ","
        },
        reply["host"].as_str().unwrap_or("-"),
        reply["url"].as_str().unwrap_or("-")
    );
}

/// Connects as a local client and waits for the daemon's hello.
fn open(listen: &str) -> Result<Socket, String> {
    let store =
        jucode_daemon::Store::open(jucode_daemon::state_dir().map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let token = store.token().map_err(|error| error.to_string())?;
    let (mut socket, _) = connect(format!("ws://{listen}/?token={token}")).map_err(|error| {
        format!("no daemon at {listen} ({error}); start it with `jucode daemon`")
    })?;
    loop {
        let frame = read(&mut socket).map_err(|error| error.to_string())?;
        if frame["type"] == "hello" {
            return Ok(socket);
        }
    }
}

/// Sends `op` with an id and returns the reply carrying that id.
fn request(socket: &mut Socket, mut op: Value) -> io::Result<Value> {
    const ID: u64 = 1;
    op["id"] = json!(ID);
    socket
        .send(Message::text(op.to_string()))
        .map_err(io::Error::other)?;
    loop {
        let frame = read(socket)?;
        if frame["id"] == ID {
            return Ok(frame);
        }
    }
}

fn read(socket: &mut Socket) -> io::Result<Value> {
    loop {
        match socket.read().map_err(io::Error::other)? {
            Message::Text(text) => {
                return serde_json::from_str(text.as_str()).map_err(io::Error::other)
            }
            Message::Close(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "the daemon closed the connection",
                ))
            }
            _ => {}
        }
    }
}
