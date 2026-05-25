use crate::event::{AgentEvent, TranscriptItem, TreeNodeView};
use crossterm::{
    cursor::{MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    style::ResetColor,
    terminal::{self, Clear, ClearType},
};
use std::{
    io::{self, Stdout, Write},
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(30);
const ANIMATION_INTERVAL: Duration = Duration::from_millis(240);
const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 128_000;
const COMMANDS: &[&str] = &[
    "/help", "/config", "/tree", "/switch", "/branch", "/resume", "/context", "/quit",
];
const CURSOR_MARKER: &str = "\x1b_jucode:cursor\x07";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const SYNC_START: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const RESET: &str = "\x1b[0m";

#[derive(Debug, Clone)]
enum ChatLine {
    Brand(String),
    User(String),
    Assistant(String),
    Tool { name: String, output: String },
    System(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiKind {
    Brand,
    User,
    Assistant,
    Tool,
    System,
    Error,
    Status,
    Selected,
    Input,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UiLine {
    kind: UiKind,
    text: String,
}

#[derive(Debug, Clone)]
struct UiDocument {
    history: Vec<UiLine>,
    controls: Vec<UiLine>,
    reset_screen: bool,
}

#[derive(Debug, Clone, Copy)]
struct CursorTarget {
    row: usize,
    column: usize,
}

struct RenderedFrame {
    history: Vec<String>,
    controls: Vec<String>,
    cursor: Option<CursorTarget>,
}

pub trait TuiRuntime {
    fn startup_events(&self) -> Vec<AgentEvent>;
    fn model_status_event(&self) -> AgentEvent;
    fn submit_user_message(&mut self, message: String) -> Vec<AgentEvent>;
    fn steer(&mut self) -> Vec<AgentEvent>;
    fn handle_command(&mut self, input: &str) -> (bool, Vec<AgentEvent>);
    fn poll_events(&mut self) -> Vec<AgentEvent>;
}

pub struct TuiApp<R> {
    runtime: R,
    input: String,
    chat: Vec<ChatLine>,
    live_assistant: Option<String>,
    status: String,
    provider: String,
    model: String,
    total_input_tokens: u64,
    total_output_tokens: u64,
    activity: ActivityState,
    completion_index: usize,
    tree_view: Option<TreeState>,
    pending_messages: Vec<String>,
    reset_screen: bool,
}

#[derive(Debug, Clone)]
struct TreeState {
    rows: Vec<TreeRow>,
    selected: usize,
}

#[derive(Debug, Clone)]
struct TreeRow {
    id: String,
    parent_id: Option<String>,
    depth: usize,
    label: String,
    active: bool,
}

#[derive(Debug, Clone)]
enum ActivityKind {
    Idle,
    Thinking,
    Output,
    Tool { name: String },
}

#[derive(Debug, Clone)]
struct ActivityState {
    kind: ActivityKind,
    turn_started_at: Option<Instant>,
    input_tokens: u64,
    output_tokens: u64,
    estimated_output_tokens: u64,
    animation_tick: usize,
}

struct TerminalGuard;

struct TerminalRenderer {
    previous_history: Vec<String>,
    previous_controls: Vec<String>,
    previous_width: u16,
    previous_height: u16,
    controls_top_row: u16,
    initialized: bool,
    force_full_redraw: bool,
}

impl<R: TuiRuntime> TuiApp<R> {
    pub fn new(runtime: R) -> Self {
        let mut app = Self {
            runtime,
            input: String::new(),
            chat: Vec::new(),
            live_assistant: None,
            status: "ready".to_string(),
            provider: "unknown".to_string(),
            model: "unknown".to_string(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            activity: ActivityState::idle(),
            completion_index: 0,
            tree_view: None,
            pending_messages: Vec::new(),
            reset_screen: false,
        };
        let events = app.runtime.startup_events();
        app.apply_events(events);
        app
    }

    pub fn run(mut self) -> io::Result<()> {
        let _guard = TerminalGuard::enter()?;
        let mut stdout = io::stdout();
        let mut renderer = TerminalRenderer::new();
        let mut next_animation_at = Instant::now() + ANIMATION_INTERVAL;

        loop {
            let events = self.runtime.poll_events();
            self.apply_events(events);
            self.apply_events(vec![self.runtime.model_status_event()]);

            let now = Instant::now();
            if self.activity.should_animate() && now >= next_animation_at {
                self.activity.advance_animation();
                next_animation_at = now + ANIMATION_INTERVAL;
            }

            let document = self.build_document();
            renderer.render(&mut stdout, &document)?;
            if document.reset_screen {
                self.reset_screen = false;
            }

            if event::poll(EVENT_POLL_INTERVAL)? {
                match event::read()? {
                    Event::Key(key)
                        if key.kind == KeyEventKind::Press
                            && self.handle_key(key.code, key.modifiers) =>
                    {
                        break;
                    }
                    Event::Resize(_, _) => renderer.force_full_redraw(),
                    _ => {}
                }
            }
        }

        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if self.tree_view.is_some() {
            return self.handle_tree_key(code, modifiers);
        }

        match code {
            KeyCode::Char('c') | KeyCode::Char('q')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                true
            }
            KeyCode::Char(ch)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.push(ch);
                self.clamp_completion_index();
                false
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.clamp_completion_index();
                false
            }
            KeyCode::Up if self.command_completion_active() => {
                let count = self.command_matches().len();
                if count > 0 {
                    self.completion_index = (self.completion_index + count - 1) % count;
                }
                false
            }
            KeyCode::Down if self.command_completion_active() => {
                let count = self.command_matches().len();
                if count > 0 {
                    self.completion_index = (self.completion_index + 1) % count;
                }
                false
            }
            KeyCode::Tab if self.command_completion_active() => {
                self.complete_selected_command();
                false
            }
            KeyCode::Esc => {
                if self.activity.should_animate() && !self.pending_messages.is_empty() {
                    let events = self.runtime.steer();
                    self.apply_events(events);
                    return false;
                }
                self.input.clear();
                self.completion_index = 0;
                false
            }
            KeyCode::Enter => {
                if self.should_complete_on_enter() {
                    self.complete_selected_command();
                    return false;
                }

                let submitted = self.input.trim().to_string();
                self.input.clear();
                self.completion_index = 0;
                if submitted.is_empty() {
                    return false;
                }

                if submitted.starts_with('/') {
                    let (quit, events) = self.runtime.handle_command(&submitted);
                    self.apply_events(events);
                    return quit;
                }

                let events = self.runtime.submit_user_message(submitted);
                self.apply_events(events);
                false
            }
            _ => false,
        }
    }

    fn handle_tree_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        match code {
            KeyCode::Char('c') | KeyCode::Char('q')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                true
            }
            KeyCode::Esc => {
                self.tree_view = None;
                false
            }
            KeyCode::Up => {
                if let Some(tree) = self.tree_view.as_mut() {
                    tree.move_previous();
                }
                false
            }
            KeyCode::Down => {
                if let Some(tree) = self.tree_view.as_mut() {
                    tree.move_next();
                }
                false
            }
            KeyCode::Left => {
                if let Some(tree) = self.tree_view.as_mut() {
                    tree.move_parent();
                }
                false
            }
            KeyCode::Right => {
                if let Some(tree) = self.tree_view.as_mut() {
                    tree.move_first_child();
                }
                false
            }
            KeyCode::Enter => {
                let Some(id) = self.tree_view.as_ref().and_then(TreeState::selected_id) else {
                    self.tree_view = None;
                    return false;
                };
                self.tree_view = None;
                let (_, events) = self.runtime.handle_command(&format!("/switch {id}"));
                self.apply_events(events);
                false
            }
            _ => false,
        }
    }

    fn apply_events(&mut self, events: Vec<AgentEvent>) {
        for event in events {
            match event {
                AgentEvent::Startup {
                    version,
                    profile_dir,
                    config_path,
                } => self.push_startup(version, profile_dir, config_path),
                AgentEvent::ModelStatus {
                    provider,
                    model,
                    state,
                } => {
                    self.provider = provider;
                    self.model = model;
                    self.apply_status(state);
                }
                AgentEvent::PendingMessages(messages) => self.pending_messages = messages,
                AgentEvent::UserMessage(message) => self.chat.push(ChatLine::User(message)),
                AgentEvent::ThinkingStart => self.activity.start_thinking(),
                AgentEvent::AssistantStart => self.live_assistant = Some(String::new()),
                AgentEvent::AssistantDelta(delta) => {
                    self.activity.add_output_delta(&delta);
                    self.append_assistant_delta(&delta);
                }
                AgentEvent::ToolStart { name } => self.activity.start_tool(name),
                AgentEvent::ToolOutput { name, output } => {
                    self.activity.start_thinking();
                    self.chat.push(ChatLine::Tool { name, output });
                }
                AgentEvent::Usage {
                    input_tokens,
                    output_tokens,
                } => {
                    self.total_input_tokens += input_tokens;
                    self.total_output_tokens += output_tokens;
                    self.activity.set_usage(input_tokens, output_tokens);
                }
                AgentEvent::TreeView(nodes) => self.tree_view = Some(TreeState::new(nodes)),
                AgentEvent::Transcript(items) => self.replace_transcript(items),
                AgentEvent::Info(message) => self.chat.push(ChatLine::System(message)),
                AgentEvent::Error(error) => {
                    self.commit_live_assistant();
                    self.activity.finish();
                    self.chat.push(ChatLine::Error(error));
                }
                AgentEvent::Status(status) => self.apply_status(status),
            }
        }
    }

    fn apply_status(&mut self, status: String) {
        if status == "ready" || status.starts_with("queued:") {
            self.commit_live_assistant();
        }
        if status == "ready" {
            self.activity.finish();
        }
        self.status = status;
    }

    fn append_assistant_delta(&mut self, delta: &str) {
        if let Some(text) = self.live_assistant.as_mut() {
            text.push_str(delta);
        } else {
            self.live_assistant = Some(delta.to_string());
        }
    }

    fn commit_live_assistant(&mut self) {
        let Some(text) = self.live_assistant.take() else {
            return;
        };
        if !text.trim().is_empty() {
            self.chat.push(ChatLine::Assistant(text));
        }
    }

    fn replace_transcript(&mut self, items: Vec<TranscriptItem>) {
        self.commit_live_assistant();
        self.chat = items
            .into_iter()
            .map(|item| match item {
                TranscriptItem::User(text) => ChatLine::User(text),
                TranscriptItem::Assistant(text) => ChatLine::Assistant(text),
                TranscriptItem::Tool { name, output } => ChatLine::Tool { name, output },
                TranscriptItem::Branch(label) => ChatLine::System(label),
            })
            .collect();
        self.reset_screen = true;
    }

    fn push_startup(&mut self, version: String, profile_dir: String, config_path: String) {
        let _ = config_path;
        self.chat
            .push(ChatLine::Brand(format!("JUCODE CLI v{version}")));
        self.chat
            .push(ChatLine::System(format!("profile {profile_dir}")));
    }

    fn build_document(&self) -> UiDocument {
        let command_matches = self.command_matches();
        UiBuilder::new()
            .chat(&self.chat)
            .live_assistant(self.live_assistant.as_deref())
            .tree(self.tree_view.as_ref())
            .pending_messages(&self.pending_messages)
            .top_status(&self.provider, &self.model, &self.status, &self.activity)
            .input(&self.input, &command_matches, self.completion_index)
            .bottom_status(self.total_input_tokens, self.total_output_tokens)
            .reset_screen(self.reset_screen)
            .finish()
    }

    fn command_completion_active(&self) -> bool {
        self.input.starts_with('/') && !self.command_matches().is_empty()
    }

    fn should_complete_on_enter(&self) -> bool {
        self.command_completion_active()
    }

    fn command_matches(&self) -> Vec<&'static str> {
        if !self.input.starts_with('/') {
            return Vec::new();
        }
        if COMMANDS.contains(&self.input.as_str()) {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .copied()
            .filter(|command| command.starts_with(self.input.as_str()))
            .collect()
    }

    fn clamp_completion_index(&mut self) {
        let count = self.command_matches().len();
        if count == 0 {
            self.completion_index = 0;
        } else if self.completion_index >= count {
            self.completion_index = count - 1;
        }
    }

    fn complete_selected_command(&mut self) {
        let matches = self.command_matches();
        if let Some(command) = matches.get(self.completion_index).copied() {
            self.input.clear();
            self.input.push_str(command);
            self.input.push(' ');
            self.completion_index = 0;
        }
    }
}

impl ActivityState {
    fn idle() -> Self {
        Self {
            kind: ActivityKind::Idle,
            turn_started_at: None,
            input_tokens: 0,
            output_tokens: 0,
            estimated_output_tokens: 0,
            animation_tick: 0,
        }
    }

    fn start_thinking(&mut self) {
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(Instant::now());
            self.input_tokens = 0;
            self.output_tokens = 0;
            self.estimated_output_tokens = 0;
            self.animation_tick = 0;
        }
        self.kind = ActivityKind::Thinking;
    }

    fn start_tool(&mut self, name: String) {
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(Instant::now());
        }
        self.kind = ActivityKind::Tool { name };
    }

    fn add_output_delta(&mut self, delta: &str) {
        self.kind = ActivityKind::Output;
        self.estimated_output_tokens += estimate_tokens(delta);
    }

    fn set_usage(&mut self, input_tokens: u64, output_tokens: u64) {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
    }

    fn finish(&mut self) {
        self.kind = ActivityKind::Idle;
        self.turn_started_at = None;
    }

    fn advance_animation(&mut self) {
        self.animation_tick = self.animation_tick.wrapping_add(1);
    }

    fn should_animate(&self) -> bool {
        !matches!(self.kind, ActivityKind::Idle)
    }

    fn line(&self, fallback_status: &str) -> String {
        let elapsed = self
            .turn_started_at
            .map(|start| format!("{:.1}s", start.elapsed().as_secs_f32()))
            .unwrap_or_else(|| "0.0s".to_string());
        let output_tokens = self.output_tokens.max(self.estimated_output_tokens);

        match &self.kind {
            ActivityKind::Idle => fallback_status.to_string(),
            ActivityKind::Thinking => format!(
                "{} thinking {} | in {} out {}",
                animation_frame(self.animation_tick),
                elapsed,
                self.input_tokens,
                output_tokens
            ),
            ActivityKind::Output => format!(
                "{} tokens | in {} out {}",
                animation_frame(self.animation_tick),
                self.input_tokens,
                output_tokens
            ),
            ActivityKind::Tool { name } => format!(
                "{} ! tool {} | in {} out {}",
                animation_frame(self.animation_tick),
                name,
                self.input_tokens,
                output_tokens
            ),
        }
    }
}

