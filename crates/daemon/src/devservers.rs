//! Dev servers the agents start (`npm run dev`, `python -m http.server`), so
//! clients can offer to open them.
//!
//! A shell command in any session opens a scan window: the daemon lists the
//! TCP ports listening on a loopback or wildcard address and keeps the ones
//! that are new since it started (by port and process), whose process works
//! in an open session's directory (macOS, Linux; Windows reports no working
//! directory, so there a port must also have been printed by a command), and
//! that answer `GET /` with HTML. A database, a debugger or a language server
//! never qualifies. The session is the one whose directory holds the
//! process's, the most recently active on a tie. Engine ancestry cannot tell
//! sessions apart: the jucode engine runs inside the daemon, so its commands
//! are all the daemon's children.

use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};

/// How long after a shell command the daemon looks for new servers (ms).
/// A dev server can take a while: installing, compiling, then listening.
const WINDOW_MS: u64 = 90_000;
/// Between scans inside a window, and while servers are up (to see them stop).
const FAST_MS: u64 = 2_000;
const SLOW_MS: u64 = 10_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub port: u16,
    pub url: String,
    pub pid: u32,
    pub name: String,
    pub session: Option<String>,
}

/// One listening socket.
#[derive(Clone, Debug, PartialEq)]
struct Listener {
    port: u16,
    pid: u32,
    name: String,
    /// Bound only to `::1`: reach it there rather than on 127.0.0.1.
    v6_only: bool,
}

/// An open session's directory, for attributing a server to it.
pub struct SessionDir {
    pub id: String,
    pub cwd: PathBuf,
    pub last_active: u64,
}

#[derive(Default)]
struct State {
    /// Listeners (port, pid) up when the daemon first looked: not ours.
    baseline: Option<HashSet<(u16, u32)>>,
    servers: Vec<Server>,
    scan_until: u64,
    last_scan: u64,
    /// URLs commands printed, by port (`http://localhost:5173/app/`).
    printed: HashMap<u16, String>,
    /// Whether a listener answered with HTML, so it is asked once.
    probed: HashMap<(u16, u32), bool>,
}

#[derive(Default)]
pub struct DevServers {
    state: Mutex<State>,
    scanning: AtomicBool,
}

impl DevServers {
    /// Opens a scan window on a shell command or a background task change,
    /// and keeps the URLs a command printed.
    pub fn observe(&self, event: &Value, now: u64) {
        let kind = event["type"].as_str().unwrap_or_default();
        let shell = matches!(kind, "tool_start" | "tool_output") && event["name"] == "bash";
        if !shell && kind != "background_tasks" {
            return;
        }
        let mut state = crate::hub::lock(&self.state);
        state.scan_until = now + WINDOW_MS;
        if let Some(output) = event["output"].as_str() {
            for (port, url) in printed_urls(output) {
                state.printed.insert(port, url);
            }
        }
    }

    /// Whether a scan is due at `now`; claims it when so (one at a time).
    pub fn due(&self, now: u64) -> bool {
        let mut state = crate::hub::lock(&self.state);
        let every = if now < state.scan_until {
            FAST_MS
        } else if !state.servers.is_empty() {
            SLOW_MS
        } else if state.baseline.is_none() {
            0
        } else {
            return false;
        };
        if now < state.last_scan + every || self.scanning.swap(true, Ordering::SeqCst) {
            return false;
        }
        state.last_scan = now;
        true
    }

