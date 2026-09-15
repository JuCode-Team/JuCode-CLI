use std::time::Instant;

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::markdown::render_markdown;
use crate::picker::{PickerMode, PickerState, TreePromptAction};
use crate::tool_preview::{format_tool_header, tool_output_preview};
use crate::{
    compact_home_path, format_context_window, pad_to_width, spinner_char, truncate_line_to_width,
    truncate_to_width, ActivityState, BottomStatus, ChatLine, CommandCandidate, UiDocument, UiKind,
    UiLine, BOX_BORDER, STARTUP_ACCENT, STARTUP_DIM, STARTUP_STRONG, STARTUP_TEXT, VISIBLE_CURSOR,
};

/// Detail lines under a tool header hang off a `⎿` gutter on the first row,
/// aligning the rest under it — the block reads as one unit.
const TOOL_GUTTER_FIRST: &str = "  ⎿  ";
const TOOL_GUTTER_REST: &str = "     ";
const INPUT_PROMPT: &str = "›";
/// Picker rows rendered at once; longer lists window around the selection.
pub(crate) const PICKER_MAX_ROWS: usize = 15;

fn format_thinking_duration(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn rounded_box_border(left: char, right: char, width: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!("{left}{}{right}", "─".repeat(width + 2)),
        BOX_BORDER,
    ))
}

fn startup_box_line(
    mascot: &str,
    text: &str,
    mascot_width: usize,
    right_width: usize,
    width: usize,
) -> Line<'static> {
    let plain_width = mascot_width + 3 + right_width;
    let text_width = UnicodeWidthStr::width(text);
    let text_padding = " ".repeat(right_width.saturating_sub(text_width));
    let fill = " ".repeat(width.saturating_sub(plain_width));
    let mut spans = vec![
        Span::styled("│", BOX_BORDER),
        Span::raw(" "),
        Span::styled(pad_to_width(mascot, mascot_width), STARTUP_ACCENT),
        Span::raw("   "),
    ];
    spans.extend(startup_text_spans(text));
    spans.push(Span::raw(format!("{text_padding}{fill} ")));
    spans.push(Span::styled("│", BOX_BORDER));
    Line::from(spans)
}

fn startup_text_spans(text: &str) -> Vec<Span<'static>> {
    if let Some(rest) = text.strip_prefix("Welcome to ") {
        if let Some(details) = rest.strip_prefix("JuCode") {
            return vec![
                Span::styled("Welcome to ", STARTUP_STRONG),
                Span::styled("JuCode", STARTUP_ACCENT),
                Span::styled(details.to_string(), STARTUP_DIM),
            ];
        }
    }
    if let Some(path) = text.strip_prefix("cwd: ") {
        return vec![
            Span::styled("cwd: ", STARTUP_TEXT),
            Span::styled(path.to_string(), STARTUP_STRONG),
        ];
    }
    if text == "/help for commands · /exit to quit" {
        return vec![
            Span::styled("/help", STARTUP_STRONG),
            Span::styled(" for commands · ", STARTUP_TEXT),
            Span::styled("/exit", STARTUP_STRONG),
            Span::styled(" to quit", STARTUP_TEXT),
        ];
    }
    vec![Span::styled(text.to_string(), STARTUP_TEXT)]
}

/// Left/right status layout; `left` spans keep their own styles (e.g. the
/// colored reasoning effort) while `right` is plain text.
fn format_status_line(left: Vec<Span<'static>>, right: &str, width: usize) -> Line<'static> {
    let width = width.max(1);
    let left_width: usize = left
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let right_width = UnicodeWidthStr::width(right);

    if left_width + 1 + right_width <= width {
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(width - left_width - right_width)));
        spans.push(Span::raw(right.to_string()));
        return Line::from(spans);
    }
    if right_width >= width {
        return Line::from(truncate_to_width(right, width));
    }

    let left_budget = width - right_width - 1;
    let mut spans = truncate_line_to_width(&Line::from(left), left_budget).spans;
    spans.push(Span::raw(format!(" {right}")));
    Line::from(spans)
}

