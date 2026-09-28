//! Plain HTTP on the daemon's port, next to the WebSocket: the remote web
//! page's files and `POST /api/pair`, which trades a pairing code shown on
//! the desktop for a device token. Everything else needs a token over the
//! WebSocket; the files themselves hold nothing private.

use crate::hub::Hub;
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Component, Path, PathBuf},
    time::Duration,
};

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;

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

/// Answers one plain HTTP request and closes the connection.
pub fn serve(
    hub: &Hub,
    mut stream: TcpStream,
    head: &str,
    web: Option<&Path>,
) -> Result<(), String> {
    let mut consumed = vec![0u8; head.len()];
    stream
        .read_exact(&mut consumed)
        .map_err(|error| error.to_string())?;
    let mut request_line = head.lines().next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default();
    let target = request_line.next().unwrap_or("/");
    let path = target.split(['?', '#']).next().unwrap_or("/");
    let response = match (method, path) {
        ("POST", "/api/pair") => {
            let body = read_body(&mut stream, head)?;
            pair(hub, &body)
        }
        ("GET" | "HEAD", "/") => Response::redirect("/remote"),
        ("GET" | "HEAD", _) => match web {
            Some(root) => static_file(root, path),
            None => Response::text(
                404,
                "no web page is installed; start jucode daemon with --web <dir>",
            ),
        },
        _ => Response::text(405, "method not allowed"),
    };
    response.write(&mut stream, method == "HEAD")
}

fn pair(hub: &Hub, body: &[u8]) -> Response {
    let Ok(request) = serde_json::from_slice::<Value>(body) else {
        return Response::json(400, json!({ "error": "expected JSON" }));
    };
    let code = request["code"].as_str().unwrap_or_default();
    let name = request["name"].as_str().unwrap_or_default();
    match hub.pair(code, name) {
        Ok((device, token)) => Response::json(
            200,
            json!({ "device": device.id, "name": device.name, "token": token }),
        ),
        Err(error) => Response::json(403, json!({ "error": error })),
    }
}

fn read_body(stream: &mut TcpStream, head: &str) -> Result<Vec<u8>, String> {
    let length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err("request body too large".to_string());
    }
    let mut body = vec![0u8; length];
    stream
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    Ok(body)
}

/// A file under `root`. Paths without an extension are page routes of the
/// single-page app and get `index.html`.
fn static_file(root: &Path, path: &str) -> Response {
    let Some(relative) = safe_relative(path) else {
        return Response::text(404, "not found");
    };
    let mut file = root.join(&relative);
    if file.is_dir() || (!file.exists() && file.extension().is_none()) {
        file = root.join("index.html");
    }
    match fs::read(&file) {
        Ok(body) => {
            let immutable = relative.starts_with("_app/immutable");
            Response {
                status: 200,
                content_type: content_type(&file),
                cache: if immutable {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                },
                location: None,
                body,
            }
        }
        Err(_) => Response::text(404, "not found"),
    }
}

/// The URL path as a relative path with no `..`, root or drive parts.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let decoded = percent_decode(path.trim_start_matches('/'))?;
    let relative = PathBuf::from(decoded);
    relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        .then_some(relative)
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "webmanifest" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

struct Response {
    status: u16,
    content_type: &'static str,
    cache: &'static str,
    location: Option<String>,
    body: Vec<u8>,
}

impl Response {
    fn text(status: u16, text: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            cache: "no-store",
            location: None,
            body: text.as_bytes().to_vec(),
        }
    }

    fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            cache: "no-store",
            location: None,
            body: value.to_string().into_bytes(),
        }
    }

    fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            content_type: "text/plain; charset=utf-8",
            cache: "no-store",
            location: Some(location.to_string()),
            body: Vec::new(),
        }
    }

    fn write(self, stream: &mut TcpStream, head_only: bool) -> Result<(), String> {
        let reason = match self.status {
            200 => "OK",
            302 => "Found",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            _ => "Method Not Allowed",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: {}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n",
            self.status,
            self.content_type,
            self.body.len(),
            self.cache
        );
        if let Some(location) = &self.location {
            head.push_str(&format!("Location: {location}\r\n"));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .and_then(|()| {
                if head_only {
                    Ok(())
                } else {
                    stream.write_all(&self.body)
                }
            })
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_cannot_leave_the_web_root() {
        assert_eq!(safe_relative("/remote"), Some(PathBuf::from("remote")));
        assert_eq!(
            safe_relative("/_app/immutable/a%20b.js"),
            Some(PathBuf::from("_app/immutable/a b.js"))
        );
        for escape in ["/../etc/passwd", "/a/../../x", "/%2e%2e/x", "/a%2"] {
            assert_eq!(safe_relative(escape), None, "{escape}");
        }
        // Extra leading slashes stay inside the root.
        assert_eq!(
            safe_relative("//etc/passwd"),
            Some(PathBuf::from("etc/passwd"))
        );
    }

    #[test]
    fn upgrade_header_marks_a_websocket() {
        assert!(is_websocket(
            "GET /?token=x HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
        ));
        assert!(!is_websocket("GET /remote HTTP/1.1\r\nHost: x\r\n\r\n"));
    }
}
