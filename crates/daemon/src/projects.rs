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

fn save(
    hub: &Hub,
    change: impl FnOnce(&mut Vec<Value>) -> Result<(), String>,
) -> Result<Value, String> {
    hub.store.update_workspaces(change)?;
    let frame = workspaces_json(hub);
    hub.broadcast(&frame);
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
    Ok(real)
}

/// Every project directory, for deciding what remote clients may read.
pub fn project_paths(hub: &Hub) -> Vec<PathBuf> {
    hub.store.workspaces()["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|ws| ws["projects"].as_array().into_iter().flatten())
        .filter_map(|project| project["path"].as_str().map(PathBuf::from))
        .collect()
}
