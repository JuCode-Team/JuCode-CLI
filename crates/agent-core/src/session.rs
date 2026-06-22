use crate::{
    event::{TranscriptItem, TreeNodeView},
    tokens, tools,
};
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
const MAX_GOAL_OBJECTIVE_CHARS: usize = 4_000;
const SESSION_LABEL_MAX_CHARS: usize = 72;
const RESUME_SUMMARY_INPUT_MAX_CHARS: usize = 4_000;
const RESUME_SUMMARY_RECENT_ENTRIES: usize = 6;
const RESUME_SUMMARY_TOOL_TEXT_MAX_CHARS: usize = 240;

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
    /// Image attachments for the immediately preceding user message, stored as
    /// local file paths and read into `input_image` parts at projection time.
    UserImage {
        paths: Vec<String>,
    },
    ResponseItem {
        item: Value,
    },
    ToolOutput {
        call_id: String,
        name: String,
        output: String,
    },
    PinnedSkill {
        name: String,
        content: String,
    },
    GoalContext {
        content: String,
    },
    /// Marks that all branch entries up to and including `replaced_through` have
    /// been folded into `summary`. Projections emit the summary in place of those
    /// entries and keep everything after it verbatim.
    Compaction {
        summary: String,
        replaced_through: u64,
    },
}

#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// Rendered text of the prior summary plus the turns being folded.
    pub folded_text: String,
    /// Recent context items kept verbatim after the fold point.
    pub kept_items: Vec<Value>,
    /// Id of the last entry folded into the new summary.
    pub replaced_through: u64,
}

#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: EntryId,
    pub parent_id: Option<EntryId>,
    pub kind: EntryKind,
    pub created_at: u64,
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
    goal: Option<ThreadGoal>,
    resume_summary: Option<String>,
    resume_summary_status: Option<ThreadGoalStatus>,
    resume_summary_updated_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadGoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadGoal {
    pub objective: String,
    pub status: ThreadGoalStatus,
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
    pub time_used_seconds: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone)]
pub struct UserTurnView {
    pub id: String,
    pub content: String,
    pub created_at: u64,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub updated_at: u64,
    pub entries: usize,
    pub leaf: String,
    pub label: String,
    pub resume_summary: Option<String>,
    pub resume_status: Option<ThreadGoalStatus>,
    #[allow(dead_code)]
    pub resume_summary_updated_at: Option<u64>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct ContextProjection {
    pub items: Vec<Value>,
    pub branch_entries: usize,
    pub projected_entries: usize,
    pub compacted: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ContextEntryCounts {
    pub branches: usize,
    pub users: usize,
    pub assistant_responses: usize,
    pub tool_calls: usize,
    pub tool_outputs: usize,
    pub pinned_skills: usize,
    pub other_response_items: usize,
}

#[derive(Debug, Clone)]
pub struct ContextTopItem {
    pub label: String,
    pub tokens: usize,
    pub chars: usize,
}

#[derive(Debug, Clone)]
pub struct ContextStatistics {
    pub branch_entries: usize,
    pub context_items: usize,
    pub projected_items: usize,
    pub compacted: bool,
    pub tokens: usize,
    pub projected_tokens: usize,
    pub tokenizer: String,
    pub counts: ContextEntryCounts,
    pub top_items: Vec<ContextTopItem>,
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

    pub fn latest_user_message(&self) -> Option<&str> {
        self.branch()
            .into_iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::User { content } => Some(content.as_str()),
                _ => None,
            })
    }

