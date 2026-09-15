use std::io::{self, Stdout};

use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Scrollbar, ScrollbarOrientation, ScrollbarState,
        StatefulWidget, Widget,
    },
    Terminal,
};

#[cfg(test)]
use crate::ProjectedDocument;
use crate::{
    extract_cursor, kind_style, padded_content_width, wrap_lines, CursorTarget, UiDocument, UiKind,
    UiLine, CONTENT_LEFT_PADDING,
};

/// Column reserved on the right of the transcript for the scrollbar.
const SCROLLBAR_WIDTH: u16 = 1;
/// Dim border color for the input box (matches the transcript separator tone).
const INPUT_BORDER: Color = Color::Rgb(105, 108, 120);
/// Faint scrollbar track and a slightly brighter thumb.
const SCROLLBAR_TRACK: Color = Color::Rgb(62, 65, 75);
const SCROLLBAR_THUMB: Color = Color::Rgb(124, 128, 142);
/// Uniform drag-selection background — a flat swatch, not per-cell inversion.
const SELECTION_BG: Color = Color::Rgb(68, 71, 90);

/// A drag selection over the whole screen, in terminal cell coordinates
/// (`(row, column)`). Covers every painted region — transcript, input box,
/// status bar — like native terminal selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextSelection {
    pub(crate) anchor: (u16, u16),
    pub(crate) cursor: (u16, u16),
}

