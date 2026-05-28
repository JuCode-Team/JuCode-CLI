use crate::event::{TranscriptItem, TreeNodeView};
use serde_json::{json, Value};
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const ROOT_BRANCH: &str = "root";

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
    branch_heads: HashMap<String, EntryId>,
    leaf_id: Option<EntryId>,
    next_id: u64,
    session_id: String,
    created_at: u64,
    updated_at: u64,
    persisted_entries: usize,
    needs_snapshot: bool,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub updated_at: u64,
    pub entries: usize,
    pub leaf: String,
}

#[derive(Debug, Clone)]
pub struct ContextProjection {
    pub items: Vec<Value>,
    pub branch_entries: usize,
    pub projected_entries: usize,
    pub compacted: bool,
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

    #[cfg(test)]
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
        self.update_head_for_entry(id);
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

    pub fn fork(&mut self, label: &str) -> Result<EntryId, String> {
        let label = normalize_branch_label(label)?;
        if label == ROOT_BRANCH {
            return Err("root is reserved".to_string());
        }
        if self.branch_heads.contains_key(&label) {
            return Err(format!("branch already exists: {label}"));
        }
        Ok(self.append(EntryKind::Branch { label }))
    }

    pub fn checkout(&mut self, label: &str) -> Result<(), String> {
        let label = normalize_branch_label(label)?;
        if let Some(id) = parse_entry_id(&label) {
            return self.checkout_before_user(id);
        }
        if label == ROOT_BRANCH {
            self.leaf_id = self.branch_heads.get(ROOT_BRANCH).copied();
            self.updated_at = now_secs();
            return Ok(());
        }
        let Some(id) = self.branch_heads.get(&label).copied() else {
            return Err(format!("branch not found: {label}"));
        };
        self.switch_to(Some(id))
    }

    fn checkout_before_user(&mut self, id: EntryId) -> Result<(), String> {
        let Some(entry) = self.entry(id) else {
            return Err(format!("entry {} not found", id.display()));
        };
        if !matches!(entry.kind, EntryKind::User { .. }) {
            return Err(format!("entry {} is not a user node", id.display()));
        }
        self.switch_to(entry.parent_id)
    }

