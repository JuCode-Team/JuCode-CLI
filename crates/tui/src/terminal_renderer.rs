use std::io::{self, Stdout, Write};

use crossterm::{
    cursor::MoveTo,
    queue,
    terminal::{self, Clear, ClearType},
};
use ratatui::{
    buffer::{Buffer, Cell as RtCell},
    layout::Rect,
    style::{Color, Modifier},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    extract_cursor, padded_content_width, render_ansi_line, split_ansi_sequence, wrap_lines,
    CursorTarget, ProjectedDocument, RenderedFrame, UiDocument, UiKind, UiLine,
    CONTENT_LEFT_PADDING, DISABLE_AUTOWRAP, ENABLE_AUTOWRAP, HIDE_CURSOR, RESET, SHOW_CURSOR,
    SHOW_HARDWARE_CURSOR, SYNC_END, SYNC_START,
};

pub(crate) struct TerminalRenderer {
    previous_transcript_lines: Vec<String>,
    previous_width: u16,
    previous_height: u16,
    previous_viewport_top: usize,
    previous_buffer: Buffer,
    current_buffer: Buffer,
    initialized: bool,
    force_transcript_rebuild: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullRenderMode {
    VisibleViewport,
}

impl TerminalRenderer {
    pub(crate) fn new() -> Self {
        Self {
            previous_transcript_lines: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            previous_viewport_top: 0,
            previous_buffer: Buffer::empty(Rect::ZERO),
            current_buffer: Buffer::empty(Rect::ZERO),
            initialized: false,
            force_transcript_rebuild: false,
        }
    }

    pub(crate) fn force_transcript_rebuild(&mut self) {
        self.force_transcript_rebuild = true;
    }

    pub(crate) fn render(&mut self, stdout: &mut Stdout, document: &UiDocument) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        let width = width.max(1);
        let height = height.max(1);
        let projection = ProjectedDocument::from_document(document, width);
        let transcript_changed = self.previous_transcript_lines != projection.transcript_lines;
        let mut frame = projection.clone().into_frame();
        if frame.lines.is_empty() {
            frame.lines.push(String::new());
        }
        let viewport_top = viewport_top(frame.lines.len(), height);

        if self.force_transcript_rebuild || transcript_changed {
            self.render_transcript_projection(stdout, &projection, width, height)?;
        } else if document.reset_screen || !self.initialized {
            self.full_render(stdout, &frame, document.reset_screen, false, width, height)?;
        } else if self.previous_width != width || self.previous_height != height {
            self.full_render(stdout, &frame, true, false, width, height)?;
        } else {
            self.buffer_diff_render(stdout, &frame, viewport_top, width, height)?;
        }

        self.position_cursor(
            stdout,
            frame.cursor,
            width,
            height,
            frame.lines.len(),
            viewport_top,
        )?;
        stdout.flush()?;

        self.previous_transcript_lines = projection.transcript_lines;
        self.previous_width = width;
        self.previous_height = height;
        self.previous_viewport_top = viewport_top;
        self.initialized = true;
        self.force_transcript_rebuild = false;
        Ok(())
    }

    fn render_transcript_projection(
        &mut self,
        stdout: &mut Stdout,
        projection: &ProjectedDocument,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let frame_lines = projection.frame_lines();
        let mut buffer = render_buffer_start();
        buffer.push_str(clear_screen_sequence(true));
        append_lines_to_buffer(&mut buffer, &frame_lines);
        buffer.push_str(&render_buffer_end());
        stdout.write_all(buffer.as_bytes())?;
        let (start, end) =
            full_render_window(frame_lines.len(), height, FullRenderMode::VisibleViewport);
        self.rebuild_buffers_from_lines(&frame_lines[start..end], width, height);
        Ok(())
    }

    fn full_render(
        &mut self,
        stdout: &mut Stdout,
        frame: &RenderedFrame,
        clear: bool,
        purge_scrollback: bool,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let mut buffer = render_buffer_start();
        if clear {
            buffer.push_str(clear_screen_sequence(purge_scrollback));
        }
        let (start, end) =
            full_render_window(frame.lines.len(), height, FullRenderMode::VisibleViewport);
        append_lines_to_buffer(&mut buffer, &frame.lines[start..end]);
        buffer.push_str(&render_buffer_end());
        stdout.write_all(buffer.as_bytes())?;
        self.rebuild_buffers_from_lines(&frame.lines[start..end], width, height);
        Ok(())
    }

