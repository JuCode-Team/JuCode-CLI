use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    style::ResetColor,
    terminal::{self, Clear, ClearType},
};
use jucode_agent_core::{
    AgentEvent, CommandView, ModelOptionView, SessionListItemView, TranscriptItem, TreeNodeView,
};
use std::{
    collections::HashSet,
    io::{self, Stdout, Write},
    path::Path,
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(30);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);
const TOOL_OUTPUT_PREVIEW_LINES: usize = 12;
const TOOL_OUTPUT_PREVIEW_BYTES: usize = 2_000;
const PASTE_PLACEHOLDER_CHARS: usize = 200;
const PASTE_BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);
const PASTE_ENTER_SUPPRESS_WINDOW: Duration = Duration::from_millis(120);
#[cfg(not(windows))]
const PASTE_BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(8);
#[cfg(windows)]
const PASTE_BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(60);
const CURSOR_MARKER: &str = "\x1b_jucode:cursor\x07";
const VISIBLE_CURSOR: &str = "|";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const SHOW_HARDWARE_CURSOR: bool = false;
const DISABLE_SCROLL_ON_OUTPUT: &str = "\x1b[?1010l";
const ENABLE_SCROLL_ON_OUTPUT: &str = "\x1b[?1010h";
const ENABLE_BRACKETED_PASTE: &str = "\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &str = "\x1b[?2004l";
const SYNC_START: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const RESET: &str = "\x1b[0m";
const INVERSE_ON: &str = "\x1b[7m";
const INVERSE_OFF: &str = "\x1b[27m";
const BOX_BORDER: &str = "\x1b[90m";
const ANSI_ESCAPE: char = '\x1b';

#[derive(Debug, Clone)]
enum ChatLine {
    Startup {
        version: String,
        profile_dir: String,
        config_path: String,
    },
    User(String),
    Assistant(String),
    Tool {
        call_id: Option<String>,
        name: String,
        output: String,
        running: bool,
    },
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
    TreeDirectory,
    DiffAdd,
    DiffRemove,
    DiffHeader,
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
    lines: Vec<String>,
    cursor: Option<CursorTarget>,
}

#[derive(Debug, Clone)]
struct ProjectedDocument {
    transcript_lines: Vec<String>,
    active_lines: Vec<String>,
    cursor: Option<CursorTarget>,
}

struct BottomStatus<'a> {
    provider: &'a str,
    model: &'a str,
    reasoning_effort: &'a str,
    input_tokens: u64,
    output_tokens: u64,
    context_window: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandCandidate {
    command: String,
    marker: Option<String>,
}

impl From<CommandView> for CommandCandidate {
    fn from(value: CommandView) -> Self {
        Self {
            command: value.command,
            marker: value.marker,
        }
    }
}

fn default_commands() -> Vec<CommandCandidate> {
    [
        "/help", "/login", "/new", "/model", "/tree", "/resume", "/context", "/doctor", "/pin",
        "/goal", "/quit",
    ]
    .iter()
    .map(|command| CommandCandidate {
        command: (*command).to_string(),
        marker: None,
    })
    .collect()
}

fn format_goal_summary(goal: Option<jucode_agent_core::GoalView>) -> String {
    let Some(goal) = goal else {
        return "Goal\nNo goal set.\nCommands: /goal <objective>".to_string();
    };
    let mut lines = vec![
        "Goal".to_string(),
        format!("Status: {}", goal.status.replace('_', " ")),
        format!("Objective: {}", goal.objective),
        format!(
            "Time used: {}",
            format_elapsed_seconds(goal.time_used_seconds)
        ),
        format!("Tokens used: {}", format_compact_number(goal.tokens_used)),
    ];
    if let Some(token_budget) = goal.token_budget {
        lines.push(format!(
            "Token budget: {}",
            format_compact_number(token_budget)
        ));
    }
    let commands = match goal.status.as_str() {
        "active" => "Commands: /goal pause, /goal complete, /goal blocked, /goal clear",
        "paused" | "blocked" | "usage_limited" => {
            "Commands: /goal resume, /goal complete, /goal clear"
        }
        _ => "Commands: /goal <objective>, /goal clear",
    };
    lines.push(String::new());
    lines.push(commands.to_string());
    lines.join("\n")
}

fn format_elapsed_seconds(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if hours >= 24 {
        let days = hours / 24;
        let remaining_hours = hours % 24;
        return format!("{days}d {remaining_hours}h {remaining_minutes}m");
    }
    if remaining_minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {remaining_minutes}m")
    }
}

