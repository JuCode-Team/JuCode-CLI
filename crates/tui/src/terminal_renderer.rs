use std::io::{self, Stdout};

use ratatui::{
    backend::CrosstermBackend,
    buffer::{Buffer, Cell as RtCell},
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{
        Block, BorderType, Borders, Scrollbar, ScrollbarOrientation, ScrollbarState,
        StatefulWidget, Widget,
    },
    Terminal,
};
use unicode_width::UnicodeWidthChar;

#[cfg(test)]
use crate::ProjectedDocument;
use crate::{
    extract_cursor, padded_content_width, render_ansi_line, split_ansi_sequence, visible_width,
    wrap_lines, CursorTarget, UiDocument, UiKind, UiLine, CONTENT_LEFT_PADDING, CURSOR_MARKER,
};

/// Column reserved on the right of the transcript for the scrollbar.
const SCROLLBAR_WIDTH: u16 = 1;
/// Dim border color for the input box (matches the transcript separator tone).
const INPUT_BORDER: Color = Color::Rgb(105, 108, 120);
/// Faint scrollbar track and a slightly brighter thumb.
const SCROLLBAR_TRACK: Color = Color::Rgb(62, 65, 75);
const SCROLLBAR_THUMB: Color = Color::Rgb(124, 128, 142);

/// Renders the UI into a ratatui-owned alternate screen.
///
/// Layout is fully native: a `Layout`-style vertical split into a scrollable transcript
/// viewport (with a `Scrollbar`), a live region, a bordered input `Block`, the command
/// completion list, and a bottom status bar. Each region's styled lines are painted into
/// its rect; ratatui writes only the cells that changed between draws.
pub(crate) struct TerminalRenderer {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalRenderer {
    pub(crate) fn new() -> io::Result<Self> {
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;
        Ok(Self { terminal })
    }

    /// `scroll` is how many lines the transcript viewport is lifted above the live tail.
    /// It is clamped to the available range in place so the caller's paging stays bounded.
    pub(crate) fn render(&mut self, document: &UiDocument, scroll: &mut usize) -> io::Result<()> {
        if document.reset_screen {
            self.terminal.clear()?;
        }
        self.terminal.draw(|frame| {
            let area = frame.area();
            if let Some((x, y)) = draw(frame.buffer_mut(), area, document, scroll) {
                frame.set_cursor_position((x, y));
            }
        })?;
        Ok(())
    }
}

/// Returns the terminal cursor position (within the input box) to show, if any.
fn draw(
    buf: &mut Buffer,
    area: Rect,
    document: &UiDocument,
    scroll: &mut usize,
) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let width = area.width as usize;

    let transcript = render_projected_lines(
        document
            .rendered_history_lines
            .clone()
            .unwrap_or_else(|| wrap_lines(&document.history, padded_content_width(width))),
        true,
    );
    let regions = ControlRegions::split(&document.controls, width);

    // Allocate region heights bottom-up so the input box and status bar always fit; the
    // transcript takes whatever is left, and an oversized live region shows its tail.
    let total = area.height as usize;
    let status_h = regions.status.len().min(total);
    let mut rest = total - status_h;
    let box_h = if regions.input.is_empty() {
        0
    } else {
        (regions.input.len() + 2).min(rest)
    };
    rest -= box_h;
    let cand_h = regions.candidates.len().min(rest);
    rest -= cand_h;
    let live_h = regions.live.len().min(rest);
    rest -= live_h;
    let transcript_h = rest;

    let mut y = area.y;
    let transcript_rect = Rect::new(area.x, y, area.width, transcript_h as u16);
    y += transcript_h as u16;
    let live_rect = Rect::new(area.x, y, area.width, live_h as u16);
    y += live_h as u16;
    let box_rect = Rect::new(area.x, y, area.width, box_h as u16);
    y += box_h as u16;
    let cand_rect = Rect::new(area.x, y, area.width, cand_h as u16);
    y += cand_h as u16;
    let status_rect = Rect::new(area.x, y, area.width, status_h as u16);

    draw_transcript(buf, transcript_rect, &transcript, scroll);
    paint_region_tail(buf, live_rect, &regions.live);
    draw_input_box(buf, box_rect, &regions.input);
    paint_region_tail(buf, cand_rect, &regions.candidates);
    paint_region_tail(buf, status_rect, &regions.status);