    /// Scans (after `due`), returning the servers when they changed.
    pub fn scan(&self, sessions: &[SessionDir]) -> Option<Vec<Server>> {
        let listeners = list_listeners();
        self.scanning.store(false, Ordering::SeqCst);
        let listeners = listeners?;
        let mut state = crate::hub::lock(&self.state);
        let Some(baseline) = &state.baseline else {
            state.baseline = Some(listeners.iter().map(|l| (l.port, l.pid)).collect());
            return None;
        };
        let own = std::process::id();
        let fresh: Vec<Listener> = listeners
            .into_iter()
            .filter(|l| l.pid != own && !baseline.contains(&(l.port, l.pid)))
            .collect();
        let printed = state.printed.clone();
        let probed = state.probed.clone();
        drop(state);
        let cwds = working_dirs(&fresh.iter().map(|l| l.pid).collect::<Vec<_>>());
        let sessions: Vec<SessionDir> = sessions
            .iter()
            .map(|s| SessionDir {
                id: s.id.clone(),
                cwd: std::fs::canonicalize(&s.cwd).unwrap_or_else(|_| s.cwd.clone()),
                last_active: s.last_active,
            })
            .collect();
        let mut servers = Vec::new();
        let mut answers = Vec::new();
        for listener in fresh {
            if servers.iter().any(|s: &Server| s.port == listener.port) {
                continue;
            }
            let session = match cwds.as_ref() {
                Some(cwds) => match cwds
                    .get(&listener.pid)
                    .and_then(|cwd| owner(&sessions, cwd))
                {
                    Some(session) => Some(session),
                    None => continue,
                },
                // No working directories (Windows): a port a command printed.
                None if printed.contains_key(&listener.port) => sessions
                    .iter()
                    .max_by_key(|s| s.last_active)
                    .map(|s| s.id.clone()),
                None => continue,
            };
            let key = (listener.port, listener.pid);
            let html = match probed.get(&key) {
                Some(html) => *html,
                None => {
                    let html = serves_html(&listener);
                    answers.push((key, html));
                    html
                }
            };
            if !html {
                continue;
            }
            let url = printed.get(&listener.port).cloned().unwrap_or_else(|| {
                let host = if listener.v6_only {
                    "[::1]"
                } else {
                    "localhost"
                };
                format!("http://{host}:{}/", listener.port)
            });
            servers.push(Server {
                port: listener.port,
                url,
                pid: listener.pid,
                name: listener.name,
                session,
            });
        }
        servers.sort_by_key(|s| s.port);
        let mut state = crate::hub::lock(&self.state);
        state.probed.extend(answers);
        if state.servers == servers {
            return None;
        }
        state.servers = servers.clone();
        Some(servers)
    }

    pub fn json(&self) -> Value {
        servers_json(&crate::hub::lock(&self.state).servers)
    }
}

pub fn servers_json(servers: &[Server]) -> Value {
    let list: Vec<Value> = servers
        .iter()
        .map(|s| {
            json!({
                "port": s.port,
                "url": s.url,
                "pid": s.pid,
                "name": s.name,
                "session": s.session,
            })
        })
        .collect();
    json!({ "type": "dev_servers", "servers": list })
}

/// The session whose directory holds `cwd`: the deepest one, the most
/// recently active among equals.
fn owner(sessions: &[SessionDir], cwd: &Path) -> Option<String> {
    sessions
        .iter()
        .filter(|s| cwd.starts_with(&s.cwd))
        .max_by_key(|s| (s.cwd.components().count(), s.last_active))
        .map(|s| s.id.clone())
}

/// `http(s)://` URLs on a local host with a port in command output (colour
/// codes removed), by port; `0.0.0.0` becomes `localhost`.
fn printed_urls(output: &str) -> Vec<(u16, String)> {
    let plain = strip_ansi(output);
    let mut found = Vec::new();
    for (start, _) in plain.match_indices("http") {
        let rest = &plain[start..];
        let end = rest
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ')' | '>' | '<' | ',')
            })
            .unwrap_or(rest.len());
        let candidate = rest[..end].trim_end_matches(['.', ';']);
        let Some(after) = candidate
            .strip_prefix("http://")
            .or_else(|| candidate.strip_prefix("https://"))
        else {
            continue;
        };
        let scheme = &candidate[..candidate.len() - after.len()];
        let (authority, path) = after.split_at(after.find('/').unwrap_or(after.len()));
        let Some((host, port)) = authority.rsplit_once(':') else {
            continue;
        };
        let Ok(port) = port.parse::<u16>() else {
            continue;
        };
        let host = match host {
            "localhost" | "127.0.0.1" | "[::1]" => host,
            "0.0.0.0" | "[::]" => "localhost",
            _ => continue,
        };
        let path = if path.is_empty() { "/" } else { path };
        found.push((port, format!("{scheme}{host}:{port}{path}")));
    }
    found
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
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

/// Whether `GET /` answers with an HTML page (any status: a dev server's
/// 404 page is still a web page).
fn serves_html(listener: &Listener) -> bool {
    let host = if listener.v6_only {
        "[::1]"
    } else {
        "127.0.0.1"
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(1500))
        .redirects(0)
        .build();
    let response = match agent
        .get(&format!("http://{host}:{}/", listener.port))
        .call()
    {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(_)) => return false,
    };
    response.content_type().eq_ignore_ascii_case("text/html")
        || (300..400).contains(&response.status())
}

