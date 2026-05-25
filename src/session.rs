use crate::event::{TranscriptItem, TreeNodeView};
use serde_json::{json, Value};
use std::{
    cmp::Reverse,
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(u64);

impl EntryId {
    pub fn display(self) -> String {
        format!("e{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub enum EntryKind {
    Branch {
        label: String,
    },
    User {
        content: String,
    },
    ResponseItem {
        item: Value,
    },
    ToolOutput {
        call_id: String,
        name: String,
        output: String,
    },
}

#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: EntryId,
    pub parent_id: Option<EntryId>,
    pub kind: EntryKind,
}

#[derive(Debug, Default)]
pub struct SessionStore {
    entries: Vec<SessionEntry>,
    by_id: HashMap<EntryId, usize>,
    leaf_id: Option<EntryId>,
    next_id: u64,
    session_id: String,
    created_at: u64,
    updated_at: u64,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub updated_at: u64,
    pub entries: usize,
    pub leaf: String,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            session_id: new_session_id(),
            created_at: now_secs(),
            updated_at: now_secs(),
            ..Self::default()
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn leaf_id(&self) -> Option<EntryId> {
        self.leaf_id
    }

    pub fn append(&mut self, kind: EntryKind) -> EntryId {
        let id = EntryId(self.next_id);
        self.next_id += 1;
        let entry = SessionEntry {
            id,
            parent_id: self.leaf_id,
            kind,
        };
        self.by_id.insert(id, self.entries.len());
        self.entries.push(entry);
        self.leaf_id = Some(id);
        self.updated_at = now_secs();
        id
    }

    pub fn switch_to(&mut self, id: Option<EntryId>) -> Result<(), String> {
        if let Some(id) = id {
            if !self.by_id.contains_key(&id) {
                return Err(format!("entry {} not found", id.display()));
            }
        }
        self.leaf_id = id;
        self.updated_at = now_secs();
        Ok(())
    }

    pub fn branch_from(&mut self, id: Option<EntryId>) -> Result<EntryId, String> {
        self.switch_to(id)?;
        let label = match id {
            Some(id) => format!("branch from {}", id.display()),
            None => "branch from root".to_string(),
        };
        Ok(self.append(EntryKind::Branch { label }))
    }

    pub fn parse_id(text: &str) -> Option<EntryId> {
        let trimmed = text.trim().trim_start_matches('e');
        trimmed.parse::<u64>().ok().map(EntryId)
    }

    pub fn branch(&self) -> Vec<&SessionEntry> {
        let mut path = Vec::new();
        let mut current = self.leaf_id;
        while let Some(id) = current {
            let Some(index) = self.by_id.get(&id) else {
                break;
            };
            let entry = &self.entries[*index];
            path.push(entry);
            current = entry.parent_id;
        }
        path.reverse();
        path
    }

    pub fn context_items(&self) -> Vec<Value> {
        self.branch()
            .into_iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Branch { .. } => None,
                EntryKind::User { content } => Some(json!({
                    "role": "user",
                    "content": [{ "type": "input_text", "text": content }]
                })),
                EntryKind::ResponseItem { item } => Some(item.clone()),
                EntryKind::ToolOutput {
                    call_id, output, ..
                } => Some(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output
                })),
            })
            .collect()
    }

    pub fn tree_view(&self) -> Vec<TreeNodeView> {
        self.entries
            .iter()
            .map(|entry| TreeNodeView {
                id: entry.id.display(),
                parent_id: entry.parent_id.map(EntryId::display),
                label: entry.summary(),
                active: Some(entry.id) == self.leaf_id,
            })
            .collect()
    }

    pub fn transcript_items(&self) -> Vec<TranscriptItem> {
        self.branch()
            .into_iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Branch { label } => Some(TranscriptItem::Branch(label.clone())),
                EntryKind::User { content } => Some(TranscriptItem::User(content.clone())),
                EntryKind::ResponseItem { item } => {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        None
                    } else {
                        let text = extract_response_text(item);
                        (!text.trim().is_empty()).then_some(TranscriptItem::Assistant(text))
                    }
                }
                EntryKind::ToolOutput { name, output, .. } => Some(TranscriptItem::Tool {
                    name: name.clone(),
                    output: output.clone(),
                }),
            })
            .collect()
    }

    pub fn save_for_cwd(&mut self, profile_dir: &Path, cwd: &Path) -> io::Result<()> {
        self.updated_at = now_secs();
        let dir = sessions_dir(profile_dir, cwd);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", self.session_id));
        fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&self.to_json())?),
        )
    }

    pub fn load_for_cwd(profile_dir: &Path, cwd: &Path, session_id: &str) -> io::Result<Self> {
        let path = sessions_dir(profile_dir, cwd).join(format!("{session_id}.json"));
        let content = fs::read_to_string(path)?;
        let value = serde_json::from_str::<Value>(&content).unwrap_or_else(|_| json!({}));
        Self::from_json(&value)
    }

    pub fn list_for_cwd(profile_dir: &Path, cwd: &Path) -> io::Result<Vec<SessionSummary>> {
        let dir = sessions_dir(profile_dir, cwd);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(content) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&content) else {
                continue;
            };
            if let Some(summary) = Self::summary_from_json(&value) {
                sessions.push(summary);
            }
        }
        sessions.sort_by_key(|summary| Reverse(summary.updated_at));
        Ok(sessions)
    }

    fn to_json(&self) -> Value {
        json!({
            "version": 1,
            "id": self.session_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "leaf_id": self.leaf_id.map(|id| id.0),
            "next_id": self.next_id,
            "entries": self.entries.iter().map(entry_to_json).collect::<Vec<_>>()
        })
    }

    fn from_json(value: &Value) -> io::Result<Self> {
        let entries = value
            .get("entries")
            .and_then(Value::as_array)
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(entry_from_json)
            .collect::<Vec<_>>();
        let mut by_id = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            by_id.insert(entry.id, index);
        }

        let leaf_id = value
            .get("leaf_id")
            .and_then(Value::as_u64)
            .map(EntryId)
            .filter(|id| by_id.contains_key(id));
        let next_id = value
            .get("next_id")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| entries.iter().map(|entry| entry.id.0).max().unwrap_or(0) + 1);
        let session_id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown")
            .to_string();

        Ok(Self {
            entries,
            by_id,
            leaf_id,
            next_id,
            session_id,
            created_at: value.get("created_at").and_then(Value::as_u64).unwrap_or(0),
            updated_at: value.get("updated_at").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    fn summary_from_json(value: &Value) -> Option<SessionSummary> {
        let id = value.get("id")?.as_str()?.to_string();
        let updated_at = value.get("updated_at").and_then(Value::as_u64).unwrap_or(0);
        let entries = value
            .get("entries")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let leaf = value
            .get("leaf_id")
            .and_then(Value::as_u64)
            .map(|id| EntryId(id).display())
            .unwrap_or_else(|| "root".to_string());
        Some(SessionSummary {
            id,
            updated_at,
            entries,
            leaf,
        })
    }
}

impl SessionEntry {
    fn summary(&self) -> String {
        match &self.kind {
            EntryKind::Branch { label } => label.clone(),
            EntryKind::User { content } => format!("user: {}", truncate(content)),
            EntryKind::ResponseItem { item } => {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
                    format!("assistant tool_call: {name}")
                } else {
                    format!("assistant: {}", truncate(&extract_response_text(item)))
                }
            }
            EntryKind::ToolOutput { name, output, .. } => {
                format!("tool {name}: {}", truncate(output))
            }
        }
    }
}