    input_cursor_position(box_rect, regions.cursor)
}

/// Translates a caret position inside the input box content to a screen position for the
/// terminal's hardware cursor, clamped to the box interior. Returns `None` when there is
/// no caret (e.g. the agent is working) or the box has no room for a border interior.
fn input_cursor_position(box_rect: Rect, cursor: Option<CursorTarget>) -> Option<(u16, u16)> {
    let cursor = cursor?;
    if box_rect.height < 3 || box_rect.width < 3 {
        return None;
    }
    let inner_x = box_rect.x + 1;
    let inner_y = box_rect.y + 1;
    let max_x = box_rect.x + box_rect.width - 2;
    let max_y = box_rect.y + box_rect.height - 2;
    let x = (inner_x + cursor.column as u16).min(max_x);
    let y = (inner_y + cursor.row as u16).min(max_y);
    Some((x, y))
}

fn draw_transcript(buf: &mut Buffer, rect: Rect, lines: &[String], scroll: &mut usize) {
    if rect.height == 0 || rect.width == 0 {
        *scroll = 0;
        return;
    }
    let view_height = rect.height as usize;
    let (start, end, offset) = visible_window(lines.len(), view_height, *scroll);
    *scroll = offset;

    let text_rect = Rect::new(
        rect.x,
        rect.y,
        rect.width.saturating_sub(SCROLLBAR_WIDTH),
        rect.height,
    );
    paint_lines(buf, text_rect, &lines[start..end]);

    // Only show the scrollbar when the transcript actually overflows; the column stays
    // reserved either way so the layout never shifts. Thin track + heavier thumb match
    // the input box border instead of ratatui's default double-line track.
    if lines.len() > view_height {
        let mut state = ScrollbarState::new(lines.len())
            .position(start)
            .viewport_content_length(view_height);
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .track_style(Style::default().fg(SCROLLBAR_TRACK))
            .thumb_style(Style::default().fg(SCROLLBAR_THUMB))
            .render(rect, buf, &mut state);
    }
}

fn draw_input_box(buf: &mut Buffer, rect: Rect, lines: &[String]) {
    if rect.height < 2 || rect.width < 2 {
        paint_lines(buf, rect, lines);
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(INPUT_BORDER));
    let inner = block.inner(rect);
    block.render(rect, buf);
    paint_lines(buf, inner, lines);
}

/// Paints the last `rect.height` lines of `lines` from the top of `rect`. Regions are
/// height-clamped to fit, so this shows everything unless the region overflowed.
fn paint_region_tail(buf: &mut Buffer, rect: Rect, lines: &[String]) {
    if rect.height == 0 {
        return;
    }
    let start = lines.len().saturating_sub(rect.height as usize);
    paint_lines(buf, rect, &lines[start..]);
}

fn paint_lines(buf: &mut Buffer, rect: Rect, lines: &[String]) {
    for (row, line) in lines.iter().enumerate().take(rect.height as usize) {
        paint_ansi_line(buf, rect.x, rect.y + row as u16, rect.width as usize, line);
    }
}

/// Splits the flat control list into screen regions using the builder's invariants:
/// the bottom status line is last, then the command-completion candidates (non-input
/// kinds), then the input box (a trailing run of `Input` lines), and everything above is
/// the live region (assistant stream, picker, pending, progress).
struct ControlRegions {
    live: Vec<String>,
    input: Vec<String>,
    candidates: Vec<String>,
    status: Vec<String>,
    /// Caret position (row, column) within the input box content, for the hardware cursor.
    cursor: Option<CursorTarget>,
}