    pub fn delete_branch(&mut self, label: &str) -> Result<(), String> {
        let label = normalize_branch_label(label)?;
        if label == ROOT_BRANCH {
            return Err("cannot delete root branch".to_string());
        }
        let Some(root_id) = self.branch_entry_id(&label) else {
            return Err(format!("branch not found: {label}"));
        };
        let parent_id = self.entry(root_id).and_then(|entry| entry.parent_id);
        let removed = self.descendant_ids(root_id);
        let active_removed = self
            .leaf_id
            .map(|id| removed.contains(&id))
            .unwrap_or(false);

        self.entries.retain(|entry| !removed.contains(&entry.id));
        self.rebuild_indexes();
        self.rebuild_branch_heads();
        if active_removed {
            self.leaf_id = parent_id.filter(|id| self.by_id.contains_key(id));
        }
        self.updated_at = now_secs();
        self.needs_snapshot = true;
        Ok(())
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

    pub fn context_projection(&self) -> ContextProjection {
        self.context_projection_with_budget(usize::MAX, usize::MAX)
    }

    pub fn context_projection_with_budget(
        &self,
        max_tokens: usize,
        keep_recent_tokens: usize,
    ) -> ContextProjection {
        let branch = self.branch();
        let mut entries = branch
            .iter()
            .filter_map(|entry| context_item_for_entry(entry).map(|item| (*entry, item)))
            .collect::<Vec<_>>();
        let full_tokens = estimate_items_tokens(entries.iter().map(|(_, item)| item));
        let compacted = full_tokens > max_tokens && entries.len() > 4;
        let items = if compacted {
            let mut tail = Vec::new();
            let mut tail_tokens = 0usize;
            while let Some((entry, item)) = entries.pop() {
                let tokens = estimate_item_tokens(&item);
                if !tail.is_empty() && tail_tokens + tokens > keep_recent_tokens {
                    entries.push((entry, item));
                    break;
                }
                tail_tokens += tokens;
                tail.push((entry, item));
            }
            tail.reverse();
            let summary = summarize_compacted_entries(entries.iter().map(|(entry, _)| *entry));
            let mut items = vec![json!({
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": format!("[CONVERSATION HISTORY SUMMARY - older turns compacted for context efficiency]\n\n{summary}")
                }]
            })];
            items.extend(tail.into_iter().map(|(_, item)| item));
            items
        } else {
            entries
                .into_iter()
                .map(|(_, item)| item)
                .collect::<Vec<_>>()
        };
        ContextProjection {
            branch_entries: branch.len(),
            projected_entries: items.len(),
            items,
            compacted,
        }
    }

    #[cfg(test)]
    pub fn context_items(&self) -> Vec<Value> {
        self.context_projection().items
    }

    pub fn tree_view(&self) -> Vec<TreeNodeView> {
        let active = self.active_user_id();
        self.entries
            .iter()
            .filter_map(|entry| {
                let EntryKind::User { content } = &entry.kind else {
                    return None;
                };
                Some(TreeNodeView {
                    id: entry.id.display(),
                    parent_id: self
                        .nearest_user_ancestor(entry.parent_id)
                        .map(EntryId::display),
                    label: truncate(content),
                    active: Some(entry.id) == active,
                })
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
        let journal_path = dir.join(format!("{}.jsonl", self.session_id));
        let mut journal = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal_path)?;
        if self.needs_snapshot || self.persisted_entries > self.entries.len() {
            write_json_line(
                &mut journal,
                &json!({ "type": "snapshot", "session": self.to_json() }),
            )?;
        } else {
            for entry in &self.entries[self.persisted_entries..] {
                write_json_line(
                    &mut journal,
                    &json!({ "type": "entry", "entry": entry_to_json(entry) }),
                )?;
            }
        }
        write_json_line(
            &mut journal,
            &json!({ "type": "state", "state": self.state_json() }),
        )?;
        self.persisted_entries = self.entries.len();
        self.needs_snapshot = false;

        let summary_path = dir.join(format!("{}.json", self.session_id));
        fs::write(
            summary_path,
            format!("{}\n", serde_json::to_string_pretty(&self.summary_json())?),
        )
    }

    pub fn load_for_cwd(profile_dir: &Path, cwd: &Path, session_id: &str) -> io::Result<Self> {
        let dir = sessions_dir(profile_dir, cwd);
        let journal_path = dir.join(format!("{session_id}.jsonl"));
        if journal_path.exists() {
            return Self::load_from_journal(&journal_path);
        }
        let path = dir.join(format!("{session_id}.json"));
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

    fn entry(&self, id: EntryId) -> Option<&SessionEntry> {
        self.by_id.get(&id).map(|index| &self.entries[*index])
    }

    fn branch_entry_id(&self, label: &str) -> Option<EntryId> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            EntryKind::Branch { label: entry_label } if entry_label == label => Some(entry.id),
            _ => None,
        })
    }

    fn descendant_ids(&self, root_id: EntryId) -> HashSet<EntryId> {
        let mut removed = HashSet::from([root_id]);
        loop {
            let before = removed.len();
            for entry in &self.entries {
                if entry
                    .parent_id
                    .map(|parent_id| removed.contains(&parent_id))
                    .unwrap_or(false)
                {
                    removed.insert(entry.id);
                }
            }
            if removed.len() == before {
                return removed;
            }
        }
    }

    fn update_head_for_entry(&mut self, id: EntryId) {
        let label = self.branch_name_for_entry(id);
        self.branch_heads.insert(label, id);
    }

    fn branch_name_for_entry(&self, id: EntryId) -> String {
        let mut current = Some(id);
        while let Some(id) = current {
            let Some(entry) = self.entry(id) else {
                break;
            };
            if let EntryKind::Branch { label } = &entry.kind {
                return label.clone();
            }
            current = entry.parent_id;
        }
        ROOT_BRANCH.to_string()
    }

    fn nearest_user_ancestor(&self, id: Option<EntryId>) -> Option<EntryId> {
        let mut current = id;
        while let Some(id) = current {
            let entry = self.entry(id)?;
            if matches!(entry.kind, EntryKind::User { .. }) {
                return Some(id);
            }
            current = entry.parent_id;
        }
        None
    }

    fn active_user_id(&self) -> Option<EntryId> {
        self.nearest_user_ancestor(self.leaf_id)
    }

    fn rebuild_indexes(&mut self) {
        self.by_id.clear();
        for (index, entry) in self.entries.iter().enumerate() {
            self.by_id.insert(entry.id, index);
        }
    }

    fn rebuild_branch_heads(&mut self) {
        self.branch_heads.clear();
        for entry in self.entries.clone() {
            self.update_head_for_entry(entry.id);
        }
    }

    fn to_json(&self) -> Value {
        let mut value = self.state_json();
        value["entries"] = json!(self.entries.iter().map(entry_to_json).collect::<Vec<_>>());
        value
    }

    fn state_json(&self) -> Value {
        json!({
            "version": 1,
            "id": self.session_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "leaf_id": self.leaf_id.map(|id| id.0),
            "next_id": self.next_id,
            "branch_heads": self.branch_heads_json()
        })
    }

    fn summary_json(&self) -> Value {
        json!({
            "version": 2,
            "id": self.session_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "leaf_id": self.leaf_id.map(|id| id.0),
            "next_id": self.next_id,
            "branch_heads": self.branch_heads_json(),
            "entries_count": self.entries.len(),
            "journal": format!("{}.jsonl", self.session_id)
        })
    }

    fn branch_heads_json(&self) -> Value {
        Value::Object(
            self.branch_heads
                .iter()
                .map(|(label, id)| (label.clone(), json!(id.0)))
                .collect::<serde_json::Map<_, _>>(),
        )
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

        let branch_heads = value
            .get("branch_heads")
            .and_then(Value::as_object)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|(label, id)| {
                        let id = EntryId(id.as_u64()?);
                        by_id.contains_key(&id).then_some((label.clone(), id))
                    })
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        let mut store = Self {
            entries,
            by_id,
            branch_heads,
            leaf_id,
            next_id,
            session_id,
            created_at: value.get("created_at").and_then(Value::as_u64).unwrap_or(0),
            updated_at: value.get("updated_at").and_then(Value::as_u64).unwrap_or(0),
            persisted_entries: 0,
            needs_snapshot: true,
        };
        if store.branch_heads.is_empty() {
            store.rebuild_branch_heads();
        }
        Ok(store)
    }

    fn load_from_journal(path: &Path) -> io::Result<Self> {
        let content = fs::read_to_string(path)?;
        let mut store = SessionStore::new();
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match record.get("type").and_then(Value::as_str) {
                Some("snapshot") => {
                    if let Some(session) = record.get("session") {
                        store = Self::from_json(session)?;
                    }
                }
                Some("entry") => {
                    if let Some(entry) = record.get("entry").and_then(entry_from_json) {
                        store.push_loaded_entry(entry);
                    }
                }
                Some("state") => {
                    if let Some(state) = record.get("state") {
                        store.apply_state_json(state);
                    }
                }
                _ => {}
            }
        }
        store.persisted_entries = store.entries.len();
        store.needs_snapshot = false;
        Ok(store)
    }

    fn push_loaded_entry(&mut self, entry: SessionEntry) {
        if self.by_id.contains_key(&entry.id) {
            return;
        }
        self.next_id = self.next_id.max(entry.id.0 + 1);
        self.by_id.insert(entry.id, self.entries.len());
        self.leaf_id = Some(entry.id);
        self.entries.push(entry.clone());
        self.update_head_for_entry(entry.id);
    }

    fn apply_state_json(&mut self, value: &Value) {
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            self.session_id = id.to_string();
        }
        if let Some(created_at) = value.get("created_at").and_then(Value::as_u64) {
            self.created_at = created_at;
        }
        if let Some(updated_at) = value.get("updated_at").and_then(Value::as_u64) {
            self.updated_at = updated_at;
        }
        if let Some(next_id) = value.get("next_id").and_then(Value::as_u64) {
            self.next_id = next_id;
        }
        self.leaf_id = value
            .get("leaf_id")
            .and_then(Value::as_u64)
            .map(EntryId)
            .filter(|id| self.by_id.contains_key(id));
        self.branch_heads = value
            .get("branch_heads")
            .and_then(Value::as_object)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|(label, id)| {
                        let id = EntryId(id.as_u64()?);
                        self.by_id.contains_key(&id).then_some((label.clone(), id))
                    })
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        if self.branch_heads.is_empty() {
            self.rebuild_branch_heads();
        }
    }

    fn summary_from_json(value: &Value) -> Option<SessionSummary> {
        let id = value.get("id")?.as_str()?.to_string();
        let updated_at = value.get("updated_at").and_then(Value::as_u64).unwrap_or(0);
        let entries = value
            .get("entries_count")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .or_else(|| value.get("entries").and_then(Value::as_array).map(Vec::len))
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

fn write_json_line(file: &mut fs::File, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *file, value)?;
    file.write_all(b"\n")
}