    pub fn session_label(&self) -> String {
        self.latest_user_message()
            .map(|message| truncate_with_limit(message, SESSION_LABEL_MAX_CHARS))
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| self.session_id.clone())
    }

    pub fn resume_summary_input(&self) -> String {
        let mut rendered = self
            .branch()
            .into_iter()
            .rev()
            .filter_map(render_entry_for_resume_summary)
            .take(RESUME_SUMMARY_RECENT_ENTRIES)
            .collect::<Vec<_>>();
        rendered.reverse();
        let mut text = rendered.join("\n");
        if text.len() > RESUME_SUMMARY_INPUT_MAX_CHARS {
            text.truncate(RESUME_SUMMARY_INPUT_MAX_CHARS);
            text.push('…');
        }
        text
    }

    pub fn updated_at(&self) -> u64 {
        self.updated_at
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn goal(&self) -> Option<&ThreadGoal> {
        self.goal.as_ref()
    }

    pub fn set_goal_objective(
        &mut self,
        objective: &str,
        token_budget: Option<u64>,
    ) -> Result<ThreadGoal, String> {
        let objective = validate_goal_objective(objective)?;
        let now = now_secs();
        let goal = match self.goal.take() {
            Some(mut goal) => {
                goal.objective = objective;
                goal.status = ThreadGoalStatus::Active;
                goal.token_budget = token_budget;
                goal.updated_at = now;
                goal
            }
            None => ThreadGoal {
                objective,
                status: ThreadGoalStatus::Active,
                token_budget,
                tokens_used: 0,
                time_used_seconds: 0,
                created_at: now,
                updated_at: now,
            },
        };
        self.goal = Some(goal.clone());
        self.mark_state_changed();
        Ok(goal)
    }

    pub fn create_goal(
        &mut self,
        objective: &str,
        token_budget: Option<u64>,
    ) -> Result<ThreadGoal, String> {
        if self.goal.is_some() {
            return Err(
                "cannot create a new goal because this session already has a goal".to_string(),
            );
        }
        self.set_goal_objective(objective, token_budget)
    }

    pub fn set_goal_status(&mut self, status: ThreadGoalStatus) -> Result<ThreadGoal, String> {
        let Some(mut goal) = self.goal.take() else {
            return Err("cannot update goal because this session has no goal".to_string());
        };
        goal.status = status;
        goal.updated_at = now_secs();
        self.goal = Some(goal.clone());
        self.mark_state_changed();
        Ok(goal)
    }

    pub fn clear_goal(&mut self) -> bool {
        let cleared = self.goal.take().is_some();
        if cleared {
            self.mark_state_changed();
        }
        cleared
    }

    pub fn account_goal_usage(&mut self, elapsed_seconds: u64, tokens: u64) -> Option<ThreadGoal> {
        if elapsed_seconds == 0 && tokens == 0 {
            return self.goal.clone();
        }
        let mut goal = self.goal.take()?;
        if !matches!(
            goal.status,
            ThreadGoalStatus::Active | ThreadGoalStatus::BudgetLimited
        ) {
            self.goal = Some(goal.clone());
            return Some(goal);
        }
        goal.time_used_seconds = goal.time_used_seconds.saturating_add(elapsed_seconds);
        goal.tokens_used = goal.tokens_used.saturating_add(tokens);
        if goal.status == ThreadGoalStatus::Active
            && goal
                .token_budget
                .is_some_and(|budget| goal.tokens_used >= budget)
        {
            goal.status = ThreadGoalStatus::BudgetLimited;
        }
        goal.updated_at = now_secs();
        self.goal = Some(goal.clone());
        self.mark_state_changed();
        Some(goal)
    }

    pub fn append_goal_context(&mut self, content: String) {
        self.append(EntryKind::GoalContext { content });
    }

    pub fn set_resume_summary(
        &mut self,
        summary: Option<String>,
        status: Option<ThreadGoalStatus>,
        summarized_at: u64,
    ) {
        self.resume_summary = summary.filter(|value| !value.trim().is_empty());
        self.resume_summary_status = status;
        self.resume_summary_updated_at = Some(summarized_at);
    }

    #[allow(dead_code)]
    pub fn resume_summary(&self) -> Option<&str> {
        self.resume_summary.as_deref()
    }

    #[allow(dead_code)]
    pub fn resume_summary_status(&self) -> Option<ThreadGoalStatus> {
        self.resume_summary_status
    }

    pub fn resume_summary_updated_at(&self) -> Option<u64> {
        self.resume_summary_updated_at
    }

    pub fn clear_resume_summary(&mut self) {
        self.resume_summary = None;
        self.resume_summary_status = None;
        self.resume_summary_updated_at = None;
    }

    fn mark_state_changed(&mut self) {
        self.updated_at = now_secs();
        self.clear_resume_summary();
        self.needs_snapshot = true;
    }

    #[cfg(test)]
    pub fn leaf_id(&self) -> Option<EntryId> {
        self.leaf_id
    }

    /// User message content for an entry label (e.g. "e3"), if it names a user node.
    pub fn user_content(&self, label: &str) -> Option<String> {
        let id = parse_entry_id(label)?;
        match &self.entry(id)?.kind {
            EntryKind::User { content } => Some(content.clone()),
            _ => None,
        }
    }

    pub fn append(&mut self, kind: EntryKind) -> EntryId {
        let id = EntryId(self.next_id);
        self.next_id += 1;
        let entry = SessionEntry {
            id,
            parent_id: self.leaf_id,
            kind,
            created_at: now_secs(),
        };
        self.by_id.insert(id, self.entries.len());
        self.entries.push(entry);
        self.leaf_id = Some(id);
        self.update_head_for_entry(id);
        self.updated_at = now_secs();
        self.clear_resume_summary();
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
        self.clear_resume_summary();
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

    /// User messages on the active branch, oldest first — the rewindable points.
    pub fn user_turns(&self) -> Vec<UserTurnView> {
        self.branch()
            .into_iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::User { content } => Some(UserTurnView {
                    id: entry.id.display(),
                    content: content.clone(),
                    created_at: entry.created_at,
                }),
                _ => None,
            })
            .collect()
    }

    /// Creation time of a user-turn entry, if `id` names one.
    pub fn user_turn_created_at(&self, id: &str) -> Option<u64> {
        let id = parse_entry_id(id)?;
        let entry = self.entry(id)?;
        matches!(entry.kind, EntryKind::User { .. }).then_some(entry.created_at)
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

    /// Latest persisted compaction summary on the active branch and the branch
    /// index after which entries are kept verbatim.
    fn compaction_view(&self, branch: &[&SessionEntry]) -> (Option<String>, usize) {
        let mut summary = None;
        let mut keep_after = 0;
        for entry in branch {
            if let EntryKind::Compaction {
                summary: text,
                replaced_through,
            } = &entry.kind
            {
                summary = Some(text.clone());
                let target = EntryId(*replaced_through);
                keep_after = branch
                    .iter()
                    .position(|candidate| candidate.id == target)
                    .map(|position| position + 1)
                    .unwrap_or(0);
            }
        }
        (summary, keep_after)
    }

    /// Context items actually sent to the model: the latest compaction summary (if
    /// any) followed by every non-folded, non-compaction entry on the branch.
    pub fn request_context_items(&self) -> Vec<Value> {
        let branch = self.branch();
        let (summary, keep_after) = self.compaction_view(&branch);
        let mut items = Vec::new();
        if let Some(summary) = &summary {
            items.push(compaction_summary_item(summary));
        }
        for (index, entry) in branch.iter().enumerate() {
            if index < keep_after || matches!(entry.kind, EntryKind::Compaction { .. }) {
                continue;
            }
            if let Some(item) = context_item_for_entry(entry) {
                items.push(item);
                if let EntryKind::ToolOutput { output, .. } = &entry.kind {
                    if let Some(image) = crate::tools::image_content_item(output) {
                        items.push(image);
                    }
                }
            }
        }
        sanitize_tool_pairs(items)
    }

    #[cfg(test)]
    pub fn context_tokens(&self, model: &str) -> usize {
        self.context_token_usage(model).0
    }

    pub fn context_token_usage(&self, model: &str) -> (usize, String) {
        let items = self.request_context_items();
        let count = tokens::count_values(model, items.iter());
        (count.tokens, count.tokenizer)
    }

    /// Plan a compaction that folds older turns (keeping roughly the most recent
    /// `keep_recent_tokens` of context verbatim) into a single summary. Returns
    /// None when there is nothing older to fold.
    pub fn plan_compaction(
        &self,
        keep_recent_tokens: usize,
        model: &str,
    ) -> Option<CompactionPlan> {
        let branch = self.branch();
        let (summary, keep_after) = self.compaction_view(&branch);
        let kept: Vec<&SessionEntry> = branch
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                *index >= keep_after && !matches!(entry.kind, EntryKind::Compaction { .. })
            })
            .map(|(_, entry)| *entry)
            .collect();

        // Walk from the newest kept entry, keeping recent turns within the budget;
        // everything older is folded.
        let mut fold_end = kept.len();
        let mut tail_tokens = 0usize;
        for index in (0..kept.len()).rev() {
            let tokens = context_item_for_entry(kept[index])
                .map(|item| tokens::count_value(model, &item).tokens)
                .unwrap_or(0);
            if fold_end != kept.len() && tail_tokens + tokens > keep_recent_tokens {
                break;
            }
            tail_tokens += tokens;
            fold_end = index;
        }

        // Never split a tool group: if the kept tail would start with a
        // function_call_output whose function_call is being folded, fold that
        // output too (so it goes into the summary instead of becoming an orphan).
        let folded_calls: HashSet<&str> = kept[..fold_end]
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::ResponseItem { item }
                    if item.get("type").and_then(Value::as_str) == Some("function_call") =>
                {
                    item.get("call_id").and_then(Value::as_str)
                }
                _ => None,
            })
            .collect();
        while fold_end < kept.len() {
            let orphan_output = matches!(
                &kept[fold_end].kind,
                EntryKind::ToolOutput { call_id, .. } if folded_calls.contains(call_id.as_str())
            );
            if orphan_output {
                fold_end += 1;
            } else {
                break;
            }
        }

        let fold = &kept[..fold_end];
        let replaced_through = fold.last()?.id.0;

        let mut folded_text = String::new();
        if let Some(summary) = &summary {
            folded_text.push_str("Earlier summary:\n");
            folded_text.push_str(summary);
            folded_text.push_str("\n\n");
        }
        for entry in fold {
            if let Some(rendered) = render_entry_for_summary(entry) {
                folded_text.push_str(&rendered);
                folded_text.push('\n');
            }
        }

        let kept_items = sanitize_tool_pairs(
            kept[fold_end..]
                .iter()
                .filter_map(|entry| context_item_for_entry(entry))
                .collect(),
        );

        Some(CompactionPlan {
            folded_text,
            kept_items,
            replaced_through,
        })
    }

    pub fn apply_compaction(&mut self, summary: String, replaced_through: u64) -> EntryId {
        self.append(EntryKind::Compaction {
            summary,
            replaced_through,
        })
    }

    #[cfg(test)]
    pub fn context_projection(&self) -> ContextProjection {
        let items = self.request_context_items();
        ContextProjection {
            branch_entries: self.branch().len(),
            projected_entries: items.len(),
            compacted: self
                .branch()
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. })),
            items,
        }
    }

    pub fn context_statistics(&self, model: &str) -> ContextStatistics {
        let branch = self.branch();
        let items = self.request_context_items();
        let projected = tokens::count_values(model, items.iter());
        let compacted = branch
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. }));
        let mut counts = ContextEntryCounts::default();
        let mut context_items = 0usize;
        let mut token_count = 0usize;
        let mut top_items = Vec::new();

        for entry in &branch {
            count_context_entry(&entry.kind, &mut counts);
            let Some(item) = context_item_for_entry(entry) else {
                continue;
            };
            let item_text = item.to_string();
            let chars = item_text.len();
            let item_tokens = tokens::count_value(model, &item).tokens;
            context_items += 1;
            token_count += item_tokens;
            top_items.push(ContextTopItem {
                label: context_entry_label(entry),
                tokens: item_tokens,
                chars,
            });
        }

        top_items.sort_by_key(|item| std::cmp::Reverse(item.tokens));
        top_items.truncate(5);

        ContextStatistics {
            branch_entries: branch.len(),
            context_items,
            projected_items: items.len(),
            compacted,
            tokens: token_count,
            projected_tokens: projected.tokens,
            tokenizer: projected.tokenizer,
            counts,
            top_items,
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
                EntryKind::UserImage { paths } => {
                    (!paths.is_empty()).then(|| TranscriptItem::User(image_attachment_label(paths)))
                }
                EntryKind::PinnedSkill { .. } => None,
                EntryKind::GoalContext { .. } => None,
                EntryKind::Compaction { .. } => None,
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
            "branch_heads": self.branch_heads_json(),
            "goal": self.goal.as_ref().map(goal_to_json)
        })
    }

    fn summary_json(&self) -> Value {
        json!({
            "version": 3,
            "id": self.session_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "leaf_id": self.leaf_id.map(|id| id.0),
            "next_id": self.next_id,
            "branch_heads": self.branch_heads_json(),
            "entries_count": self.entries.len(),
            "label": self.session_label(),
            "resume_summary": self.resume_summary,
            "resume_status": self.resume_summary_status.map(ThreadGoalStatus::as_str),
            "resume_summary_updated_at": self.resume_summary_updated_at,
            "goal": self.goal.as_ref().map(goal_to_json),
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
            goal: value.get("goal").and_then(goal_from_json),
            resume_summary: value
                .get("resume_summary")
                .and_then(Value::as_str)
                .map(str::to_string),
            resume_summary_status: value
                .get("resume_status")
                .and_then(Value::as_str)
                .and_then(|status| ThreadGoalStatus::try_from(status).ok()),
            resume_summary_updated_at: value
                .get("resume_summary_updated_at")
                .and_then(Value::as_u64),
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
        if value.get("goal").is_some() {
            self.goal = value.get("goal").and_then(goal_from_json);
        }
        self.resume_summary = value
            .get("resume_summary")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.resume_summary_status = value
            .get("resume_status")
            .and_then(Value::as_str)
            .and_then(|status| ThreadGoalStatus::try_from(status).ok());
        self.resume_summary_updated_at = value
            .get("resume_summary_updated_at")
            .and_then(Value::as_u64);
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
        let label = value
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| id.clone());
        let resume_summary = value
            .get("resume_summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let resume_status = value
            .get("resume_status")
            .and_then(Value::as_str)
            .and_then(|status| ThreadGoalStatus::try_from(status).ok());
        let resume_summary_updated_at = value
            .get("resume_summary_updated_at")
            .and_then(Value::as_u64);
        Some(SessionSummary {
            id,
            updated_at,
            entries,
            leaf,
            label,
            resume_summary,
            resume_status,
            resume_summary_updated_at,
        })
    }
}

fn write_json_line(file: &mut fs::File, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *file, value)?;
    file.write_all(b"\n")
}