impl TreeState {
    fn new(nodes: Vec<TreeNodeView>) -> Self {
        let rows = build_tree_rows(&nodes);
        let selected = rows.iter().position(|row| row.active).unwrap_or(0);
        Self { rows, selected }
    }

    fn selected_id(&self) -> Option<String> {
        self.rows.get(self.selected).map(|row| row.id.clone())
    }

    fn move_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn move_next(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    fn move_parent(&mut self) {
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

    fn move_first_child(&mut self) {
        let Some(id) = self.rows.get(self.selected).map(|row| row.id.as_str()) else {
            return;
        };
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| row.parent_id.as_deref() == Some(id))
        {
            self.selected = index;
        }
    }
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), Show)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            MoveTo(
                0,
                terminal::size()
                    .map(|(_, height)| height.saturating_sub(1))
                    .unwrap_or(0)
            ),
            Clear(ClearType::CurrentLine),
            ResetColor,
            Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

struct UiBuilder {
    history: Vec<UiLine>,
    controls: Vec<UiLine>,
    reset_screen: bool,
}

impl UiBuilder {
    fn new() -> Self {
        Self {
            history: Vec::new(),
            controls: Vec::new(),
            reset_screen: false,
        }
    }

    fn chat(mut self, chat: &[ChatLine]) -> Self {
        for item in chat {
            match item {
                ChatLine::Brand(text) => self.push_history(UiKind::Brand, text),
                ChatLine::User(text) => self.push_history(UiKind::User, text),
                ChatLine::Assistant(text) => self.push_history(UiKind::Assistant, text),
                ChatLine::Tool { name, output } => {
                    self.history_line(UiKind::Tool, format!("* tool:{name}"));
                    self.push_history_with_prefix(UiKind::Tool, output, "  ");
                }
                ChatLine::System(text) => self.push_history(UiKind::System, text),
                ChatLine::Error(text) => self.push_history(UiKind::Error, text),
            }
            if !matches!(item, ChatLine::Brand(_)) {
                self.history_line(UiKind::System, String::new());
            }
        }
        self
    }

