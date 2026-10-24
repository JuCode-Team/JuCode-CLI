//! Workspaces and their projects, shared by Desktop and remote devices.
//! The daemon keeps them in `workspaces.json`; every change is broadcast
//! as a `workspaces` frame.

use crate::hub::Hub;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn workspaces_json(hub: &Hub) -> Value {
    let mut doc = hub.store.workspaces();
    doc["type"] = json!("workspaces");
    doc
}

/// Applies a workspace op and broadcasts the new list; the reply is that
/// list too.
pub fn handle(hub: &Hub, name: &str, op: &Value) -> Result<Value, String> {
    match name {
        "workspaces_set" => set(hub, op),
        "project_add" => {
            let path = required(op, "path")?;
            add(hub, op, canonical_dir(Path::new(path))?)
        }
        "project_create" => {
            let parent = canonical_dir(Path::new(required(op, "parent")?))?;
            let name = required(op, "name")?.trim();
            if name.is_empty()
                || name == "."
                || name == ".."
                || name.contains(['/', '\\'])
                || name.chars().count() > 100
            {
                return Err(format!("not a valid folder name: {name}"));
            }
            let path = parent.join(name);
            fs::create_dir(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            if op["git_init"].as_bool().unwrap_or(false) {
                let status = Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(&path)
                    .status()
                    .map_err(|error| format!("git init: {error}"))?;
                if !status.success() {
                    return Err(format!("git init failed in {}", path.display()));
                }
            }
            add(hub, op, path)
        }
        "project_remove" => {
            let workspace = required(op, "workspace")?;
            let project = required(op, "project")?;
            save(hub, |list| {
                let ws = find(list, workspace)?;
                let projects = ws["projects"].as_array_mut().ok_or("bad workspace")?;
                let before = projects.len();
                projects.retain(|p| p["id"] != project);
                if projects.len() == before {
                    return Err(format!("unknown project {project}"));
                }
                Ok(())
            })
        }
        _ => Err(format!("unknown op {name}")),
    }
}

/// Replaces the whole list (Desktop's own edits). `rev` must be the one the
/// client last saw, so a remote change made meanwhile is never overwritten.
fn set(hub: &Hub, op: &Value) -> Result<Value, String> {
    let incoming = op["workspaces"]
        .as_array()
        .ok_or("workspaces_set requires workspaces")?
        .clone();
    for ws in &incoming {
        text(ws, "id")?;
        text(ws, "name")?;
        for project in ws["projects"]
            .as_array()
            .ok_or("a workspace needs projects")?
        {
            text(project, "id")?;
            text(project, "name")?;
            if !Path::new(text(project, "path")?).is_absolute() {
                return Err("project paths must be absolute".to_string());
            }
            check_dirs(&project["dirs"])?;
        }
    }
    // A project belongs to one workspace: its sessions name it by id. (One
    // listed twice in a workspace, as older desktops could save, is one.)
    let mut owner = std::collections::HashMap::new();
    for ws in &incoming {
        for project in ws["projects"].as_array().into_iter().flatten() {
            let id = text(project, "id")?;
            if *owner.entry(id).or_insert(&ws["id"]) != &ws["id"] {
                return Err(format!("project {id} is in two workspaces"));
            }
        }
    }
    let rev = op["rev"].as_u64().ok_or("workspaces_set requires rev")?;
    let current = hub.store.workspaces()["rev"].as_u64().unwrap_or(0);
    if rev != current {
        return Err(format!("stale workspaces (rev {rev}, current {current})"));
    }
    save(hub, |list| {
        *list = incoming;
        Ok(())
    })
}

/// Adds `path` to the workspace named by `workspace`. With no workspaces yet
/// one is created, named `workspace_name`.
fn add(hub: &Hub, op: &Value, path: PathBuf) -> Result<Value, String> {
    let workspace = op["workspace"].as_str().unwrap_or_default().to_string();
    let path_text = path.display().to_string();
    let name = op["project_name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| path_text.clone());
    let project = json!({ "id": hub.new_id("p"), "name": name, "path": path_text });
    let default_name = op["workspace_name"]
        .as_str()
        .unwrap_or("Default")
        .to_string();
    let new_workspace =
        json!({ "id": hub.new_id("w"), "name": default_name, "is_default": true, "projects": [] });
    save(hub, move |list| {
        if list.is_empty() {
            list.push(new_workspace);
        }
        let ws = if workspace.is_empty() {
            &mut list[0]
        } else {
            find(list, &workspace)?
        };
        let projects = ws["projects"].as_array_mut().ok_or("bad workspace")?;
        if projects.iter().any(|p| p["path"] == path_text.as_str()) {
            return Err(format!("{path_text} is already a project here"));
        }
        projects.push(project);
        Ok(())
    })
}

/// Saves a change to the list. The sessions of a project it removes now
/// belong to none: they are archived, so they wait out of the way instead
/// of filling every client's list of sessions of no project.
fn save(
    hub: &Hub,
    change: impl FnOnce(&mut Vec<Value>) -> Result<(), String>,
) -> Result<Value, String> {
    let before = projects(hub);
    hub.store.update_workspaces(change)?;
    let after = projects(hub);
    let gone: Vec<&str> = before
        .iter()
        .filter_map(|p| p["id"].as_str())
        .filter(|id| !after.iter().any(|p| p["id"] == *id))
        .collect();
    let orphans: Vec<String> = hub
        .store
        .sessions()
        .into_iter()
        // One recorded before sessions named their project may now resolve
        // to another of its folder's.
        .filter(|r| {
            !r.archived
                && gone.contains(&member(&before, r).as_str())
                && member(&after, r).is_empty()
        })
        .map(|r| r.id)
        .collect();
    for id in &orphans {
        hub.store
            .record_session_meta(id, &json!({ "archived": true }))
            .map_err(|error| error.to_string())?;
    }
    let frame = workspaces_json(hub);
    hub.broadcast(&frame);
    if !orphans.is_empty() {
        hub.broadcast(&hub.sessions_json());
    }
    Ok(frame)
}

fn find<'a>(list: &'a mut [Value], id: &str) -> Result<&'a mut Value, String> {
    list.iter_mut()
        .find(|ws| ws["id"] == id)
        .ok_or_else(|| format!("unknown workspace {id}"))
}