/// Drop tool-call items that would be invalid input: a `function_call_output`
/// without its `function_call`, or a `function_call` without its output. The
/// Responses API rejects either, and context assembly (compaction, checkout to a
/// mid-turn point, partial saves) can otherwise leave such orphans.
fn sanitize_tool_pairs(items: Vec<Value>) -> Vec<Value> {
    let mut calls = HashSet::new();
    let mut outputs = HashSet::new();
    for item in &items {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    calls.insert(id.to_string());
                }
            }
            Some("function_call_output") => {
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    outputs.insert(id.to_string());
                }
            }
            _ => {}
        }
    }
    items
        .into_iter()
        .filter(|item| {
            let call_id = item.get("call_id").and_then(Value::as_str);
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => call_id.is_some_and(|id| outputs.contains(id)),
                Some("function_call_output") => call_id.is_some_and(|id| calls.contains(id)),
                _ => true,
            }
        })
        .collect()
}

pub fn compaction_summary_item(summary: &str) -> Value {
    json!({
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!("[Earlier conversation compacted to a summary for context efficiency]\n\n{summary}")
        }]
    })
}

fn image_attachment_label(paths: &[String]) -> String {
    if paths.len() == 1 {
        "[image attached]".to_string()
    } else {
        format!("[{} images attached]", paths.len())
    }
}