/// Listening TCP sockets on a loopback or wildcard address; None when the
/// system could not be asked.
fn list_listeners() -> Option<Vec<Listener>> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("lsof")
            .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"])
            .output()
            .ok()?;
        // Exit code 1 with no output: nothing listens.
        Some(parse_lsof(&String::from_utf8_lossy(&output.stdout)))
    }
    #[cfg(target_os = "linux")]
    {
        linux::listeners()
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let output = std::process::Command::new("netstat")
            .args(["-ano", "-p", "TCP"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        let v6 = std::process::Command::new("netstat")
            .args(["-ano", "-p", "TCPv6"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&v6.stdout)
        );
        Some(parse_netstat(&text))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        None
    }
}

/// Working directories by pid; None where the system does not report them.
fn working_dirs(pids: &[u32]) -> Option<HashMap<u32, PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        if pids.is_empty() {
            return Some(HashMap::new());
        }
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let output = std::process::Command::new("lsof")
            .args(["-a", "-d", "cwd", "-Fn", "-p", &list])
            .output()
            .ok()?;
        Some(parse_lsof_cwd(&String::from_utf8_lossy(&output.stdout)))
    }
    #[cfg(target_os = "linux")]
    {
        Some(
            pids.iter()
                .filter_map(|pid| {
                    Some((*pid, std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?))
                })
                .collect(),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pids;
        None
    }
}

/// Whether a bound address is loopback or the wildcard; `v6_only` when it
/// is `::1`.
fn local_host(host: &str) -> Option<bool> {
    match host {
        "*" | "0.0.0.0" | "127.0.0.1" | "localhost" | "[::]" | "::" => Some(false),
        "[::1]" | "::1" => Some(true),
        _ => None,
    }
}

/// `lsof -F pcn` output: `p<pid>`, `c<command>`, then `n<host>:<port>` per socket.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_lsof(text: &str) -> Vec<Listener> {
    let mut out: Vec<Listener> = Vec::new();
    let (mut pid, mut name) = (0, String::new());
    for line in text.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => pid = value.parse().unwrap_or(0),
            "c" => name = value.to_string(),
            "n" => {
                let Some((host, port)) = value.rsplit_once(':') else {
                    continue;
                };
                let (Some(v6_only), Ok(port)) = (local_host(host), port.parse::<u16>()) else {
                    continue;
                };
                // Bound on both stacks: reachable on 127.0.0.1.
                if let Some(seen) = out.iter_mut().find(|l| l.port == port && l.pid == pid) {
                    seen.v6_only &= v6_only;
                } else if pid != 0 {
                    out.push(Listener {
                        port,
                        pid,
                        name: name.clone(),
                        v6_only,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_lsof_cwd(text: &str) -> HashMap<u32, PathBuf> {
    let mut out = HashMap::new();
    let mut pid = 0;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse().unwrap_or(0);
        } else if let Some(path) = line.strip_prefix('n') {
            if pid != 0 {
                out.insert(pid, PathBuf::from(path));
            }
        }
    }
    out
}

/// `netstat -ano` rows: `TCP  127.0.0.1:5173  0.0.0.0:0  LISTENING  1234`.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_netstat(text: &str) -> Vec<Listener> {
    let mut out: Vec<Listener> = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let [proto, local, _, state, pid] = cols[..] else {
            continue;
        };
        if !proto.eq_ignore_ascii_case("tcp") || state != "LISTENING" {
            continue;
        }
        let Some((host, port)) = local.rsplit_once(':') else {
            continue;
        };
        let (Some(v6_only), Ok(port), Ok(pid)) =
            (local_host(host), port.parse::<u16>(), pid.parse::<u32>())
        else {
            continue;
        };
        if let Some(seen) = out.iter_mut().find(|l| l.port == port && l.pid == pid) {
            seen.v6_only &= v6_only;
        } else {
            out.push(Listener {
                port,
                pid,
                name: String::new(),
                v6_only,
            });
        }
    }
    out
}

#[cfg(target_os = "linux")]
mod linux {
    use super::Listener;
    use std::collections::HashMap;