impl ControlRegions {
    fn split(controls: &[UiLine], width: usize) -> Self {
        let mut lines = controls.to_vec();
        let status = if lines
            .last()
            .is_some_and(|line| line.kind == UiKind::BottomStatus)
        {
            vec![lines.pop().expect("checked non-empty")]
        } else {
            Vec::new()
        };
        let mut candidates = pop_trailing(&mut lines, |line| line.kind != UiKind::Input);
        candidates.reverse();
        let mut input = pop_trailing(&mut lines, |line| line.kind == UiKind::Input);
        input.reverse();
        // The input builder wraps the prompt in empty spacer lines; the box border
        // replaces them, so drop them before projecting.
        input.retain(|line| !line.text.is_empty());

        // Pull the caret position out of the input lines so the renderer can place the
        // hardware cursor; this also strips the marker so it never reaches the buffer.
        let mut input = wrap_lines(&input, width);
        let cursor = extract_cursor(&mut input);
        let input = input
            .into_iter()
            .map(|line| render_projected_line(render_control_line(&line, width), false))
            .collect();

        Self {
            live: project_control_region(&lines, width),
            input,
            candidates: project_control_region(&candidates, width),
            status: project_control_region(&status, width),
            cursor,
        }
    }
}

fn pop_trailing(lines: &mut Vec<UiLine>, keep: impl Fn(&UiLine) -> bool) -> Vec<UiLine> {
    let mut taken = Vec::new();
    while lines.last().is_some_and(&keep) {
        taken.push(lines.pop().expect("checked non-empty"));
    }
    taken
}

fn project_control_region(lines: &[UiLine], width: usize) -> Vec<String> {
    wrap_lines(lines, width)
        .into_iter()
        .map(|line| {
            let mut text = render_projected_line(render_control_line(&line, width), false);
            // Strip the logical cursor marker; the caret is drawn as a reverse-video block
            // in the text itself, so the hardware cursor stays hidden.
            strip_cursor_marker(&mut text);
            text
        })
        .collect()
}

fn strip_cursor_marker(text: &mut String) {
    while let Some(index) = text.find(CURSOR_MARKER) {
        text.replace_range(index..index + CURSOR_MARKER.len(), "");
    }
}

/// Selects which `view_height` lines of a `total`-line frame are visible.
///
/// `scroll` lifts the window above the live tail and is clamped to the available range;
/// the clamped value is returned so paging can be bounded by the caller.
fn visible_window(total: usize, view_height: usize, scroll: usize) -> (usize, usize, usize) {
    let max_scroll = total.saturating_sub(view_height);
    let offset = scroll.min(max_scroll);
    let end = total - offset;
    let start = end.saturating_sub(view_height);
    (start, end, offset)
}

#[cfg(feature = "bench")]
pub(crate) fn render_document_for_bench(document: &UiDocument, width: u16, height: u16) -> usize {
    let area = Rect::new(0, 0, width.max(1), height.max(1));
    let mut buffer = Buffer::empty(area);
    let mut scroll = 0usize;
    let _ = draw(&mut buffer, area, document, &mut scroll);
    buffer_checksum(&buffer)
}

#[cfg(feature = "bench")]
fn buffer_checksum(buffer: &Buffer) -> usize {
    buffer.content.iter().fold(0usize, |acc, cell| {
        acc.wrapping_mul(31).wrapping_add(
            cell.symbol()
                .as_bytes()
                .iter()
                .map(|byte| *byte as usize)
                .sum(),
        )
    })
}

#[cfg(test)]
impl ProjectedDocument {
    pub(crate) fn from_document(document: &UiDocument, width: u16) -> Self {
        let width = width as usize;
        let history_width = padded_content_width(width);
        let control_width = width.max(1);
        let transcript_lines = document
            .rendered_history_lines
            .clone()
            .unwrap_or_else(|| wrap_lines(&document.history, history_width));
        let transcript_lines = render_projected_lines(transcript_lines, true);
        let mut controls = wrap_lines(&document.controls, control_width);
        let cursor = extract_cursor(&mut controls);
        let mut active_lines = Vec::new();
        if !transcript_lines.is_empty() && !document.controls.is_empty() {
            active_lines.push(String::new());
        }
        let controls_start_row = transcript_lines.len() + active_lines.len();
        let cursor = cursor.map(|cursor| CursorTarget {
            row: controls_start_row + cursor.row,
            column: cursor.column,
        });
        active_lines.extend(
            controls
                .into_iter()
                .map(|line| render_projected_line(render_control_line(&line, control_width), false))
                .collect::<Vec<_>>(),
        );

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
}

#[cfg(test)]
impl ProjectedDocument {
    pub(crate) fn into_frame(self) -> crate::RenderedFrame {
        crate::RenderedFrame {
            lines: self.frame_lines(),
            cursor: self.cursor,
        }
    }
}

fn render_control_line(line: &UiLine, width: usize) -> UiLine {
    let mut line = line.clone();
    if line.kind == UiKind::Input {
        let visible = visible_width(&line.text);
        if visible < width {
            line.text.push_str(&" ".repeat(width - visible));
        }
    }
    line
}

fn render_projected_lines(lines: Vec<UiLine>, history: bool) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| render_projected_line(line, history))
        .collect()
}