    fn buffer_diff_render(
        &mut self,
        stdout: &mut Stdout,
        frame: &RenderedFrame,
        viewport_top: usize,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        if viewport_top != self.previous_viewport_top {
            return self.full_render(stdout, frame, true, false, width, height);
        }
        self.render_frame_to_current_buffer(frame, viewport_top, width, height);
        write_buffer_diff(stdout, &self.previous_buffer, &self.current_buffer)?;
        std::mem::swap(&mut self.previous_buffer, &mut self.current_buffer);
        Ok(())
    }

    fn position_cursor(
        &mut self,
        stdout: &mut Stdout,
        cursor: Option<CursorTarget>,
        width: u16,
        height: u16,
        line_count: usize,
        viewport_top: usize,
    ) -> io::Result<()> {
        let Some(cursor) = cursor else {
            stdout.write_all(HIDE_CURSOR.as_bytes())?;
            return Ok(());
        };
        let target_row = cursor
            .row
            .min(line_count.saturating_sub(1))
            .saturating_sub(viewport_top)
            .min(height.saturating_sub(1) as usize);
        let column = cursor.column.min(width.saturating_sub(1) as usize);
        queue!(stdout, MoveTo(column as u16, target_row as u16))?;
        if SHOW_HARDWARE_CURSOR {
            stdout.write_all(SHOW_CURSOR.as_bytes())?;
        } else {
            stdout.write_all(HIDE_CURSOR.as_bytes())?;
        }
        Ok(())
    }

    fn rebuild_buffers_from_lines(&mut self, lines: &[String], width: u16, height: u16) {
        let area = Rect::new(0, 0, width, height);
        self.previous_buffer = Buffer::empty(area);
        self.current_buffer = Buffer::empty(area);
        render_ansi_lines_to_buffer(lines, &mut self.previous_buffer);
        self.previous_buffer = normalize_trailing_cells(self.previous_buffer.clone());
    }

    fn render_frame_to_current_buffer(
        &mut self,
        frame: &RenderedFrame,
        viewport_top: usize,
        width: u16,
        height: u16,
    ) {
        let area = Rect::new(0, 0, width, height);
        if self.current_buffer.area != area {
            self.current_buffer = Buffer::empty(area);
        } else {
            self.current_buffer.reset();
        }
        let end = viewport_top
            .saturating_add(height as usize)
            .min(frame.lines.len());
        render_ansi_lines_to_buffer(&frame.lines[viewport_top..end], &mut self.current_buffer);
        self.current_buffer = normalize_trailing_cells(self.current_buffer.clone());
    }

    #[cfg(feature = "bench")]
    pub(crate) fn render_document_for_bench(
        &mut self,
        document: &UiDocument,
        width: u16,
        height: u16,
    ) -> usize {
        let width = width.max(1);
        let height = height.max(1);
        let projection = ProjectedDocument::from_document(document, width);
        let transcript_changed = self.previous_transcript_lines != projection.transcript_lines;
        let mut frame = projection.clone().into_frame();
        if frame.lines.is_empty() {
            frame.lines.push(String::new());
        }
        let viewport_top = viewport_top(frame.lines.len(), height);
        let changed_cells = if self.force_transcript_rebuild
            || transcript_changed
            || document.reset_screen
            || !self.initialized
            || self.previous_width != width
            || self.previous_height != height
            || viewport_top != self.previous_viewport_top
        {
            let (start, end) =
                full_render_window(frame.lines.len(), height, FullRenderMode::VisibleViewport);
            self.rebuild_buffers_from_lines(&frame.lines[start..end], width, height);
            self.previous_buffer.content.len()
        } else {
            self.render_frame_to_current_buffer(&frame, viewport_top, width, height);
            let changed_cells = buffer_diff_count(&self.previous_buffer, &self.current_buffer);
            std::mem::swap(&mut self.previous_buffer, &mut self.current_buffer);
            changed_cells
        };

        self.previous_transcript_lines = projection.transcript_lines;
        self.previous_width = width;
        self.previous_height = height;
        self.previous_viewport_top = viewport_top;
        self.initialized = true;
        self.force_transcript_rebuild = false;

        changed_cells ^ buffer_checksum(&self.previous_buffer)
    }
}

