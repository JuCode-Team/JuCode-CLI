use std::collections::HashSet;

use jucode_agent_core::{ModelOptionView, SessionListItemView, TreeNodeView};

use crate::format_token_count;

#[derive(Debug, Clone)]
pub(crate) struct PickerState {
    pub(crate) rows: Vec<PickerRow>,
    pub(crate) selected: usize,
    pub(crate) mode: PickerMode,
    tree: Option<TreeRows>,
    pub(crate) efforts: Vec<String>,
    pub(crate) selected_effort: usize,
    pub(crate) prompt: Option<TreePrompt>,
}

#[derive(Debug, Clone)]
struct TreeRows {
    all_rows: Vec<PickerRow>,
    expanded: HashSet<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct PickerRow {
    pub(crate) id: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) depth: usize,
    pub(crate) prefix: String,
    pub(crate) label: String,
    pub(crate) active: bool,
    pub(crate) has_children: bool,
    pub(crate) detail: String,
    pub(crate) reasoning_efforts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerMode {
    Checkout,
    Resume,
    Model,
}

#[derive(Debug, Clone)]
pub(crate) struct TreePrompt {
    pub(crate) action: TreePromptAction,
    pub(crate) input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreePromptAction {
    Fork,
    Delete,
}

impl PickerState {
    pub(crate) fn checkout(nodes: Vec<TreeNodeView>) -> Self {
        let all_rows = build_tree_rows(&nodes);
        let expanded = adaptive_expanded_rows(&all_rows);
        let rows = visible_tree_rows(&all_rows, &expanded);
        let selected = rows.iter().position(|row| row.active).unwrap_or(0);
        Self {
            rows,
            selected,
            mode: PickerMode::Checkout,
            tree: Some(TreeRows { all_rows, expanded }),
            efforts: Vec::new(),
            selected_effort: 0,
            prompt: None,
        }
    }

    pub(crate) fn resume(sessions: Vec<SessionListItemView>) -> Self {
        let rows = sessions
            .into_iter()
            .map(|session| PickerRow {
                id: session.id,
                parent_id: None,
                depth: 0,
                prefix: String::new(),
                label: session.label,
                active: session.active,
                has_children: false,
                detail: String::new(),
                reasoning_efforts: Vec::new(),
            })
            .collect::<Vec<_>>();
        let selected = rows.iter().position(|row| row.active).unwrap_or(0);
        Self {
            rows,
            selected,
            mode: PickerMode::Resume,
            tree: None,
            efforts: Vec::new(),
            selected_effort: 0,
            prompt: None,
        }
    }

    pub(crate) fn model(models: Vec<ModelOptionView>, active_effort: String) -> Self {
        let selected = models.iter().position(|row| row.active).unwrap_or(0);
        let efforts = models
            .get(selected)
            .map(|model| model.reasoning_efforts.clone())
            .unwrap_or_default();
        let selected_effort = efforts
            .iter()
            .position(|effort| effort == &active_effort)
            .unwrap_or(0);
        let rows = models
            .into_iter()
            .map(|model| PickerRow {
                id: model.model.clone(),
                parent_id: None,
                depth: 0,
                prefix: String::new(),
                label: model.model,
                active: model.active,
                has_children: false,
                detail: format!(
                    "ctx {} out {}",
                    format_token_count(model.context_window),
                    format_token_count(model.max_output_tokens)
                ),
                reasoning_efforts: model.reasoning_efforts,
            })
            .collect::<Vec<_>>();
        Self {
            rows,
            selected,
            mode: PickerMode::Model,
            tree: None,
            efforts,
            selected_effort,
            prompt: None,
        }
    }

    pub(crate) fn selected_id(&self) -> Option<String> {
        self.rows.get(self.selected).map(|row| row.id.clone())
    }