    fn live_assistant(mut self, text: Option<&str>) -> Self {
        if let Some(text) = text.filter(|value| !value.is_empty()) {
            self.push_control(UiKind::Assistant, text);
            self.control_line(UiKind::System, String::new());
        }
        self
    }

    fn tree(mut self, tree: Option<&TreeState>) -> Self {
        let Some(tree) = tree else {
            return self;
        };
        self.control_line(
            UiKind::Status,
            "tree: arrows move, enter switch, esc close".to_string(),
        );
        if tree.rows.is_empty() {
            self.control_line(UiKind::Status, "(empty session)".to_string());
            return self;
        }
        for (index, row) in tree.rows.iter().enumerate() {
            let connector = if row.depth == 0 { "" } else { "+- " };
            let prefix = format!("{}{}", "  ".repeat(row.depth), connector);
            let active = if row.active { " *" } else { "" };
            let marker = if index == tree.selected { "> " } else { "  " };
            let kind = if index == tree.selected {
                UiKind::Selected
            } else if row.active {
                UiKind::Brand
            } else {
                UiKind::Status
            };
            self.control_line(
                kind,
                format!("{marker}{prefix}{} {}{active}", row.id, row.label),
            );
        }
        self.control_line(UiKind::System, String::new());
        self
    }

    fn pending_messages(mut self, pending_messages: &[String]) -> Self {
        if pending_messages.is_empty() {
            return self;
        }
        self.control_line(
            UiKind::Status,
            "pending: esc steer, next turn sends automatically".to_string(),
        );
        for (index, message) in pending_messages.iter().enumerate() {
            self.control_line(
                UiKind::Status,
                format!("  {}. {}", index + 1, message.replace('\n', " ")),
            );
        }
        self.control_line(UiKind::System, String::new());
        self
    }