fn format_compact_number(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
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
    input: InputBuffer,
    paste_burst: PasteBurst,
    chat: Vec<ChatLine>,
    live_assistant: Option<String>,
    status: String,
    provider: String,
    model: String,
    reasoning_effort: String,
    context_window: u64,
    max_output_tokens: u64,
    reasoning_efforts: Vec<String>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    activity: ActivityState,
    commands: Vec<CommandCandidate>,
    completion_index: usize,
    picker_view: Option<PickerState>,
    pending_messages: Vec<String>,
    reset_screen: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct InputBuffer {
    chunks: Vec<InputChunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputChunk {
    Text(String),
    LargePaste(String),
}

impl InputBuffer {
    fn text(&self) -> String {
        self.chunks
            .iter()
            .map(|chunk| match chunk {
                InputChunk::Text(text) | InputChunk::LargePaste(text) => text.as_str(),
            })
            .collect()
    }

    fn display_text(&self) -> String {
        let mut display = String::new();
        for chunk in &self.chunks {
            match chunk {
                InputChunk::Text(text) => display.push_str(text),
                InputChunk::LargePaste(text) => {
                    let char_count = text.chars().count();
                    display.push_str(&format!("[Pasted: {char_count} chars]"));
                }
            }
        }
        display
    }

    fn clear(&mut self) {
        self.chunks.clear();
    }

    fn push_char(&mut self, ch: char) {
        self.push_text(&ch.to_string());
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.chunks.last_mut() {
            Some(InputChunk::Text(existing)) => existing.push_str(text),
            _ => self.chunks.push(InputChunk::Text(text.to_string())),
        }
    }

    fn push_paste(&mut self, text: &str) {
        let text = normalize_pasted_text(text);
        if text.chars().count() > PASTE_PLACEHOLDER_CHARS {
            self.chunks.push(InputChunk::LargePaste(text));
        } else {
            self.push_text(&text);
        }
    }

    fn pop(&mut self) {
        let Some(chunk) = self.chunks.last_mut() else {
            return;
        };
        match chunk {
            InputChunk::Text(text) => {
                text.pop();
                if text.is_empty() {
                    self.chunks.pop();
                }
            }
            InputChunk::LargePaste(_) => {
                self.chunks.pop();
            }
        }
    }
}

fn normalize_pasted_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn paste_burst_render_delay() -> Duration {
    PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(1)
}

#[derive(Debug, Clone, Default)]
struct PasteBurst {
    last_plain_char_at: Option<Instant>,
    pending_first_char: Option<(char, Instant)>,
    buffer: String,
    active: bool,
    burst_window_until: Option<Instant>,
}

enum PasteCharDecision {
    RetainFirstChar,
    BeginBufferFromPending,
    BufferAppend,
}

enum PasteFlush {
    Paste(String),
    Typed(char),
    None,
}

impl PasteBurst {
    fn on_plain_ascii_char(&mut self, ch: char, now: Instant) -> PasteCharDecision {
        let rapid = self
            .last_plain_char_at
            .map(|last| now.saturating_duration_since(last) <= PASTE_BURST_CHAR_INTERVAL)
            .unwrap_or(false);
        self.last_plain_char_at = Some(now);

        if self.active {
            self.extend_window(now);
            return PasteCharDecision::BufferAppend;
        }

        if rapid {
            if let Some((held, held_at)) = self.pending_first_char.take() {
                if now.saturating_duration_since(held_at) <= PASTE_BURST_CHAR_INTERVAL {
                    self.active = true;
                    self.buffer.push(held);
                    self.extend_window(now);
                    return PasteCharDecision::BeginBufferFromPending;
                }
            }
        }

        self.pending_first_char = Some((ch, now));
        PasteCharDecision::RetainFirstChar
    }

    fn append_char(&mut self, ch: char, now: Instant) {
        self.buffer.push(ch);
        self.extend_window(now);
    }

    fn append_newline_if_active(&mut self, now: Instant) -> bool {
        if !self.is_active() {
            return false;
        }
        if let Some((ch, _)) = self.pending_first_char.take() {
            self.buffer.push(ch);
        }
        self.buffer.push('\n');
        self.extend_window(now);
        true
    }

    fn newline_should_insert_instead_of_submit(&self, now: Instant) -> bool {
        self.is_active()
            || self
                .burst_window_until
                .map(|until| now <= until)
                .unwrap_or(false)
    }

    fn flush_if_due(&mut self, now: Instant) -> PasteFlush {
        let timeout = if self.is_buffering() {
            PASTE_BURST_IDLE_TIMEOUT
        } else {
            PASTE_BURST_CHAR_INTERVAL
        };
        let timed_out = self
            .last_plain_char_at
            .map(|last| now.saturating_duration_since(last) > timeout)
            .unwrap_or(false);
        if !timed_out {
            return PasteFlush::None;
        }

        if self.is_buffering() {
            self.active = false;
            return PasteFlush::Paste(std::mem::take(&mut self.buffer));
        }
        if let Some((ch, _)) = self.pending_first_char.take() {
            return PasteFlush::Typed(ch);
        }
        PasteFlush::None
    }

    fn flush_before_non_plain_input(&mut self) -> Option<String> {
        if !self.is_active() {
            if let Some((ch, _)) = self.pending_first_char.take() {
                return Some(ch.to_string());
            }
            return None;
        }

        self.active = false;
        let mut text = std::mem::take(&mut self.buffer);
        if let Some((ch, _)) = self.pending_first_char.take() {
            text.push(ch);
        }
        Some(text)
    }

    fn clear_after_non_char(&mut self) {
        self.last_plain_char_at = None;
        self.pending_first_char = None;
        self.burst_window_until = None;
        self.active = false;
    }

    fn clear_after_explicit_paste(&mut self) {
        self.last_plain_char_at = None;
        self.pending_first_char = None;
        self.burst_window_until = None;
        self.buffer.clear();
        self.active = false;
    }

    fn extend_window(&mut self, now: Instant) {
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    fn is_active(&self) -> bool {
        self.is_buffering() || self.pending_first_char.is_some()
    }

    fn is_buffering(&self) -> bool {
        self.active || !self.buffer.is_empty()
    }
}

#[derive(Debug, Clone)]
struct PickerState {
    rows: Vec<PickerRow>,
    selected: usize,
    mode: PickerMode,
    tree: Option<TreeRows>,
    efforts: Vec<String>,
    selected_effort: usize,
    prompt: Option<TreePrompt>,
}

#[derive(Debug, Clone)]
struct TreeRows {
    all_rows: Vec<PickerRow>,
    expanded: HashSet<String>,
}

#[derive(Debug, Clone)]
struct PickerRow {
    id: String,
    parent_id: Option<String>,
    depth: usize,
    prefix: String,
    label: String,
    active: bool,
    has_children: bool,
    detail: String,
    reasoning_efforts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerMode {
    Checkout,
    Resume,
    Model,
}

#[derive(Debug, Clone)]
struct TreePrompt {
    action: TreePromptAction,
    input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreePromptAction {
    Fork,
    Delete,
}

#[derive(Debug, Clone)]
enum ActivityKind {
    Idle,
    Connecting,
    Reconnecting { attempt: usize },
    Output,
    Tool,
}

#[derive(Debug, Clone)]
struct ActivityState {
    kind: ActivityKind,
    turn_started_at: Option<Instant>,
    phase_started_at: Option<Instant>,
    last_delta_at: Option<Instant>,
    estimated_output_tokens: u64,
    animation_tick: usize,
}

struct TerminalGuard;

struct TerminalRenderer {
    previous_lines: Vec<String>,
    previous_transcript_lines: Vec<String>,
    previous_width: u16,
    previous_height: u16,
    previous_viewport_top: usize,
    hardware_cursor_row: usize,
    initialized: bool,
    force_transcript_rebuild: bool,
}

#[derive(Debug, Clone)]
struct FrameScheduler {
    next_frame_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullRenderMode {
    FullHistory,
    VisibleViewport,
}

impl FrameScheduler {
    fn new(now: Instant) -> Self {
        Self {
            next_frame_at: Some(now),
        }
    }

    fn request_now(&mut self, now: Instant) {
        self.request_at(now);
    }

    fn request_in(&mut self, now: Instant, delay: Duration) {
        self.request_at(now + delay);
    }

    fn request_at(&mut self, when: Instant) {
        self.next_frame_at = Some(self.next_frame_at.map_or(when, |current| current.min(when)));
    }

    fn take_due(&mut self, now: Instant) -> bool {
        if self.next_frame_at.is_some_and(|when| now >= when) {
            self.next_frame_at = None;
            return true;
        }
        false
    }

    fn poll_timeout(&self, now: Instant, fallback: Duration) -> Duration {
        let Some(when) = self.next_frame_at else {
            return fallback;
        };
        fallback.min(when.saturating_duration_since(now))
    }
}

impl<R: TuiRuntime> TuiApp<R> {
    pub fn new(runtime: R) -> Self {
        let mut app = Self {
            runtime,
            input: InputBuffer::default(),
            paste_burst: PasteBurst::default(),
            chat: Vec::new(),
            live_assistant: None,
            status: "ready".to_string(),
            provider: "unknown".to_string(),
            model: "unknown".to_string(),
            reasoning_effort: "medium".to_string(),
            context_window: 128_000,
            max_output_tokens: 128_000,
            reasoning_efforts: vec!["medium".to_string()],
            total_input_tokens: 0,
            total_output_tokens: 0,
            activity: ActivityState::idle(),
            commands: default_commands(),
            completion_index: 0,
            picker_view: None,
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
        let now = Instant::now();
        let mut frames = FrameScheduler::new(now);
        let mut next_progress_at = now + PROGRESS_INTERVAL;

        loop {
            let now = Instant::now();
            let events = self.runtime.poll_events();
            if self.apply_events(events) {
                frames.request_now(now);
            }
            let status_event = self.runtime.model_status_event();
            if self.apply_events(vec![status_event]) {
                frames.request_now(now);
            }

            if self.activity.is_active() && now >= next_progress_at {
                self.activity.advance_animation();
                next_progress_at = now + PROGRESS_INTERVAL;
                frames.request_now(now);
            }

            if frames.take_due(now) {
                if self.handle_paste_burst_render_tick(now, &mut frames) {
                    continue;
                }
                let (width, _) = terminal::size()?;
                let width = width.max(1) as usize;
                let document = self.build_document(width, now);
                renderer.render(&mut stdout, &document)?;
                if document.reset_screen {
                    self.reset_screen = false;
                }
                continue;
            }

            let poll_timeout = {
                let mut timeout = frames.poll_timeout(now, EVENT_POLL_INTERVAL);
                if self.activity.is_active() {
                    timeout = timeout.min(next_progress_at.saturating_duration_since(now));
                }
                timeout
            };
            if event::poll(poll_timeout)? {
                loop {
                    let event_now = Instant::now();
                    match event::read()? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            let quit = self.handle_key_at(key.code, key.modifiers, event_now);
                            frames.request_now(event_now);
                            if self.paste_burst.is_active() {
                                frames.request_in(event_now, paste_burst_render_delay());
                            }
                            if quit {
                                return Ok(());
                            }
                        }
                        Event::Resize(_, _) => {
                            renderer.force_transcript_rebuild();
                            frames.request_now(event_now);
                        }
                        Event::Paste(text) => {
                            self.handle_paste(&text);
                            frames.request_now(event_now);
                        }
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
    }

    fn handle_key_at(&mut self, code: KeyCode, modifiers: KeyModifiers, now: Instant) -> bool {
        self.flush_paste_burst_if_due(now);

        if self.picker_view.is_some() {
            self.flush_paste_burst_before_non_plain_input();
            return self.handle_picker_key(code, modifiers);
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
                if ch.is_ascii() {
                    match self.paste_burst.on_plain_ascii_char(ch, now) {
                        PasteCharDecision::RetainFirstChar => {}
                        PasteCharDecision::BeginBufferFromPending
                        | PasteCharDecision::BufferAppend => {
                            self.paste_burst.append_char(ch, now);
                        }
                    }
                } else {
                    self.flush_paste_burst_before_non_plain_input();
                    self.input.push_char(ch);
                    self.clamp_completion_index();
                }
                false
            }
            KeyCode::Backspace => {
                self.flush_paste_burst_before_non_plain_input();
                self.input.pop();
                self.clamp_completion_index();
                false
            }
            KeyCode::Up => {
                self.flush_paste_burst_before_non_plain_input();
                if self.command_completion_active() {
                    let count = self.command_matches().len();
                    if count > 0 {
                        self.completion_index = (self.completion_index + count - 1) % count;
                    }
                }
                false
            }
            KeyCode::Down => {
                self.flush_paste_burst_before_non_plain_input();
                if self.command_completion_active() {
                    let count = self.command_matches().len();
                    if count > 0 {
                        self.completion_index = (self.completion_index + 1) % count;
                    }
                }
                false
            }
            KeyCode::Tab => {
                self.flush_paste_burst_before_non_plain_input();
                if self.command_completion_active() {
                    self.complete_selected_command();
                }
                false
            }
            KeyCode::BackTab => {
                self.flush_paste_burst_before_non_plain_input();
                self.cycle_reasoning_effort();
                false
            }
            KeyCode::Esc => {
                self.flush_paste_burst_before_non_plain_input();
                if self.activity.is_active() && !self.pending_messages.is_empty() {
                    let events = self.runtime.steer();
                    self.apply_events(events);
                    return false;
                }
                self.input.clear();
                self.completion_index = 0;
                false
            }
            KeyCode::Enter => {
                if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) {
                    self.flush_paste_burst_before_non_plain_input();
                    self.input.push_char('\n');
                    self.completion_index = 0;
                    return false;
                }
                if self.paste_burst.append_newline_if_active(now) {
                    self.completion_index = 0;
                    return false;
                }
                if self
                    .paste_burst
                    .newline_should_insert_instead_of_submit(now)
                {
                    self.input.push_char('\n');
                    self.completion_index = 0;
                    return false;
                }
                self.flush_paste_burst_before_non_plain_input();
                if self.should_complete_on_enter() {
                    self.complete_selected_command();
                    return false;
                }

                let submitted = self.input.text().trim().to_string();
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
            _ => {
                self.flush_paste_burst_before_non_plain_input();
                false
            }
        }
    }

    fn handle_paste(&mut self, text: &str) {
        self.paste_burst.clear_after_explicit_paste();
        self.input.push_paste(text);
        self.clamp_completion_index();
    }

    fn flush_paste_burst_if_due(&mut self, now: Instant) -> bool {
        match self.paste_burst.flush_if_due(now) {
            PasteFlush::Paste(text) => {
                self.handle_paste(&text);
                true
            }
            PasteFlush::Typed(ch) => {
                self.input.push_char(ch);
                self.clamp_completion_index();
                true
            }
            PasteFlush::None => false,
        }
    }

    fn handle_paste_burst_render_tick(
        &mut self,
        now: Instant,
        frames: &mut FrameScheduler,
    ) -> bool {
        if self.flush_paste_burst_if_due(now) {
            frames.request_now(now);
            return true;
        }
        if self.paste_burst.is_active() {
            frames.request_in(now, paste_burst_render_delay());
            return true;
        }
        false
    }

    fn flush_paste_burst_before_non_plain_input(&mut self) {
        if let Some(text) = self.paste_burst.flush_before_non_plain_input() {
            self.handle_paste(&text);
        }
        self.paste_burst.clear_after_non_char();
    }

    fn handle_picker_prompt_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        match code {
            KeyCode::Char('c') | KeyCode::Char('q')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                true
            }
            KeyCode::Esc => {
                if let Some(picker) = self.picker_view.as_mut() {
                    picker.cancel_prompt();
                }
                false
            }
            KeyCode::Backspace => {
                if let Some(picker) = self.picker_view.as_mut() {
                    picker.pop_prompt_char();
                }
                false
            }
            KeyCode::Enter => {
                let Some(command) = self
                    .picker_view
                    .as_mut()
                    .and_then(PickerState::take_prompt_command)
                else {
                    return false;
                };
                let (_, events) = self.runtime.handle_command(&command);
                self.apply_events(events);
                false
            }
            KeyCode::Char(ch)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(picker) = self.picker_view.as_mut() {
                    picker.push_prompt_char(ch);
                }
                false
            }
            _ => false,
        }
    }

    fn handle_picker_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if self
            .picker_view
            .as_ref()
            .is_some_and(|picker| picker.prompt.is_some())
        {
            return self.handle_picker_prompt_key(code, modifiers);
        }

        match code {
            KeyCode::Char('c') | KeyCode::Char('q')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                true
            }
            KeyCode::Esc => {
                self.picker_view = None;
                false
            }
            KeyCode::Up => {
                if let Some(picker) = self.picker_view.as_mut() {
                    picker.move_previous();
                }
                false
            }
            KeyCode::Down => {
                if let Some(picker) = self.picker_view.as_mut() {
                    picker.move_next();
                }
                false
            }
            KeyCode::Left => {
                if let Some(picker) = self.picker_view.as_mut() {
                    picker.move_parent();
                }
                false
            }
            KeyCode::Right => {
                if let Some(picker) = self.picker_view.as_mut() {
                    picker.move_first_child();
                }
                false
            }
            KeyCode::BackTab => {
                if let Some(picker) = self.picker_view.as_mut() {
                    picker.cycle_effort();
                }
                false
            }
            KeyCode::Char('f') | KeyCode::Char('n')
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(picker) = self.picker_view.as_mut() {
                    picker.begin_tree_prompt(TreePromptAction::Fork);
                }
                false
            }
            KeyCode::Delete => {
                if let Some(picker) = self.picker_view.as_mut() {
                    picker.begin_tree_prompt(TreePromptAction::Delete);
                }
                false
            }
            KeyCode::Enter => {
                let Some(command) = self
                    .picker_view
                    .as_ref()
                    .and_then(PickerState::selected_command)
                else {
                    self.picker_view = None;
                    return false;
                };
                self.picker_view = None;
                let (_, events) = self.runtime.handle_command(&command);
                self.apply_events(events);
                false
            }
            _ => false,
        }
    }

    fn apply_events(&mut self, events: Vec<AgentEvent>) -> bool {
        let mut changed = false;
        for event in events {
            changed |= match event {
                AgentEvent::Startup {
                    version,
                    profile_dir,
                    config_path,
                } => {
                    self.push_startup(version, profile_dir, config_path);
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
                    true
                }
                AgentEvent::ThinkingStart => {
                    self.activity.start_connecting();
                    true
                }
                AgentEvent::Retrying { attempt } => {
                    self.activity.start_reconnecting(attempt);
                    true
                }
                AgentEvent::AssistantStart => {
                    self.live_assistant = Some(String::new());
                    true
                }
                AgentEvent::AssistantDelta(delta) => {
                    self.activity.add_output_delta(&delta);
                    self.append_assistant_delta(&delta);
                    true
                }
                AgentEvent::ToolStart { call_id, name } => {
                    self.activity.start_tool(name.clone());
                    self.upsert_tool(call_id, name, String::new(), true);
                    true
                }
                AgentEvent::ToolUpdate {
                    call_id,
                    name,
                    output,
                } => {
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
                    self.activity.start_connecting();
                    self.upsert_tool(call_id, name, output, false);
                    true
                }
                AgentEvent::Usage {
                    input_tokens,
                    output_tokens,
                } => {
                    self.total_input_tokens += input_tokens;
                    self.total_output_tokens += output_tokens;
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
                AgentEvent::ModelView {
                    models,
                    active_effort,
                } => {
                    self.picker_view = Some(PickerState::model(models, active_effort));
                    true
                }
                AgentEvent::CommandList(commands) => {
                    self.commands = commands.into_iter().map(CommandCandidate::from).collect();
                    self.clamp_completion_index();
                    true
                }
                AgentEvent::Goal(goal) => {
                    self.chat.push(ChatLine::System(format_goal_summary(goal)));
                    true
                }
                AgentEvent::Transcript(items) => {
                    self.replace_transcript(items);
                    true
                }
                AgentEvent::Info(message) => {
                    self.chat.push(ChatLine::System(message));
                    true
                }
                AgentEvent::Error(error) => {
                    self.commit_live_assistant();
                    self.activity.finish();
                    self.chat.push(ChatLine::Error(error));
                    true
                }
                AgentEvent::Status(status) => self.apply_status(status),
            };
        }
        changed
    }

    fn apply_status(&mut self, status: String) -> bool {
        let mut changed = self.status != status;
        if status == "ready" || status.starts_with("queued:") {
            changed |= self.commit_live_assistant();
        }
        if status == "ready" {
            let was_active = self.activity.is_active();
            self.activity.finish();
            changed |= was_active;
        }
        self.status = status;
        changed
    }

    fn append_assistant_delta(&mut self, delta: &str) {
        if let Some(text) = self.live_assistant.as_mut() {
            text.push_str(delta);
        } else {
            self.live_assistant = Some(delta.to_string());
        }
    }

    fn commit_live_assistant(&mut self) -> bool {
        let Some(text) = self.live_assistant.take() else {
            return false;
        };
        if !text.trim().is_empty() {
            self.chat.push(ChatLine::Assistant(text));
            return true;
        }
        true
    }

    fn upsert_tool(&mut self, call_id: String, name: String, output: String, running: bool) {
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
            return;
        }

        self.chat.push(ChatLine::Tool {
            call_id: Some(call_id),
            name,
            output,
            running,
        });
    }

    fn replace_transcript(&mut self, items: Vec<TranscriptItem>) {
        self.commit_live_assistant();
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
    }

    fn push_startup(&mut self, version: String, profile_dir: String, config_path: String) {
        self.chat.push(ChatLine::Startup {
            version,
            profile_dir,
            config_path,
        });
    }

    fn build_document(&self, width: usize, now: Instant) -> UiDocument {
        let command_matches = self.command_matches();
        let input_display = self.input.display_text();
        UiBuilder::new()
            .chat(&self.chat)
            .live_assistant(self.live_assistant.as_deref())
            .picker(self.picker_view.as_ref())
            .pending_messages(&self.pending_messages)
            .input(
                &input_display,
                &command_matches,
                self.completion_index,
                !self.activity.is_active(),
            )
            .progress(&self.activity, now, width)
            .bottom_status(
                BottomStatus {
                    provider: &self.provider,
                    model: &self.model,
                    reasoning_effort: &self.reasoning_effort,
                    input_tokens: self.total_input_tokens,
                    output_tokens: self.total_output_tokens,
                    context_window: self.context_window,
                },
                width,
            )
            .reset_screen(self.reset_screen)
            .finish()
    }

    fn command_completion_active(&self) -> bool {
        let input = self.input.text();
        !input.contains('\n') && input.starts_with('/') && !self.command_matches().is_empty()
    }

    fn should_complete_on_enter(&self) -> bool {
        self.command_completion_active()
    }

    fn command_matches(&self) -> Vec<CommandCandidate> {
        let input = self.input.text();
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
        if let Some(command) = matches.get(self.completion_index) {
            self.input.clear();
            self.input.push_text(&command.command);
            self.input.push_char(' ');
            self.completion_index = 0;
        }
    }

    fn cycle_reasoning_effort(&mut self) {
        if self.model == "unknown" {
            return;
        }
        let next = next_reasoning_effort(&self.reasoning_efforts, &self.reasoning_effort);
        let (_, events) = self
            .runtime
            .handle_command(&format!("/model {} {next}", self.model));
        self.apply_events(events);
    }
}

impl ActivityState {
    fn idle() -> Self {
        Self {
            kind: ActivityKind::Idle,
            turn_started_at: None,
            phase_started_at: None,
            last_delta_at: None,
            estimated_output_tokens: 0,
            animation_tick: 0,
        }
    }

    fn start_connecting(&mut self) {
        let now = Instant::now();
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(now);
            self.estimated_output_tokens = 0;
            self.last_delta_at = None;
        }
        self.phase_started_at = Some(now);
        self.kind = ActivityKind::Connecting;
    }

    fn start_reconnecting(&mut self, attempt: usize) {
        let now = Instant::now();
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(now);
        }
        self.phase_started_at = Some(now);
        self.kind = ActivityKind::Reconnecting { attempt };
    }

    fn start_tool(&mut self, _name: String) {
        let now = Instant::now();
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(now);
        }
        self.phase_started_at = Some(now);
        self.kind = ActivityKind::Tool;
    }

    fn add_output_delta(&mut self, delta: &str) {
        let now = Instant::now();
        if !matches!(self.kind, ActivityKind::Output) {
            self.phase_started_at = Some(now);
        }
        self.last_delta_at = Some(now);
        self.estimated_output_tokens += estimate_tokens(delta);
        self.kind = ActivityKind::Output;
    }

    fn finish(&mut self) {
        self.kind = ActivityKind::Idle;
        self.turn_started_at = None;
        self.phase_started_at = None;
        self.last_delta_at = None;
        self.estimated_output_tokens = 0;
    }

    fn is_active(&self) -> bool {
        !matches!(self.kind, ActivityKind::Idle)
    }

    fn advance_animation(&mut self) {
        self.animation_tick = self.animation_tick.wrapping_add(1);
    }

    fn progress(&self, now: Instant) -> Option<ProgressState> {
        let phase_started_at = self.phase_started_at.or(self.turn_started_at)?;
        let elapsed = now.saturating_duration_since(phase_started_at);
        match self.kind {
            ActivityKind::Idle => None,
            ActivityKind::Connecting => Some(ProgressState {
                color: gradient_color(
                    elapsed.as_secs_f32() / 10.0,
                    (255, 255, 255),
                    (255, 210, 0),
                    (255, 60, 60),
                ),
                preset: SpinnerPreset::Line,
                label: format!("connecting {:.1}s", elapsed.as_secs_f32()),
                step: 1,
            }),
            ActivityKind::Reconnecting { attempt } => Some(ProgressState {
                color: (255, 60, 60),
                preset: SpinnerPreset::Pulse,
                label: format!("reconnecting attempt {attempt}"),
                step: 2,
            }),
            ActivityKind::Output => {
                let stalled_for = self
                    .last_delta_at
                    .map(|last| now.saturating_duration_since(last))
                    .unwrap_or_default();
                let output_elapsed = elapsed.as_secs_f32().max(0.1);
                let tokens_per_sec = self.estimated_output_tokens as f32 / output_elapsed;
                let step = 1 + (tokens_per_sec / 8.0).floor().clamp(0.0, 5.0) as usize;
                Some(ProgressState {
                    color: gradient_color(
                        stalled_for.as_secs_f32() / 8.0,
                        (70, 220, 110),
                        (255, 210, 0),
                        (255, 60, 60),
                    ),
                    preset: SpinnerPreset::Dots,
                    label: format!("output {:.1} tok/s", tokens_per_sec),
                    step,
                })
            }
            ActivityKind::Tool => Some(ProgressState {
                color: (255, 210, 0),
                preset: SpinnerPreset::Scan,
                label: "tool running".to_string(),
                step: 1,
            }),
        }
    }
}