fn required<'a>(op: &'a Value, key: &str) -> Result<&'a str, String> {
    op[key].as_str().ok_or_else(|| format!("requires {key}"))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("every workspace and project needs {key}"))
}

fn canonical_dir(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("not an absolute path: {}", path.display()));
    }
    let real = path
        .canonicalize()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !real.is_dir() {
        return Err(format!("not a directory: {}", real.display()));
    }
    // Windows canonicalizes to `\\?\C:\...`; the desktop and the engines
    // know the folder as `C:\...`, and sessions are matched by that string.
    #[cfg(windows)]
    if let Some(plain) = real.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        if !plain.starts_with("UNC\\") {
            return Ok(PathBuf::from(plain));
        }
    }
    Ok(real)
}

/// A project's extra directories (`dirs`), when given, are a list of
/// absolute paths.
fn check_dirs(dirs: &Value) -> Result<(), String> {
    if dirs.is_null() {
        return Ok(());
    }
    let list = dirs.as_array().ok_or("a project's dirs must be a list")?;
    for dir in list {
        match dir.as_str() {
            Some(dir) if Path::new(dir).is_absolute() => {}
            _ => return Err("project dirs must be absolute paths".to_string()),
        }
    }
    Ok(())
}

/// Every workspace's projects.
pub fn projects(hub: &Hub) -> Vec<Value> {
    hub.store.workspaces()["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|ws| ws["projects"].as_array().cloned().unwrap_or_default())
        .collect()
}

fn dirs(project: &Value) -> impl Iterator<Item = PathBuf> + '_ {
    project["dirs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|dir| dir.as_str().map(PathBuf::from))
}

/// Every project directory (main and extra), for deciding what remote
/// clients may read.
pub fn project_paths(hub: &Hub) -> Vec<PathBuf> {
    projects(hub)
        .iter()
        .flat_map(|project| {
            let main = project["path"].as_str().map(PathBuf::from);
            main.into_iter().chain(dirs(project)).collect::<Vec<_>>()
        })
        .collect()
}

/// The project with id `id`.
pub fn project(hub: &Hub, id: &str) -> Option<Value> {
    projects(hub)
        .into_iter()
        .find(|project| project["id"] == id)
}

/// The project whose main directory is `cwd`.
pub fn project_of_cwd(hub: &Hub, cwd: &Path) -> Option<Value> {
    projects(hub)
        .into_iter()
        .find(|project| project["path"].as_str().map(Path::new) == Some(cwd))
}

/// The directories of project `project` (main and extra) that still exist,
/// besides `cwd`: an engine started there for the project may work in them
/// too. None for no project.
pub fn extra_dirs(hub: &Hub, cwd: &Path, project: &str) -> Vec<PathBuf> {
    let Some(project) = (!project.is_empty())
        .then(|| self::project(hub, project))
        .flatten()
    else {
        return Vec::new();
    };
    let main = project["path"].as_str().map(PathBuf::from);
    let mut list: Vec<PathBuf> = Vec::new();
    for dir in main.into_iter().chain(dirs(&project)) {
        // A worktree works apart from the checkout it was made of.
        if dir != cwd && dir.is_dir() && !list.contains(&dir) && !in_worktree_of(cwd, &dir) {
            list.push(dir);
        }
    }
    list
}

/// `cwd` is in a worktree the desktop or the daemon made of the repository
/// `dir` is in (`<repo's parent>/.jucode-worktrees/<repo's name>/…`, the
/// repository being `dir` or a folder above it).
fn in_worktree_of(cwd: &Path, dir: &Path) -> bool {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let cwd = real(cwd);
    real(dir).ancestors().any(|repo| match (repo.parent(), repo.file_name()) {
        (Some(parent), Some(name)) => cwd.starts_with(parent.join(".jucode-worktrees").join(name)),
        _ => false,
    })
}

/// The project session `id` belongs to ("": none, or one since removed).
pub fn of_session(hub: &Hub, id: &str) -> String {
    hub.store
        .sessions()
        .into_iter()
        .find(|record| record.id == id)
        .map(|record| of_record(hub, &record))
        .unwrap_or_default()
}

/// The project `record` belongs to ("": none, or one since removed); one
/// recorded before sessions named theirs, the one whose main directory is
/// its cwd.
pub fn of_record(hub: &Hub, record: &crate::store::SessionRecord) -> String {
    member(&projects(hub), record)
}

/// `of_record` among `projects` (every project, read once for a list).
pub fn member(projects: &[Value], record: &crate::store::SessionRecord) -> String {
    let found = match &record.project {
        Some(id) => projects.iter().find(|p| p["id"] == id.as_str()),
        None => projects
            .iter()
            .find(|p| p["path"].as_str().map(Path::new) == Some(record.cwd.as_path())),
    };
    found
        .and_then(|p| p["id"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// The project a new session in `cwd` belongs to: `requested` when the
/// client named one ("": none), else the one whose main directory is `cwd`.
pub fn for_new_session(hub: &Hub, cwd: &Path, requested: Option<&str>) -> Result<String, String> {
    match requested.map(str::trim) {
        Some("") => Ok(String::new()),
        Some(id) => project(hub, id)
            .map(|_| id.to_string())
            .ok_or_else(|| format!("unknown project {id}")),
        None => Ok(project_of_cwd(hub, cwd)
            .and_then(|p| p["id"].as_str().map(str::to_string))
            .unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SessionRecord;

    fn record(cwd: &str, project: Option<&str>) -> SessionRecord {
        SessionRecord {
            id: "s".into(),
            cwd: cwd.into(),
            schedule: None,
            created_at: 0,
            closed: false,
            title: None,
            title_auto: false,
            archived: false,
            hidden: false,
            engine: None,
            gateway: false,
            group: None,
            project: project.map(str::to_string),
            agent: None,
            agent_session: None,
            tool_provider: None,
            worktree: None,
            parent: None,
        }
    }

    #[test]
    fn a_worktree_of_the_main_directory_is_apart_from_it() {
        let main = Path::new("/w/app");
        assert!(in_worktree_of(Path::new("/w/.jucode-worktrees/app/sch-1/web"), main));
        assert!(!in_worktree_of(Path::new("/w/.jucode-worktrees/app2/sch-1"), main));
        assert!(!in_worktree_of(Path::new("/w/app/web"), main));
        // A project in a folder of the repository, and another folder of it.
        let tree = Path::new("/w/.jucode-worktrees/repo/sch-1/web");
        assert!(in_worktree_of(tree, Path::new("/w/repo/web")));
        assert!(in_worktree_of(tree, Path::new("/w/repo/shared")));
        assert!(!in_worktree_of(tree, Path::new("/w/other")));
    }

    #[test]
    fn a_session_with_no_recorded_project_belongs_to_its_folders() {
        let projects = [json!({ "id": "p1", "path": "/p/app" })];
        assert_eq!(member(&projects, &record("/p/app", None)), "p1");
        assert_eq!(member(&projects, &record("/p/other", None)), "");
        // Named: that one, wherever it runs; "" or a removed one: none.
        assert_eq!(member(&projects, &record("/p/other", Some("p1"))), "p1");
        assert_eq!(member(&projects, &record("/p/app", Some(""))), "");
        assert_eq!(member(&projects, &record("/p/app", Some("gone"))), "");
    }
}
