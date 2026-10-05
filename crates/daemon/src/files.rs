//! Read-only views of project files for clients: directory listings, file
//! contents, git status and diffs. Reads stay inside the directories the
//! daemon knows (projects, session and agent directories); picking a folder
//! for a new project may browse directories anywhere under the home
//! directory. Credentials (`sandbox::denied_reads`) are never readable.

use crate::{hub::Hub, projects};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Largest file content sent to a client.
const MAX_READ: u64 = 1024 * 1024;
const MAX_ENTRIES: usize = 2000;
const MAX_DIFF: usize = 1024 * 1024;
/// Largest image sent to a client.
const MAX_IMAGE: u64 = 16 * 1024 * 1024;

pub fn handle(hub: &Hub, name: &str, op: &Value) -> Result<Value, String> {
    let path = op["path"]
        .as_str()
        .or_else(|| op["cwd"].as_str())
        .ok_or_else(|| format!("{name} requires path"))?;
    // `~`: the home directory, where a remote client starts browsing.
    let home_dir = home().unwrap_or_default();
    let path = if path == "~" {
        home_dir.to_str().unwrap_or_default()
    } else {
        path
    };
    match name {
        "fs_list" => {
            let dirs_only = op["dirs_only"].as_bool().unwrap_or(false);
            let dir = allowed(hub, Path::new(path), dirs_only)?;
            list(&dir, dirs_only)
        }
        "fs_read" => read(&allowed(hub, Path::new(path), false)?),
        "fs_image" => image(hub, Path::new(path)),
        "git_status" => git_status(&allowed(hub, Path::new(path), false)?),
        "git_diff" => {
            let cwd = allowed(hub, Path::new(path), false)?;
            let file = match op["file"].as_str() {
                Some(file) => Some(allowed(hub, &cwd.join(file), false)?),
                None => None,
            };
            let mut diff = jucode_agent_core::git_diff(&cwd, file.as_deref())?;
            let truncated = diff.len() > MAX_DIFF;
            if truncated {
                let mut end = MAX_DIFF;
                while !diff.is_char_boundary(end) {
                    end -= 1;
                }
                diff.truncate(end);
            }
            Ok(
                json!({ "type": "git_diff", "path": cwd, "file": op["file"], "diff": diff, "truncated": truncated }),
            )
        }
        _ => Err(format!("unknown op {name}")),
    }
}

/// The real path of `path` when a client may read it: inside a known
/// directory, or with `browse` anywhere under the home directory.
fn allowed(hub: &Hub, path: &Path, browse: bool) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("not an absolute path: {}", path.display()));
    }
    let real = path
        .canonicalize()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if jucode_agent_core::sandbox::denied_reads()
        .iter()
        .any(|denied| real.starts_with(denied))
    {
        return Err(format!("{} is protected", real.display()));
    }
    let roots: Vec<PathBuf> = if browse {
        home().into_iter().collect()
    } else {
        known_dirs(hub)
    };
    if roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| real.starts_with(root))
    {
        Ok(real)
    } else {
        Err(format!("{} is outside the projects", real.display()))
    }
}

/// The real path of `path` (`~`: the home directory) when it lies inside a
/// known directory.
pub fn known_dir(hub: &Hub, path: &str) -> Result<PathBuf, String> {
    match path {
        "~" => allowed(hub, &home().unwrap_or_default(), false),
        path => allowed(hub, Path::new(path), false),
    }
}

