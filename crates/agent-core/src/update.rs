use semver::Version;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

const UPDATE_COMMAND: &str = "jucode update";
/// Redirects to the newest release's tag page. Read instead of GitHub's API,
/// whose unauthenticated rate limit is shared by everyone behind one address.
const RELEASES_URL: &str = "https://github.com/JuCode-Team/JuCode-CLI/releases/latest";
const DOWNLOAD_URL: &str = "https://github.com/JuCode-Team/JuCode-CLI/releases/download";
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// A download from GitHub slower than this after `RATE_WINDOW` moves to the
/// JuCode server (GitHub is often slow or unreachable from mainland China).
const MIN_DOWNLOAD_RATE: u64 = 200 * 1024;
const RATE_WINDOW: Duration = Duration::from_secs(5);

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
    // JuCode Desktop updates the copy it manages; no notice for it.
    if install_channel() == InstallChannel::Desktop {
        return rx;
    }
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

/// One release of the CLI: its version and downloadable files.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// Hex sha256 when the source publishes one.
    pub sha256: Option<String>,
}

impl Release {
    /// This platform's binary (`jucode-<target>`, `.exe` on Windows).
    pub fn binary(&self) -> Option<&Asset> {
        let name = binary_name()?;
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// The release asset name of this platform's binary, by target triple.
fn binary_name() -> Option<String> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    };
    Some(if cfg!(windows) {
        format!("jucode-{target}.exe")
    } else {
        format!("jucode-{target}")
    })
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .build()
}

/// The newest CLI version: GitHub first, then the JuCode server.
pub fn latest_cli_version() -> Result<String, String> {
    latest_release(UPDATE_CHECK_TIMEOUT).map(|release| release.version)
}

/// The newest release: from GitHub, else from the JuCode server.
pub fn latest_release(timeout: Duration) -> Result<Release, String> {
    github_release(timeout).or_else(|github| {
        server_release(timeout).map_err(|server| format!("GitHub: {github}; JuCode: {server}"))
    })
}

fn github_release(timeout: Duration) -> Result<Release, String> {
    let response = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .redirects(0)
        .build()
        .get(RELEASES_URL)
        .set("User-Agent", "jucode-cli")
        .call()
        .map_err(|error| error.to_string())?;
    let location = response.header("location").unwrap_or_default();
    github_release_at(location)
}

/// The release a `releases/latest` redirect points at (`…/releases/tag/v1.2.3`).
/// Its binaries are named by target (`jucode-<target>`).
fn github_release_at(location: &str) -> Result<Release, String> {
    let tag = location
        .rsplit_once("/releases/tag/")
        .map(|(_, tag)| tag)
        .filter(|tag| !tag.is_empty())
        .ok_or_else(|| format!("unexpected redirect: {location:?}"))?;
    let version = tag.trim_start_matches('v').to_string();
    let assets = binary_name()
        .map(|name| Asset {
            url: format!("{DOWNLOAD_URL}/{tag}/{name}"),
            name,
            sha256: None,
        })
        .into_iter()
        .collect();
    Ok(Release { version, assets })
}

fn server_release(timeout: Duration) -> Result<Release, String> {
    let url = format!(
        "{}/v1/public/releases/cli/latest",
        crate::config::saved_jucode_api_url().trim_end_matches('/')
    );
    let value = agent(timeout)
        .get(&url)
        .set("Accept", "application/json")
        .call()
        .map_err(|error| error.to_string())?
        .into_json::<Value>()
        .map_err(|error| error.to_string())?;
    parse_server_release(&value)
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// The JuCode server's `/v1/public/releases/cli/latest`.
fn parse_server_release(value: &Value) -> Result<Release, String> {
    let version = text(&value["version"]).trim_start_matches('v').to_string();
    if version.is_empty() {
        return Err("response has no version".to_string());
    }
    let assets = value["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|asset| Asset {
            name: text(&asset["name"]).to_string(),
            url: text(&asset["url"]).to_string(),
            sha256: Some(text(&asset["sha256"]).to_string()).filter(|sha| !sha.is_empty()),
        })
        .filter(|asset| !asset.name.is_empty() && !asset.url.is_empty())
        .collect();
    Ok(Release { version, assets })
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
    /// The copy JuCode Desktop keeps in `~/.jucode/bin` — it updates with the app.
    Desktop,
    /// Anything else: GitHub release binary, cargo install, dev build.
    Other,
}