fn truncate(text: &str) -> String {
    let compact = text.replace('\n', " ");
    let mut chars = compact.chars();
    let prefix = chars.by_ref().take(72).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn entry_to_json(entry: &SessionEntry) -> Value {
    let kind = match &entry.kind {
        EntryKind::Branch { label } => json!({ "type": "branch", "label": label }),
        EntryKind::User { content } => json!({ "type": "user", "content": content }),
        EntryKind::ResponseItem { item } => json!({ "type": "response_item", "item": item }),
        EntryKind::ToolOutput {
            call_id,
            name,
            output,
        } => json!({
            "type": "tool_output",
            "call_id": call_id,
            "name": name,
            "output": output
        }),
    };
    json!({
        "id": entry.id.0,
        "parent_id": entry.parent_id.map(|id| id.0),
        "kind": kind
    })
}

fn entry_from_json(value: &Value) -> Option<SessionEntry> {
    let id = EntryId(value.get("id")?.as_u64()?);
    let parent_id = value.get("parent_id").and_then(Value::as_u64).map(EntryId);
    let kind_value = value.get("kind")?;
    let kind_type = kind_value.get("type")?.as_str()?;
    let kind = match kind_type {
        "branch" => EntryKind::Branch {
            label: kind_value.get("label")?.as_str()?.to_string(),
        },
        "user" => EntryKind::User {
            content: kind_value.get("content")?.as_str()?.to_string(),
        },
        "response_item" => EntryKind::ResponseItem {
            item: kind_value.get("item")?.clone(),
        },
        "tool_output" => EntryKind::ToolOutput {
            call_id: kind_value.get("call_id")?.as_str()?.to_string(),
            name: kind_value.get("name")?.as_str()?.to_string(),
            output: kind_value.get("output")?.as_str()?.to_string(),
        },
        _ => return None,
    };

    Some(SessionEntry {
        id,
        parent_id,
        kind,
    })
}

fn sessions_dir(profile_dir: &Path, cwd: &Path) -> PathBuf {
    profile_dir.join("sessions").join(hash_path(cwd))
}

fn hash_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn new_session_id() -> String {
    let now = now_secs();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    format!("s{now:x}{nanos:08x}")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub fn extract_response_text(item: &Value) -> String {
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return String::new();
    };

    let mut text = String::new();
    for part in content {
        if let Some(value) = part.get("text").and_then(Value::as_str) {
            text.push_str(value);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appending_after_switch_creates_branch_without_rewriting_history() {
        let mut session = SessionStore::new();
        let first = session.append(EntryKind::User {
            content: "first".to_string(),
        });
        let _second = session.append(EntryKind::User {
            content: "second".to_string(),
        });

        session.switch_to(Some(first)).unwrap();
        let branch = session.append(EntryKind::User {
            content: "alternate".to_string(),
        });

        assert_eq!(session.leaf_id(), Some(branch));
        assert_eq!(session.branch().len(), 2);
        assert_eq!(session.entries.len(), 3);
    }

    #[test]
    fn context_projection_follows_active_leaf() {
        let mut session = SessionStore::new();
        let first = session.append(EntryKind::User {
            content: "first".to_string(),
        });
        session.append(EntryKind::User {
            content: "second".to_string(),
        });

        session.switch_to(Some(first)).unwrap();
        let context = session.context_items();

        assert_eq!(context.len(), 1);
    }
}
