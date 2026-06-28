#[cfg(feature = "bench")]
pub mod bench_support;
mod input;
mod markdown;
mod picker;
mod state;
mod terminal_renderer;
#[cfg(test)]
mod tests;
mod tool_preview;
mod ui_builder;

use input::{paste_burst_render_delay, InputBuffer, PasteBurst, PasteCharDecision, PasteFlush};
use jucode_agent_core::{AgentEvent, CommandView, TranscriptItem};
use picker::{PickerState, TreePromptAction};
use ratatui::crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use state::TuiState;
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};
use terminal_renderer::TerminalRenderer;
use ui_builder::UiBuilder;
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
const SELECT_START: &str = "\x1b[7m";
const SELECT_END: &str = "\x1b[27m";
const ENABLE_BRACKETED_PASTE: &str = "\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &str = "\x1b[?2004l";
pub(crate) const CONTENT_LEFT_PADDING: usize = 2;
pub(crate) const RESET: &str = "\x1b[0m";
const INVERSE_ON: &str = "\x1b[7m";
const INVERSE_OFF: &str = "\x1b[27m";
const BOX_BORDER: &str = "\x1b[90m";
const STARTUP_TEXT: &str = "\x1b[38;2;180;176;187m";
const STARTUP_DIM: &str = "\x1b[38;2;125;121;134m";
const STARTUP_ACCENT: &str = "\x1b[38;2;190;160;255m";
const STARTUP_STRONG: &str = "\x1b[38;2;232;228;238m";
const ANSI_ESCAPE: char = '\x1b';

#[derive(Debug, Clone)]
pub(crate) enum ChatLine {
    Startup {
        version: String,
        profile_dir: String,
        config_path: String,
        cwd: String,
        model: String,
        context_window: u64,
    },
    User(String),
    Assistant(String),
    Reasoning {
        text: String,
        collapsed: bool,
    },
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
pub(crate) enum UiKind {
    Brand,
    User,
    Assistant,
    Separator,
    ToolHeader,
    Tool,
    System,
    Error,
    Status,
    BottomStatus,
    Selected,
    Input,
    TreeDirectory,
    DiffAdd,
    DiffRemove,
    DiffHeader,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UiLine {
    pub(crate) kind: UiKind,
    pub(crate) text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct UiDocument {
    history: Vec<UiLine>,
    rendered_history_lines: Option<Vec<UiLine>>,
    controls: Vec<UiLine>,
    pub(crate) reset_screen: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CursorTarget {
    pub(crate) row: usize,
    pub(crate) column: usize,
}

#[cfg(test)]
pub(crate) struct RenderedFrame {
    pub(crate) lines: Vec<String>,
    pub(crate) cursor: Option<CursorTarget>,
}

// Flat projection of a document into terminal lines plus cursor position. The live
// renderer composes regions directly with ratatui widgets; this remains as a compact
// model for asserting layout invariants in tests.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct ProjectedDocument {
    pub(crate) transcript_lines: Vec<String>,
    active_lines: Vec<String>,
    cursor: Option<CursorTarget>,
}

pub(crate) struct BottomStatus<'a> {
    pub(crate) provider: &'a str,
    pub(crate) model: &'a str,
    pub(crate) reasoning_effort: &'a str,
    pub(crate) context_tokens: u64,
    pub(crate) context_window: u64,
    pub(crate) cost: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandCandidate {
    pub(crate) command: String,
    pub(crate) marker: Option<String>,
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
        "/help", "/login", "/new", "/model", "/tree", "/trust", "/resume", "/context", "/doctor",
        "/skills", "/pin", "/goal", "/compact", "/quit",
    ]
    .iter()
    .map(|command| CommandCandidate {
        command: (*command).to_string(),
        marker: None,
    })
    .collect()
}

fn format_plan_summary(items: &[jucode_agent_core::PlanItem]) -> String {
    if items.is_empty() {
        return "Plan cleared".to_string();
    }
    let mut lines = vec!["Plan".to_string()];
    for item in items {
        let mark = match item.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[~]",
            _ => "[ ]",
        };
        lines.push(format!("{mark} {}", item.step));
    }
    lines.join("\n")
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

fn format_context_window(value: u64) -> String {
    if value >= 1_000_000 && value.is_multiple_of(1_000_000) {
        format!("{}M", value / 1_000_000)
    } else if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{}K", value / 1_000)
    } else {
        value.to_string()
    }
}

fn compact_home_path(path: &str) -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok();
    let Some(home) = home else {
        return path.to_string();
    };
    let home = home.trim_end_matches(['\\', '/']);
    if path == home {
        "~".to_string()
    } else {
        path.strip_prefix(&format!("{home}\\"))
            .or_else(|| path.strip_prefix(&format!("{home}/")))
            .map(|rest| format!("~/{rest}").replace('\\', "/"))
            .unwrap_or_else(|| path.to_string())
    }
}

/// Number of transcript lines a PageUp/PageDown moves the viewport, one screen minus a
/// little overlap so context carries across the jump. Falls back if the size query fails.
fn scroll_page_size() -> usize {
    terminal::size()
        .map(|(_, height)| (height.saturating_sub(2)).max(1) as usize)
        .unwrap_or(10)
}

fn pad_to_width(text: &str, width: usize) -> String {
    let visible_width = UnicodeWidthStr::width(text);
    if visible_width >= width {
        text.to_string()
    } else {
        format!("{}{}", text, " ".repeat(width - visible_width))
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
    state: TuiState,
    /// Lines the transcript viewport is lifted above the live tail (PageUp/PageDown).
    /// Zero follows live output; the renderer clamps it to the available range.
    scroll_offset: usize,
}

#[derive(Debug, Clone)]
enum ActivityKind {
    Idle,
    Connecting,
    Compacting,
    Thinking,
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

#[derive(Debug, Clone)]
struct FrameScheduler {
    next_frame_at: Option<Instant>,
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
            state: TuiState::default(),
            scroll_offset: 0,
        };
        let events = app.runtime.startup_events();
        app.apply_events(events);
        app
    }

    pub fn run(mut self) -> io::Result<()> {
        let _guard = TerminalGuard::enter()?;
        let mut renderer = TerminalRenderer::new()?;
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

            if self.state.activity.is_active() && now >= next_progress_at {
                self.state.activity.advance_animation();
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
                renderer.render(&document, &mut self.scroll_offset)?;
                if document.reset_screen {
                    self.state.reset_screen = false;
                }
                continue;
            }

            let poll_timeout = {
                let mut timeout = frames.poll_timeout(now, EVENT_POLL_INTERVAL);
                if self.state.activity.is_active() {
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

        if self.state.picker_view.is_some() {
            self.flush_paste_burst_before_non_plain_input();
            return self.handle_picker_key(code, modifiers);
        }

        // Scrolling the transcript is a transient browse mode: any other interaction
        // snaps the viewport back to the live tail so new output and typing stay visible.
        if !matches!(code, KeyCode::PageUp | KeyCode::PageDown) {
            self.scroll_offset = 0;
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
                    if self.input.has_selection() {
                        self.input.delete_selection();
                        self.clamp_completion_index();
                    }
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
                    let count = self.state.command_matches(&self.input).len();
                    if count > 0 {
                        self.state.completion_index =
                            (self.state.completion_index + count - 1) % count;
                    }
                } else {
                    self.input.move_up(modifiers.contains(KeyModifiers::SHIFT));
                }
                false
            }
            KeyCode::Down => {
                self.flush_paste_burst_before_non_plain_input();
                if self.command_completion_active() {
                    let count = self.state.command_matches(&self.input).len();
                    if count > 0 {
                        self.state.completion_index = (self.state.completion_index + 1) % count;
                    }
                } else {
                    self.input
                        .move_down(modifiers.contains(KeyModifiers::SHIFT));
                }
                false
            }
            KeyCode::Left => {
                self.flush_paste_burst_before_non_plain_input();
                let extend = modifiers.contains(KeyModifiers::SHIFT);
                if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                    self.input.move_word_left(extend);
                } else {
                    self.input.move_left(extend);
                }
                false
            }
            KeyCode::Right => {
                self.flush_paste_burst_before_non_plain_input();
                let extend = modifiers.contains(KeyModifiers::SHIFT);
                if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                    self.input.move_word_right(extend);
                } else {
                    self.input.move_right(extend);
                }
                false
            }
            KeyCode::Home => {
                self.flush_paste_burst_before_non_plain_input();
                let extend = modifiers.contains(KeyModifiers::SHIFT);
                if modifiers.contains(KeyModifiers::CONTROL) {
                    self.input.move_document_start(extend);
                } else {
                    self.input.move_home(extend);
                }
                false
            }
            KeyCode::End => {
                self.flush_paste_burst_before_non_plain_input();
                let extend = modifiers.contains(KeyModifiers::SHIFT);
                if modifiers.contains(KeyModifiers::CONTROL) {
                    self.input.move_document_end(extend);
                } else {
                    self.input.move_end(extend);
                }
                false
            }
            KeyCode::Delete => {
                self.flush_paste_burst_before_non_plain_input();
                self.input.delete_forward();
                self.clamp_completion_index();
                false
            }
            KeyCode::PageUp => {
                self.flush_paste_burst_before_non_plain_input();
                self.scroll_offset = self.scroll_offset.saturating_add(scroll_page_size());
                false
            }
            KeyCode::PageDown => {
                self.flush_paste_burst_before_non_plain_input();
                self.scroll_offset = self.scroll_offset.saturating_sub(scroll_page_size());
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
                if self.state.activity.is_active() && !self.state.pending_messages.is_empty() {
                    let events = self.runtime.steer();
                    self.apply_events(events);
                    return false;
                }
                self.input.clear();
                self.state.completion_index = 0;
                false
            }
            KeyCode::Enter => {
                if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) {
                    self.flush_paste_burst_before_non_plain_input();
                    self.input.push_char('\n');
                    self.state.completion_index = 0;
                    return false;
                }
                if self.paste_burst.append_newline_if_active(now) {
                    self.state.completion_index = 0;
                    return false;
                }
                if self
                    .paste_burst
                    .newline_should_insert_instead_of_submit(now)
                {
                    self.input.push_char('\n');
                    self.state.completion_index = 0;
                    return false;
                }
                self.flush_paste_burst_before_non_plain_input();
                if self.should_complete_on_enter() {
                    self.complete_selected_command();
                    return false;
                }

                let submitted = self.input.text().trim().to_string();
                self.input.clear();
                self.state.completion_index = 0;
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
                if let Some(picker) = self.state.picker_view.as_mut() {
                    picker.cancel_prompt();
                }
                false
            }
            KeyCode::Backspace => {
                if let Some(picker) = self.state.picker_view.as_mut() {
                    picker.pop_prompt_char();
                }
                false
            }
            KeyCode::Enter => {
                let Some(command) = self
                    .state
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
                if let Some(picker) = self.state.picker_view.as_mut() {
                    picker.push_prompt_char(ch);
                }
                false
            }
            _ => false,
        }
    }

    fn handle_picker_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if self
            .state
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
                self.state.picker_view = None;
                false
            }
            KeyCode::Up => {
                if let Some(picker) = self.state.picker_view.as_mut() {
                    picker.move_previous();
                }
                false
            }
            KeyCode::Down => {
                if let Some(picker) = self.state.picker_view.as_mut() {
                    picker.move_next();
                }
                false
            }
            KeyCode::Left => {
                if let Some(picker) = self.state.picker_view.as_mut() {
                    picker.move_parent();
                }
                false
            }
            KeyCode::Right => {
                if let Some(picker) = self.state.picker_view.as_mut() {
                    picker.move_first_child();
                }
                false
            }
            KeyCode::BackTab => {
                if let Some(picker) = self.state.picker_view.as_mut() {
                    picker.cycle_effort();
                }
                false
            }
            KeyCode::Char('f') | KeyCode::Char('n')
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(picker) = self.state.picker_view.as_mut() {
                    picker.begin_tree_prompt(TreePromptAction::Fork);
                }
                false
            }
            KeyCode::Delete => {
                if let Some(picker) = self.state.picker_view.as_mut() {
                    picker.begin_tree_prompt(TreePromptAction::Delete);
                }
                false
            }
            KeyCode::Enter => {
                let Some(command) = self
                    .state
                    .picker_view
                    .as_ref()
                    .and_then(PickerState::selected_command)
                else {
                    self.state.picker_view = None;
                    return false;
                };
                self.state.picker_view = None;
                let (_, events) = self.runtime.handle_command(&command);
                self.apply_events(events);
                false
            }
            _ => false,
        }
    }

    fn build_document(&mut self, width: usize, now: Instant) -> UiDocument {
        self.state.build_document(&self.input, width, now)
    }

    fn clamp_completion_index(&mut self) {
        self.state.clamp_completion_index(&self.input);
    }

    fn command_completion_active(&self) -> bool {
        self.state.command_completion_active(&self.input)
    }

    fn should_complete_on_enter(&self) -> bool {
        self.state.should_complete_on_enter(&self.input)
    }

    fn complete_selected_command(&mut self) {
        self.state.complete_selected_command(&mut self.input);
    }

    fn apply_events(&mut self, events: Vec<AgentEvent>) -> bool {
        self.state.apply_events(events, &mut self.input)
    }

    fn cycle_reasoning_effort(&mut self) {
        if self.state.model == "unknown" {
            return;
        }
        let next =
            next_reasoning_effort(&self.state.reasoning_efforts, &self.state.reasoning_effort);
        let (_, events) = self
            .runtime
            .handle_command(&format!("/model {} {next}", self.state.model));
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

    fn start_thinking(&mut self) {
        let now = Instant::now();
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(now);
        }
        if !matches!(self.kind, ActivityKind::Thinking) {
            self.phase_started_at = Some(now);
        }
        self.kind = ActivityKind::Thinking;
    }

    fn start_compacting(&mut self) {
        let now = Instant::now();
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(now);
            self.estimated_output_tokens = 0;
            self.last_delta_at = None;
        }
        if !matches!(self.kind, ActivityKind::Compacting) {
            self.phase_started_at = Some(now);
        }
        self.kind = ActivityKind::Compacting;
    }

    fn set_compaction_output_tokens(&mut self, output_tokens: u64) {
        if !matches!(self.kind, ActivityKind::Compacting) {
            self.start_compacting();
        }
        self.estimated_output_tokens = output_tokens;
        self.last_delta_at = Some(Instant::now());
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

    fn progress(&self, now: Instant, thinking_tokens: u64) -> Option<ProgressState> {
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
            ActivityKind::Thinking => {
                let token_suffix = if thinking_tokens > 0 {
                    format!(" ({thinking_tokens} tokens)")
                } else {
                    String::new()
                };
                Some(ProgressState {
                    color: gradient_color(
                        elapsed.as_secs_f32() / 30.0,
                        (160, 130, 230),
                        (140, 110, 220),
                        (120, 90, 210),
                    ),
                    preset: SpinnerPreset::Pulse,
                    label: format!("thinking {:.1}s{token_suffix}", elapsed.as_secs_f32()),
                    step: 1,
                })
            }
            ActivityKind::Compacting => Some(ProgressState {
                color: (90, 200, 220),
                preset: SpinnerPreset::Pulse,
                label: format!(
                    "compacting context [{}] {} tok {:.1}s",
                    indeterminate_bar(self.animation_tick, 14),
                    self.estimated_output_tokens,
                    elapsed.as_secs_f32()
                ),
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

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        stdout.write_all(ENABLE_BRACKETED_PASTE.as_bytes())?;
        stdout.flush()?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(DISABLE_BRACKETED_PASTE.as_bytes());
        let _ = stdout.flush();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
impl RenderedFrame {
    fn build(document: &UiDocument, width: u16) -> Self {
        ProjectedDocument::from_document(document, width).into_frame()
    }
}

pub(crate) fn wrap_lines(lines: &[UiLine], width: usize) -> Vec<UiLine> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for line in lines {
        wrap_line(line, width, &mut wrapped);
    }
    wrapped
}

pub(crate) fn padded_content_width(width: usize) -> usize {
    width.saturating_sub(CONTENT_LEFT_PADDING).max(1)
}

fn wrap_line(line: &UiLine, width: usize, output: &mut Vec<UiLine>) {
    if line.text.is_empty() {
        output.push(line.clone());
        return;
    }

    let mut current = String::new();
    let mut current_width = 0;
    let mut rest = line.text.as_str();
    let mut reverse_active = false;

    while !rest.is_empty() {
        if let Some(next) = rest.strip_prefix(CURSOR_MARKER) {
            current.push_str(CURSOR_MARKER);
            rest = next;
            continue;
        }
        if let Some((sequence, next)) = split_ansi_sequence(rest) {
            if sequence == SELECT_START {
                reverse_active = true;
            } else if sequence == SELECT_END || sequence == RESET {
                reverse_active = false;
            }
            current.push_str(sequence);
            rest = next;
            continue;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        let ch_width = ch.width().unwrap_or(0);
        if current_width > 0 && current_width + ch_width > width {
            if reverse_active {
                current.push_str(SELECT_END);
            }
            output.push(UiLine {
                kind: line.kind,
                text: current,
            });
            current = String::new();
            if reverse_active {
                current.push_str(SELECT_START);
            }
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

pub(crate) fn split_ansi_sequence(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("\x1b[")?;
    for (index, ch) in rest.char_indices() {
        if ch.is_ascii_alphabetic() {
            let end = 2 + index + ch.len_utf8();
            return Some((&text[..end], &text[end..]));
        }
    }
    None
}

pub(crate) fn extract_cursor(lines: &mut [UiLine]) -> Option<CursorTarget> {
    for (row, line) in lines.iter_mut().enumerate().rev() {
        let Some(index) = line.text.find(CURSOR_MARKER) else {
            continue;
        };
        let before = &line.text[..index];
        let column = visible_width(before);
        line.text
            .replace_range(index..index + CURSOR_MARKER.len(), "");
        return Some(CursorTarget { row, column });
    }
    None
}

pub(crate) fn render_ansi_line(line: &UiLine) -> String {
    // Input lines carry reverse-video toggles (block caret/selection) and Assistant
    // lines carry markdown styling (bold/italic/code). Wrap both in the kind color +
    // RESET regardless, so surrounding text keeps its color and the inline toggles
    // compose with it.
    let always_color = matches!(
        line.kind,
        UiKind::Input | UiKind::Assistant | UiKind::Status | UiKind::BottomStatus
    );
    if !always_color && line.text.contains(ANSI_ESCAPE) {
        return line.text.clone();
    }
    format!("{}{}{}", color_code(line.kind), line.text, RESET)
}

pub(crate) fn spinner_char(preset: SpinnerPreset, tick: usize, step: usize) -> &'static str {
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

/// Display width of `text`, skipping ANSI escape sequences (CSI/SGR).
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((_, next)) = split_ansi_sequence(rest) {
            rest = next;
            continue;
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        width += ch.width().unwrap_or(0);
        rest = &rest[ch.len_utf8()..];
    }
    width
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

fn truncate_with_ellipsis(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if visible_width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let ellipsis_width = visible_width("…");
    let mut output = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width + ellipsis_width > max_width {
            break;
        }
        output.push(ch);
        width += ch_width;
    }
    output.push('…');
    output
}

/// An indeterminate progress bar that fills and drains across `width` cells,
/// animated by the frame tick (compaction has no known total to measure against).
fn indeterminate_bar(tick: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cycle = width.saturating_mul(2).max(1);
    let phase = tick % cycle;
    let head = if phase < width { phase } else { cycle - phase };
    (0..width)
        .map(|index| if index <= head { '=' } else { ' ' })
        .collect()
}

/// Lines of reasoning text kept visible after reasoning completes.
const THINKING_COLLAPSED_LINES: usize = 3;

pub(crate) fn color_code(kind: UiKind) -> &'static str {
    match kind {
        UiKind::Brand => "\x1b[34m",
        UiKind::User => "\x1b[36m",
        UiKind::Assistant => "\x1b[37m",
        UiKind::Separator => "\x1b[38;2;105;108;120m",
        UiKind::ToolHeader => "\x1b[37m",
        UiKind::Tool | UiKind::System | UiKind::Status => "\x1b[90m",
        UiKind::BottomStatus => "\x1b[97;48;2;30;33;43m",
        UiKind::Error => "\x1b[31m",
        UiKind::Selected => "\x1b[97m",
        UiKind::Input => "\x1b[38;2;224;226;232m",
        UiKind::TreeDirectory => "\x1b[33m",
        UiKind::DiffAdd => "\x1b[38;2;170;220;170;48;2;28;70;38m",
        UiKind::DiffRemove => "\x1b[38;2;230;150;145;48;2;85;38;32m",
        UiKind::DiffHeader => "\x1b[36m",
    }
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