    pub(crate) fn selected_command(&self) -> Option<String> {
        let id = self.selected_id()?;
        match self.mode {
            PickerMode::Checkout => Some(format!("/checkout {id}")),
            PickerMode::Resume => Some(format!("/resume {id}")),
            PickerMode::Model => {
                let effort = self.efforts.get(self.selected_effort)?;
                Some(format!("/model {id} {effort}"))
            }
        }
    }

    pub(crate) fn is_expanded_tree_row(&self, id: &str) -> bool {
        self.tree
            .as_ref()
            .is_some_and(|tree| tree.expanded.contains(id))
    }

    pub(crate) fn begin_tree_prompt(&mut self, action: TreePromptAction) {
        if self.mode != PickerMode::Checkout {
            return;
        }
        self.prompt = Some(TreePrompt {
            action,
            input: String::new(),
        });
    }

    pub(crate) fn cancel_prompt(&mut self) {
        self.prompt = None;
    }

    pub(crate) fn push_prompt_char(&mut self, ch: char) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.input.push(ch);
        }
    }

    pub(crate) fn pop_prompt_char(&mut self) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.input.pop();
        }
    }

    pub(crate) fn take_prompt_command(&mut self) -> Option<String> {
        let prompt = self.prompt.take()?;
        let label = prompt.input.trim();
        if label.is_empty() {
            return None;
        }
        match prompt.action {
            TreePromptAction::Fork => Some(format!("/fork {label}")),
            TreePromptAction::Delete => Some(format!("/delete {label}")),
        }
    }

    pub(crate) fn move_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.sync_selected_model_efforts();
    }

    pub(crate) fn move_next(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
        self.sync_selected_model_efforts();
    }

    pub(crate) fn move_parent(&mut self) {
        if self.mode != PickerMode::Checkout {
            return;
        }
        let Some(selected_id) = self.selected_id() else {
            return;
        };
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        if tree.expanded.remove(&selected_id) {
            self.rebuild_visible_rows(Some(selected_id));
            return;
        }
        let Some(parent_id) = self
            .rows
            .get(self.selected)
            .and_then(|row| row.parent_id.as_ref())
        else {
            return;
        };
        if let Some(index) = self.rows.iter().position(|row| &row.id == parent_id) {
            self.selected = index;
        }
    }

    pub(crate) fn move_first_child(&mut self) {
        if self.mode != PickerMode::Checkout {
            return;
        }
        let Some(id) = self.rows.get(self.selected).map(|row| row.id.as_str()) else {
            return;
        };
        let Some(tree) = self.tree.as_ref() else {
            return;
        };
        if self.has_children(id) && !tree.expanded.contains(id) {
            let selected_id = id.to_string();
            if let Some(tree) = self.tree.as_mut() {
                tree.expanded.insert(selected_id.clone());
            }
            self.rebuild_visible_rows(Some(selected_id));
            return;
        }
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| row.parent_id.as_deref() == Some(id))
        {
            self.selected = index;
        }
    }

    fn has_children(&self, id: &str) -> bool {
        let Some(tree) = self.tree.as_ref() else {
            return false;
        };
        tree.all_rows
            .iter()
            .any(|row| row.parent_id.as_deref() == Some(id))
    }

    fn rebuild_visible_rows(&mut self, selected_id: Option<String>) {
        let Some(tree) = self.tree.as_ref() else {
            return;
        };
        self.rows = visible_tree_rows(&tree.all_rows, &tree.expanded);
        if let Some(selected_id) = selected_id {
            if let Some(index) = self.rows.iter().position(|row| row.id == selected_id) {
                self.selected = index;
                return;
            }
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    pub(crate) fn cycle_effort(&mut self) {
        if self.mode != PickerMode::Model || self.efforts.is_empty() {
            return;
        }
        self.selected_effort = (self.selected_effort + 1) % self.efforts.len();
    }

    fn sync_selected_model_efforts(&mut self) {
        if self.mode != PickerMode::Model {
            return;
        }
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let current = self.efforts.get(self.selected_effort).cloned();
        self.efforts = row.reasoning_efforts.clone();
        self.selected_effort = current
            .and_then(|value| self.efforts.iter().position(|effort| effort == &value))
            .unwrap_or_else(|| {
                self.efforts
                    .iter()
                    .position(|effort| effort == "medium")
                    .unwrap_or(0)
            });
    }
}

fn build_tree_rows(nodes: &[TreeNodeView]) -> Vec<PickerRow> {
    let mut rows = Vec::new();
    push_tree_rows(None, 0, nodes, &mut rows);
    rows
}

/// Soft cap on rows revealed by default; deeper levels are revealed only while the
/// total visible-row count stays within it.
const TREE_VISIBLE_ROW_BUDGET: usize = 16;
/// Hard cap on how deep the default reveal goes, regardless of budget.
const TREE_MAX_REVEAL_DEPTH: usize = 6;

/// Picks how many levels to reveal by default. Sparse (mostly linear) histories
/// expand many levels; wide (heavily branched) ones stop earlier — chosen by
/// growing the revealed depth while the visible-row count stays within budget.
fn adaptive_expanded_rows(all_rows: &[PickerRow]) -> HashSet<String> {
    let max_depth = all_rows.iter().map(|row| row.depth).max().unwrap_or(0);
    // Always reveal two levels when the tree has any depth, then grow greedily.
    let mut reveal_depth = max_depth.min(1);
    let mut depth = 2;
    while depth <= max_depth.min(TREE_MAX_REVEAL_DEPTH) {
        let visible = all_rows.iter().filter(|row| row.depth <= depth).count();
        if visible <= TREE_VISIBLE_ROW_BUDGET {
            reveal_depth = depth;
            depth += 1;
        } else {
            break;
        }
    }
    all_rows
        .iter()
        .filter(|row| row.depth < reveal_depth)
        .map(|row| row.id.clone())
        .collect()
}

fn push_tree_rows(
    parent_id: Option<&str>,
    depth: usize,
    nodes: &[TreeNodeView],
    rows: &mut Vec<PickerRow>,
) {
    for node in nodes
        .iter()
        .filter(|node| node.parent_id.as_deref() == parent_id)
    {
        rows.push(PickerRow {
            id: node.id.clone(),
            parent_id: node.parent_id.clone(),
            depth,
            prefix: String::new(),
            label: node.label.clone(),
            active: node.active,
            has_children: nodes
                .iter()
                .any(|candidate| candidate.parent_id.as_deref() == Some(node.id.as_str())),
            detail: String::new(),
            reasoning_efforts: Vec::new(),
        });
        push_tree_rows(Some(node.id.as_str()), depth + 1, nodes, rows);
    }
}

fn visible_tree_rows(rows: &[PickerRow], expanded: &HashSet<String>) -> Vec<PickerRow> {
    let mut visible = Vec::new();
    push_visible_tree_rows(None, "", rows, expanded, &mut visible);
    visible
}

fn push_visible_tree_rows(
    parent_id: Option<&str>,
    ancestor_prefix: &str,
    rows: &[PickerRow],
    expanded: &HashSet<String>,
    visible: &mut Vec<PickerRow>,
) {
    let children = rows
        .iter()
        .filter(|row| row.parent_id.as_deref() == parent_id)
        .collect::<Vec<_>>();
    let child_count = children.len();
    for (index, row) in children.into_iter().enumerate() {
        let last = index + 1 == child_count;
        let connector = if last { "└── " } else { "├── " };
        let mut next = row.clone();
        next.prefix = format!("{ancestor_prefix}{connector}");
        visible.push(next);
        if expanded.contains(&row.id) {
            let branch = if last { "    " } else { "│   " };
            push_visible_tree_rows(
                Some(row.id.as_str()),
                &format!("{ancestor_prefix}{branch}"),
                rows,
                expanded,
                visible,
            );
        }
    }
}