pub fn install_channel() -> InstallChannel {
    let path = std::env::current_exe()
        .map(|path| fs::canonicalize(&path).unwrap_or(path))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let desktop = crate::config::jucode_dir()
        .map(|dir| fs::canonicalize(dir.join("bin")).unwrap_or(dir.join("bin")))
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    channel_for_path(&path, &desktop)
}

fn channel_for_path(path: &str, desktop_bin: &str) -> InstallChannel {
    let path = path.replace('\\', "/");
    let desktop_bin = desktop_bin.replace('\\', "/");
    if path.contains("node_modules/@jucode/") {
        InstallChannel::Npm
    } else if !desktop_bin.is_empty() && path.starts_with(&format!("{}/", desktop_bin.trim_end_matches('/'))) {
        InstallChannel::Desktop
    } else {
        InstallChannel::Other
    }
}

/// `jucode update` for a binary installed from a release: downloads this
/// platform's binary of `release` (GitHub first, the JuCode server when
/// GitHub fails or is too slow), checks it, and puts it in place of the
/// running one. The new version runs from the next start.
pub fn self_update(release: &Release) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    let name = binary_name().ok_or("no release binary for this platform")?;
    let partial = exe.with_file_name(format!(".{name}.download"));
    let result = download_release_binary(release, &name, &partial)
        .and_then(|()| check_binary(&partial, &release.version))
        .and_then(|()| replace_binary(&partial, &exe));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result.map(|()| format!("updated to {}: restart jucode to use it", release.version))
}

fn download_release_binary(release: &Release, name: &str, dest: &Path) -> Result<(), String> {
    let from_github = release
        .assets
        .iter()
        .find(|asset| asset.name == name && asset.url.starts_with("https://github.com/"));
    let mut errors = Vec::new();
    if let Some(asset) = from_github {
        match download(asset, dest, true) {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(format!("GitHub: {error}")),
        }
    }
    let server = match from_github {
        // Already from the server (GitHub was unreachable for the version check).
        None => release.binary().cloned(),
        Some(_) => server_release(Duration::from_secs(10))
            .ok()
            .filter(|server| server.version == release.version)
            .and_then(|server| server.binary().cloned()),
    };
    match server {
        Some(asset) => download(&asset, dest, false).map_err(|error| {
            errors.push(format!("JuCode: {error}"));
            errors.join("; ")
        }),
        None => {
            errors.push(format!("no {name} for {}", release.version));
            Err(errors.join("; "))
        }
    }
}

/// Streams `asset` to `dest`, checking its sha256 when known. With
/// `need_rate`, gives up when it runs slower than `MIN_DOWNLOAD_RATE`.
fn download(asset: &Asset, dest: &Path, need_rate: bool) -> Result<(), String> {
    let response = agent(Duration::from_secs(15))
        .get(&asset.url)
        .set("User-Agent", "jucode-cli")
        .call()
        .map_err(|error| error.to_string())?;
    let mut reader = response.into_reader();
    let mut file = fs::File::create(dest).map_err(|error| format!("{}: {error}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let started = Instant::now();
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).map_err(|error| error.to_string())?;
        hasher.update(&buffer[..read]);
        total += read as u64;
        let elapsed = started.elapsed();
        if need_rate && elapsed >= RATE_WINDOW && total < MIN_DOWNLOAD_RATE * elapsed.as_secs() {
            return Err(format!("too slow ({} KB in {}s)", total / 1024, elapsed.as_secs()));
        }
    }
    file.sync_all().map_err(|error| error.to_string())?;
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    match &asset.sha256 {
        Some(expected) if !expected.eq_ignore_ascii_case(&digest) => {
            Err(format!("checksum mismatch for {}", asset.name))
        }
        _ => Ok(()),
    }
}

