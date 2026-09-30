//! `jucode daemon install` / `uninstall`: run the daemon as a per-user
//! service that starts at login and restarts if it exits (launchd on macOS,
//! a systemd user unit on Linux).
//!
//! A service starts with a minimal environment, so the PATH of the shell
//! that ran `install` is written into the service definition; otherwise
//! commands the agent runs (git, node, cargo…) would not be found.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

const LAUNCHD_LABEL: &str = "cn.jucode.daemon";
const SYSTEMD_UNIT: &str = "jucode-daemon.service";

pub struct ServiceSpec {
    pub program: PathBuf,
    pub listen: String,
    /// Relay flags to run with (`--relay <url>` / `--no-relay`); empty keeps
    /// the default relay URL.
    pub relay_args: Vec<String>,
    pub path_env: String,
    pub log: PathBuf,
}

pub fn install(listen: &str, relay_args: Vec<String>) -> io::Result<String> {
    let home = home()?;
    let spec = ServiceSpec {
        program: stable_program()?,
        listen: listen.to_string(),
        relay_args,
        path_env: env::var("PATH").unwrap_or_default(),
        log: home.join(".jucode").join("daemon").join("daemon.log"),
    };
    fs::create_dir_all(spec.log.parent().expect("log has a parent"))?;
    // A daemon already on the port (one Desktop started, say) would keep the
    // service from binding it.
    stop_listener(listen);
    if cfg!(target_os = "macos") {
        let plist = home
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"));
        write(&plist, &launchd_plist(&spec))?;
        // Reload so a reinstall picks up a new binary path or address.
        let _ = run("launchctl", &["unload", &plist.display().to_string()]);
        run("launchctl", &["load", "-w", &plist.display().to_string()])?;
        Ok(format!("installed {}", plist.display()))
    } else if cfg!(target_os = "linux") {
        let unit = home.join(".config/systemd/user").join(SYSTEMD_UNIT);
        write(&unit, &systemd_unit(&spec))?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT])?;
        Ok(format!("installed {}", unit.display()))
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "jucode daemon install supports macOS and Linux; run `jucode daemon` directly or under WSL2",
        ))
    }
}

pub fn uninstall() -> io::Result<String> {
    let home = home()?;
    if cfg!(target_os = "macos") {
        let plist = home
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"));
        if !plist.exists() {
            return Ok("not installed".to_string());
        }
        let _ = run("launchctl", &["unload", "-w", &plist.display().to_string()]);
        fs::remove_file(&plist)?;
        Ok(format!("removed {}", plist.display()))
    } else if cfg!(target_os = "linux") {
        let unit = home.join(".config/systemd/user").join(SYSTEMD_UNIT);
        if !unit.exists() {
            return Ok("not installed".to_string());
        }
        let _ = run("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
        fs::remove_file(&unit)?;
        run("systemctl", &["--user", "daemon-reload"])?;
        Ok(format!("removed {}", unit.display()))
    } else {
        Ok("not installed".to_string())
    }
}

pub fn launchd_plist(spec: &ServiceSpec) -> String {
    let args = [
        spec.program.display().to_string(),
        "daemon".to_string(),
        "--listen".to_string(),
        spec.listen.clone(),
    ]
    .iter()
    .chain(&spec.relay_args)
    .map(|arg| format!("    <string>{}</string>", xml_escape(arg)))
    .collect::<Vec<_>>()
    .join("\n");
    let log = xml_escape(&spec.log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args}
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        path = xml_escape(&spec.path_env),
    )
}

pub fn systemd_unit(spec: &ServiceSpec) -> String {
    format!(
        "[Unit]\n\
         Description=JuCode daemon\n\
         \n\
         [Service]\n\
         ExecStart={program} daemon --listen {listen}{relay}\n\
         Environment=\"PATH={path}\"\n\
         Restart=on-failure\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        program = systemd_quote(&spec.program.display().to_string()),
        listen = spec.listen,
        relay = spec
            .relay_args
            .iter()
            .map(|arg| format!(" {}", systemd_quote(arg)))
            .collect::<String>(),
        path = spec.path_env.replace('"', "\\\""),
        log = spec.log.display(),
    )
}

