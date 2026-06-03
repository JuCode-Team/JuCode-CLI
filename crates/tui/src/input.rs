use std::time::{Duration, Instant};

use crate::{
    CURSOR_MARKER, PASTE_BURST_CHAR_INTERVAL, PASTE_BURST_IDLE_TIMEOUT,
    PASTE_ENTER_SUPPRESS_WINDOW, PASTE_PLACEHOLDER_CHARS, SELECT_END, SELECT_START,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InputBuffer {
    cells: Vec<Cell>,
    cursor: usize,
    anchor: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cell {
    Char(char),
    LargePaste(String),
}

impl InputBuffer {
    pub(crate) fn text(&self) -> String {
        let mut text = String::new();
        for cell in &self.cells {
            match cell {
                Cell::Char(ch) => text.push(*ch),
                Cell::LargePaste(paste) => text.push_str(paste),
            }
        }
        text
    }

    #[cfg(test)]
    pub(crate) fn display_text(&self) -> String {
        let mut display = String::new();
        for cell in &self.cells {
            match cell {
                Cell::Char(ch) => display.push(*ch),
                Cell::LargePaste(paste) => {
                    let char_count = paste.chars().count();
                    display.push_str(&format!("[Pasted: {char_count} chars]"));
                }
            }
        }
        display
    }

    /// Display string for the UI. The hardware cursor marker is embedded at the
    /// cursor position; the cell under the cursor is rendered as a reverse-video
    /// block (an overwrite-style caret) and any active selection is reverse-video
    /// highlighted. Selection styling is closed and reopened around newlines so
    /// every logical line stays balanced.
    pub(crate) fn render(&self, show_cursor: bool) -> String {
        let selection = if show_cursor { self.selection() } else { None };
        let mut out = String::new();
        let mut in_selection = false;
        for index in 0..=self.cells.len() {
            if let Some((_, end)) = selection {
                if in_selection && index == end {
                    out.push_str(SELECT_END);
                    in_selection = false;
                }
            }
            if show_cursor && index == self.cursor {
                out.push_str(CURSOR_MARKER);
            }
            if let Some((start, end)) = selection {
                if index == start && start != end {
                    out.push_str(SELECT_START);
                    in_selection = true;
                }
            }
            // Block caret on the cell under the cursor (only when nothing is selected,
            // so it does not double up with the selection highlight).
            let block_caret = show_cursor && selection.is_none() && index == self.cursor;
            let Some(cell) = self.cells.get(index) else {
                if block_caret {
                    out.push_str(SELECT_START);
                    out.push(' ');
                    out.push_str(SELECT_END);
                }
                continue;
            };
            match cell {
                Cell::Char('\n') => {
                    if block_caret {
                        out.push_str(SELECT_START);
                        out.push(' ');
                        out.push_str(SELECT_END);
                    }
                    if in_selection {
                        out.push_str(SELECT_END);
                    }
                    out.push('\n');
                    if in_selection {
                        out.push_str(SELECT_START);
                    }
                }
                Cell::Char(ch) => {
                    if block_caret {
                        out.push_str(SELECT_START);
                        out.push(*ch);
                        out.push_str(SELECT_END);
                    } else {
                        out.push(*ch);
                    }
                }
                Cell::LargePaste(paste) => {
                    let char_count = paste.chars().count();
                    let placeholder = format!("[Pasted: {char_count} chars]");
                    if block_caret {
                        out.push_str(SELECT_START);
                        out.push_str(&placeholder);
                        out.push_str(SELECT_END);
                    } else {
                        out.push_str(&placeholder);
                    }
                }
            }
        }
        out
    }

    /// Normalized selection range `[start, end)` over cell indices, or `None` when empty.
    pub(crate) fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            None
        } else {
            Some((anchor.min(self.cursor), anchor.max(self.cursor)))
        }
    }

    pub(crate) fn has_selection(&self) -> bool {
        self.selection().is_some()
    }

    pub(crate) fn clear(&mut self) {
        self.cells.clear();
        self.cursor = 0;
        self.anchor = None;
    }

    pub(crate) fn push_char(&mut self, ch: char) {
        self.delete_selection();
        self.cells.insert(self.cursor, Cell::Char(ch));
        self.cursor += 1;
    }

    pub(crate) fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.delete_selection();
        for ch in text.chars() {
            self.cells.insert(self.cursor, Cell::Char(ch));
            self.cursor += 1;
        }
    }

    pub(crate) fn push_paste(&mut self, text: &str) {
        let text = normalize_pasted_text(text);
        if text.chars().count() > PASTE_PLACEHOLDER_CHARS {
            self.delete_selection();
            self.cells.insert(self.cursor, Cell::LargePaste(text));
            self.cursor += 1;
        } else {
            self.push_text(&text);
        }
    }

    /// Backspace: delete the selection if any, otherwise the cell before the cursor.
    pub(crate) fn pop(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.cells.remove(self.cursor);
    }

    /// Delete key: delete the selection if any, otherwise the cell at the cursor.
    pub(crate) fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor < self.cells.len() {
            self.cells.remove(self.cursor);
        }
    }

    /// Remove the selected range and collapse the cursor to its start.
    pub(crate) fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.cells.drain(start..end);
        self.cursor = start;
        self.anchor = None;
        true
    }

    fn set_cursor(&mut self, position: usize, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = position.min(self.cells.len());
        if self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
    }

    pub(crate) fn move_left(&mut self, extend: bool) {
        if !extend {
            if let Some((start, _)) = self.selection() {
                self.cursor = start;
                self.anchor = None;
                return;
            }
        }
        self.set_cursor(self.cursor.saturating_sub(1), extend);
    }

    pub(crate) fn move_right(&mut self, extend: bool) {
        if !extend {
            if let Some((_, end)) = self.selection() {
                self.cursor = end;
                self.anchor = None;
                return;
            }
        }
        self.set_cursor(self.cursor + 1, extend);
    }

    pub(crate) fn move_home(&mut self, extend: bool) {
        self.set_cursor(self.line_start(self.cursor), extend);
    }

    pub(crate) fn move_end(&mut self, extend: bool) {
        self.set_cursor(self.line_end(self.cursor), extend);
    }

    pub(crate) fn move_document_start(&mut self, extend: bool) {
        self.set_cursor(0, extend);
    }

    pub(crate) fn move_document_end(&mut self, extend: bool) {
        self.set_cursor(self.cells.len(), extend);
    }

    pub(crate) fn move_up(&mut self, extend: bool) {
        let start = self.line_start(self.cursor);
        if start == 0 {
            self.set_cursor(0, extend);
            return;
        }
        let column = self.cursor - start;
        let prev_newline = start - 1;
        let prev_start = self.line_start(prev_newline);
        let prev_len = prev_newline - prev_start;
        self.set_cursor(prev_start + column.min(prev_len), extend);
    }

    pub(crate) fn move_down(&mut self, extend: bool) {
        let start = self.line_start(self.cursor);
        let end = self.line_end(self.cursor);
        if end >= self.cells.len() {
            self.set_cursor(self.cells.len(), extend);
            return;
        }
        let column = self.cursor - start;
        let next_start = end + 1;
        let next_end = self.line_end(next_start);
        let next_len = next_end - next_start;
        self.set_cursor(next_start + column.min(next_len), extend);
    }

    pub(crate) fn move_word_left(&mut self, extend: bool) {
        let mut position = self.cursor;
        while position > 0 && self.is_word_separator(position - 1) {
            position -= 1;
        }
        while position > 0 && !self.is_word_separator(position - 1) {
            position -= 1;
        }
        self.set_cursor(position, extend);
    }

    pub(crate) fn move_word_right(&mut self, extend: bool) {
        let len = self.cells.len();
        let mut position = self.cursor;
        while position < len && self.is_word_separator(position) {
            position += 1;
        }
        while position < len && !self.is_word_separator(position) {
            position += 1;
        }
        self.set_cursor(position, extend);
    }

    fn is_word_separator(&self, index: usize) -> bool {
        matches!(self.cells.get(index), Some(Cell::Char(ch)) if ch.is_whitespace())
    }

    /// Index of the first cell on the line containing `position`.
    fn line_start(&self, position: usize) -> usize {
        let mut start = 0;
        for index in 0..position {
            if matches!(self.cells.get(index), Some(Cell::Char('\n'))) {
                start = index + 1;
            }
        }
        start
    }

    /// Index of the newline (or buffer end) terminating the line containing `position`.
    fn line_end(&self, position: usize) -> usize {
        let mut index = position;
        while index < self.cells.len() {
            if matches!(self.cells[index], Cell::Char('\n')) {
                break;
            }
            index += 1;
        }
        index
    }
}

