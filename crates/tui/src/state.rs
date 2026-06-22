use super::*;

pub(super) struct TuiState {
    pub(super) chat: Vec<ChatLine>,
    pub(super) history_revision: u64,
    pub(super) rendered_history_cache: RenderedHistoryCache,
    pub(super) live_assistant: Option<String>,
    pub(super) reasoning_index: Option<usize>,
    pub(super) thinking_tokens: u64,
    pub(super) status: String,
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) reasoning_effort: String,
    pub(super) context_window: u64,
    pub(super) max_output_tokens: u64,
    pub(super) reasoning_efforts: Vec<String>,
    pub(super) current_context_tokens: u64,
    pub(super) current_cost: f64,
    pub(super) activity: ActivityState,
    pub(super) commands: Vec<CommandCandidate>,
    pub(super) completion_index: usize,
    pub(super) picker_view: Option<PickerState>,
    pub(super) pending_messages: Vec<String>,
    pub(super) reset_screen: bool,
}

#[derive(Debug, Clone, Default)]
pub(super) struct RenderedHistoryCache {
    revision: u64,
    width: usize,
    lines: Vec<UiLine>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            chat: Vec::new(),
            history_revision: 0,
            rendered_history_cache: RenderedHistoryCache::default(),
            live_assistant: None,
            reasoning_index: None,
            thinking_tokens: 0,
            status: "ready".to_string(),
            provider: "unknown".to_string(),
            model: "unknown".to_string(),
            reasoning_effort: "medium".to_string(),
            context_window: 128_000,
            max_output_tokens: 128_000,
            reasoning_efforts: vec!["medium".to_string()],
            current_context_tokens: 0,
            current_cost: 0.0,
            activity: ActivityState::idle(),
            commands: default_commands(),
            completion_index: 0,
            picker_view: None,
            pending_messages: Vec::new(),
            reset_screen: false,
        }
    }
}

impl TuiState {
    pub(super) fn build_document(
        &mut self,
        input: &InputBuffer,
        width: usize,
        now: Instant,
    ) -> UiDocument {
        let content_width = padded_content_width(width);
        let control_width = width.max(1);
        let command_matches = self.command_matches(input);
        let input_display = input.render(!self.activity.is_active());
        let rendered_history_lines = self.rendered_history_lines(content_width);
        UiBuilder::new()
            .rendered_history_lines(rendered_history_lines)
            .live_assistant(self.live_assistant.as_deref(), content_width)
            .picker(self.picker_view.as_ref())
            .pending_messages(&self.pending_messages)
            .progress(&self.activity, self.thinking_tokens, now, control_width)
            .input(&input_display, &command_matches, self.completion_index)
            .bottom_status(
                BottomStatus {
                    provider: &self.provider,
                    model: &self.model,
                    reasoning_effort: &self.reasoning_effort,
                    context_tokens: self.current_context_tokens,
                    context_window: self.context_window,
                    cost: self.current_cost,
                },
                control_width,
            )
            .reset_screen(self.reset_screen)
            .finish()
    }

    pub(super) fn rendered_history_lines(&mut self, width: usize) -> Vec<UiLine> {
        if self.rendered_history_cache.revision != self.history_revision
            || self.rendered_history_cache.width != width
        {
            let history = UiBuilder::new()
                .chat_with_width(&self.chat, width)
                .into_history();
            self.rendered_history_cache = RenderedHistoryCache {
                revision: self.history_revision,
                width,
                lines: wrap_lines(&history, width),
            };
        }
        self.rendered_history_cache.lines.clone()
    }

    pub(super) fn command_completion_active(&self, input: &InputBuffer) -> bool {
        let text = input.text();
        !text.contains('\n') && text.starts_with('/') && !self.command_matches(input).is_empty()
    }

    pub(super) fn should_complete_on_enter(&self, input: &InputBuffer) -> bool {
        self.command_completion_active(input)
    }

    pub(super) fn command_matches(&self, input: &InputBuffer) -> Vec<CommandCandidate> {
        let input = input.text();
        if !input.starts_with('/') || input.contains('\n') {
            return Vec::new();
        }
        if self
            .commands
            .iter()
            .any(|candidate| candidate.command == input)
        {
            return Vec::new();
        }
        self.commands
            .iter()
            .filter(|candidate| candidate.command.starts_with(input.as_str()))
            .cloned()
            .collect()
    }

