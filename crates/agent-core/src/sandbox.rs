//! OS sandbox for shell commands, modelled on Codex: the sandbox sets what a
//! command can touch, the approval mode decides what happens when a command
//! asks to run outside it (`escalate`).
//!
//! - `read-only`: commands can read, not write.
//! - `workspace-write`: the working directory, the agent's read-write
//!   directories, temp and package-cache directories are writable. Inside
//!   them `.git`, `.jucode` and `.agents` stay read-only.
//! - `full-access`: no sandbox.
//!
//! Credentials (`~/.ssh`, `~/.gnupg`, `~/.aws`, JuCode's own auth and daemon
//! state) are unreadable in every sandboxed mode. macOS uses Seatbelt
//! (`sandbox-exec`), Linux `bwrap`; Windows supports `full-access` only.
//! File tools run in-process and check writes against the same rules.

use serde_json::{json, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl SandboxMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "full-access" => Ok(Self::FullAccess),
            other => Err(format!(
                "unknown sandbox '{other}': use read-only, workspace-write or full-access"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::FullAccess => "full-access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    /// May run outside the sandbox without asking.
    Allow,
    /// Always asks, even where the approval mode would not.
    Ask,
    /// Never runs.
    Forbid,
}

impl RuleAction {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "allow" => Ok(Self::Allow),
            "ask" => Ok(Self::Ask),
            "forbid" => Ok(Self::Forbid),
            other => Err(format!(
                "unknown rule action '{other}': use allow, ask or forbid"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRule {
    /// Matches a command whose words start with these words.
    pub prefix: String,
    pub action: RuleAction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// Directories outside the working directory that commands and file
    /// tools may write.
    pub writable_dirs: Vec<PathBuf>,
    /// Directories outside the working directory that file tools may read.
    pub readable_dirs: Vec<PathBuf>,
    pub network: bool,
    pub rules: Vec<CommandRule>,
}

impl SandboxPolicy {
    /// Whether commands run inside an OS sandbox.
    pub fn is_sandboxed(&self) -> bool {
        self.mode != SandboxMode::FullAccess
    }

    /// The rule that applies to `command`: `forbid` wins over any other
    /// match, otherwise the longest matching prefix.
    pub fn rule_for(&self, command: &str) -> Option<RuleAction> {
        let words: Vec<&str> = command.split_whitespace().collect();
        let matches: Vec<&CommandRule> = self
            .rules
            .iter()
            .filter(|rule| {
                let prefix: Vec<&str> = rule.prefix.split_whitespace().collect();
                !prefix.is_empty() && words.starts_with(&prefix)
            })
            .collect();
        if matches.iter().any(|rule| rule.action == RuleAction::Forbid) {
            return Some(RuleAction::Forbid);
        }
        matches
            .into_iter()
            .max_by_key(|rule| rule.prefix.split_whitespace().count())
            .map(|rule| rule.action)
    }

    /// Directories a command started in `cwd` may write.
    pub fn writable_roots(&self, cwd: &Path) -> Vec<PathBuf> {
        if self.mode == SandboxMode::ReadOnly {
            return Vec::new();
        }
        let mut roots = vec![cwd.to_path_buf()];
        roots.extend(self.writable_dirs.iter().cloned());
        roots.extend(temp_dirs());
        roots.extend(cache_dirs());
        let mut roots: Vec<PathBuf> = roots.iter().map(|root| real(root)).collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// `.git` (and a worktree's real git directory), `.jucode` and `.agents`
    /// under the working directory and the read-write directories, and the
    /// read-only directories. A path
    /// that contains `cwd` itself is left writable, so a subagent working
    /// inside `.jucode/agents/…` can still write its own worktree.
    pub fn protected_paths(&self, cwd: &Path) -> Vec<PathBuf> {
        let cwd = real(cwd);
        let mut roots = vec![cwd.clone()];
        roots.extend(self.writable_dirs.iter().map(|dir| real(dir)));
        let mut protected = Vec::new();
        for root in roots {
            for name in [".git", ".jucode", ".agents"] {
                let path = root.join(name);
                if path.is_dir() {
                    protected.push(path);
                } else if name == ".git" && path.is_file() {
                    // A worktree's `.git` file points at its git directory.
                    protected.push(path.clone());
                    if let Some(target) = gitdir_target(&path) {
                        protected.push(real(&target));
                    }
                }
            }
        }
        // Read-only directories stay read-only even when they sit under a
        // writable root such as the temp directory.
        protected.extend(self.readable_dirs.iter().map(|dir| real(dir)));
        protected.retain(|path| !cwd.starts_with(path));
        protected.sort();
        protected.dedup();
        protected
    }

    /// Checks an in-process file write against the sandbox. `path` is the
    /// resolved target.
    pub fn check_write(&self, cwd: &Path, path: &Path) -> Result<(), String> {
        if !self.is_sandboxed() {
            return Ok(());
        }
        if self.mode == SandboxMode::ReadOnly {
            return Err("the sandbox is read-only: files cannot be written".to_string());
        }
        let target = real_or_parent(path);
        if let Some(protected) = self
            .protected_paths(cwd)
            .into_iter()
            .find(|protected| target.starts_with(protected))
        {
            return Err(format!(
                "{} is read-only in the sandbox (inside {}); change it with a shell command using escalate",
                path.display(),
                protected.display()
            ));
        }
        if self
            .writable_roots(cwd)
            .iter()
            .any(|root| target.starts_with(root))
        {
            Ok(())
        } else {
            Err(format!(
                "{} is outside the sandbox's writable directories",
                path.display()
            ))
        }
    }

    /// Whether `path` lies in one of the extra read-write directories.
    pub fn in_writable_dir(&self, path: &Path) -> bool {
        let target = real_or_parent(path);
        self.writable_dirs
            .iter()
            .any(|dir| target.starts_with(real(dir)))
    }

    /// The command line to run `program args` in the sandbox from `cwd`.
    /// Unchanged under `full-access`.
    pub fn wrap(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Result<(String, Vec<String>), String> {
        let plain = || {
            (
                program.to_string(),
                args.iter().map(|arg| arg.to_string()).collect(),
            )
        };
        if !self.is_sandboxed() {
            return Ok(plain());
        }
        if cfg!(target_os = "macos") {
            let mut wrapped = vec![
                "-p".to_string(),
                self.seatbelt_profile(cwd),
                "--".to_string(),
                program.to_string(),
            ];
            wrapped.extend(args.iter().map(|arg| arg.to_string()));
            Ok(("/usr/bin/sandbox-exec".to_string(), wrapped))
        } else if cfg!(target_os = "linux") {
            let bwrap = find_bwrap().ok_or_else(bwrap_missing)?;
            let mut wrapped = self.bwrap_args(cwd);
            wrapped.push("--".to_string());
            wrapped.push(program.to_string());
            wrapped.extend(args.iter().map(|arg| arg.to_string()));
            Ok((bwrap.display().to_string(), wrapped))
        } else {
            Err("the sandbox is not supported on this platform; set the sandbox to full-access or run under WSL2".to_string())
        }
    }

    /// Errors when this platform cannot run the sandbox, so a host can refuse
    /// to start instead of failing on the first command.
    pub fn check_available(&self) -> Result<(), String> {
        if !self.is_sandboxed() {
            return Ok(());
        }
        if cfg!(target_os = "macos") {
            Path::new("/usr/bin/sandbox-exec")
                .exists()
                .then_some(())
                .ok_or_else(|| "sandbox-exec is missing".to_string())
        } else if cfg!(target_os = "linux") {
            find_bwrap().map(|_| ()).ok_or_else(bwrap_missing)
        } else {
            Err("the sandbox is not supported on this platform; set the sandbox to full-access or run under WSL2".to_string())
        }
    }

    /// Seatbelt profile: everything is allowed except writing outside the
    /// writable roots (or into protected paths), reading credentials and,
    /// without network, outbound connections.
    pub fn seatbelt_profile(&self, cwd: &Path) -> String {
        let mut profile =
            String::from("(version 1)\n(allow default)\n(deny file-write*\n  (require-all\n");
        for root in self.writable_roots(cwd) {
            profile.push_str(&format!(
                "    (require-not (subpath {}))\n",
                sbpl_string(&root)
            ));
        }
        profile.push_str("    (require-not (subpath \"/dev\"))))\n");
        for path in self.protected_paths(cwd) {
            profile.push_str(&format!(
                "(deny file-write* (subpath {}))\n",
                sbpl_string(&path)
            ));
        }
        for path in denied_reads() {
            profile.push_str(&format!(
                "(deny file-read* file-write* (subpath {}))\n",
                sbpl_string(&path)
            ));
        }
        if !self.network {
            profile.push_str("(deny network-outbound (remote ip))\n");
        }
        profile
    }

    /// bwrap arguments: the whole file system read-only, writable roots
    /// bound read-write, protected paths re-bound read-only on top,
    /// credentials hidden and, without network, a new network namespace.
    pub fn bwrap_args(&self, cwd: &Path) -> Vec<String> {
        let mut args: Vec<String> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        let path = |path: &Path| path.display().to_string();
        for root in self.writable_roots(cwd).iter().filter(|root| root.exists()) {
            args.extend(["--bind".to_string(), path(root), path(root)]);
        }
        for protected in self.protected_paths(cwd) {
            args.extend(["--ro-bind".to_string(), path(&protected), path(&protected)]);
        }
        for secret in denied_reads().into_iter().filter(|secret| secret.exists()) {
            if secret.is_dir() {
                args.extend(["--tmpfs".to_string(), path(&secret)]);
            } else {
                args.extend([
                    "--ro-bind".to_string(),
                    "/dev/null".to_string(),
                    path(&secret),
                ]);
            }
        }
        if !self.network {
            args.push("--unshare-net".to_string());
        }
        args.extend([
            "--die-with-parent".to_string(),
            "--chdir".to_string(),
            path(cwd),
        ]);
        args
    }

    /// A paragraph for the system prompt describing the sandbox.
    pub fn prompt(&self, cwd: &Path) -> String {
        if !self.is_sandboxed() {
            return String::new();
        }
        let writable = match self.mode {
            SandboxMode::ReadOnly => "Nothing is writable.".to_string(),
            _ => format!(
                "Writable: the working directory{}, temp and package-cache directories; .git, .jucode and .agents inside them are read-only.",
                if self.writable_dirs.is_empty() {
                    String::new()
                } else {
                    format!(
                        ", {}",
                        self.writable_dirs
                            .iter()
                            .map(|dir| dir.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            ),
        };
        let readable = if self.readable_dirs.is_empty() {
            String::new()
        } else {
            format!(
                " Also readable with file tools: {}.",
                self.readable_dirs
                    .iter()
                    .map(|dir| dir.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        format!(
            "<sandbox mode=\"{}\">\nShell commands run in a sandbox. {writable}{readable} Network is {}. Credentials such as ~/.ssh are unreadable. When a command needs more (committing to git, writing elsewhere{}), run it with `escalate: true` and a one-line `justification`; it then runs outside the sandbox if approved. Working directory: {}.\n</sandbox>",
            self.mode.as_str(),
            if self.network { "on" } else { "off" },
            if self.network { "" } else { ", network access" },
            cwd.display()
        )
    }
}

impl SandboxPolicy {
    /// The default: the working directory writable, network on, commits
    /// allowed to leave the sandbox and pushes always asked. Windows has no
    /// sandbox yet and starts at `full-access`.
    pub fn default_for_platform() -> Self {
        Self {
            mode: if cfg!(windows) {
                SandboxMode::FullAccess
            } else {
                SandboxMode::WorkspaceWrite
            },
            writable_dirs: Vec::new(),
            readable_dirs: Vec::new(),
            network: true,
            rules: rules_from_json(&default_rules_json()).expect("default rules parse"),
        }
    }
}

pub fn default_rules_json() -> Value {
    json!([
        { "prefix": "git add", "action": "allow" },
        { "prefix": "git commit", "action": "allow" },
        { "prefix": "git push", "action": "ask" },
    ])
}

/// `[{"prefix": "git push", "action": "allow" | "ask" | "forbid"}]`.
pub fn rules_from_json(value: &Value) -> Result<Vec<CommandRule>, String> {
    let Some(items) = value.as_array() else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| {
            let prefix = item["prefix"].as_str().unwrap_or_default().trim();
            if prefix.is_empty() {
                return Err("a command rule needs a prefix".to_string());
            }
            Ok(CommandRule {
                prefix: prefix.to_string(),
                action: RuleAction::parse(item["action"].as_str().unwrap_or_default())?,
            })
        })
        .collect()
}

pub fn rules_to_json(rules: &[CommandRule]) -> Value {
    json!(rules
        .iter()
        .map(|rule| json!({
            "prefix": rule.prefix,
            "action": match rule.action {
                RuleAction::Allow => "allow",
                RuleAction::Ask => "ask",
                RuleAction::Forbid => "forbid",
            },
        }))
        .collect::<Vec<_>>())
}

/// `[{"path": "/abs/dir", "mode": "ro" | "rw"}]` → (read-write, read-only).
/// A directory that no longer exists is dropped when `strict` is false (a
/// saved setting), and is an error when true (a change being made).
pub fn directories_from_json(
    value: &Value,
    strict: bool,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let mut writable = Vec::new();
    let mut readable = Vec::new();
    for item in value.as_array().into_iter().flatten() {
        let mode = item["mode"].as_str().unwrap_or("ro");
        if !matches!(mode, "ro" | "rw") {
            return Err(format!("directory mode must be ro or rw, not '{mode}'"));
        }
        let path = PathBuf::from(item["path"].as_str().unwrap_or_default());
        if !path.is_absolute() || !path.is_dir() {
            if strict {
                return Err(format!(
                    "not an existing absolute directory: {}",
                    path.display()
                ));
            }
            continue;
        }
        if mode == "rw" {
            writable.push(path);
        } else {
            readable.push(path);
        }
    }
    Ok((writable, readable))
}

pub fn directories_to_json(writable: &[PathBuf], readable: &[PathBuf]) -> Value {
    let entry =
        |path: &PathBuf, mode: &str| json!({ "path": path.display().to_string(), "mode": mode });
    json!(writable
        .iter()
        .map(|path| entry(path, "rw"))
        .chain(readable.iter().map(|path| entry(path, "ro")))
        .collect::<Vec<_>>())
}

fn gitdir_target(git_file: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(git_file).ok()?;
    let target = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let target = PathBuf::from(target);
    Some(if target.is_absolute() {
        target
    } else {
        git_file.parent()?.join(target)
    })
}

fn home() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Credentials no sandboxed command, and no remote client, may read.
pub fn denied_reads() -> Vec<PathBuf> {
    let Some(home) = home() else {
        return Vec::new();
    };
    [
        ".ssh",
        ".gnupg",
        ".aws",
        ".jucode/auth.json",
        ".jucode/daemon",
    ]
    .iter()
    .map(|path| real(&home.join(path)))
    .collect()
}

fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![env::temp_dir()];
    if cfg!(unix) {
        dirs.push(PathBuf::from("/tmp"));
    }
    dirs.into_iter().filter(|dir| dir.exists()).collect()
}

/// Package managers' caches, so installs and builds work in the sandbox.
fn cache_dirs() -> Vec<PathBuf> {
    let Some(home) = home() else {
        return Vec::new();
    };
    [
        ".cache",
        ".npm",
        ".pnpm-store",
        ".yarn",
        ".bun",
        ".cargo/registry",
        ".cargo/git",
        ".rustup",
        "go/pkg",
        ".m2",
        ".gradle",
        "Library/Caches",
    ]
    .iter()
    .map(|dir| home.join(dir))
    .filter(|dir| dir.exists())
    .collect()
}

/// The real path, or the path as given when it does not exist.
fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The real path of `path`, resolving its nearest existing ancestor when
/// `path` itself does not exist yet (a file about to be created).
fn real_or_parent(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if let Ok(real) = current.canonicalize() {
            return missing.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (current.file_name(), current.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn find_bwrap() -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join("bwrap"))
            .find(|candidate| candidate.is_file())
    })
}

fn bwrap_missing() -> String {
    "the sandbox needs bubblewrap (bwrap) on PATH: install it (apt install bubblewrap / dnf install bubblewrap), or set the sandbox to full-access".to_string()
}

fn sbpl_string(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: SandboxMode) -> SandboxPolicy {
        SandboxPolicy {
            mode,
            writable_dirs: Vec::new(),
            readable_dirs: Vec::new(),
            network: true,
            rules: vec![
                CommandRule {
                    prefix: "git commit".into(),
                    action: RuleAction::Allow,
                },
                CommandRule {
                    prefix: "git".into(),
                    action: RuleAction::Ask,
                },
                CommandRule {
                    prefix: "git push --force".into(),
                    action: RuleAction::Forbid,
                },
            ],
        }
    }

    fn work(label: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("jucode-sandbox-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join(".jucode/agents/sub")).unwrap();
        real(&dir)
    }

    #[test]
    fn rules_pick_forbid_first_then_the_longest_prefix() {
        let policy = policy(SandboxMode::WorkspaceWrite);
        assert_eq!(policy.rule_for("git commit -m x"), Some(RuleAction::Allow));
        assert_eq!(policy.rule_for("git status"), Some(RuleAction::Ask));
        assert_eq!(
            policy.rule_for("git push --force origin"),
            Some(RuleAction::Forbid)
        );
        assert_eq!(policy.rule_for("gitk"), None);
        assert_eq!(policy.rule_for("ls"), None);
    }

    #[test]
    fn writes_stay_in_writable_roots_and_out_of_protected_paths() {
        let dir = work("writes");
        let policy = policy(SandboxMode::WorkspaceWrite);
        assert!(policy.check_write(&dir, &dir.join("src/new.rs")).is_ok());
        assert!(policy.check_write(&dir, &dir.join(".git/config")).is_err());
        assert!(policy.check_write(&dir, &dir.join(".jucode/x")).is_err());
        assert!(policy.check_write(&dir, Path::new("/etc/hosts")).is_err());
        // A subagent working inside .jucode can still write its own worktree.
        let sub = dir.join(".jucode/agents/sub");
        fs::write(sub.join(".git"), "gitdir: ../../../.git/worktrees/sub\n").unwrap();
        assert!(policy.check_write(&sub, &sub.join("file.txt")).is_ok());
        assert!(policy.check_write(&sub, &sub.join(".git")).is_err());
        let read_only = SandboxPolicy {
            mode: SandboxMode::ReadOnly,
            ..policy.clone()
        };
        assert!(read_only
            .check_write(&dir, &dir.join("src/new.rs"))
            .is_err());
        let full = SandboxPolicy {
            mode: SandboxMode::FullAccess,
            ..policy
        };
        assert!(full.check_write(&dir, Path::new("/etc/hosts")).is_ok());
    }

    #[test]
    fn a_worktree_protects_its_real_git_directory() {
        let dir = work("worktree");
        let real_git = dir.join("main-repo-git");
        fs::create_dir_all(&real_git).unwrap();
        let wt = dir.join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", real_git.display())).unwrap();
        let protected = policy(SandboxMode::WorkspaceWrite).protected_paths(&wt);
        assert!(protected.contains(&wt.join(".git")));
        assert!(protected.contains(&real(&real_git)));
    }

    #[test]
    fn bwrap_binds_roots_then_protects_and_hides_secrets() {
        let dir = work("bwrap");
        let mut offline = policy(SandboxMode::WorkspaceWrite);
        offline.network = false;
        let args = offline.bwrap_args(&dir).join(" ");
        let bind = args
            .find(&format!("--bind {0} {0}", dir.display()))
            .unwrap();
        let protect = args
            .find(&format!("--ro-bind {0} {0}", dir.join(".git").display()))
            .unwrap();
        assert!(bind < protect, "{args}");
        assert!(args.starts_with("--ro-bind / /"));
        assert!(args.contains("--unshare-net"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_os_sandbox_enforces_the_policy() {
        use std::process::Command;
        let dir = work("seatbelt");
        let outside = real(&env::temp_dir())
            .parent()
            .unwrap()
            .join(format!("jucode-sbx-outside-{}", std::process::id()));
        let _ = fs::remove_file(&outside);
        let policy = policy(SandboxMode::WorkspaceWrite);
        let run = |script: &str| {
            let (program, args) = policy.wrap("/bin/sh", &["-c", script], &dir).unwrap();
            Command::new(program)
                .args(args)
                .current_dir(&dir)
                .status()
                .unwrap()
                .success()
        };
        assert!(run("echo ok > inside.txt"));
        assert!(!run("echo no > .git/config"));
        assert!(!run(&format!("echo no > {}", outside.display())));
        assert!(!outside.exists());
    }
}
