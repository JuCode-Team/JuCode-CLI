//! The ACP agents JuCode starts itself, by id (`options.agent`): the daemon
//! finds the program and builds its command line, so a client never names
//! one. Approval modes and the JuCode gateway reach the agent only through
//! its launch flags, environment and a config directory of JuCode's own;
//! the user's own agent config is never written.

use super::{find_program, home, Options};
use jucode_agent_core::ModelConfig;
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// pi-acp, the community adapter Pi runs behind (pinned: tested version).
const PI_ACP: &str = "pi-acp@0.0.34";

pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    /// The program whose version decides whether it can run.
    program: &'static str,
    /// Other install directories it is found in.
    extra: &'static [&'static str],
    pub min_version: Option<&'static str>,
    /// It asks before running tools (else it never asks).
    pub approvals: bool,
    /// It can run on the JuCode gateway.
    pub gateway: bool,
    pub install: &'static str,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "opencode",
        name: "OpenCode",
        program: "opencode",
        extra: &[".opencode/bin/opencode"],
        min_version: Some("2.0.0"),
        approvals: true,
        gateway: false,
        install: "curl -fsSL https://opencode.ai/v2/install | bash",
    },
    Preset {
        id: "omp",
        name: "oh-my-pi",
        program: "omp",
        extra: &[".bun/bin/omp"],
        min_version: None,
        approvals: true,
        gateway: true,
        install: "bun install -g @oh-my-pi/pi-coding-agent",
    },
    Preset {
        id: "pi",
        name: "Pi",
        program: "pi",
        extra: &[],
        min_version: Some("0.81.0"),
        approvals: false,
        gateway: true,
        install: "npm install -g @earendil-works/pi-coding-agent",
    },
];

pub fn find(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// What a client's approval mode (jucode or desktop names) launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Ask,
    Edits,
    All,
}

impl Mode {
    pub fn of(name: Option<&str>) -> Self {
        match name.unwrap_or_default() {
            "auto-edit" => Mode::Edits,
            "full-access" | "full-auto" | "bypassPermissions" => Mode::All,
            _ => Mode::Ask,
        }
    }

    /// The engine name clients get in `approval_mode`.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Ask => "read-only",
            Mode::Edits => "auto-edit",
            Mode::All => "full-auto",
        }
    }
}

/// The program to start and the one that has the version: the same,
/// except for Pi, which runs behind pi-acp from npx. None: not installed.
fn locate(preset: &Preset) -> Option<(PathBuf, PathBuf)> {
    let found = |name: &str, extra: &[&str]| {
        let extra: Vec<PathBuf> = extra.iter().map(|path| home().join(path)).collect();
        Some(find_program(name, &extra)).filter(|path| path.is_file())
    };
    let program = found(preset.program, preset.extra)?;
    let launcher = match preset.id {
        "pi" => found("npx", &[])?,
        _ => program.clone(),
    };
    Some((launcher, program))
}

/// The daemon's PATH behind the programs' own directories, so
/// `#!/usr/bin/env bun|node` finds the interpreter installed beside them.
fn search_path(programs: &[&Path]) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in programs.iter().filter_map(|program| program.parent()) {
        if !dirs.iter().any(|known| known == dir) {
            dirs.push(dir.to_path_buf());
        }
    }
    let rest = std::env::var_os("PATH").unwrap_or_default();
    let paths = dirs.into_iter().chain(std::env::split_paths(&rest));
    std::env::join_paths(paths).unwrap_or(rest)
}

/// `program --version`, read as `x.y.z`; None when it does not answer in
/// time or prints no version.
fn version(program: &Path, path: &OsString) -> Option<String> {
    let mut child = Command::new(program)
        .arg("--version")
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    parse_version(&out)
}

/// The first `x.y[.z]` in a version line (`v2.0.20`, `omp/18.1.21`).
fn parse_version(text: &str) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|part| part.trim_matches('.'))
        .find(|part| part.contains('.') && part.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_string)
}

fn older(version: &str, min: &str) -> bool {
    let parts = |text: &str| {
        let mut parts = [0u64; 3];
        for (slot, part) in parts.iter_mut().zip(text.split('.')) {
            *slot = part.parse().unwrap_or(0);
        }
        parts
    };
    parts(version) < parts(min)
}