    fn top_status(
        mut self,
        provider: &str,
        model: &str,
        status: &str,
        activity: &ActivityState,
    ) -> Self {
        self.control_line(
            UiKind::Status,
            format!("{provider} / {model} | {}", activity.line(status)),
        );
        self
    }

    fn input(mut self, input: &str, command_matches: &[&str], selected_index: usize) -> Self {
        if input.starts_with('/') && !command_matches.is_empty() {
            for (index, command) in command_matches.iter().enumerate() {
                let marker = if index == selected_index { ">" } else { " " };
                self.control_line(UiKind::Status, format!("{marker} {command}"));
            }
        }
        self.control_line(UiKind::Input, format!("> {input}{CURSOR_MARKER}"));
        self
    }

    fn bottom_status(mut self, input_tokens: u64, output_tokens: u64) -> Self {
        let total = input_tokens + output_tokens;
        let percent = (total as f64 / DEFAULT_CONTEXT_WINDOW_TOKENS as f64 * 100.0).min(100.0);
        self.control_line(
            UiKind::Status,
            format!("tokens in {input_tokens} out {output_tokens} | context {percent:.1}%"),
        );
        self
    }

    fn reset_screen(mut self, reset_screen: bool) -> Self {
        self.reset_screen = reset_screen;
        self
    }

