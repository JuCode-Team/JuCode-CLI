//! A Noise responder for interop tests (`docs/relay-protocol.md` §6).
//!
//! `cargo run -p jucode-daemon --example noise_peer -- <static private key hex>`
//!
//! Reads Noise messages as hex lines on stdin and writes replies as hex
//! lines on stdout. Line 1 is msg 1: its payload goes to stderr as
//! `payload:<text>` and msg 2 carries `{"ok":true,"device":"test","name":"peer"}`.
//! Every later line is a transport message; each complete frame (after chunk
//! reassembly) is echoed back, sealed and chunked the same way.

use jucode_daemon::noise;
use std::io::{self, BufRead, Write};

fn main() -> Result<(), String> {
    let key = std::env::args()
        .nth(1)
        .ok_or("usage: noise_peer <static private key hex>")?;
    let mut handshake = noise::responder(&decode(&key)?)?;
    let mut lines = io::stdin().lock().lines();
    let mut out = io::stdout().lock();
    let mut buffer = vec![0u8; noise::MAX_MESSAGE];

    let first = lines
        .next()
        .ok_or("missing msg 1")?
        .map_err(|e| e.to_string())?;
    let read = handshake
        .read_message(&decode(&first)?, &mut buffer)
        .map_err(|error| error.to_string())?;
    eprintln!("payload:{}", String::from_utf8_lossy(&buffer[..read]));
    let written = handshake
        .write_message(br#"{"ok":true,"device":"test","name":"peer"}"#, &mut buffer)
        .map_err(|error| error.to_string())?;
    writeln!(out, "{}", encode(&buffer[..written])).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;

    let mut transport = noise::Transport::new(handshake)?;
    for line in lines {
        let line = line.map_err(|error| error.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(frame) = transport.open(&decode(&line)?)? {
            for message in transport.seal(&frame)? {
                writeln!(out, "{}", encode(&message)).map_err(|e| e.to_string())?;
            }
            out.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn decode(hex: &str) -> Result<Vec<u8>, String> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) || !hex.is_ascii() {
        return Err("invalid hex".to_string());
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).map_err(|error| error.to_string()))
        .collect()
}

fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