    pub(super) fn clamp_completion_index(&mut self, input: &InputBuffer) {
        let count = self.command_matches(input).len();
        if count == 0 {
            self.completion_index = 0;
        } else if self.completion_index >= count {
            self.completion_index = count - 1;
        }
    }

    pub(super) fn complete_selected_command(&mut self, input: &mut InputBuffer) {
        let matches = self.command_matches(input);
        if let Some(command) = matches.get(self.completion_index) {
            input.clear();
            input.push_text(&command.command);
            input.push_char(' ');
            self.completion_index = 0;
        }
    }
}

impl TuiState {
    pub(super) fn apply_events(
        &mut self,
        events: Vec<AgentEvent>,
        input: &mut InputBuffer,
    ) -> bool {
        let mut changed = false;
        for event in events {
            changed |= match event {
                AgentEvent::Startup {
                    version,
                    session_id: _,
                    profile_dir,
                    config_path,
                    cwd,
                    model,
                    context_window,
                } => {
                    self.push_startup(
                        version,
                        profile_dir,
                        config_path,
                        cwd,
                        model,
                        context_window,
                    );
                    true
                }
                AgentEvent::ModelStatus {
                    provider,
                    model,
                    reasoning_effort,
                    context_window,
                    max_output_tokens,
                    reasoning_efforts,
                    state,
                } => {
                    let changed = self.provider != provider
                        || self.model != model
                        || self.reasoning_effort != reasoning_effort
                        || self.context_window != context_window
                        || self.max_output_tokens != max_output_tokens
                        || self.reasoning_efforts != reasoning_efforts;
                    self.provider = provider;
                    self.model = model;
                    self.reasoning_effort = reasoning_effort;
                    self.context_window = context_window;
                    self.max_output_tokens = max_output_tokens;
                    self.reasoning_efforts = reasoning_efforts;
                    self.apply_status(state) || changed
                }
                AgentEvent::PendingMessages(messages) => {
                    let changed = self.pending_messages != messages;
                    self.pending_messages = messages;
                    changed
                }
                AgentEvent::UserMessage(message) => {
                    self.chat.push(ChatLine::User(message));
                    self.mark_history_dirty();
                    true
                }
                AgentEvent::FillInput(content) => {
                    input.clear();
                    input.push_text(&content);
                    self.completion_index = 0;
                    true
                }
                AgentEvent::Connecting => {
                    self.begin_reasoning_turn_if_idle();
                    self.activity.start_connecting();
                    true
                }
                AgentEvent::CompactionStart => {
                    self.activity.start_compacting();
                    self.chat.push(ChatLine::System(
                        "Compacting earlier conversation to free up context…".to_string(),
                    ));
                    self.mark_history_dirty();
                    true
                }
                AgentEvent::CompactionProgress { output_tokens } => {
                    self.activity.set_compaction_output_tokens(output_tokens);
                    true
                }
                AgentEvent::CompactionEnd => {
                    self.chat
                        .push(ChatLine::System("Context compacted.".to_string()));
                    self.mark_history_dirty();
                    true
                }
                AgentEvent::CompactionFailed(error) => {
                    self.chat.push(ChatLine::System(format!(
                        "Context compaction failed ({error}); continuing with full context."
                    )));
                    self.mark_history_dirty();
                    true
                }
                AgentEvent::ContextUsage { tokens, cost, .. } => {
                    let changed =
                        self.current_context_tokens != tokens || self.current_cost != cost;
                    self.current_context_tokens = tokens;
                    self.current_cost = cost;
                    changed
                }
                AgentEvent::ThinkingStart => {
                    self.begin_reasoning_turn_if_idle();
                    self.activity.start_thinking();
                    true
                }
                AgentEvent::ReasoningDelta(delta) => {
                    self.activity.start_thinking();
                    self.append_thinking_delta(&delta);
                    true
                }
                AgentEvent::Retrying { attempt } => {
                    // The request is re-sent from scratch, so drop any partial
                    // streamed output to avoid duplicating it on the retry.
                    self.live_assistant = None;
                    self.discard_partial_reasoning();
                    self.thinking_tokens = 0;
                    self.activity.start_reconnecting(attempt);
                    true
                }
                AgentEvent::AssistantStart => {
                    self.collapse_live_thinking();
                    self.live_assistant = Some(String::new());
                    true
                }
                AgentEvent::AssistantDelta(delta) => {
                    self.collapse_live_thinking();
                    self.activity.add_output_delta(&delta);
                    self.append_assistant_delta(&delta);
                    true
                }
                AgentEvent::ToolStart { call_id, name } => {
                    self.collapse_live_thinking();
                    self.activity.start_tool(name.clone());
                    self.upsert_tool(call_id, name, String::new(), true);
                    true
                }
                AgentEvent::ToolUpdate {
                    call_id,
                    name,
                    output,
                } => {
                    self.collapse_live_thinking();
                    self.activity.start_tool(name.clone());
                    self.upsert_tool(call_id, name, output, true);
                    true
                }
                AgentEvent::ToolOutput {
                    call_id,
                    name,
                    output,
                    ..
                } => {
                    self.collapse_live_thinking();
                    self.activity.start_connecting();
                    self.upsert_tool(call_id, name, output, false);
                    true
                }
                AgentEvent::SubagentLifecycle {
                    path,
                    status,
                    message,
                } => {
                    self.chat.push(ChatLine::System(format!(
                        "Agent {path}: {status} — {message}"
                    )));
                    self.mark_history_dirty();
                    true
                }
                AgentEvent::Usage {
                    input_tokens,
                    output_tokens,
                    reasoning_tokens,
                    ..
                } => {
                    let _ = (input_tokens, output_tokens);
                    self.record_reasoning_tokens(reasoning_tokens);
                    true
                }
                AgentEvent::TreeView(nodes) => {
                    self.picker_view = Some(PickerState::checkout(nodes));
                    true
                }
                AgentEvent::ResumeView(sessions) => {
                    self.picker_view = Some(PickerState::resume(sessions));
                    true
                }
                AgentEvent::CheckpointView(items) => {
                    self.picker_view = Some(PickerState::checkpoint(items));
                    true
                }
                AgentEvent::ApprovalRequest {
                    call_id,
                    name,
                    summary,
                } => {
                    self.picker_view = Some(PickerState::approval(call_id, name, summary));
                    true
                }
                AgentEvent::TrustPrompt { cwd, repo_root } => {
                    self.picker_view = Some(PickerState::trust(cwd, repo_root));
                    true
                }
                AgentEvent::ModelView {
                    models,
                    active_effort,
                } => {
                    self.picker_view = Some(PickerState::model(models, active_effort));
                    true
                }
                AgentEvent::CommandList(commands) => {
                    self.commands = commands.into_iter().map(CommandCandidate::from).collect();
                    self.clamp_completion_index(input);
                    true
                }
                AgentEvent::Goal(goal) => {
                    self.chat.push(ChatLine::System(format_goal_summary(goal)));
                    self.mark_history_dirty();
                    true
                }
                AgentEvent::Plan(items) => {
                    self.chat.push(ChatLine::System(format_plan_summary(&items)));
                    self.mark_history_dirty();
                    true
                }
                AgentEvent::Transcript(items) => {
                    self.replace_transcript(items);
                    true
                }
                AgentEvent::Info(message) => {
                    self.chat.push(ChatLine::System(message));
                    self.mark_history_dirty();
                    true
                }
                AgentEvent::Error(error) => {
                    self.collapse_live_thinking();
                    self.commit_live_assistant();
                    self.activity.finish();
                    self.chat.push(ChatLine::Error(error));
                    self.mark_history_dirty();
                    true
                }
                AgentEvent::Status(status) => self.apply_status(status),
            };
        }
        changed
    }