fn render_entry_for_summary(entry: &SessionEntry) -> Option<String> {
    match &entry.kind {
        EntryKind::User { content } => Some(format!("User: {content}")),
        EntryKind::UserImage { paths } => Some(format!("User attached {} image(s)", paths.len())),
        EntryKind::ResponseItem { item } => {
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
                let args = item.get("arguments").and_then(Value::as_str).unwrap_or("");
                Some(format!("Assistant tool call {name}: {args}"))
            } else {
                let text = extract_response_text(item);
                if text.trim().is_empty() {
                    None
                } else {
                    Some(format!("Assistant: {text}"))
                }
            }
        }
        EntryKind::ToolOutput { name, output, .. } => Some(format!("Tool {name} output: {output}")),
        EntryKind::PinnedSkill { name, .. } => Some(format!("(pinned skill: {name})")),
        EntryKind::GoalContext { content } => Some(format!("Goal context: {content}")),
        EntryKind::Branch { .. } | EntryKind::Compaction { .. } => None,
    }
}

fn render_entry_for_resume_summary(entry: &SessionEntry) -> Option<String> {
    match &entry.kind {
        EntryKind::User { content } => Some(format!(
            "User: {}",
            single_line_with_limit(content, SESSION_LABEL_MAX_CHARS)
        )),
        EntryKind::UserImage { paths } => Some(format!("User attached {} image(s)", paths.len())),
        EntryKind::ResponseItem { item } => {
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
                let args = item.get("arguments").and_then(Value::as_str).unwrap_or("");
                Some(format!(
                    "Assistant tool call {name}: {}",
                    single_line_with_limit(args, RESUME_SUMMARY_TOOL_TEXT_MAX_CHARS)
                ))
            } else {
                let text = extract_response_text(item);
                let text = text.trim();
                if text.is_empty() {
                    None
                } else {
                    Some(format!(
                        "Assistant: {}",
                        single_line_with_limit(text, SESSION_LABEL_MAX_CHARS)
                    ))
                }
            }
        }
        EntryKind::ToolOutput { name, output, .. } => Some(format!(
            "Tool {name} output: {}",
            single_line_with_limit(output, RESUME_SUMMARY_TOOL_TEXT_MAX_CHARS)
        )),
        EntryKind::GoalContext { content } => Some(format!(
            "Goal context: {}",
            single_line_with_limit(content, SESSION_LABEL_MAX_CHARS)
        )),
        EntryKind::PinnedSkill { .. } | EntryKind::Branch { .. } | EntryKind::Compaction { .. } => {
            None
        }
    }
}