struct ProgressState {
    color: (u8, u8, u8),
    preset: SpinnerPreset,
    label: String,
    step: usize,
}

#[derive(Debug, Clone, Copy)]
enum SpinnerPreset {
    Line,
    Dots,
    Pulse,
    Scan,
}

impl PickerState {
    fn checkout(nodes: Vec<TreeNodeView>) -> Self {
        let all_rows = build_tree_rows(&nodes);
        let expanded = all_rows
            .iter()
            .filter(|row| row.depth == 0)
            .map(|row| row.id.clone())
            .collect::<HashSet<_>>();
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

    fn resume(sessions: Vec<SessionListItemView>) -> Self {
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

    fn model(models: Vec<ModelOptionView>, active_effort: String) -> Self {
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

    fn selected_id(&self) -> Option<String> {
        self.rows.get(self.selected).map(|row| row.id.clone())
    }

    fn selected_command(&self) -> Option<String> {
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

    fn begin_tree_prompt(&mut self, action: TreePromptAction) {
        if self.mode != PickerMode::Checkout {
            return;
        }
        self.prompt = Some(TreePrompt {
            action,
            input: String::new(),
        });
    }

    fn cancel_prompt(&mut self) {
        self.prompt = None;
    }

    fn push_prompt_char(&mut self, ch: char) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.input.push(ch);
        }
    }

    fn pop_prompt_char(&mut self) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.input.pop();
        }
    }

    fn take_prompt_command(&mut self) -> Option<String> {
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

    fn move_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.sync_selected_model_efforts();
    }

    fn move_next(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
        self.sync_selected_model_efforts();
    }