    pub(super) fn apply_status(&mut self, status: String) -> bool {
        let mut changed = self.status != status;
        if status == "ready" || status.starts_with("queued:") {
            // The reasoning message (collapsed) stays in the transcript, but the
            // above-input status indicator is reset once the reply is done.
            self.collapse_live_thinking();
            changed |= self.commit_live_assistant();
            changed |= self.thinking_tokens != 0;
            self.thinking_tokens = 0;
            self.reasoning_index = None;
        }
        if status == "ready" {
            let was_active = self.activity.is_active();
            self.activity.finish();
            changed |= was_active;
        }
        self.status = status;
        changed
    }

    pub(super) fn append_assistant_delta(&mut self, delta: &str) {
        if let Some(text) = self.live_assistant.as_mut() {
            text.push_str(delta);
        } else {
            self.live_assistant = Some(delta.to_string());
        }
    }

    /// Stream reasoning into a transcript message. A delta after the current
    /// reasoning message was collapsed starts a new one (e.g. a new phase after a
    /// tool call).
    pub(super) fn append_thinking_delta(&mut self, delta: &str) {
        if let Some(index) = self.reasoning_index {
            if let Some(ChatLine::Reasoning {
                text,
                collapsed: false,
            }) = self.chat.get_mut(index)
            {
                text.push_str(delta);
                self.mark_history_dirty();
                return;
            }
        }
        self.chat.push(ChatLine::Reasoning {
            text: delta.to_string(),
            collapsed: false,
        });
        self.reasoning_index = Some(self.chat.len() - 1);
        self.mark_history_dirty();
    }