fn render_projected_line(line: UiLine, history: bool) -> String {
    let rendered = render_ansi_line(&line);
    if rendered.is_empty() || !should_pad_line(line.kind, history) {
        rendered
    } else {
        format!("{}{}", " ".repeat(CONTENT_LEFT_PADDING), rendered)
    }
}

fn should_pad_line(kind: UiKind, history: bool) -> bool {
    if !history {
        return matches!(kind, UiKind::Assistant);
    }
    matches!(
        kind,
        UiKind::User | UiKind::Assistant | UiKind::System | UiKind::Error | UiKind::Status
    )
}

#[cfg(test)]
fn render_ansi_lines_to_buffer(lines: &[String], buffer: &mut Buffer) {
    let area = buffer.area;
    for (row, line) in lines.iter().enumerate().take(area.height as usize) {
        paint_ansi_line(
            buffer,
            area.x,
            area.y + row as u16,
            area.width as usize,
            line,
        );
    }
}

fn paint_ansi_line(buffer: &mut Buffer, x0: u16, y: u16, width: usize, line: &str) {
    let mut rest = line;
    let mut column = 0usize;
    let mut style = AnsiCellStyle::default();
    let mut row_background = Color::Reset;
    while !rest.is_empty() && column < width {
        if let Some((sequence, tail)) = split_ansi_sequence(rest) {
            style.apply_sequence(sequence);
            if style.bg != Color::Reset {
                row_background = style.bg;
            }
            rest = tail;
            continue;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        rest = &rest[ch.len_utf8()..];
        if ch_width == 0 {
            continue;
        }
        if column + ch_width > width {
            break;
        }

        let x = x0 + column as u16;
        if let Some(cell) = buffer.cell_mut((x, y)) {
            cell.set_char(ch);
            apply_cell_style(cell, style);
        }
        for offset in 1..ch_width {
            if let Some(cell) = buffer.cell_mut((x + offset as u16, y)) {
                cell.set_symbol(" ");
                apply_cell_style(cell, style);
                cell.skip = true;
            }
        }
        column += ch_width;
    }

    if row_background != Color::Reset {
        for column in 0..width {
            if let Some(cell) = buffer.cell_mut((x0 + column as u16, y)) {
                if cell.bg == Color::Reset {
                    cell.bg = row_background;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AnsiCellStyle {
    fg: Color,
    bg: Color,
    modifier: Modifier,
}

impl Default for AnsiCellStyle {
    fn default() -> Self {
        Self {
            fg: Color::Reset,
            bg: Color::Reset,
            modifier: Modifier::empty(),
        }
    }
}

impl AnsiCellStyle {
    fn apply_sequence(&mut self, sequence: &str) {
        let Some(body) = sequence
            .strip_prefix("\x1b[")
            .and_then(|value| value.strip_suffix('m'))
        else {
            return;
        };
        let params = if body.is_empty() {
            vec![0]
        } else {
            body.split(';')
                .filter_map(|part| part.parse::<u16>().ok())
                .collect::<Vec<_>>()
        };
        let mut index = 0;
        while index < params.len() {
            match params[index] {
                0 => *self = Self::default(),
                1 => self.modifier.insert(Modifier::BOLD),
                3 => self.modifier.insert(Modifier::ITALIC),
                7 => self.modifier.insert(Modifier::REVERSED),
                22 => self.modifier.remove(Modifier::BOLD),
                23 => self.modifier.remove(Modifier::ITALIC),
                27 => self.modifier.remove(Modifier::REVERSED),
                30..=37 => self.fg = ansi_color(params[index], false),
                39 => self.fg = Color::Reset,
                40..=47 => self.bg = ansi_color(params[index], true),
                49 => self.bg = Color::Reset,
                90..=97 => self.fg = ansi_color(params[index], false),
                100..=107 => self.bg = ansi_color(params[index], true),
                38 | 48 => {
                    if let Some((color, consumed)) = parse_extended_color(&params[index..]) {
                        if params[index] == 38 {
                            self.fg = color;
                        } else {
                            self.bg = color;
                        }
                        index += consumed.saturating_sub(1);
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }
}

fn apply_cell_style(cell: &mut RtCell, style: AnsiCellStyle) {
    cell.fg = style.fg;
    cell.bg = style.bg;
    cell.modifier = style.modifier;
}

fn ansi_color(code: u16, background: bool) -> Color {
    let foreground_code = if background {
        code.saturating_sub(10)
    } else {
        code
    };
    match foreground_code {
        30 => Color::Black,
        31 => Color::Red,
        32 => Color::Green,
        33 => Color::Yellow,
        34 => Color::Blue,
        35 => Color::Magenta,
        36 => Color::Cyan,
        37 => Color::Gray,
        90 => Color::DarkGray,
        91 => Color::LightRed,
        92 => Color::LightGreen,
        93 => Color::LightYellow,
        94 => Color::LightBlue,
        95 => Color::LightMagenta,
        96 => Color::LightCyan,
        97 => Color::White,
        _ => Color::Reset,
    }
}

fn parse_extended_color(params: &[u16]) -> Option<(Color, usize)> {
    match params {
        [_, 5, index, ..] => Some((Color::Indexed((*index).min(u8::MAX as u16) as u8), 3)),
        [_, 2, red, green, blue, ..] => Some((
            Color::Rgb(
                (*red).min(u8::MAX as u16) as u8,
                (*green).min(u8::MAX as u16) as u8,
                (*blue).min(u8::MAX as u16) as u8,
            ),
            5,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_builder::UiBuilder;
    use crate::{BottomStatus, ChatLine};
    use ratatui::style::Color;

    #[test]
    fn window_follows_live_tail_by_default() {
        assert_eq!(visible_window(22, 5, 0), (17, 22, 0));
    }

    #[test]
    fn window_scrolls_up_and_clamps_to_top() {
        assert_eq!(visible_window(22, 5, 3), (14, 19, 3));
        assert_eq!(visible_window(22, 5, 999), (0, 5, 17));
    }

    #[test]
    fn window_handles_fewer_lines_than_viewport() {
        assert_eq!(visible_window(3, 5, 2), (0, 3, 0));
    }

    #[test]
    fn ansi_lines_render_into_ratatui_cells_with_style() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        render_ansi_lines_to_buffer(
            &["\x1b[31;1mA\x1b[0m \x1b[48;2;1;2;3mB".to_string()],
            &mut buffer,
        );

        let first = &buffer[(0, 0)];
        assert_eq!(first.symbol(), "A");
        assert_eq!(first.fg, Color::Red);
        assert!(first.modifier.contains(Modifier::BOLD));

        let third = &buffer[(2, 0)];
        assert_eq!(third.symbol(), "B");
        assert_eq!(third.bg, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn background_color_extends_to_end_of_line() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        render_ansi_lines_to_buffer(
            &["\x1b[38;2;170;220;170;48;2;28;70;38m+x\x1b[0m".to_string()],
            &mut buffer,
        );

        assert!(buffer
            .content
            .iter()
            .all(|cell| cell.bg == Color::Rgb(28, 70, 38)));
    }

    #[test]
    fn control_regions_split_separates_input_box_candidates_and_status() {
        let controls = vec![
            UiLine {
                kind: UiKind::Status,
                text: "  spinner".to_string(),
            },
            UiLine {
                kind: UiKind::Input,
                text: String::new(),
            },
            UiLine {
                kind: UiKind::Input,
                text: "› hi".to_string(),
            },
            UiLine {
                kind: UiKind::Input,
                text: String::new(),
            },
            UiLine {
                kind: UiKind::Selected,
                text: "  /help".to_string(),
            },
            UiLine {
                kind: UiKind::BottomStatus,
                text: "model · tokens".to_string(),
            },
        ];

        let regions = ControlRegions::split(&controls, 40);

        assert_eq!(regions.live.len(), 1);
        assert!(strip_ansi(&regions.live[0]).contains("spinner"));
        // Wrapper blank lines stripped, leaving just the prompt line for the box.
        assert_eq!(regions.input.len(), 1);
        assert!(strip_ansi(&regions.input[0]).contains("› hi"));
        assert_eq!(regions.candidates.len(), 1);
        assert!(strip_ansi(&regions.candidates[0]).contains("/help"));
        assert_eq!(regions.status.len(), 1);
        assert!(strip_ansi(&regions.status[0]).contains("model"));
    }

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while !rest.is_empty() {
            if let Some((_, tail)) = split_ansi_sequence(rest) {
                rest = tail;
                continue;
            }
            let ch = rest.chars().next().expect("non-empty");
            if ch != '\u{1b}' {
                out.push(ch);
            }
            rest = &rest[ch.len_utf8()..];
        }
        out
    }

    fn sample_document() -> UiDocument {
        let history: Vec<ChatLine> = (0..10)
            .map(|index| ChatLine::Assistant(format!("line {index}")))
            .collect();
        UiBuilder::new()
            .chat_with_width(&history, 18)
            .input("hi", &[], 0)
            .bottom_status(
                BottomStatus {
                    provider: "p",
                    model: "m",
                    reasoning_effort: "low",
                    context_tokens: 1,
                    context_window: 100,
                    cost: 0.0,
                },
                20,
            )
            .finish()
    }

    fn drawn_rows(width: u16, height: u16, scroll: &mut usize) -> Vec<String> {
        use ratatui::{backend::TestBackend, Terminal};
        let document = sample_document();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame.buffer_mut(), area, &document, scroll);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn draw_pins_input_box_and_status_and_follows_tail() {
        let mut scroll = 0usize;
        let rows = drawn_rows(20, 8, &mut scroll).join("\n");
        // Newest transcript line shows; oldest is scrolled off the tail.
        assert!(rows.contains("line 9"), "newest line visible: {rows:?}");
        assert!(!rows.contains("line 0"), "oldest line off-screen: {rows:?}");
        // Bordered input box and bottom status bar are present.
        assert!(rows.contains("hi"), "input visible: {rows:?}");
        assert!(
            rows.contains('╭') && rows.contains('╰'),
            "rounded box: {rows:?}"
        );
        assert!(rows.contains("tokens"), "status bar visible: {rows:?}");
    }

    #[test]
    fn draw_scrolls_transcript_while_input_stays_pinned() {
        let mut scroll = 999usize;
        let rows = drawn_rows(20, 8, &mut scroll).join("\n");
        // Scrolled to the very top of the transcript.
        assert!(rows.contains("line 0"), "oldest line visible: {rows:?}");
        // Input box and status stay pinned regardless of transcript scroll.
        assert!(rows.contains("hi"), "input stays visible: {rows:?}");
        assert!(rows.contains("tokens"), "status stays visible: {rows:?}");
        // Offset clamped to the transcript region's max scroll (not the whole screen).
        assert!(scroll < 999 && scroll > 0, "offset clamped: {scroll}");
    }

    #[test]
    fn draw_places_hardware_cursor_before_char_under_caret() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut input = crate::input::InputBuffer::default();
        for ch in "hello world".chars() {
            input.push_char(ch);
        }
        input.move_left(false);
        input.move_left(false); // caret before the 'l' of "world" (index 9)
        let document = UiBuilder::new()
            .input(&input.render(true), &[], 0)
            .bottom_status(
                BottomStatus {
                    provider: "p",
                    model: "m",
                    reasoning_effort: "low",
                    context_tokens: 1,
                    context_window: 100,
                    cost: 0.0,
                },
                48,
            )
            .finish();
        let mut scroll = 0usize;
        let mut terminal = Terminal::new(TestBackend::new(48, 8)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                if let Some((x, y)) = draw(frame.buffer_mut(), area, &document, &mut scroll) {
                    frame.set_cursor_position((x, y));
                }
            })
            .unwrap();
        let pos = terminal.get_cursor_position().unwrap();
        // "│› hello wor|ld": border(0) ›(1) space(2) hello(3..8) space(8) wor(9..12) -> col 12,
        // on the input content row (box border at y=4, content at y=5).
        assert_eq!((pos.x, pos.y), (12, 5));
    }
}
