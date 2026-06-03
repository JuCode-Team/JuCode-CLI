use unicode_width::UnicodeWidthChar;

use crate::{split_ansi_sequence, visible_width};

pub(crate) const MD_BOLD_ON: &str = "\x1b[1m";
pub(crate) const MD_BOLD_OFF: &str = "\x1b[22m";
pub(crate) const MD_ITALIC_ON: &str = "\x1b[3m";
pub(crate) const MD_ITALIC_OFF: &str = "\x1b[23m";
pub(crate) const MD_CODE_ON: &str = "\x1b[38;5;117m"; // light blue inline-code text
const MD_CODE_OFF: &str = "\x1b[39m"; // restore default foreground
pub(crate) const MD_DIM_ON: &str = "\x1b[90m";
pub(crate) const MD_DIM_OFF: &str = "\x1b[39m";

#[derive(Clone, Copy)]
enum MdAlign {
    Left,
    Right,
    Center,
}

/// Render markdown into terminal lines: headings/bold/italic/inline-code become
/// ANSI styling, and pipe tables become aligned box-drawn tables. `base` is the
/// line's foreground color (so inline-code can restore it); the full color is still
/// applied later by `render_ansi_line`.
pub(crate) fn render_markdown(text: &str, width: usize, base: &str) -> Vec<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        // Fenced code block: render verbatim (no inline markdown) until the closing
        // fence, or until the end of the text while still streaming.
        if let Some(rest) = line.trim_start().strip_prefix("```") {
            let _lang = rest.trim();
            let mut code = Vec::new();
            let mut end = index + 1;
            let mut closed = false;
            while end < lines.len() {
                if lines[end].trim_start().starts_with("```") {
                    closed = true;
                    break;
                }
                code.push(lines[end]);
                end += 1;
            }
            out.extend(render_code_block(&code));
            index = if closed { end + 1 } else { end };
            continue;
        }
        if index + 1 < lines.len() && line.contains('|') && is_table_separator(lines[index + 1]) {
            let header = parse_table_row(line);
            let aligns = parse_table_aligns(lines[index + 1], header.len());
            let mut rows = vec![header];
            let mut end = index + 2;
            while end < lines.len() && lines[end].contains('|') && !is_table_separator(lines[end]) {
                rows.push(parse_table_row(lines[end]));
                end += 1;
            }
            out.extend(render_table(&rows, &aligns, width, base));
            index = end;
            continue;
        }
        out.push(render_markdown_line(line, base));
        index += 1;
    }
    out
}

/// Render code-block lines verbatim with a dim left gutter; no inline markdown.
fn render_code_block(code: &[&str]) -> Vec<String> {
    code.iter()
        .map(|line| format!("{MD_DIM_ON}│ {line}{MD_DIM_OFF}"))
        .collect()
}

fn render_markdown_line(line: &str, base: &str) -> String {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&hashes) {
        let after = &trimmed[hashes..];
        if after.is_empty() || after.starts_with(' ') {
            return format!(
                "{MD_BOLD_ON}{}{MD_BOLD_OFF}",
                render_inline(after.trim_start(), base)
            );
        }
    }
    render_inline(line, base)
}

fn render_inline(text: &str, base: &str) -> String {
    // Code spans first so emphasis markers inside them stay literal. Inline code is
    // recolored, then restored to the line's base color.
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        out.push_str(&render_emphasis(&rest[..start]));
        let after = &rest[start + 1..];
        if let Some(end) = after.find('`') {
            out.push_str(MD_CODE_ON);
            out.push_str(&after[..end]);
            out.push_str(base);
            rest = &after[end + 1..];
        } else {
            out.push('`');
            rest = after;
        }
    }
    out.push_str(&render_emphasis(rest));
    out
}

fn render_emphasis(text: &str) -> String {
    let text = replace_pair(text, "**", MD_BOLD_ON, MD_BOLD_OFF);
    let text = replace_pair(&text, "__", MD_BOLD_ON, MD_BOLD_OFF);
    replace_pair(&text, "*", MD_ITALIC_ON, MD_ITALIC_OFF)
}

/// Replace balanced `delim`-wrapped spans with `on`/`off`; unbalanced markers stay
/// literal.
fn replace_pair(text: &str, delim: &str, on: &str, off: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let Some(start) = rest.find(delim) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start + delim.len()..];
        match after.find(delim) {
            Some(end) if end > 0 => {
                out.push_str(on);
                out.push_str(&after[..end]);
                out.push_str(off);
                rest = &after[end + delim.len()..];
            }
            _ => {
                out.push_str(delim);
                rest = after;
            }
        }
    }
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed.contains('-')
        && trimmed.contains('|')
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '|' | '-' | ':' | ' '))
}