    pub(super) fn begin_reasoning_turn_if_idle(&mut self) {
        if self.status == "ready" || !self.activity.is_active() {
            self.reset_thinking();
        }
    }

    /// Forget the current reasoning message and clear the token indicator (next turn).
    pub(super) fn reset_thinking(&mut self) {
        self.reasoning_index = None;
        self.thinking_tokens = 0;
    }

    /// Reasoning finished: collapse its transcript message to a short preview.
    pub(super) fn collapse_live_thinking(&mut self) {
        if let Some(index) = self.reasoning_index.take() {
            if let Some(ChatLine::Reasoning { collapsed, .. }) = self.chat.get_mut(index) {
                *collapsed = true;
                self.mark_history_dirty();
            }
        }
    }

    /// Drop a partial reasoning message before a retry re-streams it.
    pub(super) fn discard_partial_reasoning(&mut self) {
        if let Some(index) = self.reasoning_index.take() {
            if matches!(
                self.chat.get(index),
                Some(ChatLine::Reasoning {
                    collapsed: false,
                    ..
                })
            ) {
                if index + 1 == self.chat.len() {
                    self.chat.pop();
                    self.mark_history_dirty();
                } else if let Some(ChatLine::Reasoning { text, .. }) = self.chat.get_mut(index) {
                    text.clear();
                    self.mark_history_dirty();
                }
            }
        }
    }

    pub(super) fn commit_live_assistant(&mut self) -> bool {
        let Some(text) = self.live_assistant.take() else {
            return false;
        };
        if !text.trim().is_empty() {
            self.chat.push(ChatLine::Assistant(text));
            self.mark_history_dirty();
            return true;
        }
        true
    }

    /// Record reasoning tokens from response usage for the active thinking status.
    pub(super) fn record_reasoning_tokens(&mut self, reasoning_tokens: u64) {
        if reasoning_tokens > 0 {
            self.thinking_tokens = reasoning_tokens;
        }
    }

    pub(super) fn upsert_tool(
        &mut self,
        call_id: String,
        name: String,
        output: String,
        running: bool,
    ) {
        if let Some(ChatLine::Tool {
            name: existing_name,
            output: existing_output,
            running: existing_running,
            ..
        }) = self.chat.iter_mut().find(|line| {
            matches!(
                line,
                ChatLine::Tool {
                    call_id: Some(existing),
                    ..
                } if existing == &call_id
            )
        }) {
            *existing_name = name;
            *existing_output = output;
            *existing_running = running;
            self.mark_history_dirty();
            return;
        }

        self.chat.push(ChatLine::Tool {
            call_id: Some(call_id),
            name,
            output,
            running,
        });
        self.mark_history_dirty();
    }

    pub(super) fn replace_transcript(&mut self, items: Vec<TranscriptItem>) {
        self.commit_live_assistant();
        self.reset_thinking();
        self.chat = items
            .into_iter()
            .map(|item| match item {
                TranscriptItem::User(text) => ChatLine::User(text),
                TranscriptItem::Assistant(text) => ChatLine::Assistant(text),
                TranscriptItem::Tool { name, output } => ChatLine::Tool {
                    call_id: None,
                    name,
                    output,
                    running: false,
                },
                TranscriptItem::Branch(label) => ChatLine::System(label),
            })
            .collect();
        self.reset_screen = true;
        self.mark_history_dirty();
    }

    pub(super) fn push_startup(
        &mut self,
        version: String,
        profile_dir: String,
        config_path: String,
        cwd: String,
        model: String,
        context_window: u64,
    ) {
        self.chat.push(ChatLine::Startup {
            version,
            profile_dir,
            config_path,
            cwd,
            model,
            context_window,
        });
        self.mark_history_dirty();
    }

    pub(super) fn mark_history_dirty(&mut self) {
        self.history_revision = self.history_revision.wrapping_add(1);
    }
}