    fn finish(self) -> UiDocument {
        UiDocument {
            history: self.history,
            controls: self.controls,
            reset_screen: self.reset_screen,
        }
    }

    fn push_history(&mut self, kind: UiKind, text: &str) {
        if text.is_empty() {
            self.history_line(kind, String::new());
            return;
        }

        for line in text.lines() {
            self.history_line(kind, line.to_string());
        }
    }

    fn push_history_with_prefix(&mut self, kind: UiKind, text: &str, prefix: &str) {
        if text.is_empty() {
            self.history_line(kind, prefix.to_string());
            return;
        }

        for line in text.lines() {
            self.history_line(kind, format!("{prefix}{line}"));
        }
    }

    fn push_control(&mut self, kind: UiKind, text: &str) {
        if text.is_empty() {
            self.control_line(kind, String::new());
            return;
        }

        for line in text.lines() {
            self.control_line(kind, line.to_string());
        }
    }

    fn history_line(&mut self, kind: UiKind, text: String) {
        self.history.push(UiLine { kind, text });
    }

    fn control_line(&mut self, kind: UiKind, text: String) {
        self.controls.push(UiLine { kind, text });
    }
}

impl TerminalRenderer {
    fn new() -> Self {
        Self {
            previous_history: Vec::new(),
            previous_controls: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            controls_top_row: 0,
            initialized: false,
            force_full_redraw: false,
        }
    }