    fn move_parent(&mut self) {
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

    fn move_first_child(&mut self) {
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

    fn cycle_effort(&mut self) {
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

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, Hide)?;
        stdout.write_all(ENABLE_BRACKETED_PASTE.as_bytes())?;
        stdout.write_all(DISABLE_SCROLL_ON_OUTPUT.as_bytes())?;
        stdout.flush()?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = io::stdout().write_all(DISABLE_BRACKETED_PASTE.as_bytes());
        let _ = io::stdout().write_all(ENABLE_SCROLL_ON_OUTPUT.as_bytes());
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
                ChatLine::Startup {
                    version,
                    profile_dir,
                    config_path,
                } => self.push_startup_box(version, profile_dir, config_path),
                ChatLine::User(text) => self.push_history(UiKind::User, text),
                ChatLine::Assistant(text) => self.push_history(UiKind::Assistant, text),
                ChatLine::Tool {
                    name,
                    output,
                    running,
                    ..
                } => {
                    let suffix = if *running { " running" } else { "" };
                    self.history_line(UiKind::Tool, format!("* tool:{name}{suffix}"));
                    self.push_tool_preview(&tool_output_preview(name, output, *running), "  ");
                }
                ChatLine::System(text) => self.push_history(UiKind::System, text),
                ChatLine::Error(text) => self.push_history(UiKind::Error, text),
            }
            self.history_line(UiKind::System, String::new());
        }
        self
    }

    fn live_assistant(mut self, text: Option<&str>) -> Self {
        if let Some(text) = text.filter(|value| !value.is_empty()) {
            self.push_control_block(UiKind::Assistant, text);
            self.control_line(UiKind::System, String::new());
        }
        self
    }

    fn picker(mut self, picker: Option<&PickerState>) -> Self {
        let Some(picker) = picker else {
            return self;
        };
        let hint = match picker.mode {
            PickerMode::Checkout => {
                "tree: arrows move/expand, enter checkout, f fork, delete branch, esc close"
            }
            PickerMode::Resume => "resume: arrows move, enter resume, esc close",
            PickerMode::Model => "model: arrows move, shift+tab effort, enter select, esc close",
        };
        self.control_line(UiKind::Status, hint.to_string());
        if let Some(prompt) = picker.prompt.as_ref() {
            let label = match prompt.action {
                TreePromptAction::Fork => "fork branch",
                TreePromptAction::Delete => "delete branch",
            };
            self.control_line(
                UiKind::Input,
                format!("> {label}: {}{CURSOR_MARKER}{VISIBLE_CURSOR}", prompt.input),
            );
        }
        if picker.mode == PickerMode::Model && !picker.efforts.is_empty() {
            let effort = &picker.efforts[picker.selected_effort];
            self.control_line(
                UiKind::Status,
                format!("thinking: {}", colored_reasoning_effort(effort)),
            );
        }
        if picker.rows.is_empty() {
            self.control_line(UiKind::Status, "(empty)".to_string());
            return self;
        }
        for (index, row) in picker.rows.iter().enumerate() {
            let active = if row.active { " *" } else { "" };
            let marker = if index == picker.selected { "> " } else { "  " };
            let directory = if row.has_children {
                if picker
                    .tree
                    .as_ref()
                    .is_some_and(|tree| tree.expanded.contains(&row.id))
                {
                    "[-] "
                } else {
                    "[+] "
                }
            } else {
                "    "
            };
            let kind = if index == picker.selected {
                UiKind::Selected
            } else if row.active {
                UiKind::Brand
            } else if row.has_children {
                UiKind::TreeDirectory
            } else {
                UiKind::Status
            };
            let detail = if row.detail.is_empty() {
                String::new()
            } else {
                format!(" {}", row.detail)
            };
            self.control_line(
                kind,
                format!(
                    "{marker}{}{directory}{}{}{active}",
                    row.prefix, row.label, detail
                ),
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

    fn input(
        mut self,
        input: &str,
        command_matches: &[CommandCandidate],
        selected_index: usize,
        show_cursor: bool,
    ) -> Self {
        let cursor = if show_cursor {
            format!("{CURSOR_MARKER}{VISIBLE_CURSOR}")
        } else {
            String::new()
        };
        let lines = input.split('\n').collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            let prefix = if index == 0 { "> " } else { "  " };
            let cursor = if index + 1 == lines.len() {
                cursor.as_str()
            } else {
                ""
            };
            self.control_line(UiKind::Input, format!("{prefix}{line}{cursor}"));
        }
        if input.starts_with('/') && !command_matches.is_empty() {
            for (index, candidate) in command_matches.iter().enumerate() {
                let kind = if index == selected_index {
                    UiKind::Selected
                } else {
                    UiKind::Status
                };
                let marker = candidate
                    .marker
                    .as_ref()
                    .map(|marker| format!(" {marker}"))
                    .unwrap_or_default();
                self.control_line(kind, format!("  {}{marker}", candidate.command));
            }
        }
        self
    }

    fn progress(mut self, activity: &ActivityState, now: Instant, width: usize) -> Self {
        let Some(progress) = activity.progress(now) else {
            return self;
        };
        let indicator = spinner_char(progress.preset, activity.animation_tick, progress.step);
        let (red, green, blue) = progress.color;
        let label = truncate_to_width(&progress.label, width.saturating_sub(4));
        let line = format!(
            "  \x1b[38;2;{red};{green};{blue}m{indicator}{RESET}{} {label}",
            color_code(UiKind::Status),
        );
        self.control_line(UiKind::Status, line);
        self
    }

    fn bottom_status(mut self, status: BottomStatus<'_>, width: usize) -> Self {
        let total = status.input_tokens + status.output_tokens;
        let percent = if status.context_window == 0 {
            0.0
        } else {
            (total as f64 / status.context_window as f64 * 100.0).min(100.0)
        };
        let plain_left = format!(
            "{} / {} ({})",
            status.provider, status.model, status.reasoning_effort
        );
        let right = format!(
            "tokens {}/{} | context {percent:.1}%",
            status.input_tokens, status.output_tokens
        );
        let line = format_status_line(&plain_left, &right, width).replace(
            &format!("({})", status.reasoning_effort),
            &format!("({})", colored_reasoning_effort(status.reasoning_effort)),
        );
        self.control_line(UiKind::Status, line);
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

    fn push_startup_box(&mut self, version: &str, profile_dir: &str, _config_path: &str) {
        let title = format!(">_ JuCode CLI (v{version})");
        let model = "model:     /model to choose";
        let profile = format!("profile:   {profile_dir}");
        let content_width = [title.as_str(), model, profile.as_str()]
            .iter()
            .map(|line| UnicodeWidthStr::width(*line))
            .max()
            .unwrap_or(0)
            .min(96);

        self.history_line(UiKind::Brand, rounded_box_border('╭', '╮', content_width));
        self.history_line(
            UiKind::Brand,
            rounded_box_line(&title, content_width, UiKind::Brand),
        );
        self.history_line(
            UiKind::Brand,
            rounded_box_line("", content_width, UiKind::Brand),
        );
        self.history_line(
            UiKind::System,
            rounded_box_line(model, content_width, UiKind::System),
        );
        self.history_line(
            UiKind::System,
            rounded_box_line(&profile, content_width, UiKind::System),
        );
        self.history_line(UiKind::Brand, rounded_box_border('╰', '╯', content_width));
    }

    fn push_tool_preview(&mut self, text: &str, prefix: &str) {
        if text.is_empty() {
            self.history_line(UiKind::Tool, prefix.to_string());
            return;
        }

        for line in text.lines() {
            self.history_line(diff_line_kind(line), format!("{prefix}{line}"));
        }
    }

    fn push_control_block(&mut self, kind: UiKind, text: &str) {
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
            previous_lines: Vec::new(),
            previous_transcript_lines: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            previous_viewport_top: 0,
            hardware_cursor_row: 0,
            initialized: false,
            force_transcript_rebuild: false,
        }
    }

    fn force_transcript_rebuild(&mut self) {
        self.force_transcript_rebuild = true;
    }

    fn render(&mut self, stdout: &mut Stdout, document: &UiDocument) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        let width = width.max(1);
        let height = height.max(1);
        let projection = ProjectedDocument::from_document(document, width);
        let transcript_changed = self.previous_transcript_lines != projection.transcript_lines;
        let mut frame = projection.clone().into_frame();
        if frame.lines.is_empty() {
            frame.lines.push(String::new());
        }

        if self.force_transcript_rebuild || transcript_changed {
            self.render_transcript_projection(stdout, &projection, height)?;
        } else if document.reset_screen || !self.initialized {
            self.full_render(
                stdout,
                &frame,
                document.reset_screen,
                false,
                height,
                FullRenderMode::FullHistory,
            )?;
        } else if self.previous_width != width || self.previous_height != height {
            self.full_render(
                stdout,
                &frame,
                true,
                false,
                height,
                FullRenderMode::VisibleViewport,
            )?;
        } else {
            self.diff_render(stdout, &frame, height)?;
        }

        self.position_cursor(stdout, frame.cursor, width, frame.lines.len())?;
        stdout.flush()?;

        self.previous_lines = frame.lines;
        self.previous_transcript_lines = projection.transcript_lines;
        self.previous_width = width;
        self.previous_height = height;
        self.initialized = true;
        self.force_transcript_rebuild = false;
        Ok(())
    }

    fn render_transcript_projection(
        &mut self,
        stdout: &mut Stdout,
        projection: &ProjectedDocument,
        height: u16,
    ) -> io::Result<()> {
        let frame_lines = projection.frame_lines();
        let mut buffer = render_buffer_start();
        buffer.push_str(clear_screen_sequence(true));
        append_lines_to_buffer(&mut buffer, &frame_lines);
        buffer.push_str(SYNC_END);
        stdout.write_all(buffer.as_bytes())?;
        self.hardware_cursor_row = frame_lines.len().saturating_sub(1);
        self.previous_viewport_top = viewport_top(frame_lines.len(), height);
        Ok(())
    }

    fn full_render(
        &mut self,
        stdout: &mut Stdout,
        frame: &RenderedFrame,
        clear: bool,
        purge_scrollback: bool,
        height: u16,
        mode: FullRenderMode,
    ) -> io::Result<()> {
        let mut buffer = render_buffer_start();
        if clear {
            buffer.push_str(clear_screen_sequence(purge_scrollback));
        }
        let (start, end) = full_render_window(frame.lines.len(), height, mode);
        append_lines_to_buffer(&mut buffer, &frame.lines[start..end]);
        buffer.push_str(SYNC_END);
        stdout.write_all(buffer.as_bytes())?;
        self.hardware_cursor_row = end.saturating_sub(1);
        self.previous_viewport_top = viewport_top(frame.lines.len(), height);
        Ok(())
    }

    fn diff_render(
        &mut self,
        stdout: &mut Stdout,
        frame: &RenderedFrame,
        height: u16,
    ) -> io::Result<()> {
        let Some((first_changed, last_changed)) = changed_range(&self.previous_lines, &frame.lines)
        else {
            return Ok(());
        };

        if first_changed < self.previous_viewport_top {
            return self.full_render(
                stdout,
                frame,
                true,
                false,
                height,
                FullRenderMode::VisibleViewport,
            );
        }

        if first_changed >= frame.lines.len() {
            return self.clear_deleted_tail(stdout, frame, height);
        }

        let height = height as usize;
        let mut buffer = render_buffer_start();
        let mut viewport_top = self.previous_viewport_top;
        let mut hardware_cursor_row = self.hardware_cursor_row;
        let append_start = first_changed == self.previous_lines.len()
            && frame.lines.len() > self.previous_lines.len()
            && first_changed > 0;
        let move_target = if append_start {
            first_changed - 1
        } else {
            first_changed
        };
        let previous_viewport_bottom = viewport_top + height.saturating_sub(1);

        if move_target > previous_viewport_bottom {
            let current_screen_row = hardware_cursor_row.saturating_sub(viewport_top);
            let move_to_bottom = height.saturating_sub(1).saturating_sub(current_screen_row);
            if move_to_bottom > 0 {
                buffer.push_str(&format!("\x1b[{move_to_bottom}B"));
            }
            let scroll = move_target - previous_viewport_bottom;
            for _ in 0..scroll {
                buffer.push_str("\r\n");
            }
            viewport_top += scroll;
            hardware_cursor_row = move_target;
        }

        let line_delta = move_target as isize - hardware_cursor_row as isize;
        if line_delta > 0 {
            buffer.push_str(&format!("\x1b[{line_delta}B"));
        } else if line_delta < 0 {
            buffer.push_str(&format!("\x1b[{}A", -line_delta));
        }
        buffer.push_str(if append_start { "\r\n" } else { "\r" });

        let render_end = last_changed.min(frame.lines.len().saturating_sub(1));
        for index in first_changed..=render_end {
            if index > first_changed {
                buffer.push_str("\r\n");
            }
            buffer.push_str("\x1b[2K");
            buffer.push_str(&frame.lines[index]);
        }

        let mut final_cursor_row = render_end;
        if self.previous_lines.len() > frame.lines.len() {
            if render_end < frame.lines.len().saturating_sub(1) {
                let move_down = frame.lines.len().saturating_sub(1) - render_end;
                buffer.push_str(&format!("\x1b[{move_down}B"));
                final_cursor_row = frame.lines.len().saturating_sub(1);
            }
            let extra_lines = self.previous_lines.len() - frame.lines.len();
            for _ in 0..extra_lines {
                buffer.push_str("\r\n\x1b[2K");
            }
            if extra_lines > 0 {
                buffer.push_str(&format!("\x1b[{extra_lines}A"));
            }
        }

        buffer.push_str(SYNC_END);
        stdout.write_all(buffer.as_bytes())?;
        self.hardware_cursor_row = final_cursor_row;
        self.previous_viewport_top =
            viewport_top.max(final_cursor_row.saturating_add(1).saturating_sub(height));
        Ok(())
    }

    fn clear_deleted_tail(
        &mut self,
        stdout: &mut Stdout,
        frame: &RenderedFrame,
        height: u16,
    ) -> io::Result<()> {
        let target_row = frame.lines.len().saturating_sub(1);
        if target_row < self.previous_viewport_top {
            return self.full_render(
                stdout,
                frame,
                true,
                false,
                height,
                FullRenderMode::VisibleViewport,
            );
        }

        let mut buffer = render_buffer_start();
        let row_delta = target_row as isize - self.hardware_cursor_row as isize;
        if row_delta > 0 {
            buffer.push_str(&format!("\x1b[{row_delta}B"));
        } else if row_delta < 0 {
            buffer.push_str(&format!("\x1b[{}A", -row_delta));
        }
        buffer.push('\r');

        let extra_lines = self.previous_lines.len().saturating_sub(frame.lines.len());
        if extra_lines > 0 {
            buffer.push_str("\x1b[1B");
            for index in 0..extra_lines {
                buffer.push_str("\r\x1b[2K");
                if index + 1 < extra_lines {
                    buffer.push_str("\x1b[1B");
                }
            }
            buffer.push_str(&format!("\x1b[{extra_lines}A"));
        }

        buffer.push_str(SYNC_END);
        stdout.write_all(buffer.as_bytes())?;
        self.hardware_cursor_row = target_row;
        Ok(())
    }

    fn position_cursor(
        &mut self,
        stdout: &mut Stdout,
        cursor: Option<CursorTarget>,
        width: u16,
        line_count: usize,
    ) -> io::Result<()> {
        let Some(cursor) = cursor else {
            stdout.write_all(HIDE_CURSOR.as_bytes())?;
            return Ok(());
        };
        let target_row = cursor.row.min(line_count.saturating_sub(1));
        let column = cursor.column.min(width.saturating_sub(1) as usize);
        let row_delta = target_row as isize - self.hardware_cursor_row as isize;
        if row_delta > 0 {
            stdout.write_all(format!("\x1b[{row_delta}B").as_bytes())?;
        } else if row_delta < 0 {
            stdout.write_all(format!("\x1b[{}A", -row_delta).as_bytes())?;
        }
        stdout.write_all(format!("\x1b[{}G", column + 1).as_bytes())?;
        if SHOW_HARDWARE_CURSOR {
            stdout.write_all(SHOW_CURSOR.as_bytes())?;
        } else {
            stdout.write_all(HIDE_CURSOR.as_bytes())?;
        }
        self.hardware_cursor_row = target_row;
        Ok(())
    }
}