fn known_dirs(hub: &Hub) -> Vec<PathBuf> {
    let mut dirs = projects::project_paths(hub);
    dirs.extend(hub.store.sessions().into_iter().map(|record| record.cwd));
    dirs.extend(hub.agents.list().into_iter().map(|agent| agent.cwd));
    dirs
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn list(dir: &Path, dirs_only: bool) -> Result<Value, String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        // Follows symlinks, so a linked directory lists as a directory.
        let Ok(meta) = fs::metadata(entry.path()) else {
            continue;
        };
        if name == ".git" || (dirs_only && (!meta.is_dir() || name.starts_with('.'))) {
            continue;
        }
        entries.push((name, meta.is_dir(), meta.len()));
    }
    if !dirs_only {
        let ignored = git_ignored(dir, entries.iter().map(|(name, _, _)| name.as_str()));
        entries.retain(|(name, _, _)| !ignored.contains(name));
    }
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    let truncated = entries.len() > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|(name, dir, size)| json!({ "name": name, "dir": dir, "size": size }))
        .collect();
    Ok(json!({
        "type": "fs_list",
        "path": dir,
        "git": dir.join(".git").exists(),
        "entries": entries,
        "truncated": truncated,
    }))
}

/// The names in `dir` that git ignores; empty outside a repository.
fn git_ignored<'a>(dir: &Path, names: impl Iterator<Item = &'a str>) -> HashSet<String> {
    let input: Vec<u8> = names
        .flat_map(|name| [name.as_bytes(), b"\0"].concat())
        .collect();
    if input.is_empty() {
        return HashSet::new();
    }
    let Ok(mut child) = Command::new("git")
        .args(["check-ignore", "--stdin", "-z"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return HashSet::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&input);
    }
    let Ok(output) = child.wait_with_output() else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

fn read(path: &Path) -> Result<Value, String> {
    let meta = fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("not a file: {}", path.display()));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(MAX_READ).read_to_end(&mut bytes))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let binary = bytes.iter().take(8000).any(|byte| *byte == 0);
    Ok(json!({
        "type": "fs_read",
        "path": path,
        "size": meta.len(),
        "binary": binary,
        "text": if binary { Value::Null } else { json!(String::from_utf8_lossy(&bytes)) },
        "truncated": meta.len() > MAX_READ,
    }))
}

/// An image a message showed: a file in a known directory or one a client
/// uploaded, as a data URL for the remote page (which cannot open local paths).
fn image(hub: &Hub, path: &Path) -> Result<Value, String> {
    let real = match allowed(hub, path, false) {
        Ok(real) => real,
        Err(error) => {
            let real = path
                .canonicalize()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let uploads = hub
                .uploads
                .dir()
                .canonicalize()
                .map_err(|_| error.clone())?;
            if !real.starts_with(uploads) {
                return Err(error);
            }
            real
        }
    };
    let media = match real
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return Err(format!("not an image: {}", real.display())),
    };
    let meta = fs::metadata(&real).map_err(|error| format!("{}: {error}", real.display()))?;
    if meta.len() > MAX_IMAGE {
        return Err(format!(
            "{} is larger than {} MB",
            real.display(),
            MAX_IMAGE >> 20
        ));
    }
    let bytes = fs::read(&real).map_err(|error| format!("{}: {error}", real.display()))?;
    use base64::{engine::general_purpose::STANDARD, Engine};
    let data = format!("data:{media};base64,{}", STANDARD.encode(bytes));
    Ok(json!({ "type": "fs_image", "path": real, "data": data }))
}

/// Branch and changed files (`git status --porcelain`); `repo: false`
/// outside a repository.
fn git_status(cwd: &Path) -> Result<Value, String> {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("git: {error}"))
    };
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let status = git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    if !status.status.success() {
        return Ok(json!({ "type": "git_status", "path": cwd, "repo": false, "files": [] }));
    }
    let text = String::from_utf8_lossy(&status.stdout);
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    let mut files = Vec::new();
    while let Some(field) = fields.next() {
        let (code, file) = field.split_at(field.len().min(3));
        let code = code.trim_end();
        // A rename or copy is followed by its original path.
        let from = if code.starts_with(['R', 'C']) {
            fields.next()
        } else {
            None
        };
        files.push(json!({ "path": file, "status": code, "from": from }));
    }
    Ok(json!({
        "type": "git_status",
        "path": cwd,
        "repo": true,
        "branch": String::from_utf8_lossy(&branch.stdout).trim(),
        "files": files,
    }))
}