    /// LISTEN rows of /proc/net/tcp{,6}, joined to the processes holding
    /// their socket inodes.
    pub fn listeners() -> Option<Vec<Listener>> {
        let mut sockets: HashMap<u64, (u16, bool)> = HashMap::new();
        for (file, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            for (inode, port, v6_only) in super::parse_proc_net(&text, v6) {
                sockets.insert(inode, (port, v6_only));
            }
        }
        let mut out: Vec<Listener> = Vec::new();
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                let Ok(link) = std::fs::read_link(fd.path()) else {
                    continue;
                };
                let link = link.to_string_lossy();
                let Some(inode) = link
                    .strip_prefix("socket:[")
                    .and_then(|s| s.strip_suffix(']'))
                    .and_then(|s| s.parse::<u64>().ok())
                else {
                    continue;
                };
                let Some(&(port, v6_only)) = sockets.get(&inode) else {
                    continue;
                };
                if let Some(seen) = out.iter_mut().find(|l| l.port == port && l.pid == pid) {
                    seen.v6_only &= v6_only;
                    continue;
                }
                let name = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
                out.push(Listener {
                    port,
                    pid,
                    name: name.trim().to_string(),
                    v6_only,
                });
            }
        }
        Some(out)
    }
}

/// `/proc/net/tcp` rows in LISTEN state (`0A`) on loopback or the wildcard:
/// (inode, port, bound only to `::1`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_net(text: &str, v6: bool) -> Vec<(u64, u16, bool)> {
    const V6_ANY: &str = "00000000000000000000000000000000";
    const V6_LOOPBACK: &str = "00000000000000000000000001000000";
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 10 || cols[3] != "0A" {
            continue;
        }
        let Some((addr, port)) = cols[1].split_once(':') else {
            continue;
        };
        let Ok(port) = u16::from_str_radix(port, 16) else {
            continue;
        };
        let Ok(inode) = cols[9].parse::<u64>() else {
            continue;
        };
        // Addresses are little-endian words: 127.x.x.x ends in 7F.
        let v6_only = match (v6, addr) {
            (false, a) if a == "00000000" || a.ends_with("7F") => false,
            (true, V6_ANY) => false,
            (true, V6_LOOPBACK) => true,
            (true, a) if a.starts_with("0000000000000000FFFF0000") && a.ends_with("7F") => false,
            _ => continue,
        };
        out.push((inode, port, v6_only));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_listeners_on_local_addresses() {
        let text = "p101\ncnode\nf23\nn*:5173\nf24\nn[::1]:5173\np202\ncpostgres\nf5\nn127.0.0.1:5432\np303\ncsshd\nf3\nn192.168.1.4:22\np404\ncvite\nf9\nn[::1]:4000\n";
        assert_eq!(
            parse_lsof(text),
            vec![
                Listener {
                    port: 5173,
                    pid: 101,
                    name: "node".into(),
                    v6_only: false
                },
                Listener {
                    port: 5432,
                    pid: 202,
                    name: "postgres".into(),
                    v6_only: false
                },
                Listener {
                    port: 4000,
                    pid: 404,
                    name: "vite".into(),
                    v6_only: true
                },
            ]
        );
    }

    #[test]
    fn lsof_working_directories() {
        let cwds = parse_lsof_cwd("p101\nfcwd\nn/Users/me/app\np202\nfcwd\nn/tmp\n");
        assert_eq!(cwds[&101], PathBuf::from("/Users/me/app"));
        assert_eq!(cwds[&202], PathBuf::from("/tmp"));
    }

    #[test]
    fn netstat_listening_rows() {
        let text = "\nActive Connections\n\n  Proto  Local Address          Foreign Address        State           PID\n  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1044\n  TCP    127.0.0.1:5173         0.0.0.0:0              LISTENING       8812\n  TCP    10.0.0.5:139           0.0.0.0:0              LISTENING       4\n  TCP    127.0.0.1:5173         127.0.0.1:50000        ESTABLISHED     8812\n  TCP    [::1]:3000             [::]:0                 LISTENING       77\n";
        let ports: Vec<(u16, u32, bool)> = parse_netstat(text)
            .into_iter()
            .map(|l| (l.port, l.pid, l.v6_only))
            .collect();
        assert_eq!(
            ports,
            vec![(135, 1044, false), (5173, 8812, false), (3000, 77, true)]
        );
    }

    #[test]
    fn proc_net_listen_rows() {
        let v4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0\n   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 17 1 0\n   2: 0501A8C0:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 18 1 0\n   3: 0100007F:1435 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 19 1 0\n";
        assert_eq!(
            parse_proc_net(v4, false),
            vec![(4242, 5173, false), (17, 22, false)]
        );
        let v6 = "  sl  local_address remote_address st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000000000000000000001000000:0FA0 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 55 1 0\n";
        assert_eq!(parse_proc_net(v6, true), vec![(55, 4000, true)]);
    }

    #[test]
    fn urls_printed_by_dev_servers() {
        let vite = "\u{1b}[32m  VITE v5.0.0\u{1b}[39m  ready in 300 ms\n\n  ➜  Local:   \u{1b}[36mhttp://localhost:\u{1b}[1m5173\u{1b}[22m/app/\u{1b}[39m\n  ➜  Network: use --host to expose\n";
        assert_eq!(
            printed_urls(vite),
            vec![(5173, "http://localhost:5173/app/".to_string())]
        );
        let next = "- Local:        http://0.0.0.0:3000.\nSee https://nextjs.org/docs and http://example.com:80/x";
        assert_eq!(
            printed_urls(next),
            vec![(3000, "http://localhost:3000/".to_string())]
        );
        assert!(
            printed_urls("Serving HTTP on :: port 8000 (http://[::]:8000/) ...")
                .contains(&(8000, "http://localhost:8000/".to_string()))
        );
    }

    #[test]
    fn a_server_belongs_to_the_deepest_session_directory() {
        let dir = |id: &str, cwd: &str, last_active| SessionDir {
            id: id.into(),
            cwd: cwd.into(),
            last_active,
        };
        let sessions = [
            dir("root", "/w", 9),
            dir("app", "/w/app", 1),
            dir("twin", "/w/app", 5),
        ];
        assert_eq!(
            owner(&sessions, Path::new("/w/app/web")),
            Some("twin".to_string())
        );
        assert_eq!(
            owner(&sessions, Path::new("/w/lib")),
            Some("root".to_string())
        );
        assert_eq!(owner(&sessions, Path::new("/elsewhere")), None);
    }

    #[test]
    fn a_shell_command_opens_a_scan_window() {
        let servers = DevServers::default();
        assert!(servers.due(1_000), "the first scan takes the baseline");
        servers.scanning.store(false, Ordering::SeqCst);
        crate::hub::lock(&servers.state).baseline = Some(HashSet::new());
        assert!(!servers.due(2_000), "no window, no servers: no scan");
        servers.observe(
            &json!({ "type": "tool_output", "name": "read", "output": "" }),
            2_000,
        );
        assert!(!servers.due(2_000));
        servers.observe(
            &json!({ "type": "tool_output", "name": "bash", "output": "Local: http://localhost:5173/" }),
            2_000,
        );
        assert!(servers.due(4_000));
        assert!(!servers.due(4_500), "one scan at a time");
        servers.scanning.store(false, Ordering::SeqCst);
        assert!(!servers.due(5_000), "not again before FAST_MS");
        assert!(servers.due(6_000));
        assert_eq!(
            crate::hub::lock(&servers.state).printed[&5173],
            "http://localhost:5173/"
        );
    }

    /// Starts a real server in a directory and finds it. Needs python3 and
    /// a free port; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn finds_a_server_started_in_a_session_directory() {
        let dir = std::env::temp_dir().join(format!("jucode-devservers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sessions = [SessionDir {
            id: "s1".into(),
            cwd: dir.clone(),
            last_active: 1,
        }];
        let servers = DevServers::default();
        assert!(servers.due(0));
        assert_eq!(servers.scan(&sessions), None);
        let mut child = std::process::Command::new("python3")
            .args(["-m", "http.server", "58517", "--bind", "127.0.0.1"])
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let mut found = None;
        for i in 0..20 {
            std::thread::sleep(Duration::from_millis(300));
            servers.observe(
                &json!({ "type": "tool_start", "name": "bash" }),
                10_000 * (i + 1),
            );
            if servers.due(10_000 * (i + 1)) {
                if let Some(list) = servers.scan(&sessions) {
                    found = Some(list);
                    break;
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        let found = found.expect("server found");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].port, 58517);
        assert_eq!(found[0].url, "http://localhost:58517/");
        assert_eq!(found[0].session.as_deref(), Some("s1"));
    }
}