#[cfg(test)]
impl RenderedFrame {
    fn build(document: &UiDocument, width: u16) -> Self {
        ProjectedDocument::from_document(document, width).into_frame()
    }
}

impl ProjectedDocument {
    fn from_document(document: &UiDocument, width: u16) -> Self {
        let width = width as usize;
        let transcript_lines = wrap_lines(&document.history, width)
            .into_iter()
            .map(|line| render_ansi_line(&line))
            .collect::<Vec<_>>();
        let mut controls = wrap_lines(&document.controls, width);
        let cursor = extract_cursor(&mut controls).map(|cursor| CursorTarget {
            row: cursor.row,
            column: cursor.column,
        });
        let mut active_lines = Vec::new();
        if !transcript_lines.is_empty() && !document.controls.is_empty() {
            active_lines.push(String::new());
        }
        let controls_start_row = transcript_lines.len() + active_lines.len();
        let cursor = cursor.map(|cursor| CursorTarget {
            row: controls_start_row + cursor.row,
            column: cursor.column,
        });
        active_lines.extend(controls.into_iter().map(|line| render_ansi_line(&line)));

        Self {
            transcript_lines,
            active_lines,
            cursor,
        }
    }

    fn frame_lines(&self) -> Vec<String> {
        let mut lines = self.transcript_lines.clone();
        lines.extend(self.active_lines.clone());
        lines
    }

    fn into_frame(self) -> RenderedFrame {
        RenderedFrame {
            lines: self.frame_lines(),
            cursor: self.cursor,
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

fn clear_screen_sequence(purge_scrollback: bool) -> &'static str {
    if purge_scrollback {
        "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H"
    } else {
        "\x1b[2J\x1b[H"
    }
}

fn changed_range(previous: &[String], next: &[String]) -> Option<(usize, usize)> {
    let max_len = previous.len().max(next.len());
    let mut first = None;
    let mut last = 0;

    for index in 0..max_len {
        let previous_line = previous.get(index).map(String::as_str).unwrap_or("");
        let next_line = next.get(index).map(String::as_str).unwrap_or("");
        if previous_line != next_line {
            first.get_or_insert(index);
            last = index;
        }
    }

    first.map(|first| (first, last))
}

fn viewport_top(line_count: usize, height: u16) -> usize {
    line_count
        .max(height as usize)
        .saturating_sub(height as usize)
}

fn full_render_window(line_count: usize, height: u16, mode: FullRenderMode) -> (usize, usize) {
    match mode {
        FullRenderMode::FullHistory => (0, line_count),
        FullRenderMode::VisibleViewport => {
            let start = viewport_top(line_count, height);
            let end = start.saturating_add(height as usize).min(line_count);
            (start, end)
        }
    }
}

fn build_tree_rows(nodes: &[TreeNodeView]) -> Vec<PickerRow> {
    let mut rows = Vec::new();
    push_tree_rows(None, 0, nodes, &mut rows);
    rows
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
        if let Some((sequence, next)) = split_ansi_sequence(rest) {
            current.push_str(sequence);
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

fn split_ansi_sequence(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("\x1b[")?;
    for (index, ch) in rest.char_indices() {
        if ch.is_ascii_alphabetic() {
            let end = 2 + index + ch.len_utf8();
            return Some((&text[..end], &text[end..]));
        }
    }
    None
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
    if line.text.contains(ANSI_ESCAPE) {
        return line.text.clone();
    }
    format!("{}{}{}", color_code(line.kind), line.text, RESET)
}

fn tool_output_preview(name: &str, output: &str, running: bool) -> String {
    if name == "bash" && running {
        return limited_preview(output);
    }
    if let Some(preview) = projected_tool_output(name, output) {
        return preview;
    }
    if let Some(diff) = diff_from_tool_output(output) {
        return diff_preview(&diff);
    }
    limited_preview(output)
}

fn projected_tool_output(name: &str, output: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(output).ok()?;
    match name {
        "read" => {
            let path = value.get("path").and_then(serde_json::Value::as_str)?;
            let lines = value
                .get("lines_read")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let offset = value
                .get("offset")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1);
            let truncated = value
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let suffix = if truncated { " truncated" } else { "" };
            Some(format!(
                "read {}: {lines} lines from line {offset}{suffix}",
                display_path_name(path)
            ))
        }
        "ls" => {
            let path = value.get("path").and_then(serde_json::Value::as_str)?;
            Some(format!("ls {}", path))
        }
        "bash" => Some(project_bash_output(&value)),
        _ => None,
    }
}

fn project_bash_output(value: &serde_json::Value) -> String {
    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
        return format!("error: {error}");
    }
    let command = value
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("bash");
    let exit_code = value
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let timed_out = value
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let stdout = value
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let stderr = value
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mut lines = vec![format!(
        "bash: {command} (exit {exit_code}{})",
        if timed_out { ", timed out" } else { "" }
    )];
    if !stdout.trim().is_empty() {
        lines.push(format!("stdout:\n{}", tail_lines(stdout, 8)));
    }
    if !stderr.trim().is_empty() {
        lines.push(format!("stderr:\n{}", tail_lines(stderr, 8)));
    }
    lines.join("\n")
}

fn display_path_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn tail_lines(text: &str, limit: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(limit);
    lines[start..].join("\n")
}

fn diff_from_tool_output(output: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(output).ok()?;
    value
        .get("diff")
        .and_then(serde_json::Value::as_str)
        .filter(|diff| !diff.trim().is_empty())
        .map(str::to_string)
}

fn diff_preview(diff: &str) -> String {
    let mut preview = Vec::new();
    let mut preview_bytes = 0usize;
    let mut file_label = None;
    let mut hunk_header = None;
    let mut change_lines = Vec::new();
    let mut in_first_hunk = false;
    let mut saw_next_hunk = false;

    for line in diff.lines() {
        if file_label.is_none() && line.starts_with("diff --git ") {
            file_label = Some(diff_file_label(line));
            continue;
        }
        if line.starts_with("@@") {
            if in_first_hunk {
                saw_next_hunk = true;
                break;
            }
            hunk_header = Some(line);
            in_first_hunk = true;
            continue;
        }
        if in_first_hunk && is_diff_change_line(line) {
            change_lines.push(line);
        }
    }

    let Some(header) = hunk_header else {
        return limited_preview(diff);
    };
    if change_lines.is_empty() {
        return limited_preview(diff);
    }

    let mut truncated = saw_next_hunk;
    if let Some(label) = file_label.as_deref() {
        truncated |= !push_preview_line(&mut preview, &mut preview_bytes, label);
    }
    truncated |= !push_preview_line(&mut preview, &mut preview_bytes, header);

    let line_budget = TOOL_OUTPUT_PREVIEW_LINES.saturating_sub(preview.len());
    let selected = balanced_diff_lines(&change_lines, line_budget);
    truncated |= selected.len() < change_lines.len();
    for line in render_intra_line_diff(&selected) {
        truncated |= !push_preview_line(&mut preview, &mut preview_bytes, &line);
    }

    if truncated {
        preview.push("... diff truncated in UI".to_string());
    }

    preview.join("\n")
}

fn diff_file_label(line: &str) -> String {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 4 {
        return line.to_string();
    }
    let old_path = parts[2].strip_prefix("a/").unwrap_or(parts[2]);
    let new_path = parts[3].strip_prefix("b/").unwrap_or(parts[3]);
    if old_path == new_path {
        format!("diff {new_path}")
    } else {
        format!("diff {old_path} -> {new_path}")
    }
}

fn is_diff_change_line(line: &str) -> bool {
    (line.starts_with('+') && !line.starts_with("+++"))
        || (line.starts_with('-') && !line.starts_with("---"))
}

fn balanced_diff_lines<'a>(lines: &[&'a str], limit: usize) -> Vec<&'a str> {
    if lines.len() <= limit {
        return lines.to_vec();
    }
    if limit == 0 {
        return Vec::new();
    }

    let added = lines.iter().filter(|line| line.starts_with('+')).count();
    let removed = lines.iter().filter(|line| line.starts_with('-')).count();
    if added == 0 || removed == 0 || limit == 1 {
        return lines.iter().copied().take(limit).collect();
    }

    let mut added_limit = added.min((limit / 2).max(1));
    let mut removed_limit = removed.min(limit.saturating_sub(added_limit));
    let unused = limit.saturating_sub(added_limit + removed_limit);
    if unused > 0 {
        let added_left = added.saturating_sub(added_limit);
        let removed_left = removed.saturating_sub(removed_limit);
        if added_left >= removed_left {
            let extra = unused.min(added_left);
            added_limit += extra;
            removed_limit += unused.saturating_sub(extra).min(removed_left);
        } else {
            let extra = unused.min(removed_left);
            removed_limit += extra;
            added_limit += unused.saturating_sub(extra).min(added_left);
        }
    }
    let mut added_used = 0usize;
    let mut removed_used = 0usize;
    let mut selected = Vec::new();

    for line in lines {
        if line.starts_with('+') {
            if added_used >= added_limit {
                continue;
            }
            added_used += 1;
        } else if line.starts_with('-') {
            if removed_used >= removed_limit {
                continue;
            }
            removed_used += 1;
        }
        selected.push(*line);
    }

    selected
}