    fn force_full_redraw(&mut self) {
        self.force_full_redraw = true;
    }

    fn render(&mut self, stdout: &mut Stdout, document: &UiDocument) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        let width = width.max(1);
        let height = height.max(1);
        let frame = RenderedFrame::build(document, width);

        if document.reset_screen {
            self.reset_render(stdout, &frame)?;
        } else if !self.initialized {
            self.initial_render(stdout, &frame)?;
        } else if self.force_full_redraw || self.previous_width != width {
            self.full_render(stdout, &frame)?;
        } else {
            self.diff_render(stdout, &frame)?;
        }

        self.position_cursor(stdout, frame.cursor, width)?;
        stdout.flush()?;

        self.previous_history = frame.history;
        self.previous_controls = frame.controls;
        self.previous_width = width;
        self.previous_height = height;
        self.initialized = true;
        self.force_full_redraw = false;
        Ok(())
    }

    fn initial_render(&mut self, stdout: &mut Stdout, frame: &RenderedFrame) -> io::Result<()> {
        let mut buffer = render_buffer_start();
        append_lines_to_buffer(&mut buffer, &frame.history);
        if !frame.history.is_empty() && !frame.controls.is_empty() {
            buffer.push_str("\r\n");
        }
        append_lines_to_buffer(&mut buffer, &frame.controls);

        if frame.history.is_empty() && frame.controls.is_empty() {
            buffer.push(' ');
        }

        buffer.push_str(&render_buffer_end(false));
        stdout.write_all(buffer.as_bytes())?;
        stdout.flush()?;

        let (_, cursor_row) = crossterm::cursor::position()?;
        self.controls_top_row = controls_top_from_cursor(cursor_row, frame.controls.len());
        Ok(())
    }

    fn reset_render(&mut self, stdout: &mut Stdout, frame: &RenderedFrame) -> io::Result<()> {
        let mut buffer = render_buffer_start();
        buffer.push_str("\x1b[2J\x1b[H");
        append_lines_to_buffer(&mut buffer, &frame.history);
        if !frame.history.is_empty() && !frame.controls.is_empty() {
            buffer.push_str("\r\n");
        }
        append_lines_to_buffer(&mut buffer, &frame.controls);
        buffer.push_str(&render_buffer_end(false));
        stdout.write_all(buffer.as_bytes())?;
        stdout.flush()?;

        let (_, cursor_row) = crossterm::cursor::position()?;
        self.controls_top_row = controls_top_from_cursor(cursor_row, frame.controls.len());
        Ok(())
    }

    fn full_render(&self, stdout: &mut Stdout, frame: &RenderedFrame) -> io::Result<()> {
        let mut buffer = render_buffer_start();
        buffer.push_str(&format!("\x1b[{};1H\x1b[J", self.controls_top_row + 1));
        append_lines_to_buffer(&mut buffer, &frame.history);
        if !frame.history.is_empty() && !frame.controls.is_empty() {
            buffer.push_str("\r\n");
        }
        append_lines_to_buffer(&mut buffer, &frame.controls);
        buffer.push_str(&render_buffer_end(false));
        stdout.write_all(buffer.as_bytes())
    }

    fn diff_render(&mut self, stdout: &mut Stdout, frame: &RenderedFrame) -> io::Result<()> {
        if !is_prefix(&self.previous_history, &frame.history) {
            return self.full_render(stdout, frame);
        }

        let mut buffer = render_buffer_start();
        let new_history = &frame.history[self.previous_history.len()..];
        if !new_history.is_empty() {
            buffer.push_str(&format!("\x1b[{};1H\x1b[J", self.controls_top_row + 1));
            append_lines_to_buffer(&mut buffer, new_history);
            if !frame.controls.is_empty() {
                buffer.push_str("\r\n");
            }
            append_lines_to_buffer(&mut buffer, &frame.controls);
        } else if self.previous_controls != frame.controls {
            buffer.push_str(&format!("\x1b[{};1H\x1b[J", self.controls_top_row + 1));
            append_lines_to_buffer(&mut buffer, &frame.controls);
        }

        if new_history.is_empty() && self.previous_controls == frame.controls {
            buffer.push_str(&render_buffer_end(false));
            stdout.write_all(buffer.as_bytes())?;
            return Ok(());
        }

        buffer.push_str(&render_buffer_end(false));
        stdout.write_all(buffer.as_bytes())?;
        stdout.flush()?;

        let (_, cursor_row) = crossterm::cursor::position()?;
        self.controls_top_row = controls_top_from_cursor(cursor_row, frame.controls.len());
        Ok(())
    }

    fn position_cursor(
        &self,
        stdout: &mut Stdout,
        cursor: Option<CursorTarget>,
        width: u16,
    ) -> io::Result<()> {
        let Some(cursor) = cursor else {
            stdout.write_all(SHOW_CURSOR.as_bytes())?;
            return Ok(());
        };
        let column = cursor.column.min(width.saturating_sub(1) as usize);
        let terminal_row = self.controls_top_row as usize + cursor.row + 1;
        stdout.write_all(format!("\x1b[{terminal_row};{}H{SHOW_CURSOR}", column + 1).as_bytes())
    }
}