fn parse_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
    trimmed
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn parse_table_aligns(line: &str, columns: usize) -> Vec<MdAlign> {
    let cells = parse_table_row(line);
    (0..columns)
        .map(|index| {
            let cell = cells.get(index).map(|cell| cell.trim()).unwrap_or("");
            match (cell.starts_with(':'), cell.ends_with(':')) {
                (true, true) => MdAlign::Center,
                (false, true) => MdAlign::Right,
                _ => MdAlign::Left,
            }
        })
        .collect()
}

fn render_table(rows: &[Vec<String>], aligns: &[MdAlign], width: usize, base: &str) -> Vec<String> {
    let columns = rows
        .iter()
        .map(|row| row.len())
        .max()
        .unwrap_or(0)
        .max(aligns.len());
    if columns == 0 {
        return Vec::new();
    }

    // Style each cell (header bold) and measure its visible width.
    let styled: Vec<Vec<(String, usize)>> = rows
        .iter()
        .enumerate()
        .map(|(row_index, row)| {
            (0..columns)
                .map(|col| {
                    let raw = row.get(col).map(String::as_str).unwrap_or("");
                    let mut cell = render_inline(raw, base);
                    if row_index == 0 {
                        cell = format!("{MD_BOLD_ON}{cell}{MD_BOLD_OFF}");
                    }
                    let visible = visible_width(&cell);
                    (cell, visible)
                })
                .collect()
        })
        .collect();

    let mut col_widths = vec![1usize; columns];
    for row in &styled {
        for (col, (_, visible)) in row.iter().enumerate() {
            col_widths[col] = col_widths[col].max(*visible).max(1);
        }
    }

    // Keep the table within the terminal width by shrinking the widest columns.
    if width != usize::MAX {
        let overhead = columns * 3 + 1;
        let available = width.saturating_sub(overhead);
        while col_widths.iter().sum::<usize>() > available && col_widths.iter().any(|w| *w > 1) {
            let widest = col_widths
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| **w)
                .map(|(index, _)| index);
            match widest {
                Some(index) => col_widths[index] -= 1,
                None => break,
            }
        }
    }

    let mut out = vec![table_border('┌', '┬', '┐', &col_widths)];
    for (row_index, row) in styled.iter().enumerate() {
        let mut line = String::from("│");
        for (col, (cell, visible)) in row.iter().enumerate() {
            let target = col_widths[col];
            let (content, content_width) = if *visible > target {
                let truncated = truncate_visible(cell, target);
                let measured = visible_width(&truncated);
                (truncated, measured)
            } else {
                (cell.clone(), *visible)
            };
            let pad = target.saturating_sub(content_width);
            let (left, right) = match aligns.get(col).copied().unwrap_or(MdAlign::Left) {
                MdAlign::Left => (0, pad),
                MdAlign::Right => (pad, 0),
                MdAlign::Center => (pad / 2, pad - pad / 2),
            };
            line.push(' ');
            line.push_str(&" ".repeat(left));
            line.push_str(&content);
            line.push_str(&" ".repeat(right));
            line.push_str(" │");
        }
        out.push(line);
        if row_index == 0 {
            out.push(table_border('├', '┼', '┤', &col_widths));
        }
    }
    out.push(table_border('└', '┴', '┘', &col_widths));
    out
}

fn table_border(left: char, middle: char, right: char, col_widths: &[usize]) -> String {
    let mut out = String::new();
    out.push(left);
    for (index, width) in col_widths.iter().enumerate() {
        if index > 0 {
            out.push(middle);
        }
        out.push_str(&"─".repeat(width + 2));
    }
    out.push(right);
    out
}

/// Truncate an already-styled string to `max` visible columns, keeping ANSI
/// sequences, appending an ellipsis, and closing any open styles.
fn truncate_visible(styled: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut visible = 0;
    let mut rest = styled;
    while !rest.is_empty() {
        if let Some((sequence, next)) = split_ansi_sequence(rest) {
            out.push_str(sequence);
            rest = next;
            continue;
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        let ch_width = ch.width().unwrap_or(0);
        if visible + ch_width > budget {
            break;
        }
        out.push(ch);
        visible += ch_width;
        rest = &rest[ch.len_utf8()..];
    }
    out.push('…');
    out.push_str(MD_BOLD_OFF);
    out.push_str(MD_ITALIC_OFF);
    out.push_str(MD_CODE_OFF);
    out
}