fn render_intra_line_diff(lines: &[&str]) -> Vec<String> {
    let mut rendered = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        if !lines[index].starts_with('-') {
            rendered.push(lines[index].to_string());
            index += 1;
            continue;
        }

        let removed_start = index;
        while index < lines.len() && lines[index].starts_with('-') {
            index += 1;
        }
        let added_start = index;
        while index < lines.len() && lines[index].starts_with('+') {
            index += 1;
        }

        let removed = &lines[removed_start..added_start];
        let added = &lines[added_start..index];
        if removed.len() == 1 && added.len() == 1 {
            let (old_line, new_line) = render_intra_line_pair(removed[0], added[0]);
            rendered.push(old_line);
            rendered.push(new_line);
        } else {
            rendered.extend(removed.iter().map(|line| (*line).to_string()));
            rendered.extend(added.iter().map(|line| (*line).to_string()));
        }
    }

    rendered
}

fn render_intra_line_pair(old_line: &str, new_line: &str) -> (String, String) {
    let old_content = old_line.strip_prefix('-').unwrap_or(old_line);
    let new_content = new_line.strip_prefix('+').unwrap_or(new_line);
    let old_chars = old_content.chars().collect::<Vec<_>>();
    let new_chars = new_content.chars().collect::<Vec<_>>();
    let mut prefix = 0usize;

    while prefix < old_chars.len()
        && prefix < new_chars.len()
        && old_chars[prefix] == new_chars[prefix]
    {
        prefix += 1;
    }

    let mut old_suffix = old_chars.len();
    let mut new_suffix = new_chars.len();
    while old_suffix > prefix
        && new_suffix > prefix
        && old_chars[old_suffix - 1] == new_chars[new_suffix - 1]
    {
        old_suffix -= 1;
        new_suffix -= 1;
    }

    (
        format!(
            "-{}",
            highlight_changed_range(old_content, prefix, old_suffix)
        ),
        format!(
            "+{}",
            highlight_changed_range(new_content, prefix, new_suffix)
        ),
    )
}

fn highlight_changed_range(text: &str, start: usize, end: usize) -> String {
    if start >= end {
        return text.to_string();
    }

    let mut output = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index == start {
            output.push_str(INVERSE_ON);
        }
        output.push(ch);
        if index + 1 == end {
            output.push_str(INVERSE_OFF);
        }
    }
    output
}

fn push_preview_line(preview: &mut Vec<String>, preview_bytes: &mut usize, line: &str) -> bool {
    if preview.len() >= TOOL_OUTPUT_PREVIEW_LINES {
        return false;
    }

    let next_bytes = preview_bytes
        .saturating_add(line.len())
        .saturating_add(usize::from(!preview.is_empty()));
    if next_bytes > TOOL_OUTPUT_PREVIEW_BYTES {
        return false;
    }

    preview.push(line.to_string());
    *preview_bytes = next_bytes;
    true
}

fn limited_preview(output: &str) -> String {
    let mut preview = String::new();
    let mut lines = 0usize;
    let mut truncated = false;

    for line in output.lines() {
        if lines >= TOOL_OUTPUT_PREVIEW_LINES
            || preview.len().saturating_add(line.len()) > TOOL_OUTPUT_PREVIEW_BYTES
        {
            truncated = true;
            break;
        }
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str(line);
        lines += 1;
    }

    if output.is_empty() {
        preview.push_str("(empty output)");
    } else if output.lines().count() > lines {
        truncated = true;
    }

    if truncated {
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str("... tool output truncated in UI");
    }

    preview
}

fn diff_line_kind(line: &str) -> UiKind {
    if line.starts_with("+++") || line.starts_with("---") {
        UiKind::DiffHeader
    } else if line.starts_with('+') {
        UiKind::DiffAdd
    } else if line.starts_with('-') {
        UiKind::DiffRemove
    } else if line.starts_with("@@") || line.starts_with("diff --git") || line.starts_with("index ")
    {
        UiKind::DiffHeader
    } else {
        UiKind::Tool
    }
}

fn rounded_box_border(left: char, right: char, width: usize) -> String {
    format!("{BOX_BORDER}{left}{}{right}{RESET}", "─".repeat(width + 2))
}

fn rounded_box_line(text: &str, width: usize, text_kind: UiKind) -> String {
    let text = truncate_to_width(text, width);
    let text_width = UnicodeWidthStr::width(text.as_str());
    format!(
        "{BOX_BORDER}│{RESET} {}{text}{RESET}{} {BOX_BORDER}│{RESET}",
        color_code(text_kind),
        " ".repeat(width.saturating_sub(text_width))
    )
}

fn spinner_char(preset: SpinnerPreset, tick: usize, step: usize) -> &'static str {
    const LINE: &[&str] = &["-", "\\", "|", "/"];
    const DOTS: &[&str] = &[".", "o", "O", "o"];
    const PULSE: &[&str] = &["+", "x", "*", "x"];
    const SCAN: &[&str] = &["<", "^", ">", "v"];

    let chars = match preset {
        SpinnerPreset::Line => LINE,
        SpinnerPreset::Dots => DOTS,
        SpinnerPreset::Pulse => PULSE,
        SpinnerPreset::Scan => SCAN,
    };
    chars[(tick * step.max(1)) % chars.len()]
}

fn gradient_color(
    value: f32,
    start: (u8, u8, u8),
    middle: (u8, u8, u8),
    end: (u8, u8, u8),
) -> (u8, u8, u8) {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.5 {
        interpolate_color(start, middle, value * 2.0)
    } else {
        interpolate_color(middle, end, (value - 0.5) * 2.0)
    }
}

fn interpolate_color(from: (u8, u8, u8), to: (u8, u8, u8), amount: f32) -> (u8, u8, u8) {
    let amount = amount.clamp(0.0, 1.0);
    (
        interpolate_channel(from.0, to.0, amount),
        interpolate_channel(from.1, to.1, amount),
        interpolate_channel(from.2, to.2, amount),
    )
}

fn interpolate_channel(from: u8, to: u8, amount: f32) -> u8 {
    (from as f32 + (to as f32 - from as f32) * amount).round() as u8
}

fn format_status_line(left: &str, right: &str, width: usize) -> String {
    let width = width.max(1);
    let left_width = UnicodeWidthStr::width(left);
    let right_width = UnicodeWidthStr::width(right);

    if left_width + 1 + right_width <= width {
        return format!(
            "{left}{}{right}",
            " ".repeat(width - left_width - right_width)
        );
    }
    if right_width >= width {
        return truncate_to_width(right, width);
    }

    let left_width = width - right_width - 1;
    format!("{} {right}", truncate_to_width(left, left_width))
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
    let mut output = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        output.push(ch);
        width += ch_width;
    }
    output
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
        UiKind::TreeDirectory => "\x1b[33m",
        UiKind::DiffAdd => "\x1b[32m",
        UiKind::DiffRemove => "\x1b[31m",
        UiKind::DiffHeader => "\x1b[36m",
    }
}

fn colored_reasoning_effort(effort: &str) -> String {
    let color = match effort {
        "none" | "minimal" => "\x1b[38;2;150;150;150m",
        "low" => "\x1b[38;2;90;190;140m",
        "medium" => "\x1b[38;2;230;200;90m",
        "high" => "\x1b[38;2;245;150;70m",
        "xhigh" => "\x1b[38;2;245;90;90m",
        _ => color_code(UiKind::Status),
    };
    format!("{color}{effort}{RESET}{}", color_code(UiKind::Status))
}

fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let chars = text.chars().count() as u64;
    u64::max(1, chars.div_ceil(4))
}

fn format_token_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.2}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{}k", value / 1_000)
    } else {
        value.to_string()
    }
}