/// The preset's entry for `acp_presets`: whether it can start here.
fn status(preset: &Preset) -> Value {
    let found = locate(preset);
    let version = found.as_ref().and_then(|(launcher, program)| {
        version(
            program,
            &search_path(&[launcher.as_path(), program.as_path()]),
        )
    });
    let status = match (&found, &version, preset.min_version) {
        (None, _, _) => "missing",
        (Some(_), Some(version), Some(min)) if older(version, min) => "unsupported",
        _ => "ready",
    };
    json!({
        "id": preset.id,
        "name": preset.name,
        "status": status,
        "path": found.map(|(_, program)| program.display().to_string()),
        "version": version,
        "min_version": preset.min_version,
        "approvals": preset.approvals,
        "gateway": preset.gateway,
        "install": preset.install,
        "tui": true,
    })
}

/// `acp_presets`: every preset, its programs probed in parallel.
pub fn presets_json() -> Value {
    let probes: Vec<_> = PRESETS
        .iter()
        .map(|preset| thread::spawn(move || status(preset)))
        .collect();
    let presets: Vec<Value> = probes
        .into_iter()
        .filter_map(|probe| probe.join().ok())
        .collect();
    json!({ "type": "acp_presets", "presets": presets })
}

/// The preset's launcher and program, and the `PATH` to run them with;
/// an error when it is missing or too old.
fn installed(preset: &Preset) -> Result<(PathBuf, PathBuf, OsString), String> {
    let (launcher, program) = locate(preset)
        .ok_or_else(|| format!("{} is not installed: {}", preset.name, preset.install))?;
    let path = search_path(&[launcher.as_path(), program.as_path()]);
    if let Some(min) = preset.min_version {
        if let Some(version) = version(&program, &path).filter(|v| older(v, min)) {
            return Err(format!(
                "{} {version} is too old (needs {min})",
                preset.name
            ));
        }
    }
    Ok((launcher, program, path))
}

/// The preset's command line in `options.approval_mode`.
pub fn command(preset: &Preset, options: &Options) -> Result<Command, String> {
    let (launcher, program, path) = installed(preset)?;
    let mut command = launch(
        preset,
        &launcher,
        &program,
        Mode::of(options.approval_mode.as_deref()),
    );
    command.env("PATH", path);
    Ok(command)
}

/// The preset's own TUI resuming `conversation` (the ACP session id, which
/// is the TUI's too) in `options.approval_mode`.
pub fn tui(preset: &Preset, options: &Options, conversation: &str) -> Result<Command, String> {
    let (_, program, path) = installed(preset)?;
    let mut command = tui_launch(
        preset,
        &program,
        Mode::of(options.approval_mode.as_deref()),
        conversation,
    );
    command.env("PATH", path);
    Ok(command)
}

fn launch(preset: &Preset, launcher: &Path, program: &Path, mode: Mode) -> Command {
    let mut command = Command::new(launcher);
    match preset.id {
        "opencode" => {
            command.arg("acp");
            opencode_permission(&mut command, mode);
        }
        "omp" => {
            command.args(["acp", "--approval-mode", omp_mode(mode)]);
        }
        _ => {
            command
                .args(["-y", PI_ACP])
                .env("PI_ACP_PI_COMMAND", program);
        }
    }
    command
}

fn tui_launch(preset: &Preset, program: &Path, mode: Mode, conversation: &str) -> Command {
    let mut command = Command::new(program);
    match preset.id {
        // `--standalone`: the TUI runs its own server, which sees the
        // environment, instead of attaching to a running one.
        "opencode" => {
            command.args(["--standalone", "--session", conversation]);
            opencode_permission(&mut command, mode);
        }
        "omp" => {
            command.args(["--approval-mode", omp_mode(mode), "--resume", conversation]);
        }
        _ => {
            command.args(["--session", conversation]);
        }
    }
    command
}

/// Its own permission rules would let OpenCode edit and run without asking.
fn opencode_permission(command: &mut Command, mode: Mode) {
    let permission = match mode {
        Mode::Ask => Some(r#"{"permission":{"edit":"ask","bash":"ask"}}"#),
        Mode::Edits => Some(r#"{"permission":{"edit":"allow","bash":"ask"}}"#),
        Mode::All => None,
    };
    if let Some(permission) = permission {
        command.env("OPENCODE_CONFIG_CONTENT", permission);
    }
}

/// oh-my-pi's approval mode (its default is yolo).
fn omp_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "always-ask",
        Mode::Edits => "write",
        Mode::All => "yolo",
    }
}