fn reasoning_effort_style(effort: &str) -> Style {
    let color = match effort {
        "none" | "minimal" => Color::Rgb(150, 150, 150),
        "low" => Color::Rgb(90, 190, 140),
        "medium" => Color::Rgb(230, 200, 90),
        "high" => Color::Rgb(245, 150, 70),
        "xhigh" => Color::Rgb(245, 90, 90),
        _ => return Style::default(),
    };
    Style::new().fg(color)
}

fn reasoning_effort_span(effort: &str) -> Span<'static> {
    Span::styled(effort.to_string(), reasoning_effort_style(effort))
}

pub(crate) struct UiBuilder {
    history: Vec<UiLine>,
    rendered_history_lines: Option<Vec<UiLine>>,
    controls: Vec<UiLine>,
    reset_screen: bool,
}

impl UiBuilder {
    pub(crate) fn new() -> Self {
        Self {
            history: Vec::new(),
            rendered_history_lines: None,
            controls: Vec::new(),
            reset_screen: false,
        }
    }

    pub(crate) fn rendered_history_lines(mut self, lines: Vec<UiLine>) -> Self {
        self.rendered_history_lines = Some(lines);
        self
    }

    #[cfg(test)]
    pub(crate) fn chat(self, chat: &[ChatLine]) -> Self {
        self.chat_with_width(chat, usize::MAX)
    }

    pub(crate) fn chat_with_width(mut self, chat: &[ChatLine], width: usize) -> Self {
        for (item_index, item) in chat.iter().enumerate() {
            self.separate_block();
            match item {
                ChatLine::Startup {
                    version,
                    profile_dir,
                    config_path,
                    cwd,
                    model,
                    context_window,
                } => self.push_startup_box(
                    version,
                    profile_dir,
                    config_path,
                    cwd,
                    model,
                    *context_window,
                ),
                ChatLine::User(text) => self.push_user_message(text),
                ChatLine::Assistant(text) => {
                    for line in render_markdown(text, width) {
                        self.history_line(UiKind::Assistant, line);
                    }
                }
                ChatLine::Reasoning {
                    text,
                    collapsed,
                    duration_secs,
                } => {
                    // Clicking the header toggles the block; collapsed keeps
                    // only the header with its recorded duration.
                    let header = match duration_secs {
                        Some(secs) => {
                            format!("✻ thought for {}", format_thinking_duration(*secs))
                        }
                        None if *collapsed => "✻ thought".to_string(),
                        None => "✻ thinking".to_string(),
                    };
                    self.history_clickable_line(UiKind::Status, header, item_index);
                    if !*collapsed {
                        for line in render_markdown(text, width.saturating_sub(2)) {
                            let mut spans = vec![Span::raw("  ")];
                            spans.extend(line.spans);
                            self.history_line(UiKind::Status, Line::from(spans));
                        }
                    }
                }
                ChatLine::Tool {
                    name,
                    output,
                    running,
                    ..
                } => self.push_tool_block(name, output, *running, width),
                ChatLine::System(text) => self.push_history(UiKind::System, text),
                ChatLine::Error(text) => self.push_history(UiKind::Error, text),
            }
        }
        self
    }

