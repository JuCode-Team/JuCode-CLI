use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use crate::config::profile_dir;

/// Remembers which project directories the user has trusted to load project-local
/// resources (skills, hooks). Stored as an absolute-path -> decision map in
/// `~/.jucode/trust.json`.
#[derive(Debug, Clone)]
pub struct TrustStore {
    decisions: BTreeMap<String, bool>,
    path: PathBuf,
}

impl TrustStore {
    pub fn load_or_create() -> io::Result<Self> {
        let path = profile_dir()?.join("trust.json");
        if !path.exists() {
            return Ok(Self {
                decisions: BTreeMap::new(),
                path,
            });
        }
        let content = fs::read_to_string(&path)?;
        let value = serde_json::from_str::<Value>(&content).unwrap_or_else(|_| json!({}));
        let mut decisions = BTreeMap::new();
        if let Some(map) = value.get("projects").and_then(Value::as_object) {
            for (key, decision) in map {
                if let Some(decision) = decision.as_bool() {
                    decisions.insert(key.clone(), decision);
                }
            }
        }
        Ok(Self { decisions, path })
    }

    /// The decision for `cwd`, inherited from the nearest trusted/untrusted
    /// ancestor. `None` means the user has not decided yet.
    pub fn decision_for(&self, cwd: &Path) -> Option<bool> {
        let mut current = normalize(cwd);
        loop {
            if let Some(decision) = self.decisions.get(&current.to_string_lossy().to_string()) {
                return Some(*decision);
            }
            match current.parent() {
                Some(parent) if parent != current => current = parent.to_path_buf(),
                _ => return None,
            }
        }
    }

    pub fn set(&mut self, path: &Path, trusted: bool) -> io::Result<()> {
        let key = normalize(path).to_string_lossy().to_string();
        self.decisions.insert(key, trusted);
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let projects = self
            .decisions
            .iter()
            .map(|(key, decision)| (key.clone(), json!(decision)))
            .collect::<serde_json::Map<_, _>>();
        let value = json!({ "projects": projects });
        fs::write(
            &self.path,
            format!("{}\n", serde_json::to_string_pretty(&value)?),
        )
    }
}

/// Whether `cwd` ships project-local resources that can run code or inject
/// instructions, and therefore require a trust decision before loading.
pub fn project_has_local_resources(cwd: &Path) -> bool {
    let base = cwd.join(".jucode");
    base.join("skills").exists() || base.join("hooks.json").exists()
}

/// The nearest ancestor of `cwd` that holds the `.git` marker, offered as a
/// "trust the whole repository" option. `None` when `cwd` is itself the repo
/// root or no repo marker is found.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let normalized = normalize(cwd);
    let mut current = normalized.as_path();
    while let Some(parent) = current.parent() {
        if current.join(".git").exists() && current != normalized {
            return Some(current.to_path_buf());
        }
        current = parent;
    }
    None
}

fn normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(decisions: &[(&str, bool)]) -> TrustStore {
        TrustStore {
            decisions: decisions
                .iter()
                .map(|(key, decision)| ((*key).to_string(), *decision))
                .collect(),
            path: PathBuf::from("unused.json"),
        }
    }

    #[test]
    fn decision_inherits_from_nearest_ancestor() {
        // Use paths that do not exist so normalize() returns them unchanged.
        let store = store(&[("/repo", true), ("/repo/vendor", false)]);
        assert_eq!(store.decision_for(Path::new("/repo/src/app")), Some(true));
        assert_eq!(
            store.decision_for(Path::new("/repo/vendor/lib")),
            Some(false)
        );
        assert_eq!(store.decision_for(Path::new("/other")), None);
    }
}