/// The downloaded binary runs and reports the expected version.
fn check_binary(path: &Path, version: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|error| error.to_string())?;
    }
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|error| format!("the downloaded binary does not run: {error}"))?;
    let reported = String::from_utf8_lossy(&output.stdout);
    if reported.split_whitespace().last() == Some(version) {
        Ok(())
    } else {
        Err(format!("the downloaded binary reports {}", reported.trim()))
    }
}

/// Puts `new` where `exe` is. Windows keeps a running executable locked but
/// lets it be renamed, so the old one moves aside first.
fn replace_binary(new: &Path, exe: &Path) -> Result<(), String> {
    let denied = |error: std::io::Error| {
        format!(
            "cannot replace {} ({error}); download it from {RELEASES_URL}",
            exe.display()
        )
    };
    if cfg!(windows) {
        let old: PathBuf = exe.with_extension("old.exe");
        let _ = fs::remove_file(&old);
        fs::rename(exe, &old).map_err(denied)?;
        if let Err(error) = fs::rename(new, exe) {
            let _ = fs::rename(&old, exe);
            return Err(denied(error));
        }
        Ok(())
    } else {
        fs::rename(new, exe).map_err(denied)
    }
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
    fn detects_install_channels_across_platforms() {
        let desktop = "/home/x/.jucode/bin";
        assert_eq!(
            channel_for_path("/usr/local/lib/node_modules/@jucode/cli-darwin-arm64/bin/jucode", desktop),
            InstallChannel::Npm
        );
        assert_eq!(
            channel_for_path(
                "C:\\Users\\x\\AppData\\Roaming\\npm\\node_modules\\@jucode\\cli-win32-x64\\bin\\jucode.exe",
                desktop
            ),
            InstallChannel::Npm
        );
        assert_eq!(channel_for_path("/home/x/.jucode/bin/jucode", desktop), InstallChannel::Desktop);
        assert_eq!(
            channel_for_path("C:\\Users\\x\\.jucode\\bin\\jucode.exe", "C:\\Users\\x\\.jucode\\bin"),
            InstallChannel::Desktop
        );
        assert_eq!(channel_for_path("/home/x/.jucode/binaries/jucode", desktop), InstallChannel::Other);
        assert_eq!(channel_for_path("/home/x/bin/jucode", desktop), InstallChannel::Other);
        assert_eq!(channel_for_path("/repo/target/debug/jucode", desktop), InstallChannel::Other);
    }

    #[test]
    fn reads_releases_from_github_and_the_jucode_server() {
        let github = github_release_at("https://github.com/JuCode-Team/JuCode-CLI/releases/tag/v0.4.0").unwrap();
        assert_eq!(github.version, "0.4.0");
        let binary = github.binary().unwrap();
        assert!(binary.url.starts_with("https://github.com/JuCode-Team/JuCode-CLI/releases/download/v0.4.0/jucode-"));
        assert_eq!(binary.sha256, None);
        assert!(github_release_at("https://github.com/JuCode-Team/JuCode-CLI/releases").is_err());
        let server = parse_server_release(&serde_json::json!({
            "version": "0.4.0",
            "assets": [{ "name": "jucode-aarch64-apple-darwin", "url": "https://api.jucode.net/v1/public/releases/cli/0.4.0/files/jucode-aarch64-apple-darwin", "sha256": "cd34" }],
        }))
        .unwrap();
        assert_eq!(server.assets[0].sha256.as_deref(), Some("cd34"));
        assert!(parse_server_release(&serde_json::json!({ "assets": [] })).is_err());
    }

    /// Serves `body` once over HTTP on a loopback port; returns its URL.
    fn serve_once(body: &'static [u8]) -> String {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/jucode", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });
        url
    }

    #[test]
    fn a_download_is_checked_against_its_checksum() {
        let dir = std::env::temp_dir().join(format!("jucode-update-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("jucode");
        // sha256("binary")
        let good = "9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd";
        let asset = |url: String, sha: &str| Asset { name: "jucode".into(), url, sha256: Some(sha.into()) };
        download(&asset(serve_once(b"binary"), good), &dest, false).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"binary");
        let error = download(&asset(serve_once(b"tampered"), good), &dest, false).unwrap_err();
        assert!(error.contains("checksum mismatch"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }
}