    pub(crate) fn picker(mut self, picker: Option<&PickerState>) -> Self {
        let Some(picker) = picker else {
            return self;
        };
        let hint = match picker.mode {
            PickerMode::Checkout => {
                "tree: arrows move/expand, enter checkout, f fork, delete branch, esc close"
            }
            PickerMode::Resume => "resume: arrows move, enter resume, esc close",
            PickerMode::Rewind => "rewind: arrows move, enter rewind to turn, esc close",
            PickerMode::Approval => "approve tool? arrows move, enter select, esc deny",
            PickerMode::Model => "model: arrows move, shift+tab effort, enter select, esc close",
            PickerMode::Trust => {
                "trust project? arrows move, enter select (loads project skills & hooks if trusted)"
            }
            PickerMode::Login => "login: arrows move, pgup/pgdn page, enter select, esc close",
            PickerMode::LoginPaste => {
                "finish sign-in: paste the redirect URL or code, enter submit, esc close"
            }
        };
        self.control_line(UiKind::Status, hint.to_string());
        if let Some(prompt) = picker.prompt.as_ref() {
            let label = match prompt.action {
                TreePromptAction::Fork => "fork branch",
                TreePromptAction::Delete => "delete branch",
                TreePromptAction::ApiKey => "paste api key",
                TreePromptAction::LoginPaste => "redirect url or code",
            };
            // The `|` is the prompt's visible caret; the hardware cursor stays
            // in the composer below.
            self.control_line(
                UiKind::Input,
                format!("{INPUT_PROMPT} {label}: {}{VISIBLE_CURSOR}", prompt.input),
            );
        }
        if picker.mode == PickerMode::Model && !picker.efforts.is_empty() {
            let effort = &picker.efforts[picker.selected_effort];
            self.control_line(
                UiKind::Status,
                Line::from(vec![Span::raw("thinking: "), reasoning_effort_span(effort)]),
            );
        }
        if picker.rows.is_empty() {
            if picker.mode != PickerMode::LoginPaste {
                self.control_line(UiKind::Status, "(empty)".to_string());
            }
            return self;
        }
        let is_tree = picker.mode == PickerMode::Checkout;
        let active_path = if is_tree {
            picker.active_path_ids()
        } else {
            std::collections::HashSet::new()
        };
        // Long lists (the login provider picker has ~60 rows) would flood the
        // control area; render a window centered on the selection instead.
        let total = picker.rows.len();
        let (start, end) = if total <= PICKER_MAX_ROWS {
            (0, total)
        } else {
            let start = picker
                .selected
                .saturating_sub(PICKER_MAX_ROWS / 2)
                .min(total - PICKER_MAX_ROWS);
            (start, start + PICKER_MAX_ROWS)
        };
        if start > 0 {
            self.control_line(UiKind::Status, format!("    ↑ {start} more"));
        }
        for (index, row) in picker.rows[start..end].iter().enumerate() {
            let index = index + start;
            let selected = index == picker.selected;
            let cursor = if selected { "\u{203a} " } else { "  " };
            let directory = if row.has_children {
                if picker.is_expanded_tree_row(&row.id) {
                    "[-] "
                } else {
                    "[+] "
                }
            } else {
                "    "
            };
            let kind = if selected {
                UiKind::Selected
            } else if row.active {
                UiKind::Brand
            } else if row.has_children {
                UiKind::TreeDirectory
            } else {
                UiKind::Status
            };
            let line = if is_tree {
                // HEAD marker for the current position, and a bullet for nodes on
                // the path from the root to it, so the current branch reads clearly.
                let dot = if active_path.contains(&row.id) {
                    "\u{2022} "
                } else {
                    "  "
                };
                let head = if row.active { "  \u{25c0} current" } else { "" };
                format!(
                    "{cursor}{}{directory}{dot}user: {}{head}",
                    row.prefix, row.label
                )
            } else {
                let active = if row.active { " *" } else { "" };
                let detail = if row.detail.is_empty() {
                    String::new()
                } else {
                    format!(" {}", row.detail)
                };
                format!(
                    "{cursor}{}{directory}{}{}{active}",
                    row.prefix, row.label, detail
                )
            };
            self.control_line(kind, line);
        }
        if end < total {
            self.control_line(UiKind::Status, format!("    ↓ {} more", total - end));
        }
        if is_tree || total > PICKER_MAX_ROWS {
            self.control_line(
                UiKind::Status,
                format!("({}/{})", picker.selected + 1, total),
            );
        }
        self.control_line(UiKind::System, String::new());
        self
    }

