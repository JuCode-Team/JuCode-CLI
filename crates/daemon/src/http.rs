//! Plain HTTP on the daemon's port, next to the WebSocket: telling a
//! WebSocket upgrade from anything else. Plain requests get 404.

use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

const MAX_HEAD: usize = 16 * 1024;

/// The request head (up to the blank line) without consuming it, so a
/// WebSocket handshake can still read the stream from the start.
pub fn peek_head(stream: &TcpStream) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let mut buffer = vec![0u8; MAX_HEAD];
    let mut last = 0;
    for _ in 0..200 {
        let read = stream
            .peek(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("connection closed before the request head".to_string());
        }
        if let Some(end) = find(&buffer[..read], b"\r\n\r\n") {
            return Ok(String::from_utf8_lossy(&buffer[..end + 4]).into_owned());
        }
        if read == MAX_HEAD {
            return Err("request head too large".to_string());
        }
        if read == last {
            std::thread::sleep(Duration::from_millis(10));
        }
        last = read;
    }
    Err("incomplete request head".to_string())
}

pub fn is_websocket(head: &str) -> bool {
    head.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("upgrade")
                && value.trim().eq_ignore_ascii_case("websocket")
        })
    })
}

/// Answers a plain HTTP request with 404 and closes the connection: the
/// port serves only the WebSocket (and the local gateway).
pub fn not_found(mut stream: TcpStream, head: &str) -> Result<(), String> {
    let mut consumed = vec![0u8; head.len()];
    stream
        .read_exact(&mut consumed)
        .map_err(|error| error.to_string())?;
    let body = "not found";
    stream
        .write_all(
            format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .map_err(|error| error.to_string())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_header_marks_a_websocket() {
        assert!(is_websocket(
            "GET /?token=x HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
        ));
        assert!(!is_websocket("GET /remote HTTP/1.1\r\nHost: x\r\n\r\n"));
    }
}