fn normalize_pasted_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub(crate) fn paste_burst_render_delay() -> Duration {
    PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(1)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PasteBurst {
    last_plain_char_at: Option<Instant>,
    pending_first_char: Option<(char, Instant)>,
    buffer: String,
    active: bool,
    burst_window_until: Option<Instant>,
}

pub(crate) enum PasteCharDecision {
    RetainFirstChar,
    BeginBufferFromPending,
    BufferAppend,
}

pub(crate) enum PasteFlush {
    Paste(String),
    Typed(char),
    None,
}

impl PasteBurst {
    pub(crate) fn on_plain_ascii_char(&mut self, ch: char, now: Instant) -> PasteCharDecision {
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

    pub(crate) fn append_char(&mut self, ch: char, now: Instant) {
        self.buffer.push(ch);
        self.extend_window(now);
    }

    pub(crate) fn append_newline_if_active(&mut self, now: Instant) -> bool {
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

    pub(crate) fn newline_should_insert_instead_of_submit(&self, now: Instant) -> bool {
        self.is_active()
            || self
                .burst_window_until
                .map(|until| now <= until)
                .unwrap_or(false)
    }

    pub(crate) fn flush_if_due(&mut self, now: Instant) -> PasteFlush {
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

    pub(crate) fn flush_before_non_plain_input(&mut self) -> Option<String> {
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

    pub(crate) fn clear_after_non_char(&mut self) {
        self.last_plain_char_at = None;
        self.pending_first_char = None;
        self.burst_window_until = None;
        self.active = false;
    }

    pub(crate) fn clear_after_explicit_paste(&mut self) {
        self.last_plain_char_at = None;
        self.pending_first_char = None;
        self.burst_window_until = None;
        self.buffer.clear();
        self.active = false;
    }

    fn extend_window(&mut self, now: Instant) {
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    pub(crate) fn is_active(&self) -> bool {
        self.is_buffering() || self.pending_first_char.is_some()
    }

    fn is_buffering(&self) -> bool {
        self.active || !self.buffer.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VISIBLE_CURSOR;

    fn typed(text: &str) -> InputBuffer {
        let mut input = InputBuffer::default();
        input.push_text(text);
        input
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
    fn cursor_moves_and_inserts_in_the_middle() {
        let mut input = typed("helloworld");
        for _ in 0..5 {
            input.move_left(false);
        }
        assert_eq!(input.cursor, 5);
        input.push_char(' ');
        assert_eq!(input.text(), "hello world");
        assert_eq!(input.cursor, 6);
    }

    #[test]
    fn home_end_jump_within_line() {
        let mut input = typed("first\nsecond");
        input.move_home(false);
        assert_eq!(input.cursor, 6); // start of "second"
        input.move_end(false);
        assert_eq!(input.cursor, input.cells.len());
        input.move_document_start(false);
        assert_eq!(input.cursor, 0);
        input.move_document_end(false);
        assert_eq!(input.cursor, input.cells.len());
    }

    #[test]
    fn up_down_preserve_column() {
        let mut input = typed("abcd\nxy\nlongline");
        input.move_document_start(false);
        input.move_right(false);
        input.move_right(false);
        input.move_right(false); // column 3 on line 0
        input.move_down(false);
        assert_eq!(input.cursor, 5 + 2); // clamped to end of "xy"
        input.move_down(false);
        // column was clamped to 2 on "xy", so it stays 2 on "longline"
        assert_eq!(input.cursor, 8 + 2);
    }

    #[test]
    fn shift_selection_then_backspace_deletes_range() {
        let mut input = typed("hello world");
        input.move_home(false);
        for _ in 0..5 {
            input.move_right(true); // select "hello"
        }
        assert_eq!(input.selection(), Some((0, 5)));
        input.pop();
        assert_eq!(input.text(), " world");
        assert_eq!(input.cursor, 0);
        assert!(input.selection().is_none());
    }

    #[test]
    fn typing_replaces_selection() {
        let mut input = typed("abc");
        input.move_document_start(false);
        input.move_right(true);
        input.move_right(true); // select "ab"
        input.push_char('X');
        assert_eq!(input.text(), "Xc");
        assert_eq!(input.cursor, 1);
    }

    #[test]
    fn word_movement_skips_to_boundaries() {
        let mut input = typed("foo bar baz");
        input.move_document_start(false);
        input.move_word_right(false);
        assert_eq!(input.cursor, 3); // after "foo"
        input.move_word_right(false);
        assert_eq!(input.cursor, 7); // after "bar"
        input.move_word_left(false);
        assert_eq!(input.cursor, 4); // start of "bar"
    }

    #[test]
    fn block_caret_highlights_char_under_cursor() {
        let mut input = typed("abc");
        input.move_left(false); // cursor at index 2, on 'c'
        assert_eq!(
            input.render(true),
            format!("ab{CURSOR_MARKER}{SELECT_START}c{SELECT_END}")
        );
    }

    #[test]
    fn block_caret_at_end_renders_trailing_block() {
        let input = typed("ab"); // cursor at end
        assert_eq!(
            input.render(true),
            format!("ab{CURSOR_MARKER}{SELECT_START} {SELECT_END}")
        );
    }

    #[test]
    fn render_embeds_cursor_and_highlights_selection() {
        let mut input = typed("abc");
        input.move_document_start(false);
        input.move_right(true);
        input.move_right(true); // select "ab", cursor at end of selection
        let rendered = input.render(true);
        // Selection is highlighted; no separate block caret while selecting.
        assert_eq!(
            rendered,
            format!("{SELECT_START}ab{SELECT_END}{CURSOR_MARKER}c")
        );
        assert_eq!(input.render(false), "abc");
    }

    #[test]
    fn delete_forward_removes_char_at_cursor() {
        let mut input = typed("abc");
        input.move_document_start(false);
        input.delete_forward();
        assert_eq!(input.text(), "bc");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn paste_placeholder_is_atomic_for_cursor() {
        let mut input = InputBuffer::default();
        let pasted = "x".repeat(PASTE_PLACEHOLDER_CHARS + 1);
        input.push_text("a");
        input.push_paste(&pasted);
        input.push_text("b");
        // a | paste | b  -> 3 cells, cursor at 3
        assert_eq!(input.cells.len(), 3);
        input.move_left(false); // before "b"
        input.move_left(false); // before the paste (atomic)
        assert_eq!(input.cursor, 1);
        input.pop(); // removes "a"
        assert_eq!(input.text(), format!("{pasted}b"));
    }

    #[test]
    fn block_caret_uses_visible_cursor_placeholder_shape() {
        let input = typed("");
        assert_eq!(
            input.render(true),
            format!("{CURSOR_MARKER}{SELECT_START} {SELECT_END}")
        );
        assert_eq!(VISIBLE_CURSOR, "|");
    }
}