fn context_item_for_entry(entry: &SessionEntry) -> Option<Value> {
    match &entry.kind {
        EntryKind::Branch { .. } | EntryKind::Compaction { .. } => None,
        EntryKind::User { content } => Some(json!({
            "role": "user",
            "content": [{ "type": "input_text", "text": content }]
        })),
        EntryKind::UserImage { paths } => {
            let content = paths
                .iter()
                .filter_map(|path| tools::image_attachment_part(Path::new(path)).ok())
                .collect::<Vec<_>>();
            (!content.is_empty()).then(|| json!({ "role": "user", "content": content }))
        }
        EntryKind::ResponseItem { item } => Some(item.clone()),
        EntryKind::ToolOutput {
            call_id, output, ..
        } => Some(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output
        })),
        EntryKind::PinnedSkill { name, content } => Some(json!({
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": format!("Pinned skill for future turns: {name}\n\n{content}")
            }]
        })),
        EntryKind::GoalContext { content } => Some(json!({
            "role": "user",
            "content": [{ "type": "input_text", "text": content }]
        })),
    }
}

fn count_context_entry(kind: &EntryKind, counts: &mut ContextEntryCounts) {
    match kind {
        EntryKind::Branch { .. } => counts.branches += 1,
        EntryKind::User { .. } => counts.users += 1,
        EntryKind::ResponseItem { item } => {
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                counts.tool_calls += 1;
            } else if !extract_response_text(item).trim().is_empty() {
                counts.assistant_responses += 1;
            } else {
                counts.other_response_items += 1;
            }
        }
        EntryKind::ToolOutput { .. } => counts.tool_outputs += 1,
        EntryKind::PinnedSkill { .. } => counts.pinned_skills += 1,
        EntryKind::GoalContext { .. } => counts.users += 1,
        EntryKind::UserImage { .. } => {}
        EntryKind::Compaction { .. } => {}
    }
}