/// The program the service should run: the `jucode` on PATH when there is
/// one, else this executable. An npm install runs from a versioned path inside
/// node_modules that the next upgrade or uninstall removes; the PATH entry
/// (a link or shim) keeps pointing at whatever is installed.
fn stable_program() -> io::Result<PathBuf> {
    let name = if cfg!(windows) {
        "jucode.exe"
    } else {
        "jucode"
    };
    let on_path = env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    });
    match on_path {
        Some(path) => Ok(path),
        None => env::current_exe(),
    }
}

/// Ends the process listening on `listen` (the daemon's address), if any, and
/// waits up to 5s for the port to free up. Best effort, Unix only.
fn stop_listener(listen: &str) {
    #[cfg(unix)]
    {
        let Some(port) = listen.rsplit(':').next() else {
            return;
        };
        let Ok(out) = Command::new("lsof")
            .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
            .output()
        else {
            return;
        };
        let pids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|pid| !pid.is_empty())
            .collect();
        if pids.is_empty() {
            return;
        }
        for pid in &pids {
            let _ = Command::new("kill").args(["-TERM", pid]).status();
        }
        for _ in 0..50 {
            if std::net::TcpStream::connect(listen).is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    #[cfg(not(unix))]
    let _ = listen;
}

fn home() -> io::Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

fn write(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)
}

fn run(program: &str, args: &[&str]) -> io::Result<()> {
    let status = Command::new(program).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{program} {} failed with {status}",
            args.join(" ")
        )))
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Quotes an ExecStart path that contains spaces.
fn systemd_quote(text: &str) -> String {
    if text.contains(' ') {
        format!("\"{}\"", text.replace('"', "\\\""))
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            program: PathBuf::from("/opt/Ju Code/bin/jucode"),
            listen: "127.0.0.1:7788".to_string(),
            relay_args: Vec::new(),
            path_env: "/opt/homebrew/bin:/usr/bin".to_string(),
            log: PathBuf::from("/home/u/.jucode/daemon/daemon.log"),
        }
    }

    #[test]
    fn launchd_plist_runs_the_daemon_with_the_install_path() {
        let plist = launchd_plist(&spec());
        assert!(plist.contains("<string>/opt/Ju Code/bin/jucode</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("<string>127.0.0.1:7788</string>"));
        assert!(plist.contains("<string>/opt/homebrew/bin:/usr/bin</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        // Restarted only after a failure: a daemon that found the port taken
        // exits 0 and is left alone.
        assert!(plist.contains("<key>SuccessfulExit</key>\n    <false/>"));
    }

    #[test]
    fn systemd_unit_quotes_paths_with_spaces() {
        let unit = systemd_unit(&spec());
        assert!(
            unit.contains("ExecStart=\"/opt/Ju Code/bin/jucode\" daemon --listen 127.0.0.1:7788")
        );
        assert!(unit.contains("Environment=\"PATH=/opt/homebrew/bin:/usr/bin\""));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn relay_flags_are_passed_to_the_service() {
        let mut spec = spec();
        spec.relay_args = vec!["--relay".to_string(), "wss://relay.example/v1".to_string()];
        assert!(launchd_plist(&spec).contains("<string>wss://relay.example/v1</string>"));
        assert!(systemd_unit(&spec)
            .contains("--listen 127.0.0.1:7788 --relay wss://relay.example/v1\n"));
        spec.relay_args = vec!["--no-relay".to_string()];
        assert!(systemd_unit(&spec).contains("--listen 127.0.0.1:7788 --no-relay\n"));
    }

    #[test]
    fn xml_special_characters_are_escaped() {
        assert_eq!(xml_escape("a&b<c>"), "a&amp;b&lt;c&gt;");
    }
}