fn next_reasoning_effort(efforts: &[String], current: &str) -> String {
    if efforts.is_empty() {
        return current.to_string();
    }
    let index = efforts
        .iter()
        .position(|effort| effort == current)
        .map(|index| (index + 1) % efforts.len())
        .unwrap_or(0);
    efforts[index].clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(text: &str) -> String {
        let mut output = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                output.push(ch);
                continue;
            }
            if chars.next() != Some('[') {
                continue;
            }
            for ch in chars.by_ref() {
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        output
    }

    #[derive(Default)]
    struct TestRuntime {
        submitted: Vec<String>,
        commands: Vec<String>,
    }

    impl TuiRuntime for TestRuntime {
        fn startup_events(&self) -> Vec<AgentEvent> {
            Vec::new()
        }

        fn model_status_event(&self) -> AgentEvent {
            AgentEvent::Status("ready".to_string())
        }

        fn submit_user_message(&mut self, message: String) -> Vec<AgentEvent> {
            self.submitted.push(message.clone());
            vec![AgentEvent::UserMessage(message)]
        }

        fn steer(&mut self) -> Vec<AgentEvent> {
            Vec::new()
        }

        fn handle_command(&mut self, input: &str) -> (bool, Vec<AgentEvent>) {
            self.commands.push(input.to_string());
            (false, Vec::new())
        }

        fn poll_events(&mut self) -> Vec<AgentEvent> {
            Vec::new()
        }
    }

    #[test]
    fn input_buffer_displays_large_paste_as_placeholder() {
        let mut input = InputBuffer::default();
        let pasted = "x".repeat(PASTE_PLACEHOLDER_CHARS + 1);

        input.push_text("prefix ");
        input.push_paste(&pasted);
        input.push_text(" suffix");

        assert_eq!(
            input.display_text(),
            format!(
                "prefix [Pasted: {} chars] suffix",
                PASTE_PLACEHOLDER_CHARS + 1
            )
        );
        assert_eq!(input.text(), format!("prefix {pasted} suffix"));
    }

    #[test]
    fn paste_normalizes_newlines_without_submitting() {
        let mut app = TuiApp::new(TestRuntime::default());

        app.handle_paste("hello\r\nworld");

        assert_eq!(app.input.text(), "hello\nworld");
        assert!(app.runtime.submitted.is_empty());
    }

    #[test]
    fn modified_enter_inserts_newline_and_plain_enter_submits_once() {
        let mut app = TuiApp::new(TestRuntime::default());
        let now = Instant::now();

        app.handle_key_at(KeyCode::Char('a'), KeyModifiers::empty(), now);
        app.handle_key_at(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
            now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(1),
        );
        app.handle_key_at(
            KeyCode::Char('b'),
            KeyModifiers::empty(),
            now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(2),
        );
        app.handle_key_at(
            KeyCode::Enter,
            KeyModifiers::empty(),
            now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(11),
        );

        assert_eq!(app.runtime.submitted, vec!["a\nb".to_string()]);
    }

    #[test]
    fn ctrl_enter_inserts_newline() {
        let mut app = TuiApp::new(TestRuntime::default());
        let now = Instant::now();

        app.handle_key_at(KeyCode::Char('a'), KeyModifiers::empty(), now);
        app.handle_key_at(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
            now + Duration::from_millis(1),
        );
        app.handle_key_at(
            KeyCode::Char('b'),
            KeyModifiers::empty(),
            now + Duration::from_millis(2),
        );
        app.flush_paste_burst_if_due(now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(3));

        assert_eq!(app.input.text(), "a\nb");
        assert!(app.runtime.submitted.is_empty());
    }

    #[test]
    fn paste_burst_keeps_multiline_text_in_input() {
        let mut app = TuiApp::new(TestRuntime::default());
        let now = Instant::now();

        for (index, ch) in "hello".chars().enumerate() {
            app.handle_key_at(
                KeyCode::Char(ch),
                KeyModifiers::empty(),
                now + Duration::from_millis(index as u64),
            );
        }
        app.handle_key_at(
            KeyCode::Enter,
            KeyModifiers::empty(),
            now + Duration::from_millis(5),
        );
        for (index, ch) in "world".chars().enumerate() {
            app.handle_key_at(
                KeyCode::Char(ch),
                KeyModifiers::empty(),
                now + Duration::from_millis(6 + index as u64),
            );
        }

        assert!(app.runtime.submitted.is_empty());
        app.flush_paste_burst_if_due(now + PASTE_BURST_IDLE_TIMEOUT + Duration::from_millis(20));

        assert_eq!(app.input.text(), "hello\nworld");
        assert!(app.runtime.submitted.is_empty());
    }

    #[test]
    fn paste_burst_large_text_uses_placeholder() {
        let mut app = TuiApp::new(TestRuntime::default());
        let now = Instant::now();
        let pasted = "x".repeat(PASTE_PLACEHOLDER_CHARS + 1);

        for (index, ch) in pasted.chars().enumerate() {
            app.handle_key_at(
                KeyCode::Char(ch),
                KeyModifiers::empty(),
                now + Duration::from_millis(index as u64),
            );
        }
        app.flush_paste_burst_if_due(
            now + Duration::from_millis(pasted.len() as u64) + PASTE_BURST_IDLE_TIMEOUT,
        );

        assert_eq!(
            app.input.display_text(),
            format!("[Pasted: {} chars]", PASTE_PLACEHOLDER_CHARS + 1)
        );
        assert_eq!(app.input.text(), pasted);
    }

    #[test]
    fn paste_burst_render_tick_skips_until_pending_char_flushes() {
        let mut app = TuiApp::new(TestRuntime::default());
        let now = Instant::now();
        let mut frames = FrameScheduler {
            next_frame_at: None,
        };

        app.handle_key_at(KeyCode::Char('a'), KeyModifiers::empty(), now);

        assert_eq!(app.input.text(), "");
        assert!(app.paste_burst.is_active());
        assert!(app.handle_paste_burst_render_tick(now, &mut frames));
        assert_eq!(app.input.text(), "");
        assert_eq!(frames.next_frame_at, Some(now + paste_burst_render_delay()));

        assert!(app.handle_paste_burst_render_tick(now + paste_burst_render_delay(), &mut frames));
        assert_eq!(app.input.text(), "a");
        assert_eq!(frames.next_frame_at, Some(now + paste_burst_render_delay()));
        assert!(!app.handle_paste_burst_render_tick(now + paste_burst_render_delay(), &mut frames));
    }

    #[test]
    fn single_typed_char_flushes_after_burst_window() {
        let mut app = TuiApp::new(TestRuntime::default());
        let now = Instant::now();

        app.handle_key_at(KeyCode::Char('a'), KeyModifiers::empty(), now);
        assert_eq!(app.input.text(), "");

        app.flush_paste_burst_if_due(now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(1));

        assert_eq!(app.input.text(), "a");
    }

    #[test]
    fn input_renders_multiple_lines() {
        let document = UiBuilder::new().input("one\ntwo", &[], 0, true).finish();

        assert_eq!(document.controls[0].text, "> one");
        assert_eq!(
            document.controls[1].text,
            format!("  two{CURSOR_MARKER}{VISIBLE_CURSOR}")
        );
    }

    #[test]
    fn cursor_row_is_relative_to_whole_frame() {
        let frame = RenderedFrame::build(
            &UiDocument {
                history: vec![
                    UiLine {
                        kind: UiKind::User,
                        text: "hello".to_string(),
                    },
                    UiLine {
                        kind: UiKind::Assistant,
                        text: "world".to_string(),
                    },
                ],
                controls: vec![UiLine {
                    kind: UiKind::Input,
                    text: format!("> prompt{CURSOR_MARKER}"),
                }],
                reset_screen: false,
            },
            80,
        );

        let cursor = frame.cursor.expect("cursor marker should be found");
        assert_eq!(cursor.row, 3);
        assert_eq!(cursor.column, 8);
    }

    #[test]
    fn command_completion_renders_below_input_with_selected_color() {
        let document = UiBuilder::new()
            .input(
                "/",
                &[
                    CommandCandidate {
                        command: "/help".to_string(),
                        marker: None,
                    },
                    CommandCandidate {
                        command: "/review".to_string(),
                        marker: Some("SKILL".to_string()),
                    },
                ],
                1,
                true,
            )
            .finish();

        assert_eq!(document.controls.len(), 3);
        assert_eq!(document.controls[0].kind, UiKind::Input);
        assert_eq!(
            document.controls[0].text,
            format!("> /{CURSOR_MARKER}{VISIBLE_CURSOR}")
        );
        assert_eq!(document.controls[1].kind, UiKind::Status);
        assert_eq!(document.controls[1].text, "  /help");
        assert_eq!(document.controls[2].kind, UiKind::Selected);
        assert_eq!(document.controls[2].text, "  /review SKILL");
    }

    #[test]
    fn model_and_tokens_render_below_input_without_ready_status() {
        let document = UiBuilder::new()
            .input("hello", &[], 0, true)
            .bottom_status(
                BottomStatus {
                    provider: "openai",
                    model: "gpt-5",
                    reasoning_effort: "medium",
                    input_tokens: 12,
                    output_tokens: 34,
                    context_window: 400_000,
                },
                64,
            )
            .finish();

        assert_eq!(document.controls.len(), 2);
        assert_eq!(
            document.controls[0].text,
            format!("> hello{CURSOR_MARKER}{VISIBLE_CURSOR}")
        );
        assert!(document.controls[1].text.starts_with("openai / gpt-5 ("));
        assert!(document.controls[1]
            .text
            .contains(&colored_reasoning_effort("medium")));
        assert!(document.controls[1]
            .text
            .ends_with("tokens 12/34 | context 0.0%"));
        assert!(!document.controls[1].text.contains("ready"));
        assert_eq!(
            UnicodeWidthStr::width(strip_ansi(&document.controls[1].text).as_str()),
            64
        );
    }

    #[test]
    fn colored_status_line_does_not_wrap_at_visible_width() {
        let document = UiBuilder::new()
            .input("", &[], 0, true)
            .bottom_status(
                BottomStatus {
                    provider: "jucode",
                    model: "claude-opus-4.7",
                    reasoning_effort: "high",
                    input_tokens: 1620,
                    output_tokens: 13,
                    context_window: 400_000,
                },
                64,
            )
            .finish();

        let frame = RenderedFrame::build(&document, 64);

        assert_eq!(frame.lines.len(), 2);
        assert!(strip_ansi(&frame.lines[1]).contains("tokens 1620/13 | context 0.4%"));
        assert_eq!(
            UnicodeWidthStr::width(strip_ansi(&frame.lines[1]).as_str()),
            64
        );
    }

    #[test]
    fn input_can_render_without_hardware_cursor_marker() {
        let document = UiBuilder::new().input("hello", &[], 0, false).finish();

        assert_eq!(document.controls[0].text, "> hello");
    }

    #[test]
    fn startup_renders_inside_box() {
        let document = UiBuilder::new()
            .chat(&[ChatLine::Startup {
                version: "0.1.2".to_string(),
                profile_dir: "C:\\Users\\me\\.jucode".to_string(),
                config_path: "E:\\Code\\Projects\\JuCode\\JuCode-CLI".to_string(),
            }])
            .finish();

        assert!(document.history[0].text.starts_with("\x1b[90m╭"));
        assert!(document.history[1].text.contains("\x1b[90m│"));
        assert!(strip_ansi(&document.history[0].text).starts_with('╭'));
        assert!(strip_ansi(&document.history[1].text).contains(">_ JuCode CLI (v0.1.2)"));
        assert!(strip_ansi(&document.history[3].text).contains("model:"));
        assert!(!document
            .history
            .iter()
            .any(|line| strip_ansi(&line.text).contains("directory:")));
        assert!(strip_ansi(&document.history[4].text).contains("profile:   C:\\Users\\me\\.jucode"));
        assert!(strip_ansi(&document.history[5].text).starts_with('╰'));
    }

    #[test]
    fn connecting_progress_uses_continuous_color_gradient() {
        let now = Instant::now();
        let mut activity = ActivityState::idle();
        activity.kind = ActivityKind::Connecting;
        activity.turn_started_at = Some(now - Duration::from_secs(11));
        activity.phase_started_at = Some(now - Duration::from_secs(11));

        assert_eq!(activity.progress(now).unwrap().color, (255, 60, 60));

        activity.phase_started_at = Some(now - Duration::from_secs(5));
        assert_eq!(activity.progress(now).unwrap().color, (255, 210, 0));

        activity.phase_started_at = Some(now - Duration::from_secs(1));
        assert_eq!(activity.progress(now).unwrap().color, (255, 246, 204));
    }

    #[test]
    fn output_progress_color_warms_as_stream_stalls() {
        let now = Instant::now();
        let mut activity = ActivityState::idle();
        activity.kind = ActivityKind::Output;
        activity.turn_started_at = Some(now - Duration::from_secs(6));
        activity.phase_started_at = Some(now - Duration::from_secs(6));
        activity.estimated_output_tokens = 12;

        activity.last_delta_at = Some(now);
        let fresh = activity.progress(now).unwrap().color;
        activity.last_delta_at = Some(now - Duration::from_secs(3));
        let stalled = activity.progress(now).unwrap().color;

        assert_eq!(fresh, (70, 220, 110));
        assert!(stalled.0 > fresh.0);
        assert!(stalled.2 < fresh.2);
    }

    #[test]
    fn spinner_presets_are_single_ascii_chars() {
        let presets = [
            SpinnerPreset::Line,
            SpinnerPreset::Dots,
            SpinnerPreset::Pulse,
            SpinnerPreset::Scan,
        ];

        for preset in presets {
            let value = spinner_char(preset, 1, 1);
            assert_eq!(value.len(), 1);
            assert!(value.is_ascii());
        }
    }

    #[test]
    fn tool_output_preview_truncates_long_output() {
        let output = (0..20)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = tool_output_preview("edit", &output, false);

        assert!(preview.contains("line 0"));
        assert!(!preview.contains("line 19"));
        assert!(preview.contains("tool output truncated"));
    }

    #[test]
    fn tool_output_preview_prefers_diff_field() {
        let output = serde_json::json!({
            "stdout": "raw",
            "diff": "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n"
        })
        .to_string();

        let preview = tool_output_preview("edit", &output, false);
        let visible_preview = strip_ansi(&preview);

        assert!(preview.contains("diff a"));
        assert!(preview.contains("@@ -1 +1 @@"));
        assert!(visible_preview.contains("-old"));
        assert!(visible_preview.contains("+new"));
        assert!(!visible_preview.contains("raw"));
    }

    #[test]
    fn tool_output_preview_highlights_single_line_replacements() {
        let output = serde_json::json!({
            "diff": "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-JuCode is slow today\n+JuCode is fast today\n"
        })
        .to_string();

        let preview = tool_output_preview("edit", &output, false);

        assert!(preview.contains(&format!("-JuCode is {INVERSE_ON}slow{INVERSE_OFF} today")));
        assert!(preview.contains(&format!("+JuCode is {INVERSE_ON}fast{INVERSE_OFF} today")));
        assert!(strip_ansi(&preview).contains("-JuCode is slow today"));
        assert!(strip_ansi(&preview).contains("+JuCode is fast today"));
    }

    #[test]
    fn tool_output_preview_keeps_additions_after_large_removals() {
        let removals = (0..30)
            .map(|index| format!("-old line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = format!(
            "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1,30 +1,2 @@\n{removals}\n+new important line\n+another important line\n"
        );
        let output = serde_json::json!({
            "stdout": "raw",
            "diff": diff
        })
        .to_string();

        let preview = tool_output_preview("edit", &output, false);

        assert!(preview.contains("diff README.md"));
        assert!(preview.contains("-old line 0"));
        assert!(preview.contains("+new important line"));
        assert!(preview.contains("+another important line"));
        assert!(!preview.contains("--- a/README.md"));
        assert!(!preview.contains("+++ b/README.md"));
        assert!(preview.contains("diff truncated"));
    }

    #[test]
    fn tool_output_preview_projects_read_and_ls() {
        let read = serde_json::json!({
            "path": "C:\\repo\\src\\main.rs",
            "offset": 5,
            "lines_read": 12,
            "content": "hidden"
        })
        .to_string();
        let ls = serde_json::json!({
            "path": "C:\\repo\\src",
            "entries": ["main.rs"]
        })
        .to_string();

        let read_preview = tool_output_preview("read", &read, false);
        let ls_preview = tool_output_preview("ls", &ls, false);

        assert_eq!(read_preview, "read main.rs: 12 lines from line 5");
        assert!(!read_preview.contains("hidden"));
        assert_eq!(ls_preview, "ls C:\\repo\\src");
    }

    #[test]
    fn tool_output_preview_projects_bash_latest_logs() {
        let output = serde_json::json!({
            "command": "cargo test",
            "exit_code": 0,
            "stdout": "one\ntwo\nthree\n",
            "stderr": "",
            "timed_out": false
        })
        .to_string();

        let preview = tool_output_preview("bash", &output, false);

        assert!(preview.contains("bash: cargo test (exit 0)"));
        assert!(preview.contains("stdout:"));
        assert!(preview.contains("three"));
    }

    #[test]
    fn tool_preview_colors_diff_lines() {
        let document = UiBuilder::new()
            .chat(&[ChatLine::Tool {
                call_id: None,
                name: "edit".to_string(),
                output: serde_json::json!({
                    "diff": "diff --git a/a b/a\n-old\n+new\n"
                })
                .to_string(),
                running: false,
            }])
            .finish();

        assert!(document
            .history
            .iter()
            .any(|line| line.kind == UiKind::DiffRemove && line.text.contains("-old")));
        assert!(document
            .history
            .iter()
            .any(|line| line.kind == UiKind::DiffAdd && line.text.contains("+new")));
    }

    #[test]
    fn rendered_frame_keeps_full_history_for_native_scrollback() {
        let document = UiBuilder::new().finish_with_history_and_input(20);

        let frame = RenderedFrame::build(&document, 80);
        let output = frame.lines.join("\n");

        assert!(output.contains("line 0"));
        assert!(output.contains("line 19"));
        assert_eq!(frame.lines.len(), 22);
    }

    #[test]
    fn resize_full_redraw_only_renders_visible_viewport() {
        let document = UiBuilder::new().finish_with_history_and_input(20);
        let frame = RenderedFrame::build(&document, 80);
        let (start, end) =
            full_render_window(frame.lines.len(), 5, FullRenderMode::VisibleViewport);
        let visible = frame.lines[start..end].join("\n");

        assert_eq!(end - start, 5);
        assert!(!visible.contains("line 0"));
        assert!(visible.contains("line 19"));
        assert!(visible.contains(">"));
    }

    #[test]
    fn resize_rebuild_clear_sequence_purges_scrollback() {
        assert_eq!(clear_screen_sequence(false), "\x1b[2J\x1b[H");
        assert!(clear_screen_sequence(true).contains("\x1b[3J"));
        assert!(clear_screen_sequence(true).starts_with("\x1b[r\x1b[0m"));
    }

    #[test]
    fn projection_keeps_live_assistant_out_of_transcript() {
        let document = UiBuilder::new()
            .chat(&[ChatLine::User("hello".to_string())])
            .live_assistant(Some("streaming"))
            .input("", &[], 0, true)
            .finish();

        let projection = ProjectedDocument::from_document(&document, 80);

        assert!(projection
            .transcript_lines
            .iter()
            .any(|line| line.contains("hello")));
        assert!(!projection
            .transcript_lines
            .iter()
            .any(|line| line.contains("streaming")));
        assert!(projection
            .active_lines
            .iter()
            .any(|line| line.contains("streaming")));
    }

    #[test]
    fn cursor_row_accounts_for_full_history() {
        let document = UiBuilder::new().finish_with_history_and_input(20);

        let frame = RenderedFrame::build(&document, 80);
        let cursor = frame.cursor.expect("cursor marker should be found");

        assert_eq!(frame.lines.len(), 22);
        assert_eq!(cursor.row, 21);
        assert_eq!(cursor.column, 2);
    }

    #[test]
    fn checkout_tree_enter_maps_to_checkout_command() {
        let mut tree = PickerState::checkout(vec![TreeNodeView {
            id: "e3".to_string(),
            parent_id: None,
            label: "selected prompt".to_string(),
            active: true,
        }]);

        assert_eq!(tree.selected_command().as_deref(), Some("/checkout e3"));

        tree.begin_tree_prompt(TreePromptAction::Fork);
        for ch in "feature".chars() {
            tree.push_prompt_char(ch);
        }
        assert_eq!(tree.take_prompt_command().as_deref(), Some("/fork feature"));
    }

    #[test]
    fn checkout_tree_fork_and_delete_are_interactive_commands() {
        let mut app = TuiApp::new(TestRuntime::default());
        let now = Instant::now();
        app.picker_view = Some(PickerState::checkout(vec![TreeNodeView {
            id: "e3".to_string(),
            parent_id: None,
            label: "selected prompt".to_string(),
            active: true,
        }]));

        app.handle_key_at(KeyCode::Char('f'), KeyModifiers::empty(), now);
        for (index, ch) in "feature".chars().enumerate() {
            app.handle_key_at(
                KeyCode::Char(ch),
                KeyModifiers::empty(),
                now + Duration::from_millis(index as u64 + 1),
            );
        }
        app.handle_key_at(
            KeyCode::Enter,
            KeyModifiers::empty(),
            now + Duration::from_millis(20),
        );

        app.handle_key_at(
            KeyCode::Delete,
            KeyModifiers::empty(),
            now + Duration::from_millis(30),
        );
        for (index, ch) in "feature".chars().enumerate() {
            app.handle_key_at(
                KeyCode::Char(ch),
                KeyModifiers::empty(),
                now + Duration::from_millis(index as u64 + 31),
            );
        }
        app.handle_key_at(
            KeyCode::Enter,
            KeyModifiers::empty(),
            now + Duration::from_millis(50),
        );

        assert_eq!(
            app.runtime.commands,
            vec!["/fork feature".to_string(), "/delete feature".to_string()]
        );
    }

    #[test]
    fn checkout_tree_defaults_to_two_visible_levels_and_expands_right() {
        let mut tree = PickerState::checkout(vec![
            TreeNodeView {
                id: "e1".to_string(),
                parent_id: None,
                label: "first".to_string(),
                active: false,
            },
            TreeNodeView {
                id: "e2".to_string(),
                parent_id: Some("e1".to_string()),
                label: "second".to_string(),
                active: false,
            },
            TreeNodeView {
                id: "e3".to_string(),
                parent_id: Some("e2".to_string()),
                label: "third".to_string(),
                active: false,
            },
        ]);

        assert_eq!(
            tree.rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["e1", "e2"]
        );
        assert!(tree.rows[0].prefix.contains("──"));

        tree.selected = 1;
        tree.move_first_child();

        assert_eq!(
            tree.rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["e1", "e2", "e3"]
        );
    }

    #[test]
    fn checkout_tree_marks_rows_with_children_as_directories() {
        let tree = PickerState::checkout(vec![
            TreeNodeView {
                id: "e1".to_string(),
                parent_id: None,
                label: "first".to_string(),
                active: false,
            },
            TreeNodeView {
                id: "e2".to_string(),
                parent_id: Some("e1".to_string()),
                label: "second".to_string(),
                active: false,
            },
            TreeNodeView {
                id: "e3".to_string(),
                parent_id: Some("e2".to_string()),
                label: "third".to_string(),
                active: false,
            },
        ]);
        let document = UiBuilder::new().picker(Some(&tree)).finish();

        assert!(document
            .controls
            .iter()
            .any(|line| line.text.contains("[-] first")));
        assert!(document
            .controls
            .iter()
            .any(|line| line.kind == UiKind::TreeDirectory && line.text.contains("[+] second")));
    }

    #[test]
    fn resume_picker_enter_maps_to_resume_command_without_delete() {
        let tree = PickerState::resume(vec![SessionListItemView {
            id: "s123".to_string(),
            label: "s123 | entries 3 | leaf e2 | updated 1".to_string(),
            active: false,
        }]);

        assert_eq!(tree.selected_command().as_deref(), Some("/resume s123"));
    }

    #[test]
    fn model_picker_enter_includes_selected_effort() {
        let mut picker = PickerState::model(
            vec![
                ModelOptionView {
                    model: "gpt-5.2".to_string(),
                    active: false,
                    context_window: 400_000,
                    max_output_tokens: 128_000,
                    reasoning_efforts: vec!["none".to_string(), "low".to_string()],
                },
                ModelOptionView {
                    model: "gpt-5.3-codex".to_string(),
                    active: true,
                    context_window: 400_000,
                    max_output_tokens: 128_000,
                    reasoning_efforts: vec![
                        "low".to_string(),
                        "medium".to_string(),
                        "high".to_string(),
                        "xhigh".to_string(),
                    ],
                },
            ],
            "low".to_string(),
        );

        assert_eq!(
            picker.selected_command().as_deref(),
            Some("/model gpt-5.3-codex low")
        );

        picker.cycle_effort();

        assert_eq!(
            picker.selected_command().as_deref(),
            Some("/model gpt-5.3-codex medium")
        );
    }

    #[test]
    fn model_picker_renders_effort_hint() {
        let picker = PickerState::model(
            vec![ModelOptionView {
                model: "gpt-5.2".to_string(),
                active: true,
                context_window: 400_000,
                max_output_tokens: 128_000,
                reasoning_efforts: vec!["none".to_string(), "low".to_string()],
            }],
            "none".to_string(),
        );
        let document = UiBuilder::new().picker(Some(&picker)).finish();

        assert!(document.controls.iter().any(|line| line
            .text
            .contains(&format!("thinking: {}", colored_reasoning_effort("none")))));
        assert!(document
            .controls
            .iter()
            .any(|line| line.text.contains("gpt-5.2") && line.text.contains(" *")));
    }

    #[test]
    fn shift_tab_effort_cycle_wraps() {
        let efforts = vec!["none".to_string(), "low".to_string(), "medium".to_string()];
        assert_eq!(next_reasoning_effort(&efforts, "none"), "low");
        assert_eq!(next_reasoning_effort(&efforts, "low"), "medium");
        assert_eq!(next_reasoning_effort(&efforts, "medium"), "none");
        assert_eq!(next_reasoning_effort(&efforts, "unknown"), "none");
    }
}

#[cfg(test)]
trait TestUiBuilderExt {
    fn finish_with_history_and_input(self, history_lines: usize) -> UiDocument;
}

#[cfg(test)]
impl TestUiBuilderExt for UiBuilder {
    fn finish_with_history_and_input(mut self, history_lines: usize) -> UiDocument {
        for index in 0..history_lines {
            self.history_line(UiKind::Assistant, format!("line {index}"));
        }
        self.input("", &[], 0, true).finish()
    }
}