impl TextSelection {
    pub(crate) fn new(at: (u16, u16)) -> Self {
        Self {
            anchor: at,
            cursor: at,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}

/// Renders the UI into a ratatui-owned alternate screen.
///
/// Layout is fully native: a `Layout`-style vertical split into a scrollable transcript
/// viewport (with a `Scrollbar`), a live region, a bordered input `Block`, the command
/// completion list, and a bottom status bar. Each region's styled lines are painted into
/// its rect; ratatui writes only the cells that changed between draws.
pub(crate) struct TerminalRenderer {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    /// The last painted frame; drag-selection copy reads text straight from it.
    frame: Option<Buffer>,
    /// Screen rows of clickable transcript lines (e.g. thinking headers),
    /// mapped to the chat item they toggle. Rebuilt every frame.
    clickables: ClickTargets,
}

impl TerminalRenderer {
    pub(crate) fn new() -> io::Result<Self> {
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;
        Ok(Self {
            terminal,
            frame: None,
            clickables: Vec::new(),
        })
    }

    /// `scroll` is how many lines the transcript viewport is lifted above the live tail.
    /// It is clamped to the available range in place so the caller's paging stays bounded.
    pub(crate) fn render(
        &mut self,
        document: &UiDocument,
        scroll: &mut usize,
        selection: Option<TextSelection>,
    ) -> io::Result<()> {
        if document.reset_screen {
            self.terminal.clear()?;
        }
        let mut painted = None;
        let mut clickables = Vec::new();
        self.terminal.draw(|frame| {
            let area = frame.area();
            let (cursor, clicks) = draw(frame.buffer_mut(), area, document, scroll, selection);
            if let Some((x, y)) = cursor {
                frame.set_cursor_position((x, y));
            }
            painted = Some(frame.buffer_mut().clone());
            clickables = clicks;
        })?;
        self.frame = painted;
        self.clickables = clickables;
        Ok(())
    }

    /// The chat item index whose clickable line paints at this screen row.
    pub(crate) fn clickable_at(&self, row: u16) -> Option<usize> {
        self.clickables
            .iter()
            .find(|(r, _)| *r == row)
            .map(|(_, index)| *index)
    }

    /// Extracts the selected text exactly as painted on screen.
    pub(crate) fn selected_text(&self, selection: TextSelection) -> String {
        self.frame
            .as_ref()
            .map(|buf| selected_text(buf, selection))
            .unwrap_or_default()
    }
}

/// Extracts the selected cell range from a painted buffer, skipping wide-char
/// continuation cells and trimming each row's trailing blanks.
fn selected_text(buf: &Buffer, selection: TextSelection) -> String {
    let area = buf.area;
    let (from, to) = selection.ordered();
    let right = area.x + area.width;
    let bottom = area.y + area.height;
    let mut out = String::new();
    for row in from.0..=to.0.min(bottom.saturating_sub(1)) {
        let col_start = if row == from.0 { from.1 } else { area.x };
        let col_end = if row == to.0 { to.1 + 1 } else { right };
        if row > from.0 {
            out.push('\n');
        }
        let mut text = String::new();
        for col in col_start..col_end.min(right) {
            if let Some(cell) = buf.cell((col, row)) {
                if !cell.skip {
                    text.push_str(cell.symbol());
                }
            }
        }
        out.push_str(text.trim_end());
    }
    out
}

/// Clickable transcript rows painted this frame: screen row → chat item index.
type ClickTargets = Vec<(u16, usize)>;

/// Paints the frame, returning the input-box cursor position to show (if any)
/// and the clickable transcript rows mapped to chat item indexes.
fn draw(
    buf: &mut Buffer,
    area: Rect,
    document: &UiDocument,
    scroll: &mut usize,
    selection: Option<TextSelection>,
) -> (Option<(u16, u16)>, ClickTargets) {
    if area.width == 0 || area.height == 0 {
        return (None, Vec::new());
    }
    let width = area.width as usize;

    // Transcript lines keep their click target (chat item index) on the UiLine
    // so painted rows map back to toggleable items.
    let transcript: Vec<UiLine> = document
        .rendered_history_lines
        .clone()
        .unwrap_or_else(|| wrap_lines(&document.history, padded_content_width(width)))
        .into_iter()
        .map(|line| project_line(line, true))
        .collect();
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

    let mut clickables = Vec::new();
    draw_transcript(buf, transcript_rect, &transcript, scroll, &mut clickables);
    paint_region_tail(buf, live_rect, &regions.live);
    draw_input_box(buf, box_rect, &regions.input);
    paint_region_tail(buf, cand_rect, &regions.candidates);
    paint_region_tail(buf, status_rect, &regions.status);

    // Selection highlight applies last, over every region.
    if let Some(selection) = selection.filter(|sel| !sel.is_empty()) {
        paint_selection(buf, area, selection);
    }

    (input_cursor_position(box_rect, regions.cursor), clickables)
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

fn draw_transcript(
    buf: &mut Buffer,
    rect: Rect,
    lines: &[UiLine],
    scroll: &mut usize,
    clickables: &mut ClickTargets,
) {
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
    for (row, line) in lines[start..end].iter().enumerate() {
        if let Some(index) = line.click {
            clickables.push((text_rect.y + row as u16, index));
        }
        paint_line(
            buf,
            text_rect.x,
            text_rect.y + row as u16,
            text_rect.width as usize,
            line,
        );
    }

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

/// Paints a uniform selection background over the dragged screen range. Keeping
/// the foreground untouched stays readable; inverting per-cell colors would
/// turn styled text into confetti.
fn paint_selection(buf: &mut Buffer, area: Rect, selection: TextSelection) {
    let (from, to) = selection.ordered();
    let right = area.x + area.width;
    let bottom = area.y + area.height;
    for row in from.0..=to.0.min(bottom.saturating_sub(1)) {
        if row < area.y {
            continue;
        }
        let col_start = if row == from.0 { from.1 } else { area.x };
        let col_end = if row == to.0 { to.1 + 1 } else { right };
        for col in col_start..col_end.min(right) {
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.bg = SELECTION_BG;
                cell.modifier.remove(Modifier::REVERSED);
            }
        }
    }
}

fn draw_input_box(buf: &mut Buffer, rect: Rect, lines: &[UiLine]) {
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
fn paint_region_tail(buf: &mut Buffer, rect: Rect, lines: &[UiLine]) {
    if rect.height == 0 {
        return;
    }
    let start = lines.len().saturating_sub(rect.height as usize);
    paint_lines(buf, rect, &lines[start..]);
}

fn paint_lines(buf: &mut Buffer, rect: Rect, lines: &[UiLine]) {
    for (row, line) in lines.iter().enumerate().take(rect.height as usize) {
        paint_line(buf, rect.x, rect.y + row as u16, rect.width as usize, line);
    }
}

/// Paints one styled line: kind style is the base each span overrides, the row's
/// remaining cells are reset to the kind background — clearing stale glyphs and
/// extending diff/status backgrounds to the region edge in one pass.
fn paint_line(buf: &mut Buffer, x0: u16, y: u16, width: usize, line: &UiLine) {
    let base = kind_style(line.kind);
    let mut styled = line.line.clone();
    for span in &mut styled.spans {
        span.style = base.patch(span.style);
    }
    let painted = styled.width().min(width);
    buf.set_line(x0, y, &styled, width as u16);
    let row_bg = base.bg.unwrap_or(Color::Reset);
    for column in painted..width {
        if let Some(cell) = buf.cell_mut((x0 + column as u16, y)) {
            cell.reset();
            cell.bg = row_bg;
        }
    }
}

/// Splits the flat control list into screen regions using the builder's invariants:
/// the bottom status line is last, then the command-completion candidates (non-input
/// kinds), then the input box (a trailing run of `Input` lines), and everything above is
/// the live region (assistant stream, picker, pending, progress).
struct ControlRegions {
    live: Vec<UiLine>,
    input: Vec<UiLine>,
    candidates: Vec<UiLine>,
    status: Vec<UiLine>,
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
        // replaces them, so drop them before projecting. A caret-only line (empty
        // continuation row) must stay — it carries the cursor position.
        input.retain(|line| !line.plain().is_empty() || line.cursor.is_some());

        // The caret rides on the wrapped input line as a field, not a text marker.
        let input = wrap_lines(&input, width);
        let cursor = extract_cursor(&input);

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

fn project_control_region(lines: &[UiLine], width: usize) -> Vec<UiLine> {
    wrap_lines(lines, width)
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
    let _ = draw(&mut buffer, area, document, &mut scroll, None);
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
        let transcript_lines: Vec<String> = transcript_lines
            .into_iter()
            .map(|line| project_line(line, true).plain())
            .collect();
        let controls = wrap_lines(&document.controls, control_width);
        let cursor = extract_cursor(&controls);
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
                .map(|line| {
                    let mut text = line.plain();
                    // Input rows are padded to the region width at paint time;
                    // mirror that so the projection matches the screen.
                    let visible = line.line.width();
                    if line.kind == UiKind::Input && visible < control_width {
                        text.push_str(&" ".repeat(control_width - visible));
                    }
                    text
                })
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

fn project_line(line: UiLine, history: bool) -> UiLine {
    if line.line.spans.is_empty() || !should_pad_line(line.kind, history) {
        return line;
    }
    let mut spans = Vec::with_capacity(line.line.spans.len() + 1);
    spans.push(Span::raw(" ".repeat(CONTENT_LEFT_PADDING)));
    spans.extend(line.line.spans);
    UiLine {
        line: Line::from(spans),
        ..line
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
    fn styled_spans_paint_into_cells_with_kind_fallback() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let line = UiLine::new(
            UiKind::Assistant,
            Line::from(vec![
                Span::styled(
                    "A",
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled("B", Style::new().bg(Color::Rgb(1, 2, 3))),
            ]),
        );
        paint_line(&mut buffer, 0, 0, 8, &line);

        let first = &buffer[(0, 0)];
        assert_eq!(first.symbol(), "A");
        assert_eq!(first.fg, Color::Red);
        assert!(first.modifier.contains(Modifier::BOLD));

        // Unstyled spans take the kind color (Assistant = bright white).
        let second = &buffer[(1, 0)];
        assert_eq!(second.fg, Color::White);

        let third = &buffer[(2, 0)];
        assert_eq!(third.symbol(), "B");
        assert_eq!(third.bg, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn kind_background_extends_to_end_of_line() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let line = UiLine::new(UiKind::DiffAdd, "+x");
        paint_line(&mut buffer, 0, 0, 8, &line);

        assert!(buffer
            .content
            .iter()
            .all(|cell| cell.bg == Color::Rgb(28, 70, 38)));
    }

    #[test]
    fn control_regions_split_separates_input_box_candidates_and_status() {
        let controls = vec![
            UiLine::new(UiKind::Status, "  spinner"),
            UiLine::new(UiKind::Input, ""),
            UiLine::new(UiKind::Input, "› hi"),
            UiLine::new(UiKind::Input, ""),
            UiLine::new(UiKind::Selected, "  /help"),
            UiLine::new(UiKind::BottomStatus, "model · tokens"),
        ];

        let regions = ControlRegions::split(&controls, 40);

        assert_eq!(regions.live.len(), 1);
        assert!(regions.live[0].plain().contains("spinner"));
        // Wrapper blank lines stripped, leaving just the prompt line for the box.
        assert_eq!(regions.input.len(), 1);
        assert!(regions.input[0].plain().contains("› hi"));
        assert_eq!(regions.candidates.len(), 1);
        assert!(regions.candidates[0].plain().contains("/help"));
        assert_eq!(regions.status.len(), 1);
        assert!(regions.status[0].plain().contains("model"));
    }

    fn input_lines(text: &str) -> Vec<UiLine> {
        let mut input = crate::input::InputBuffer::default();
        input.push_text(text);
        input.render(true)
    }

    fn sample_document() -> UiDocument {
        let history: Vec<ChatLine> = (0..10)
            .map(|index| ChatLine::Assistant(format!("line {index}")))
            .collect();
        UiBuilder::new()
            .chat_with_width(&history, 18)
            .input(&input_lines("hi"), &[], 0)
            .bottom_status(
                BottomStatus {
                    provider: "p",
                    model: "m",
                    reasoning_effort: "low",
                    git: None,
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
                draw(frame.buffer_mut(), area, &document, scroll, None);
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
                    git: None,
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
                let (cursor, _) = draw(frame.buffer_mut(), area, &document, &mut scroll, None);
                if let Some((x, y)) = cursor {
                    frame.set_cursor_position((x, y));
                }
            })
            .unwrap();
        let pos = terminal.get_cursor_position().unwrap();
        // "│› hello wor|ld": border(0) ›(1) space(2) hello(3..8) space(8) wor(9..12) -> col 12,
        // on the input content row (box border at y=4, content at y=5).
        assert_eq!((pos.x, pos.y), (12, 5));
    }

    #[test]
    fn thinking_header_row_reports_click_target() {
        use ratatui::{backend::TestBackend, Terminal};
        let document = UiBuilder::new()
            .chat(&[ChatLine::Reasoning {
                text: "deep".to_string(),
                collapsed: true,
                duration_secs: Some(3),
            }])
            .input(&input_lines("hi"), &[], 0)
            .finish();
        let mut scroll = 0usize;
        let mut terminal = Terminal::new(TestBackend::new(30, 8)).unwrap();
        let mut clickables = Vec::new();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let (_, clicks) = draw(frame.buffer_mut(), area, &document, &mut scroll, None);
                clickables = clicks;
            })
            .unwrap();
        // The thinking header is the only clickable row, pointing at chat item 0.
        assert_eq!(clickables, vec![(0, 0)]);
    }

    #[test]
    fn selection_covers_whole_screen_and_extracts_text() {
        use ratatui::{backend::TestBackend, Terminal};
        let document = sample_document();
        let mut scroll = 0usize;
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();

        // Drag from row 0 col 2 to row 2 col 8 across the transcript.
        let mut selection = TextSelection::new((0, 2));
        selection.cursor = (2, 8);
        let mut buffer = None;
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(
                    frame.buffer_mut(),
                    area,
                    &document,
                    &mut scroll,
                    Some(selection),
                );
                buffer = Some(frame.buffer_mut().clone());
            })
            .unwrap();
        let buffer = buffer.expect("frame painted");

        // First row: selected from col 2 to the right edge, blank cells included.
        for col in 2..20 {
            assert_eq!(buffer[(col, 0)].bg, SELECTION_BG, "row0 col{col}");
        }
        assert_ne!(buffer[(1, 0)].bg, SELECTION_BG);
        // Last row: cols 0..=8 selected, col 9 untouched.
        for col in 0..9 {
            assert_eq!(buffer[(col, 2)].bg, SELECTION_BG, "row2 col{col}");
        }
        assert_ne!(buffer[(9, 2)].bg, SELECTION_BG);

        // Copy reads the painted cells; trailing blanks are trimmed.
        let text = selected_text(&buffer, selection);
        assert!(text.contains("line 8"), "copied: {text:?}");

        // A backwards drag covers the same range.
        let mut back = TextSelection::new((2, 8));
        back.cursor = (0, 2);
        assert_eq!(selected_text(&buffer, back), text);
    }
}