impl RenderedFrame {
    fn build(document: &UiDocument, width: u16) -> Self {
        let history = wrap_lines(&document.history, width as usize)
            .into_iter()
            .map(|line| render_ansi_line(&line))
            .collect();
        let mut controls = wrap_lines(&document.controls, width as usize);
        let cursor = extract_cursor(&mut controls);
        let controls = controls
            .into_iter()
            .map(|line| render_ansi_line(&line))
            .collect();

        Self {
            history,
            controls,
            cursor,
        }
    }
}

fn append_lines_to_buffer(buffer: &mut String, lines: &[String]) {
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            buffer.push_str("\r\n");
        }
        buffer.push_str(line);
    }
}

fn render_buffer_start() -> String {
    format!("{SYNC_START}{HIDE_CURSOR}")
}

fn render_buffer_end(show_cursor: bool) -> String {
    if show_cursor {
        format!("{SHOW_CURSOR}{SYNC_END}")
    } else {
        SYNC_END.to_string()
    }
}

fn controls_top_from_cursor(cursor_row: u16, controls_len: usize) -> u16 {
    cursor_row
        .saturating_add(1)
        .saturating_sub(controls_len.max(1) as u16)
}

fn is_prefix(previous: &[String], next: &[String]) -> bool {
    previous.len() <= next.len()
        && previous
            .iter()
            .zip(next)
            .all(|(previous, next)| previous == next)
}

