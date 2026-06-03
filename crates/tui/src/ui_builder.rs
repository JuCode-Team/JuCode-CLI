use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use crate::markdown::render_markdown;
use crate::picker::{PickerMode, PickerState, TreePromptAction};
use crate::tool_preview::{
    compact_tool_preview, diff_line_kind, format_tool_header, tool_output_preview,
};
use crate::{
    color_code, compact_home_path, format_context_window, pad_to_width, spinner_char,
    truncate_to_width, ActivityState, BottomStatus, ChatLine, CommandCandidate, UiDocument, UiKind,
    UiLine, BOX_BORDER, CURSOR_MARKER, RESET, STARTUP_ACCENT, STARTUP_DIM, STARTUP_STRONG,
    STARTUP_TEXT, THINKING_COLLAPSED_LINES, VISIBLE_CURSOR,
};

fn rounded_box_border(left: char, right: char, width: usize) -> String {
    format!("{BOX_BORDER}{left}{}{right}{RESET}", "─".repeat(width + 2))
}

fn startup_box_line(
    mascot: &str,
    text: &str,
    mascot_width: usize,
    right_width: usize,
    width: usize,
) -> String {
    let plain_width = mascot_width + 3 + right_width;
    let text_width = UnicodeWidthStr::width(text);
    let text_padding = " ".repeat(right_width.saturating_sub(text_width));
    let colored = format!(
        "{}{}{}   {}{}{}",
        STARTUP_ACCENT,
        pad_to_width(mascot, mascot_width),
        STARTUP_TEXT,
        color_startup_text(text),
        text_padding,
        RESET
    );
    format!(
        "{BOX_BORDER}│{RESET} {colored}{} {BOX_BORDER}│{RESET}",
        " ".repeat(width.saturating_sub(plain_width))
    )
}

fn color_startup_text(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("Welcome to ") {
        if let Some(details) = rest.strip_prefix("JuCode") {
            return format!(
                "{STARTUP_STRONG}Welcome to {STARTUP_ACCENT}JuCode{STARTUP_DIM}{details}{STARTUP_TEXT}"
            );
        }
    }
    if let Some(path) = text.strip_prefix("cwd: ") {
        return format!("{STARTUP_TEXT}cwd: {STARTUP_STRONG}{path}{STARTUP_TEXT}");
    }
    if text == "/help for commands · /exit to quit" {
        return format!(
            "{STARTUP_STRONG}/help{STARTUP_TEXT} for commands · {STARTUP_STRONG}/exit{STARTUP_TEXT} to quit"
        );
    }
    format!("{STARTUP_TEXT}{text}")
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

pub(crate) fn colored_reasoning_effort(effort: &str) -> String {
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

pub(crate) struct UiBuilder {
    history: Vec<UiLine>,
    rendered_history_lines: Option<Vec<String>>,
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

    pub(crate) fn rendered_history_lines(mut self, lines: Vec<String>) -> Self {
        self.rendered_history_lines = Some(lines);
        self
    }

    #[cfg(test)]
    pub(crate) fn chat(self, chat: &[ChatLine]) -> Self {
        self.chat_with_width(chat, usize::MAX)
    }

    pub(crate) fn chat_with_width(mut self, chat: &[ChatLine], width: usize) -> Self {
        for item in chat {
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
                ChatLine::User(text) => self.push_history(UiKind::User, text),
                ChatLine::Assistant(text) => {
                    for line in render_markdown(text, width, color_code(UiKind::Assistant)) {
                        self.history_line(UiKind::Assistant, line);
                    }
                }
                ChatLine::Reasoning { text, collapsed } => {
                    self.history_line(UiKind::Status, "* thinking".to_string());
                    let rendered = render_markdown(text, width, color_code(UiKind::Status));
                    let shown = if *collapsed {
                        THINKING_COLLAPSED_LINES.min(rendered.len())
                    } else {
                        rendered.len()
                    };
                    for line in &rendered[..shown] {
                        self.history_line(UiKind::Status, format!("  {line}"));
                    }
                    if *collapsed && rendered.len() > shown {
                        self.history_line(UiKind::Status, "  …".to_string());
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
            self.history_line(UiKind::System, String::new());
        }
        self
    }

    pub(crate) fn live_assistant(mut self, text: Option<&str>, width: usize) -> Self {
        if let Some(text) = text.filter(|value| !value.is_empty()) {
            for line in render_markdown(text, width, color_code(UiKind::Assistant)) {
                self.control_line(UiKind::Assistant, line);
            }
            self.control_line(UiKind::System, String::new());
        }
        self
    }

    /// A compact reasoning-token indicator shown directly above the input. The
    /// reasoning text itself lives in the transcript, not here.
    pub(crate) fn thinking_indicator(mut self, thinking: bool, tokens: u64) -> Self {
        if !thinking && tokens == 0 {
            return self;
        }
        let label = if tokens > 0 {
            format!("thinking · {tokens} reasoning tokens")
        } else {
            "thinking…".to_string()
        };
        self.control_line(UiKind::Status, label);
        self.control_line(UiKind::System, String::new());
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
                if picker.is_expanded_tree_row(&row.id) {
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

    pub(crate) fn pending_messages(mut self, pending_messages: &[String]) -> Self {
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

    pub(crate) fn input(
        mut self,
        input: &str,
        command_matches: &[CommandCandidate],
        selected_index: usize,
    ) -> Self {
        let lines = input.split('\n').collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            let prefix = if index == 0 { "> " } else { "  " };
            self.control_line(UiKind::Input, format!("{prefix}{line}"));
        }
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

    pub(crate) fn progress(mut self, activity: &ActivityState, now: Instant, width: usize) -> Self {
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

    pub(crate) fn bottom_status(mut self, status: BottomStatus<'_>, width: usize) -> Self {
        let percent = if status.context_window == 0 {
            0.0
        } else {
            (status.context_tokens as f64 / status.context_window as f64 * 100.0).min(100.0)
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

    fn push_history(&mut self, kind: UiKind, text: &str) {
        if text.is_empty() {
            self.history_line(kind, String::new());
            return;
        }

        for line in text.lines() {
            self.history_line(kind, line.to_string());
        }
    }

    fn push_startup_box(
        &mut self,
        _version: &str,
        _profile_dir: &str,
        _config_path: &str,
        cwd: &str,
        model: &str,
        context_window: u64,
    ) {
        let mascot = [" \\/", "<'l", " ll", " llama~", " || ||", " '' ''"];
        let title = format!(
            "Welcome to JuCode ({} · {} context)",
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
                UiKind::System,
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

    fn push_tool_preview(&mut self, text: &str, prefix: &str) {
        if text.is_empty() {
            self.history_line(UiKind::Tool, prefix.to_string());
            return;
        }

        for line in text.lines() {
            self.history_line(diff_line_kind(line), format!("{prefix}{line}"));
        }
    }

    fn push_tool_block(&mut self, name: &str, output: &str, running: bool, width: usize) {
        let preview = tool_output_preview(name, output, running);
        let header = format_tool_header(name, running, &preview, width);
        self.history_line(UiKind::ToolHeader, header);

        if preview == compact_tool_preview(name, output, running) {
            return;
        }

        self.push_tool_preview(&preview, "  ");
    }

    pub(crate) fn history_line(&mut self, kind: UiKind, text: String) {
        self.history.push(UiLine { kind, text });
    }

    fn control_line(&mut self, kind: UiKind, text: String) {
        self.controls.push(UiLine { kind, text });
    }
}