    pub(crate) fn pending_messages(mut self, pending_messages: &[String]) -> Self {
        if pending_messages.is_empty() {
            return self;
        }
        self.control_line(
            UiKind::Status,
            "pending: esc interrupts current turn, sends next".to_string(),
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

    pub(crate) fn input(
        mut self,
        input_lines: &[UiLine],
        command_matches: &[CommandCandidate],
        selected_index: usize,
    ) -> Self {
        self.control_line(UiKind::Input, Line::default());
        for (index, source) in input_lines.iter().enumerate() {
            let prefix = if index == 0 { "› " } else { "  " };
            let mut spans = vec![Span::raw(prefix)];
            spans.extend(source.line.spans.iter().cloned());
            let mut line = UiLine::new(UiKind::Input, Line::from(spans));
            line.cursor = source.cursor.map(|column| column + 2);
            self.controls.push(line);
        }
        self.control_line(UiKind::Input, Line::default());
        if !command_matches.is_empty() {
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

    pub(crate) fn progress(
        mut self,
        activity: &ActivityState,
        thinking_tokens: u64,
        now: Instant,
        width: usize,
    ) -> Self {
        let Some(progress) = activity.progress(now, thinking_tokens) else {
            return self;
        };
        let indicator = spinner_char(progress.preset, activity.animation_tick, progress.step);
        let (red, green, blue) = progress.color;
        let label = truncate_to_width(&progress.label, width.saturating_sub(4));
        self.control_line(
            UiKind::Status,
            Line::from(vec![
                Span::raw("  "),
                Span::styled(indicator, Style::new().fg(Color::Rgb(red, green, blue))),
                Span::raw(format!(" {label}")),
            ]),
        );
        self
    }

    pub(crate) fn bottom_status(mut self, status: BottomStatus<'_>, width: usize) -> Self {
        let percent = if status.context_window == 0 {
            0.0
        } else {
            (status.context_tokens as f64 / status.context_window as f64 * 100.0).min(100.0)
        };
        let left = vec![
            Span::raw(format!("{} / {} (", status.provider, status.model)),
            reasoning_effort_span(status.reasoning_effort),
            Span::raw(format!(
                "){}",
                crate::git_bar::format_git_segment(status.git)
            )),
        ];
        let cost = if status.cost > 0.0 {
            format!(" | ${:.4}", status.cost)
        } else {
            String::new()
        };
        let right = format!(
            "tokens {}/{} | context {percent:.1}%{cost}",
            status.context_tokens, status.context_window
        );
        let line = format_status_line(left, &right, width);
        self.control_line(UiKind::BottomStatus, line);
        self
    }

    pub(crate) fn reset_screen(mut self, reset_screen: bool) -> Self {
        self.reset_screen = reset_screen;
        self
    }

    pub(crate) fn finish(self) -> UiDocument {
        UiDocument {
            history: self.history,
            rendered_history_lines: self.rendered_history_lines,
            controls: self.controls,
            reset_screen: self.reset_screen,
        }
    }

    pub(crate) fn into_history(self) -> Vec<UiLine> {
        self.history
    }

    /// One blank line between top-level blocks (turns, tool calls, notices)
    /// gives the transcript its rhythm; leading blank is skipped.
    fn separate_block(&mut self) {
        if self.history.last().is_some_and(|line| !line.is_blank()) {
            self.history_line(UiKind::Status, Line::default());
        }
    }

    /// User turns echo with a dim `›` marker — same glyph as the composer
    /// prompt — so their own messages read as quoted input, not output.
    fn push_user_message(&mut self, text: &str) {
        if text.is_empty() {
            self.history_line(
                UiKind::User,
                Line::from(Span::styled("›", Style::new().fg(Color::DarkGray))),
            );
            return;
        }
        for (index, line) in text.lines().enumerate() {
            let marker = if index == 0 { "› " } else { "  " };
            self.history_line(
                UiKind::User,
                Line::from(vec![
                    Span::styled(marker, Style::new().fg(Color::DarkGray)),
                    Span::raw(line.to_string()),
                ]),
            );
        }
    }

    fn push_history(&mut self, kind: UiKind, text: &str) {
        if text.is_empty() {
            self.history_line(kind, Line::default());
            return;
        }

        for line in text.lines() {
            self.history_line(kind, line.to_string());
        }
    }

    fn push_startup_box(
        &mut self,
        version: &str,
        _profile_dir: &str,
        _config_path: &str,
        cwd: &str,
        model: &str,
        context_window: u64,
    ) {
        let mascot = [" \\/", "<'l", " ll", " llama~", " || ||", " '' ''"];
        let title = format!(
            "Welcome to JuCode v{} ({} · {} context)",
            version,
            model,
            format_context_window(context_window)
        );
        let cwd = format!("cwd: {}", compact_home_path(cwd));
        let help = "/help for commands · /exit to quit";
        let content_width = [title.as_str(), cwd.as_str(), help]
            .iter()
            .map(|line| UnicodeWidthStr::width(*line))
            .max()
            .unwrap_or(0);
        let mascot_width = mascot
            .iter()
            .map(|line| UnicodeWidthStr::width(*line))
            .max()
            .unwrap_or(0);
        let content_width = (mascot_width + 3 + content_width).min(96);
        let right_width = content_width.saturating_sub(mascot_width + 3);
        let right_lines = [title.as_str(), "", cwd.as_str(), "", help, ""];

        self.history_line(UiKind::Brand, rounded_box_border('╭', '╮', content_width));
        for (index, mascot_line) in mascot.iter().enumerate() {
            self.history_line(
                UiKind::Brand,
                startup_box_line(
                    mascot_line,
                    right_lines[index],
                    mascot_width,
                    right_width,
                    content_width,
                ),
            );
        }
        self.history_line(UiKind::Brand, rounded_box_border('╰', '╯', content_width));
    }

    fn push_tool_preview(&mut self, lines: &[UiLine]) {
        if lines.is_empty() {
            self.history_line(UiKind::Tool, TOOL_GUTTER_FIRST);
            return;
        }

        for (index, line) in lines.iter().enumerate() {
            let gutter = if index == 0 {
                TOOL_GUTTER_FIRST
            } else {
                TOOL_GUTTER_REST
            };
            let mut spans = vec![Span::raw(gutter)];
            spans.extend(line.line.spans.iter().cloned());
            self.history.push(UiLine {
                kind: line.kind,
                line: Line::from(spans),
                click: None,
                cursor: None,
            });
        }
    }

    fn push_tool_block(&mut self, name: &str, output: &str, running: bool, width: usize) {
        let preview = tool_output_preview(name, output, running);
        let header =
            format_tool_header(name, running, preview.first().map(|line| &line.line), width);
        self.history_line(UiKind::ToolHeader, header);

        // The header already shows the first preview line; the gutter block
        // holds only what follows it.
        if preview.iter().skip(1).any(|line| !line.is_blank()) {
            self.push_tool_preview(&preview[1..]);
        }
    }

    pub(crate) fn history_line(&mut self, kind: UiKind, line: impl Into<Line<'static>>) {
        self.history.push(UiLine::new(kind, line));
    }

    /// A history line carrying a click target: clicking its painted row toggles
    /// the chat item at `index`.
    fn history_clickable_line(
        &mut self,
        kind: UiKind,
        line: impl Into<Line<'static>>,
        index: usize,
    ) {
        self.history.push(UiLine::clickable(kind, line, index));
    }

    fn control_line(&mut self, kind: UiKind, line: impl Into<Line<'static>>) {
        self.controls.push(UiLine::new(kind, line));
    }
}