fn context_item_for_entry(entry: &SessionEntry) -> Option<Value> {
    match &entry.kind {
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
    }
}

fn estimate_items_tokens<'a>(items: impl Iterator<Item = &'a Value>) -> usize {
    items.map(estimate_item_tokens).sum()
}

fn estimate_item_tokens(item: &Value) -> usize {
    item.to_string().len().saturating_add(3) / 4
}

fn summarize_compacted_entries<'a>(entries: impl Iterator<Item = &'a SessionEntry>) -> String {
    let mut user_turns = 0usize;
    let mut assistant_turns = 0usize;
    let mut tool_calls = Vec::new();
    let mut recent_user = None;
    for entry in entries {
        match &entry.kind {
            EntryKind::User { content } => {
                user_turns += 1;
                recent_user = Some(truncate_summary_line(content));
            }
            EntryKind::ResponseItem { item } => {
                if !extract_response_text(item).trim().is_empty() {
                    assistant_turns += 1;
                }
            }
            EntryKind::ToolOutput { name, .. } => tool_calls.push(name.clone()),
            EntryKind::Branch { .. } => {}
        }
    }
    tool_calls.sort();
    tool_calls.dedup();
    let mut lines = vec![format!(
        "Compacted {user_turns} user turn(s) and {assistant_turns} assistant response(s)."
    )];
    if !tool_calls.is_empty() {
        lines.push(format!("Tools used earlier: {}.", tool_calls.join(", ")));
    }
    if let Some(recent_user) = recent_user {
        lines.push(format!("Most recent compacted user request: {recent_user}"));
    }
    lines.join("\n")
}