fn context_entry_label(entry: &SessionEntry) -> String {
    match &entry.kind {
        EntryKind::Branch { label } => format!("branch:{label}"),
        EntryKind::User { .. } => "user".to_string(),
        EntryKind::UserImage { .. } => "user_image".to_string(),
        EntryKind::ResponseItem { item } => match item.get("type").and_then(Value::as_str) {
            Some("function_call") => item
                .get("name")
                .and_then(Value::as_str)
                .map(|name| format!("tool call:{name}"))
                .unwrap_or_else(|| "tool call".to_string()),
            Some(kind) if !extract_response_text(item).trim().is_empty() => {
                format!("assistant:{kind}")
            }
            Some(kind) => format!("response item:{kind}"),
            None => "response item".to_string(),
        },
        EntryKind::ToolOutput { name, .. } => format!("tool output:{name}"),
        EntryKind::PinnedSkill { name, .. } => format!("pinned skill:{name}"),
        EntryKind::GoalContext { .. } => "goal context".to_string(),
        EntryKind::Compaction { .. } => "compaction".to_string(),
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
    truncate_with_limit(text, 72)
}

fn truncate_with_limit(text: &str, limit: usize) -> String {
    single_line_with_limit(text, limit)
}

fn single_line_with_limit(text: &str, limit: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = compact.chars();
    let prefix = chars.by_ref().take(limit).collect::<String>();
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
        EntryKind::UserImage { paths } => json!({ "type": "user_image", "paths": paths }),
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
        EntryKind::PinnedSkill { name, content } => {
            json!({ "type": "pinned_skill", "name": name, "content": content })
        }
        EntryKind::GoalContext { content } => json!({ "type": "goal_context", "content": content }),
        EntryKind::Compaction {
            summary,
            replaced_through,
        } => json!({
            "type": "compaction",
            "summary": summary,
            "replaced_through": replaced_through
        }),
    };
    json!({
        "id": entry.id.0,
        "parent_id": entry.parent_id.map(|id| id.0),
        "created_at": entry.created_at,
        "kind": kind
    })
}

fn entry_from_json(value: &Value) -> Option<SessionEntry> {
    let id = EntryId(value.get("id")?.as_u64()?);
    let parent_id = value.get("parent_id").and_then(Value::as_u64).map(EntryId);
    let created_at = value.get("created_at").and_then(Value::as_u64).unwrap_or(0);
    let kind_value = value.get("kind")?;
    let kind_type = kind_value.get("type")?.as_str()?;
    let kind = match kind_type {
        "branch" => EntryKind::Branch {
            label: kind_value.get("label")?.as_str()?.to_string(),
        },
        "user" => EntryKind::User {
            content: kind_value.get("content")?.as_str()?.to_string(),
        },
        "user_image" => EntryKind::UserImage {
            paths: kind_value
                .get("paths")?
                .as_array()?
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect(),
        },
        "response_item" => EntryKind::ResponseItem {
            item: kind_value.get("item")?.clone(),
        },
        "tool_output" => EntryKind::ToolOutput {
            call_id: kind_value.get("call_id")?.as_str()?.to_string(),
            name: kind_value.get("name")?.as_str()?.to_string(),
            output: kind_value.get("output")?.as_str()?.to_string(),
        },
        "pinned_skill" => EntryKind::PinnedSkill {
            name: kind_value.get("name")?.as_str()?.to_string(),
            content: kind_value.get("content")?.as_str()?.to_string(),
        },
        "goal_context" => EntryKind::GoalContext {
            content: kind_value.get("content")?.as_str()?.to_string(),
        },
        "compaction" => EntryKind::Compaction {
            summary: kind_value.get("summary")?.as_str()?.to_string(),
            replaced_through: kind_value.get("replaced_through")?.as_u64()?,
        },
        _ => return None,
    };

    Some(SessionEntry {
        id,
        parent_id,
        kind,
        created_at,
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

fn validate_goal_objective(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("goal objective must not be empty".to_string());
    }
    if value.chars().count() > MAX_GOAL_OBJECTIVE_CHARS {
        return Err(format!(
            "goal objective must be at most {MAX_GOAL_OBJECTIVE_CHARS} characters"
        ));
    }
    Ok(value.to_string())
}

impl ThreadGoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::UsageLimited => "usage_limited",
            Self::BudgetLimited => "budget_limited",
            Self::Complete => "complete",
        }
    }
}

impl TryFrom<&str> for ThreadGoalStatus {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "blocked" => Ok(Self::Blocked),
            "usage_limited" => Ok(Self::UsageLimited),
            "budget_limited" => Ok(Self::BudgetLimited),
            "complete" => Ok(Self::Complete),
            other => Err(format!("unknown goal status: {other}")),
        }
    }
}

fn goal_to_json(goal: &ThreadGoal) -> Value {
    json!({
        "objective": goal.objective,
        "status": goal.status.as_str(),
        "token_budget": goal.token_budget,
        "tokens_used": goal.tokens_used,
        "time_used_seconds": goal.time_used_seconds,
        "created_at": goal.created_at,
        "updated_at": goal.updated_at,
    })
}