fn build_tree_rows(nodes: &[TreeNodeView]) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    push_tree_rows(None, 0, nodes, &mut rows);
    rows
}

fn push_tree_rows(
    parent_id: Option<&str>,
    depth: usize,
    nodes: &[TreeNodeView],
    rows: &mut Vec<TreeRow>,
) {
    for node in nodes
        .iter()
        .filter(|node| node.parent_id.as_deref() == parent_id)
    {
        rows.push(TreeRow {
            id: node.id.clone(),
            parent_id: node.parent_id.clone(),
            depth,
            label: node.label.clone(),
            active: node.active,
        });
        push_tree_rows(Some(node.id.as_str()), depth + 1, nodes, rows);
    }
}

fn wrap_lines(lines: &[UiLine], width: usize) -> Vec<UiLine> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for line in lines {
        wrap_line(line, width, &mut wrapped);
    }
    wrapped
}

fn wrap_line(line: &UiLine, width: usize, output: &mut Vec<UiLine>) {
    if line.text.is_empty() {
        output.push(line.clone());
        return;
    }

    let mut current = String::new();
    let mut current_width = 0;
    let mut rest = line.text.as_str();

    while !rest.is_empty() {
        if let Some(next) = rest.strip_prefix(CURSOR_MARKER) {
            current.push_str(CURSOR_MARKER);
            rest = next;
            continue;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        let ch_width = ch.width().unwrap_or(0);
        if current_width > 0 && current_width + ch_width > width {
            output.push(UiLine {
                kind: line.kind,
                text: current,
            });
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
        rest = &rest[ch.len_utf8()..];
    }

    if !current.is_empty() {
        output.push(UiLine {
            kind: line.kind,
            text: current,
        });
    }
}

fn extract_cursor(lines: &mut [UiLine]) -> Option<CursorTarget> {
    for (row, line) in lines.iter_mut().enumerate().rev() {
        let Some(index) = line.text.find(CURSOR_MARKER) else {
            continue;
        };
        let before = &line.text[..index];
        let column = UnicodeWidthStr::width(before);
        line.text
            .replace_range(index..index + CURSOR_MARKER.len(), "");
        return Some(CursorTarget { row, column });
    }
    None
}

fn render_ansi_line(line: &UiLine) -> String {
    format!("{}{}{}", color_code(line.kind), line.text, RESET)
}

fn color_code(kind: UiKind) -> &'static str {
    match kind {
        UiKind::Brand => "\x1b[34m",
        UiKind::User => "\x1b[36m",
        UiKind::Assistant => "\x1b[37m",
        UiKind::Tool | UiKind::System | UiKind::Status => "\x1b[90m",
        UiKind::Error => "\x1b[31m",
        UiKind::Selected => "\x1b[97m",
        UiKind::Input => "\x1b[97m",
    }
}

fn animation_frame(tick: usize) -> &'static str {
    const FRAMES: [&str; 4] = ["[=   ]", "[==  ]", "[ ===]", "[  ==]"];
    FRAMES[tick % FRAMES.len()]
}

fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let chars = text.chars().count() as u64;
    u64::max(1, chars.div_ceil(4))
}
