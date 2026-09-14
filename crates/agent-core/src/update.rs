use semver::Version;
use serde_json::Value;
use std::{
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

const NPM_LATEST_URL: &str = "https://registry.npmjs.org/@jucode%2Fcli/latest";
const UPDATE_COMMAND: &str = "jucode update";
const RELEASES_URL: &str = "https://github.com/JuCode-Team/JuCode-CLI/releases/latest";
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct UpdateNotice {
    pub current_version: String,
    pub latest_version: String,
}

impl UpdateNotice {
    pub fn message(&self) -> String {
        format!(
            "update available: JuCode {} -> {}, run {}",
            self.current_version, self.latest_version, UPDATE_COMMAND
        )
    }
}

pub fn spawn_update_check(current_version: &'static str) -> Receiver<UpdateNotice> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok(Some(notice)) = check_for_update(current_version) {
            let _ = tx.send(notice);
        }
    });
    rx
}

fn check_for_update(current_version: &str) -> Result<Option<UpdateNotice>, String> {
    let latest_version = latest_cli_version()?;
    if !is_newer_version(current_version, &latest_version) {
        return Ok(None);
    }
    Ok(Some(UpdateNotice {
        current_version: current_version.to_string(),
        latest_version,
    }))
}

/// Fetches the latest published `@jucode/cli` version from the npm registry.
/// Note this always queries registry.npmjs.org, not a user's configured mirror.
pub fn latest_cli_version() -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(UPDATE_CHECK_TIMEOUT)
        .timeout_read(UPDATE_CHECK_TIMEOUT)
        .build();
    let value = agent
        .get(NPM_LATEST_URL)
        .set("Accept", "application/json")
        .call()
        .map_err(|error| error.to_string())?
        .into_json::<Value>()
        .map_err(|error| error.to_string())?;
    value
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "registry response missing version".to_string())
}

pub fn is_newer_version(current_version: &str, latest_version: &str) -> bool {
    let Ok(current) = Version::parse(current_version.trim_start_matches('v')) else {
        return false;
    };
    let Ok(latest) = Version::parse(latest_version.trim_start_matches('v')) else {
        return false;
    };
    latest > current
}

/// How the running binary was installed, detected from its own path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallChannel {
    /// Inside an npm global `node_modules/@jucode/` tree — `npm i -g` manages it.
    Npm,
    /// Anything else: GitHub release binary, cargo install, dev build.
    Other,
}

pub fn install_channel() -> InstallChannel {
    let path = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    channel_for_path(&path)
}

fn channel_for_path(path: &str) -> InstallChannel {
    if path.replace('\\', "/").contains("node_modules/@jucode/") {
        InstallChannel::Npm
    } else {
        InstallChannel::Other
    }
}

/// Guidance shown by `jucode update` for binaries not managed by npm.
pub fn non_npm_update_hint() -> String {
    let exe = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    format!(
        "this jucode binary was not installed via npm ({exe})\n\
         download the latest release: {RELEASES_URL}\n\
         or reinstall via npm: npm i -g @jucode/cli@latest"
    )
}

/// Runs `npm i -g @jucode/cli@latest` for an npm-installed binary.
///
/// On Unix the foreground npm replaces the package files while this process
/// keeps running; the new version takes effect on the next launch. On Windows
/// the running executable is locked, so a detached helper waits for this
/// process to exit before running npm — its result is not visible here.
pub fn run_npm_update() -> Result<String, String> {
    #[cfg(windows)]
    {
        use std::{os::windows::process::CommandExt, process::Stdio};
        // DETACHED_PROCESS | CREATE_NO_WINDOW
        const FLAGS: u32 = 0x00000008 | 0x08000000;
        Command::new("cmd")
            .args([
                "/C",
                "timeout /t 2 /nobreak >nul && npm i -g @jucode/cli@latest",
            ])
            .creation_flags(FLAGS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("failed to start the updater: {error}"))?;
        Ok("update scheduled: npm i -g runs after jucode exits; check `jucode --version` in a few seconds".to_string())
    }
    #[cfg(not(windows))]
    {
        let status = Command::new("npm")
            .args(["i", "-g", "@jucode/cli@latest"])
            .status()
            .map_err(|error| format!("failed to run npm (is it on PATH?): {error}"))?;
        if status.success() {
            Ok("updated: restart jucode to use the new version".to_string())
        } else {
            Err(format!("npm i -g failed ({status})"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_semver_versions() {
        assert!(is_newer_version("0.1.9", "0.1.10"));
        assert!(is_newer_version("v0.1.9", "v0.2.0"));
        assert!(!is_newer_version("0.1.10", "0.1.9"));
        assert!(!is_newer_version("0.1.10", "0.1.10"));
        assert!(!is_newer_version("0.1.10", "not-a-version"));
    }

    #[test]
    fn notice_points_at_jucode_update() {
        let notice = UpdateNotice {
            current_version: "0.1.3".to_string(),
            latest_version: "0.1.4".to_string(),
        };
        assert!(notice.message().contains("jucode update"));
    }

    #[test]
    fn detects_npm_install_paths_across_platforms() {
        assert_eq!(
            channel_for_path("/usr/local/lib/node_modules/@jucode/cli-darwin-arm64/bin/jucode"),
            InstallChannel::Npm
        );
        assert_eq!(
            channel_for_path(
                "C:\\Users\\x\\AppData\\Roaming\\npm\\node_modules\\@jucode\\cli-win32-x64\\bin\\jucode.exe"
            ),
            InstallChannel::Npm
        );
        assert_eq!(
            channel_for_path("/home/x/bin/jucode"),
            InstallChannel::Other
        );
        assert_eq!(
            channel_for_path("/repo/target/debug/jucode"),
            InstallChannel::Other
        );
    }
}