impl ProjectedDocument {
    pub(crate) fn from_document(document: &UiDocument, width: u16) -> Self {
        let width = width as usize;
        let content_width = padded_content_width(width);
        let transcript_lines = document.rendered_history_lines.clone().unwrap_or_else(|| {
            wrap_lines(&document.history, content_width)
                .into_iter()
                .map(|line| render_ansi_line(&line))
                .collect::<Vec<_>>()
        });
        let transcript_lines = pad_projected_lines(transcript_lines);
        let mut controls = wrap_lines(&document.controls, content_width);
        let cursor = extract_cursor(&mut controls).map(|cursor| CursorTarget {
            row: cursor.row,
            column: cursor.column + CONTENT_LEFT_PADDING,
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
        active_lines.extend(pad_projected_lines(
            controls
                .into_iter()
                .map(|line| render_control_line(&line, content_width))
                .collect(),
        ));

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

    pub(crate) fn into_frame(self) -> RenderedFrame {
        RenderedFrame {
            lines: self.frame_lines(),
            cursor: self.cursor,
        }
    }
}

fn render_control_line(line: &UiLine, width: usize) -> String {
    let mut line = line.clone();
    if line.kind == UiKind::Input {
        let visible = crate::visible_width(&line.text);
        if visible < width {
            line.text.push_str(&" ".repeat(width - visible));
        }
    }
    render_ansi_line(&line)
}

fn pad_projected_lines(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| {
            if line.is_empty() {
                line
            } else {
                format!("{}{}", " ".repeat(CONTENT_LEFT_PADDING), line)
            }
        })
        .collect()
}

fn append_lines_to_buffer(buffer: &mut String, lines: &[String]) {
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            buffer.push_str("\r\n");
        }
        buffer.push_str(line);
    }
}

fn render_ansi_lines_to_buffer(lines: &[String], buffer: &mut Buffer) {
    let width = buffer.area.width as usize;
    for (row, line) in lines.iter().enumerate().take(buffer.area.height as usize) {
        render_ansi_line_to_buffer(line, row, width, buffer);
    }
}

fn render_ansi_line_to_buffer(line: &str, row: usize, width: usize, buffer: &mut Buffer) {
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

        let index = row * width + column;
        if let Some(cell) = buffer.content.get_mut(index) {
            cell.set_char(ch);
            apply_cell_style(cell, style);
        }
        for offset in 1..ch_width {
            if let Some(cell) = buffer.content.get_mut(index + offset) {
                cell.set_symbol(" ");
                apply_cell_style(cell, style);
                cell.skip = true;
            }
        }
        column += ch_width;
    }

    if row_background != Color::Reset {
        let row_start = row * width;
        let row_end = row_start + width;
        for cell in &mut buffer.content[row_start..row_end] {
            if cell.bg == Color::Reset {
                cell.bg = row_background;
            }
        }
    }
}

fn normalize_trailing_cells(mut buffer: Buffer) -> Buffer {
    for cell in &mut buffer.content {
        cell.skip = false;
    }
    buffer
}

fn write_buffer_diff<W: Write>(stdout: &mut W, previous: &Buffer, next: &Buffer) -> io::Result<()> {
    stdout.write_all(render_buffer_start().as_bytes())?;
    let width = next.area.width as usize;
    let height = next.area.height as usize;
    for y in 0..height {
        let row_start = y * width;
        let row_end = row_start + width;
        let previous_row = &previous.content[row_start..row_end];
        let next_row = &next.content[row_start..row_end];
        let clear_from = row_clear_from(next_row);
        let mut cleared = false;
        let mut x = 0usize;
        while x < width {
            if Some(x) == clear_from {
                stdout.write_all(RESET.as_bytes())?;
                queue!(
                    stdout,
                    MoveTo(x as u16, y as u16),
                    Clear(ClearType::UntilNewLine)
                )?;
                cleared = true;
                break;
            }
            let current = &next_row[x];
            let previous_cell = &previous_row[x];
            if !current.skip && current != previous_cell {
                queue!(stdout, MoveTo(x as u16, y as u16))?;
                write_cell(stdout, current)?;
            }
            x += UnicodeWidthStr::width(current.symbol()).max(1);
        }
        if cleared {
            continue;
        }
    }
    stdout.write_all(render_buffer_end().as_bytes())?;
    Ok(())
}