fn goal_from_json(value: &Value) -> Option<ThreadGoal> {
    if value.is_null() {
        return None;
    }
    let objective = value.get("objective")?.as_str()?.to_string();
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .and_then(|value| ThreadGoalStatus::try_from(value).ok())
        .unwrap_or(ThreadGoalStatus::Active);
    Some(ThreadGoal {
        objective,
        status,
        token_budget: value.get("token_budget").and_then(Value::as_u64),
        tokens_used: value
            .get("tokens_used")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        time_used_seconds: value
            .get("time_used_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        created_at: value.get("created_at").and_then(Value::as_u64).unwrap_or(0),
        updated_at: value.get("updated_at").and_then(Value::as_u64).unwrap_or(0),
    })
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
    fn context_projection_reflects_persisted_compaction_only() {
        let mut session = SessionStore::new();
        for index in 0..6 {
            session.append(EntryKind::User {
                content: format!("turn {index} {}", "x".repeat(200)),
            });
        }

        let before = session.context_projection();
        assert!(!before.compacted);

        let plan = session
            .plan_compaction(80, "gpt-5")
            .expect("older turns should be foldable");
        session.apply_compaction("ROLLED UP SUMMARY".to_string(), plan.replaced_through);

        let after = session.context_projection();
        assert!(after.compacted);
        assert!(after.items.len() < before.items.len());
    }

    #[test]
    fn persisted_compaction_folds_older_turns_and_keeps_recent() {
        let mut session = SessionStore::new();
        for index in 0..6 {
            session.append(EntryKind::User {
                content: format!("turn {index} {}", "x".repeat(200)),
            });
        }
        let before_items = session.request_context_items().len();
        let before_tokens = session.context_tokens("gpt-5");

        let plan = session
            .plan_compaction(80, "gpt-5")
            .expect("older turns should be foldable");
        assert!(plan.folded_text.contains("User: turn 0"));
        session.apply_compaction("ROLLED UP SUMMARY".to_string(), plan.replaced_through);

        let items = session.request_context_items();
        assert!(items[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("ROLLED UP SUMMARY"));
        assert!(items.len() < before_items);
        assert!(session.context_tokens("gpt-5") < before_tokens);
    }

    #[test]
    fn user_content_resolves_only_user_nodes() {
        let mut session = SessionStore::new();
        let user = session.append(EntryKind::User {
            content: "please resend".to_string(),
        });
        let response = session.append(EntryKind::ResponseItem {
            item: json!({ "type": "message" }),
        });

        assert_eq!(
            session.user_content(&user.display()).as_deref(),
            Some("please resend")
        );
        assert_eq!(session.user_content(&response.display()), None);
        assert_eq!(session.user_content("nope"), None);
    }

    #[test]
    fn sanitize_drops_orphan_tool_call_items() {
        let items = vec![
            json!({ "type": "function_call", "call_id": "a", "name": "read", "arguments": "{}" }),
            json!({ "type": "function_call_output", "call_id": "a", "output": "ok" }),
            json!({ "type": "function_call_output", "call_id": "b", "output": "orphan" }),
            json!({ "type": "function_call", "call_id": "c", "name": "read", "arguments": "{}" }),
            json!({ "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }),
        ];

        let out = sanitize_tool_pairs(items);

        assert_eq!(out.len(), 3);
        assert!(out
            .iter()
            .any(|item| item["type"] == "function_call" && item["call_id"] == "a"));
        assert!(out
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "a"));
        assert!(!out.iter().any(|item| item["call_id"] == "b"));
        assert!(!out.iter().any(|item| item["call_id"] == "c"));
    }

    fn tool_pairs_balanced(items: &[Value]) -> bool {
        let calls: std::collections::HashSet<&str> = items
            .iter()
            .filter(|item| item["type"] == "function_call")
            .filter_map(|item| item["call_id"].as_str())
            .collect();
        items
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .all(|item| {
                item["call_id"]
                    .as_str()
                    .is_some_and(|id| calls.contains(id))
            })
    }

    #[test]
    fn compaction_never_splits_parallel_tool_groups() {
        let mut session = SessionStore::new();
        for turn in 0..4 {
            session.append(EntryKind::User {
                content: format!("turn {turn} {}", "x".repeat(60)),
            });
            session.append(EntryKind::ResponseItem {
                item: json!({ "type": "function_call", "call_id": format!("a{turn}"), "name": "read", "arguments": "{}" }),
            });
            session.append(EntryKind::ResponseItem {
                item: json!({ "type": "function_call", "call_id": format!("b{turn}"), "name": "read", "arguments": "{}" }),
            });
            session.append(EntryKind::ToolOutput {
                call_id: format!("a{turn}"),
                name: "read".to_string(),
                output: "x".repeat(60),
            });
            session.append(EntryKind::ToolOutput {
                call_id: format!("b{turn}"),
                name: "read".to_string(),
                output: "x".repeat(60),
            });
            session.append(EntryKind::ResponseItem {
                item: json!({ "type": "message", "content": [{ "type": "output_text", "text": "ok" }] }),
            });
        }

        // Across many keep-recent budgets the fold boundary must never leave a
        // function_call_output without its function_call.
        for keep in [10usize, 30, 60, 120, 240, 400] {
            if let Some(plan) = session.plan_compaction(keep, "gpt-5") {
                assert!(
                    tool_pairs_balanced(&plan.kept_items),
                    "keep_recent={keep} produced an orphan tool output"
                );
            }
        }
    }

    #[test]
    fn persisted_compaction_survives_journal_round_trip() {
        let mut session = SessionStore::new();
        session.append(EntryKind::User {
            content: "older".to_string(),
        });
        let replaced = session.append(EntryKind::User {
            content: "boundary".to_string(),
        });
        session.append(EntryKind::User {
            content: "recent".to_string(),
        });
        session.apply_compaction("SUMMARY".to_string(), replaced.0);

        let value = entry_to_json(session.branch().last().unwrap());
        let restored = entry_from_json(&value).expect("compaction entry round-trips");
        assert!(matches!(restored.kind, EntryKind::Compaction { .. }));
    }

    #[test]
    fn user_image_attachment_survives_journal_round_trip() {
        let mut session = SessionStore::new();
        session.append(EntryKind::User {
            content: "look".to_string(),
        });
        session.append(EntryKind::UserImage {
            paths: vec!["/tmp/a.png".to_string(), "/tmp/b.png".to_string()],
        });

        let value = entry_to_json(session.branch().last().unwrap());
        let restored = entry_from_json(&value).expect("user_image entry round-trips");
        match restored.kind {
            EntryKind::UserImage { paths } => {
                assert_eq!(paths, vec!["/tmp/a.png".to_string(), "/tmp/b.png".to_string()]);
            }
            _ => panic!("expected UserImage entry"),
        }
    }

    #[test]
    fn user_image_with_missing_files_yields_no_context_item() {
        let entry = SessionEntry {
            id: EntryId(9),
            parent_id: None,
            kind: EntryKind::UserImage {
                paths: vec!["/nonexistent/never.png".to_string()],
            },
            created_at: 0,
        };
        assert!(context_item_for_entry(&entry).is_none());
    }

    #[test]
    fn context_statistics_reports_counts_and_largest_items() {
        let mut session = SessionStore::new();
        session.fork("work").unwrap();
        session.append(EntryKind::User {
            content: "please inspect the terminal renderer".to_string(),
        });
        session.append(EntryKind::ResponseItem {
            item: json!({
                "type": "message",
                "content": [{ "text": "I inspected the renderer." }]
            }),
        });
        session.append(EntryKind::ResponseItem {
            item: json!({
                "type": "function_call",
                "name": "read",
                "call_id": "call_1"
            }),
        });
        session.append(EntryKind::ToolOutput {
            call_id: "call_1".to_string(),
            name: "read".to_string(),
            output: "line\n".repeat(100),
        });
        session.append(EntryKind::PinnedSkill {
            name: "review".to_string(),
            content: "Be careful.".to_string(),
        });

        let stats = session.context_statistics("gpt-5");

        assert_eq!(stats.branch_entries, 6);
        assert_eq!(stats.context_items, 5);
        assert_eq!(stats.counts.branches, 1);
        assert_eq!(stats.counts.users, 1);
        assert_eq!(stats.counts.assistant_responses, 1);
        assert_eq!(stats.counts.tool_calls, 1);
        assert_eq!(stats.counts.tool_outputs, 1);
        assert_eq!(stats.counts.pinned_skills, 1);
        assert!(stats.tokens > 0);
        assert_eq!(stats.tokenizer, "gpt-5");
        assert_eq!(stats.top_items[0].label, "tool output:read");
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
    fn pinned_skill_is_context_but_not_transcript() {
        let mut session = SessionStore::new();
        session.append(EntryKind::PinnedSkill {
            name: "review".to_string(),
            content: "Review carefully.".to_string(),
        });

        let context = session.context_items();

        assert!(context[0].to_string().contains("Pinned skill"));
        assert!(session.transcript_items().is_empty());
    }

    #[test]
    fn goal_context_is_context_but_not_transcript() {
        let mut session = SessionStore::new();
        session.append_goal_context("<goal_context>Continue.</goal_context>".to_string());

        let context = session.context_items();

        assert!(context[0].to_string().contains("goal_context"));
        assert!(session.transcript_items().is_empty());
    }

    #[test]
    fn goal_persists_through_journal_state() {
        let root = test_dir("goal-journal");
        let profile = root.join("profile");
        let cwd = root.join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let mut session = SessionStore::new();
        let session_id = session.session_id().to_string();

        session
            .set_goal_objective("finish goal support", Some(100))
            .unwrap();
        session.save_for_cwd(&profile, &cwd).unwrap();
        let loaded = SessionStore::load_for_cwd(&profile, &cwd, &session_id).unwrap();

        let goal = loaded.goal().unwrap();
        assert_eq!(goal.objective, "finish goal support");
        assert_eq!(goal.token_budget, Some(100));
        assert_eq!(goal.status, ThreadGoalStatus::Active);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn goal_accounting_applies_budget_limit() {
        let mut session = SessionStore::new();
        session.set_goal_objective("finish", Some(10)).unwrap();

        let goal = session.account_goal_usage(5, 12).unwrap();

        assert_eq!(goal.tokens_used, 12);
        assert_eq!(goal.time_used_seconds, 5);
        assert_eq!(goal.status, ThreadGoalStatus::BudgetLimited);
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