/// Runs a gateway preset on the JuCode gateway: its agent directory is
/// JuCode's own (`~/.jucode/daemon/acp/<id>`), holding the JuCode models
/// with the local `key` read from the environment; its sessions stay where
/// the user's own setup keeps them.
pub fn use_gateway(
    command: &mut Command,
    preset: &Preset,
    cwd: &Path,
    base: &str,
    key: &str,
    model: Option<&str>,
) -> Result<(), String> {
    // Sessions starting at once share the directory (and the temp file
    // `write_private` names by process).
    static FILES: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _files = FILES.lock().unwrap_or_else(|e| e.into_inner());
    configure(
        command,
        preset,
        &home(),
        cwd,
        base,
        key,
        &jucode_agent_core::jucode_visible_models(),
        model,
    )
}

#[allow(clippy::too_many_arguments)]
fn configure(
    command: &mut Command,
    preset: &Preset,
    home: &Path,
    cwd: &Path,
    base: &str,
    key: &str,
    models: &[ModelConfig],
    model: Option<&str>,
) -> Result<(), String> {
    // A model as the agent named it (`jucode/<model>`, kept from an earlier
    // open), by its JuCode name.
    let model = model.map(|m| m.split_once('/').map_or(m, |(_, name)| name));
    let dir = home
        .join(".jucode")
        .join("daemon")
        .join("acp")
        .join(preset.id);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let user_dir = match preset.id {
        "omp" => home.join(".omp").join("agent"),
        _ => home.join(".pi").join("agent"),
    };
    for (name, content) in gateway_files(preset, base, models, model) {
        let path = dir.join(name);
        crate::store::write_private(&path, format!("{content:#}\n").as_bytes())
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    if preset.id == "omp" {
        if let Some(model) = model {
            let provider = match model.starts_with("claude-") {
                true => "jucode-claude",
                false => "jucode",
            };
            command.args(["--model", &format!("{provider}/{model}")]);
        }
    }
    command
        .env("JUCODE_GATEWAY_TOKEN", key)
        .env("PI_CODING_AGENT_DIR", &dir);
    if preset.id == "omp" {
        // oh-my-pi writes to `PI_CODING_AGENT_SESSION_DIR` but looks sessions
        // up for `session/load` only under its agent directory: that
        // directory's `sessions` is the user's own.
        let sessions = user_dir.join("sessions");
        let link = dir.join("sessions");
        if std::fs::read_link(&link).ok() != Some(sessions.clone()) {
            std::fs::create_dir_all(&sessions)
                .map_err(|error| format!("cannot create {}: {error}", sessions.display()))?;
            let _ = std::fs::remove_file(&link);
            symlink_dir(&sessions, &link)
                .map_err(|error| format!("cannot link {}: {error}", link.display()))?;
        }
    } else {
        command.env(
            "PI_CODING_AGENT_SESSION_DIR",
            user_dir.join("sessions").join(session_dir_name(cwd)),
        );
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

/// The per-directory session folder name Pi uses:
/// `--<path without the leading "/", separators as "-">--`.
fn session_dir_name(cwd: &Path) -> String {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let cwd = cwd.to_string_lossy();
    let name: String = cwd
        .trim_start_matches('/')
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{name}--")
}

/// The files of the preset's JuCode agent directory. Claude models go over
/// Anthropic Messages, the rest over Responses. No secret: the key is an
/// environment variable's name.
fn gateway_files(
    preset: &Preset,
    base: &str,
    models: &[ModelConfig],
    model: Option<&str>,
) -> Vec<(&'static str, Value)> {
    let entry = |m: &ModelConfig| {
        let mut entry = json!({
            "id": m.name,
            "name": m.display_name.as_deref().unwrap_or(&m.name),
            "reasoning": !m.reasoning_efforts.is_empty(),
            "input": ["text", "image"],
        });
        if m.context_window > 0 {
            entry["contextWindow"] = json!(m.context_window);
        }
        if m.max_output_tokens > 0 {
            entry["maxTokens"] = json!(m.max_output_tokens);
        }
        entry
    };
    let claude = |m: &&ModelConfig| m.name.starts_with("claude-");
    if preset.id == "omp" {
        let provider = |api: &str, base: String, models: Vec<Value>| json!({ "baseUrl": base, "api": api, "apiKey": "JUCODE_GATEWAY_TOKEN", "models": models });
        let responses = models.iter().filter(|m| !claude(m)).map(entry).collect();
        let messages = models.iter().filter(claude).map(entry).collect();
        let providers = json!({
            "jucode": provider("openai-responses", format!("{base}/v1"), responses),
            "jucode-claude": provider("anthropic-messages", base.to_string(), messages),
        });
        // JSON is YAML.
        return vec![("models.yml", json!({ "providers": providers }))];
    }
    let pi_models: Vec<Value> = models
        .iter()
        .map(|m| {
            let mut entry = entry(m);
            if claude(&m) {
                entry["api"] = json!("anthropic-messages");
                entry["baseUrl"] = json!(base);
            }
            entry
        })
        .collect();
    let default = model
        .map(str::to_string)
        .or_else(|| models.first().map(|m| m.name.clone()));
    vec![
        (
            "models.json",
            json!({ "providers": { "jucode": {
                "name": "JuCode", "baseUrl": format!("{base}/v1"), "api": "openai-responses",
                "apiKey": "$JUCODE_GATEWAY_TOKEN", "models": pi_models,
            } } }),
        ),
        (
            "settings.json",
            json!({ "defaultProvider": "jucode", "defaultModel": default, "quietStartup": true }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect()
    }

    fn env(command: &Command, name: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().to_string()))
    }

    #[test]
    fn approval_modes_launch_each_agent_asking_as_the_user_chose() {
        let (bin, pi) = (Path::new("/bin/agent"), Path::new("/bin/pi"));
        assert_eq!(Mode::of(Some("manual")), Mode::Ask);
        assert_eq!(Mode::of(Some("plan")), Mode::Ask);
        assert_eq!(Mode::of(None), Mode::Ask);
        assert_eq!(Mode::of(Some("auto-edit")), Mode::Edits);
        assert_eq!(Mode::of(Some("bypassPermissions")), Mode::All);

        let omp = find("omp").unwrap();
        for (mode, flag) in [
            (Mode::Ask, "always-ask"),
            (Mode::Edits, "write"),
            (Mode::All, "yolo"),
        ] {
            assert_eq!(
                args(&launch(omp, bin, bin, mode)),
                ["acp", "--approval-mode", flag]
            );
        }
        let opencode = find("opencode").unwrap();
        let ask = launch(opencode, bin, bin, Mode::Ask);
        assert_eq!(args(&ask), ["acp"]);
        assert_eq!(
            env(&ask, "OPENCODE_CONFIG_CONTENT").unwrap(),
            r#"{"permission":{"edit":"ask","bash":"ask"}}"#
        );
        let edits = launch(opencode, bin, bin, Mode::Edits);
        assert!(env(&edits, "OPENCODE_CONFIG_CONTENT")
            .unwrap()
            .contains(r#""edit":"allow""#));
        let all = launch(opencode, bin, bin, Mode::All);
        assert_eq!(env(&all, "OPENCODE_CONFIG_CONTENT"), None);

        let preset = find("pi").unwrap();
        let pi_command = launch(preset, Path::new("/bin/npx"), pi, Mode::All);
        assert_eq!(pi_command.get_program(), "/bin/npx");
        assert_eq!(args(&pi_command), ["-y", PI_ACP]);
        assert_eq!(env(&pi_command, "PI_ACP_PI_COMMAND").unwrap(), "/bin/pi");
        assert!(!preset.approvals);
    }

    #[test]
    fn each_agents_tui_resumes_the_conversation_asking_as_the_user_chose() {
        let bin = Path::new("/bin/agent");
        let opencode = find("opencode").unwrap();
        for (mode, permission) in [
            (
                Mode::Ask,
                Some(r#"{"permission":{"edit":"ask","bash":"ask"}}"#),
            ),
            (
                Mode::Edits,
                Some(r#"{"permission":{"edit":"allow","bash":"ask"}}"#),
            ),
            (Mode::All, None),
        ] {
            let tui = tui_launch(opencode, bin, mode, "ses_1");
            assert_eq!(tui.get_program(), "/bin/agent");
            assert_eq!(args(&tui), ["--standalone", "--session", "ses_1"]);
            assert_eq!(env(&tui, "OPENCODE_CONFIG_CONTENT").as_deref(), permission);
        }
        let omp = find("omp").unwrap();
        for (mode, flag) in [
            (Mode::Ask, "always-ask"),
            (Mode::Edits, "write"),
            (Mode::All, "yolo"),
        ] {
            assert_eq!(
                args(&tui_launch(omp, bin, mode, "c-1")),
                ["--approval-mode", flag, "--resume", "c-1"]
            );
        }
        // The pi program itself, not pi-acp.
        let pi = tui_launch(find("pi").unwrap(), Path::new("/bin/pi"), Mode::Ask, "c-2");
        assert_eq!(pi.get_program(), "/bin/pi");
        assert_eq!(args(&pi), ["--session", "c-2"]);
        assert_eq!(env(&pi, "PI_ACP_PI_COMMAND"), None);
    }

    #[test]
    fn a_tui_on_the_gateway_gets_the_agent_dir_its_engine_has() {
        let home = std::env::temp_dir().join(format!("jucode-acp-tui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        for (id, program) in [("omp", "/o"), ("pi", "/p")] {
            let preset = find(id).unwrap();
            let mut tui = tui_launch(preset, Path::new(program), Mode::Ask, "c-3");
            configure(
                &mut tui,
                preset,
                &home,
                Path::new("/no/such/dir"),
                "http://gw",
                "k-3",
                &models(),
                Some("gpt-6"),
            )
            .unwrap();
            assert_eq!(env(&tui, "JUCODE_GATEWAY_TOKEN").unwrap(), "k-3");
            assert_eq!(
                env(&tui, "PI_CODING_AGENT_DIR").unwrap(),
                home.join(".jucode/daemon/acp")
                    .join(id)
                    .display()
                    .to_string()
            );
        }
        let omp = find("omp").unwrap();
        let mut tui = tui_launch(omp, Path::new("/o"), Mode::All, "c-3");
        configure(
            &mut tui,
            omp,
            &home,
            Path::new("/no/such/dir"),
            "http://gw",
            "k-3",
            &models(),
            Some("gpt-6"),
        )
        .unwrap();
        assert_eq!(
            args(&tui),
            [
                "--approval-mode",
                "yolo",
                "--resume",
                "c-3",
                "--model",
                "jucode/gpt-6"
            ]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn every_preset_entry_says_it_has_a_tui() {
        let presets = presets_json();
        let entries = presets["presets"].as_array().unwrap();
        assert_eq!(entries.len(), PRESETS.len());
        for entry in entries {
            assert_eq!(entry["tui"], true);
            assert!(entry["status"].is_string());
        }
    }

    #[test]
    fn versions_are_read_from_each_agents_output_and_checked() {
        assert_eq!(parse_version("v2.0.20\n").as_deref(), Some("2.0.20"));
        assert_eq!(parse_version("omp/18.1.21").as_deref(), Some("18.1.21"));
        assert_eq!(parse_version("1.2.0").as_deref(), Some("1.2.0"));
        assert_eq!(parse_version("no version"), None);
        let opencode = find("opencode").unwrap().min_version.unwrap();
        assert!(older("1.14.3", opencode));
        assert!(!older("2.0.20", opencode));
        let pi = find("pi").unwrap().min_version.unwrap();
        assert!(older("0.80.9", pi));
        assert!(!older("0.81.0", pi));
        assert!(!older("1.2.0", pi));
        assert_eq!(find("deepseek").map(|p| p.id), None);
    }

    #[test]
    fn session_folders_are_named_as_the_agents_name_them() {
        assert_eq!(
            session_dir_name(Path::new("/no/such/dir")),
            "--no-such-dir--"
        );
    }

    fn models() -> Vec<ModelConfig> {
        let model = |name: &str, efforts: &[&str], window: u64| ModelConfig {
            name: name.to_string(),
            context_window: window,
            max_context_window: 0,
            max_output_tokens: 0,
            reasoning_efforts: efforts.iter().map(|e| e.to_string()).collect(),
            input_cost: 0.0,
            cached_input_cost: 0.0,
            output_cost: 0.0,
            display_name: None,
            group_windows: Default::default(),
        };
        vec![
            model("gpt-6", &["low", "high"], 400_000),
            model("claude-sonnet-5", &[], 0),
        ]
    }

    #[test]
    fn the_gateway_gives_pi_its_own_agent_dir_with_jucode_models() {
        let home = std::env::temp_dir().join(format!("jucode-acp-pi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let preset = find("pi").unwrap();
        let mut command = launch(preset, Path::new("/n"), Path::new("/p"), Mode::Ask);
        configure(
            &mut command,
            preset,
            &home,
            Path::new("/no/such/dir"),
            "http://127.0.0.1:9/gw",
            "k-1",
            &models(),
            None,
        )
        .unwrap();
        let dir = home.join(".jucode/daemon/acp/pi");
        assert_eq!(env(&command, "JUCODE_GATEWAY_TOKEN").unwrap(), "k-1");
        assert_eq!(
            env(&command, "PI_CODING_AGENT_DIR").unwrap(),
            dir.display().to_string()
        );
        assert_eq!(
            env(&command, "PI_CODING_AGENT_SESSION_DIR").unwrap(),
            home.join(".pi/agent/sessions/--no-such-dir--")
                .display()
                .to_string()
        );
        let read = |name: &str| -> Value {
            serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
        };
        let provider = &read("models.json")["providers"]["jucode"];
        assert_eq!(provider["baseUrl"], "http://127.0.0.1:9/gw/v1");
        assert_eq!(provider["api"], "openai-responses");
        assert_eq!(provider["apiKey"], "$JUCODE_GATEWAY_TOKEN");
        assert_eq!(
            provider["models"][0],
            json!({ "id": "gpt-6", "name": "gpt-6", "reasoning": true, "input": ["text", "image"], "contextWindow": 400_000 })
        );
        assert_eq!(provider["models"][1]["api"], "anthropic-messages");
        assert_eq!(provider["models"][1]["baseUrl"], "http://127.0.0.1:9/gw");
        assert!(provider["models"][1].get("contextWindow").is_none());
        assert_eq!(
            read("settings.json"),
            json!({ "defaultProvider": "jucode", "defaultModel": "gpt-6", "quietStartup": true })
        );
        assert!(!std::fs::read_to_string(dir.join("models.json"))
            .unwrap()
            .contains("k-1"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_gateway_gives_omp_two_providers_and_the_chosen_model() {
        let home = std::env::temp_dir().join(format!("jucode-acp-omp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let preset = find("omp").unwrap();
        let mut command = launch(preset, Path::new("/o"), Path::new("/o"), Mode::Ask);
        configure(
            &mut command,
            preset,
            &home,
            Path::new("/no/such/dir"),
            "http://gw",
            "k-2",
            &models(),
            Some("jucode-claude/claude-sonnet-5"),
        )
        .unwrap();
        assert_eq!(
            args(&command),
            [
                "acp",
                "--approval-mode",
                "always-ask",
                "--model",
                "jucode-claude/claude-sonnet-5"
            ]
        );
        assert_eq!(env(&command, "PI_CODING_AGENT_SESSION_DIR"), None);
        let sessions = home.join(".jucode/daemon/acp/omp/sessions");
        assert_eq!(
            std::fs::read_link(&sessions).unwrap(),
            home.join(".omp/agent/sessions")
        );
        let text = std::fs::read_to_string(home.join(".jucode/daemon/acp/omp/models.yml")).unwrap();
        let providers = &serde_json::from_str::<Value>(&text).unwrap()["providers"];
        assert_eq!(providers["jucode"]["baseUrl"], "http://gw/v1");
        assert_eq!(providers["jucode"]["apiKey"], "JUCODE_GATEWAY_TOKEN");
        assert_eq!(providers["jucode"]["models"][0]["id"], "gpt-6");
        assert_eq!(providers["jucode-claude"]["api"], "anthropic-messages");
        assert_eq!(providers["jucode-claude"]["baseUrl"], "http://gw");
        assert_eq!(
            providers["jucode-claude"]["models"][0]["id"],
            "claude-sonnet-5"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