#[cfg(feature = "bench")]
fn buffer_diff_count(previous: &Buffer, next: &Buffer) -> usize {
    previous
        .content
        .iter()
        .zip(next.content.iter())
        .filter(|(previous, next)| *previous != *next)
        .count()
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

fn row_clear_from(row: &[RtCell]) -> Option<usize> {
    let mut last_meaningful = None;
    for (index, cell) in row.iter().enumerate() {
        if cell.symbol() != " "
            || cell.fg != Color::Reset
            || cell.bg != Color::Reset
            || !cell.modifier.is_empty()
        {
            last_meaningful = Some(index);
        }
    }
    let clear_from = last_meaningful.map_or(0, |index| index + 1);
    (clear_from < row.len()).then_some(clear_from)
}

fn write_cell<W: Write>(stdout: &mut W, cell: &RtCell) -> io::Result<()> {
    write_style(stdout, cell)?;
    stdout.write_all(cell.symbol().as_bytes())
}

fn write_style<W: Write>(stdout: &mut W, cell: &RtCell) -> io::Result<()> {
    stdout.write_all(RESET.as_bytes())?;
    write_color(stdout, cell.fg, false)?;
    write_color(stdout, cell.bg, true)?;
    if cell.modifier.contains(Modifier::BOLD) {
        stdout.write_all(b"\x1b[1m")?;
    }
    if cell.modifier.contains(Modifier::ITALIC) {
        stdout.write_all(b"\x1b[3m")?;
    }
    if cell.modifier.contains(Modifier::REVERSED) {
        stdout.write_all(b"\x1b[7m")?;
    }
    Ok(())
}

fn write_color<W: Write>(stdout: &mut W, color: Color, background: bool) -> io::Result<()> {
    match color {
        Color::Reset => {}
        Color::Black => stdout.write_all(if background { b"\x1b[40m" } else { b"\x1b[30m" })?,
        Color::Red => stdout.write_all(if background { b"\x1b[41m" } else { b"\x1b[31m" })?,
        Color::Green => stdout.write_all(if background { b"\x1b[42m" } else { b"\x1b[32m" })?,
        Color::Yellow => stdout.write_all(if background { b"\x1b[43m" } else { b"\x1b[33m" })?,
        Color::Blue => stdout.write_all(if background { b"\x1b[44m" } else { b"\x1b[34m" })?,
        Color::Magenta => stdout.write_all(if background { b"\x1b[45m" } else { b"\x1b[35m" })?,
        Color::Cyan => stdout.write_all(if background { b"\x1b[46m" } else { b"\x1b[36m" })?,
        Color::Gray => stdout.write_all(if background { b"\x1b[47m" } else { b"\x1b[37m" })?,
        Color::DarkGray => stdout.write_all(if background {
            b"\x1b[100m"
        } else {
            b"\x1b[90m"
        })?,
        Color::LightRed => stdout.write_all(if background {
            b"\x1b[101m"
        } else {
            b"\x1b[91m"
        })?,
        Color::LightGreen => stdout.write_all(if background {
            b"\x1b[102m"
        } else {
            b"\x1b[92m"
        })?,
        Color::LightYellow => stdout.write_all(if background {
            b"\x1b[103m"
        } else {
            b"\x1b[93m"
        })?,
        Color::LightBlue => stdout.write_all(if background {
            b"\x1b[104m"
        } else {
            b"\x1b[94m"
        })?,
        Color::LightMagenta => stdout.write_all(if background {
            b"\x1b[105m"
        } else {
            b"\x1b[95m"
        })?,
        Color::LightCyan => stdout.write_all(if background {
            b"\x1b[106m"
        } else {
            b"\x1b[96m"
        })?,
        Color::White => stdout.write_all(if background {
            b"\x1b[107m"
        } else {
            b"\x1b[97m"
        })?,
        Color::Indexed(index) => write!(
            stdout,
            "\x1b[{};5;{index}m",
            if background { 48 } else { 38 }
        )?,
        Color::Rgb(red, green, blue) => write!(
            stdout,
            "\x1b[{};2;{red};{green};{blue}m",
            if background { 48 } else { 38 }
        )?,
    }
    Ok(())
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

fn render_buffer_start() -> String {
    format!("{SYNC_START}{HIDE_CURSOR}{DISABLE_AUTOWRAP}")
}

fn render_buffer_end() -> String {
    format!("{RESET}{ENABLE_AUTOWRAP}{SYNC_END}")
}

fn clear_screen_sequence(purge_scrollback: bool) -> &'static str {
    if purge_scrollback {
        "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H"
    } else {
        "\x1b[2J\x1b[H"
    }
}

fn viewport_top(line_count: usize, height: u16) -> usize {
    line_count
        .max(height as usize)
        .saturating_sub(height as usize)
}

fn full_render_window(line_count: usize, height: u16, mode: FullRenderMode) -> (usize, usize) {
    match mode {
        FullRenderMode::VisibleViewport => {
            let start = viewport_top(line_count, height);
            let end = start.saturating_add(height as usize).min(line_count);
            (start, end)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn buffer_plain_text(buffer: &Buffer) -> Vec<String> {
        let width = buffer.area.width as usize;
        buffer
            .content
            .chunks(width)
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn full_redraw_window_only_covers_visible_viewport() {
        let (start, end) = full_render_window(22, 5, FullRenderMode::VisibleViewport);

        assert_eq!((start, end), (17, 22));
    }

    #[test]
    fn resize_rebuild_clear_sequence_purges_scrollback() {
        assert_eq!(clear_screen_sequence(false), "\x1b[2J\x1b[H");
        assert!(clear_screen_sequence(true).contains("\x1b[3J"));
        assert!(clear_screen_sequence(true).starts_with("\x1b[r\x1b[0m"));
    }

    #[test]
    fn ansi_lines_render_into_ratatui_cells_with_style() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        render_ansi_lines_to_buffer(
            &["\x1b[31;1mA\x1b[0m \x1b[48;2;1;2;3mB".to_string()],
            &mut buffer,
        );

        let first = &buffer.content[0];
        assert_eq!(first.symbol(), "A");
        assert_eq!(first.fg, Color::Red);
        assert!(first.modifier.contains(Modifier::BOLD));

        let third = &buffer.content[2];
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
    fn terminal_renderer_buffers_only_visible_viewport() {
        let mut renderer = TerminalRenderer::new();
        let frame = RenderedFrame {
            lines: (0..10).map(|index| format!("line {index}")).collect(),
            cursor: None,
        };

        renderer.render_frame_to_current_buffer(&frame, 7, 8, 3);

        assert_eq!(
            buffer_plain_text(&renderer.current_buffer),
            vec![
                "line 7".to_string(),
                "line 8".to_string(),
                "line 9".to_string()
            ]
        );
    }

    #[test]
    fn clear_to_end_of_line_resets_background_first() {
        let previous = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut next = Buffer::empty(Rect::new(0, 0, 4, 1));
        next.content[0].set_symbol("x");
        next.content[0].bg = Color::Rgb(48, 52, 62);

        let mut output = Vec::new();
        write_buffer_diff(&mut output, &previous, &next).expect("diff render should write");
        let text = String::from_utf8(output).expect("terminal output should be utf8");

        let clear_index = text
            .find("\x1b[K")
            .or_else(|| text.find("\x1b[0K"))
            .expect("should clear to end of line");
        let reset_index = text[..clear_index]
            .rfind(RESET)
            .expect("should reset style before clearing");
        assert!(reset_index < clear_index);
    }
}