fn truncate_summary_line(text: &str) -> String {
    let compact = text.replace('\n', " ");
    let mut chars = compact.chars();
    let prefix = chars.by_ref().take(160).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn normalize_branch_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    if label.is_empty() {
        return Err("branch label is required".to_string());
    }
    if label.contains('\n') || label.contains('\r') {
        return Err("branch label must be a single line".to_string());
    }
    Ok(label.to_string())
}

fn parse_entry_id(text: &str) -> Option<EntryId> {
    let trimmed = text.trim().trim_start_matches('e');
    trimmed.parse::<u64>().ok().map(EntryId)
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

    #[test]
    fn context_projection_reports_branch_and_projected_entries() {
        let mut session = SessionStore::new();
        session.fork("work").unwrap();
        session.append(EntryKind::User {
            content: "first".to_string(),
        });

        let projection = session.context_projection();

        assert_eq!(projection.branch_entries, 2);
        assert_eq!(projection.projected_entries, 1);
        assert_eq!(projection.items.len(), 1);
    }

    #[test]
    fn context_projection_compacts_when_budget_is_exceeded() {
        let mut session = SessionStore::new();
        for index in 0..10 {
            session.append(EntryKind::User {
                content: format!("request {index} {}", "x".repeat(200)),
            });
            session.append(EntryKind::ResponseItem {
                item: json!({
                    "type": "message",
                    "content": [{ "text": format!("answer {index} {}", "y".repeat(200)) }]
                }),
            });
        }

        let projection = session.context_projection_with_budget(100, 80);

        assert!(projection.compacted);
        assert!(projection.items[0]
            .to_string()
            .contains("CONVERSATION HISTORY SUMMARY"));
        assert!(projection.items.len() < session.context_projection().items.len());
    }

    #[test]
    fn checkout_returns_to_branch_last_leaf() {
        let mut session = SessionStore::new();
        session.append(EntryKind::User {
            content: "root".to_string(),
        });
        session.fork("work").unwrap();
        session.append(EntryKind::User {
            content: "work 1".to_string(),
        });
        let work_leaf = session.append(EntryKind::User {
            content: "work 2".to_string(),
        });
        session.checkout(ROOT_BRANCH).unwrap();

        session.checkout("work").unwrap();

        assert_eq!(session.leaf_id(), Some(work_leaf));
    }

    #[test]
    fn checkout_entry_id_moves_to_before_user_message() {
        let mut session = SessionStore::new();
        let first = session.append(EntryKind::User {
            content: "first".to_string(),
        });
        let second = session.append(EntryKind::User {
            content: "second".to_string(),
        });

        session.checkout(&second.display()).unwrap();

        assert_eq!(session.leaf_id(), Some(first));
    }

    #[test]
    fn tree_view_shows_only_user_messages_with_user_parent_links() {
        let mut session = SessionStore::new();
        let first = session.append(EntryKind::User {
            content: "first prompt".to_string(),
        });
        session.append(EntryKind::ResponseItem {
            item: json!({
                "type": "message",
                "content": [{ "text": "assistant response" }]
            }),
        });
        let second = session.append(EntryKind::User {
            content: "second prompt".to_string(),
        });

        let tree = session.tree_view();

        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].id, first.display());
        assert_eq!(tree[0].label, "first prompt");
        assert_eq!(tree[1].id, second.display());
        assert_eq!(tree[1].parent_id, Some(first.display()));
        assert_eq!(tree[1].label, "second prompt");
    }

    #[test]
    fn delete_branch_removes_descendants_and_moves_active_leaf_to_parent() {
        let mut session = SessionStore::new();
        let root_leaf = session.append(EntryKind::User {
            content: "root".to_string(),
        });
        session.fork("feature").unwrap();
        session.append(EntryKind::User {
            content: "work".to_string(),
        });

        session.delete_branch("feature").unwrap();

        assert_eq!(session.leaf_id(), Some(root_leaf));
        assert_eq!(session.tree_view().len(), 1);
        assert!(session.checkout("feature").is_err());
    }

    #[test]
    fn save_writes_append_only_journal_and_small_summary() {
        let root = test_dir("session-journal");
        let profile = root.join("profile");
        let cwd = root.join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let mut session = SessionStore::new();
        let session_id = session.session_id().to_string();

        session.append(EntryKind::User {
            content: "first prompt".to_string(),
        });
        session.save_for_cwd(&profile, &cwd).unwrap();
        session.append(EntryKind::User {
            content: "second prompt".to_string(),
        });
        session.save_for_cwd(&profile, &cwd).unwrap();

        let dir = sessions_dir(&profile, &cwd);
        let summary = fs::read_to_string(dir.join(format!("{session_id}.json"))).unwrap();
        let journal = fs::read_to_string(dir.join(format!("{session_id}.jsonl"))).unwrap();
        let loaded = SessionStore::load_for_cwd(&profile, &cwd, &session_id).unwrap();

        assert!(summary.contains("\"entries_count\": 2"));
        assert!(!summary.contains("first prompt"));
        assert!(journal
            .lines()
            .any(|line| line.contains("\"type\":\"entry\"")));
        assert_eq!(loaded.branch().len(), 2);
        assert_eq!(loaded.transcript_items().len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn journal_snapshot_preserves_branch_deletions() {
        let root = test_dir("session-journal-delete");
        let profile = root.join("profile");
        let cwd = root.join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let mut session = SessionStore::new();
        let session_id = session.session_id().to_string();

        session.append(EntryKind::User {
            content: "root".to_string(),
        });
        session.fork("feature").unwrap();
        session.append(EntryKind::User {
            content: "branch only".to_string(),
        });
        session.save_for_cwd(&profile, &cwd).unwrap();
        session.delete_branch("feature").unwrap();
        session.save_for_cwd(&profile, &cwd).unwrap();

        let mut loaded = SessionStore::load_for_cwd(&profile, &cwd, &session_id).unwrap();

        assert_eq!(loaded.tree_view().len(), 1);
        assert!(loaded.checkout("feature").is_err());
        let _ = fs::remove_dir_all(root);
    }

    fn test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("jucode-{name}-{nanos}"))
    }
}
