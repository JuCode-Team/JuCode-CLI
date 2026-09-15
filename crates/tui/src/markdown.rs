use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
pub(crate) const MD_BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
pub(crate) const MD_ITALIC: Style = Style::new().add_modifier(Modifier::ITALIC);
/// Inline code reads as light-blue text on the shared background.
pub(crate) const MD_CODE: Style = Style::new().fg(Color::Indexed(117));
pub(crate) const MD_DIM: Style = Style::new().fg(Color::DarkGray);

#[derive(Clone, Copy)]
enum MdAlign {
    Left,
    Right,
    Center,
}

/// Render markdown into styled lines: headings/bold/italic/inline-code become
/// span styles, and pipe tables become aligned box-drawn tables. The line's
/// fallback color comes from its `UiKind` at paint time.
pub(crate) fn render_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
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
            out.extend(render_table(&rows, &aligns, width));
            index = end;
            continue;
        }
        out.push(render_markdown_line(line));
        index += 1;
    }
    out
}

/// Render code-block lines verbatim with a dim left gutter; no inline markdown.
fn render_code_block(code: &[&str]) -> Vec<Line<'static>> {
    code.iter()
        .map(|line| Line::from(Span::styled(format!("│ {line}"), MD_DIM)))
        .collect()
}

fn render_markdown_line(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&hashes) {
        let after = &trimmed[hashes..];
        if after.is_empty() || after.starts_with(' ') {
            return Line::from(
                render_inline(after.trim_start())
                    .into_iter()
                    .map(|span| Span::styled(span.content, span.style.add_modifier(Modifier::BOLD)))
                    .collect::<Vec<_>>(),
            );
        }
    }
    Line::from(render_inline(line))
}

fn render_inline(text: &str) -> Vec<Span<'static>> {
    // Code spans first so emphasis markers inside them stay literal.
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        push_emphasis(&mut spans, &rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('`') {
            spans.push(Span::styled(after[..end].to_string(), MD_CODE));
            rest = &after[end + 1..];
        } else {
            spans.push(Span::raw("`".to_string()));
            rest = after;
        }
    }
    push_emphasis(&mut spans, rest);
    spans
}

/// Apply `**`/`__`/`*` pair emphasis; markers inside styled spans keep their
/// style and gain the emphasis modifier, unbalanced markers stay literal.
fn push_emphasis(spans: &mut Vec<Span<'static>>, text: &str) {
    let mut segments = vec![(text.to_string(), Style::default())];
    for (delim, style) in [("**", MD_BOLD), ("__", MD_BOLD), ("*", MD_ITALIC)] {
        let mut next = Vec::new();
        for (segment, segment_style) in segments {
            for (part, matched) in split_pair(&segment, delim) {
                let style = if matched {
                    segment_style.patch(style)
                } else {
                    segment_style
                };
                next.push((part, style));
            }
        }
        segments = next;
    }
    spans.extend(
        segments
            .into_iter()
            .filter(|(text, _)| !text.is_empty())
            .map(|(text, style)| Span::styled(text, style)),
    );
}

/// Split `text` on balanced `delim` pairs: `(chunk, inside_delimiters)` parts.
fn split_pair(text: &str, delim: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let Some(start) = rest.find(delim) else {
            out.push((rest.to_string(), false));
            return out;
        };
        out.push((rest[..start].to_string(), false));
        let after = &rest[start + delim.len()..];
        match after.find(delim) {
            Some(end) if end > 0 => {
                out.push((after[..end].to_string(), true));
                rest = &after[end + delim.len()..];
            }
            _ => {
                out.push((delim.to_string(), false));
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

fn render_table(rows: &[Vec<String>], aligns: &[MdAlign], width: usize) -> Vec<Line<'static>> {
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
    let styled: Vec<Vec<(Line<'static>, usize)>> = rows
        .iter()
        .enumerate()
        .map(|(row_index, row)| {
            (0..columns)
                .map(|col| {
                    let raw = row.get(col).map(String::as_str).unwrap_or("");
                    let mut cell = render_inline(raw);
                    if row_index == 0 {
                        cell = cell
                            .into_iter()
                            .map(|span| {
                                Span::styled(span.content, span.style.add_modifier(Modifier::BOLD))
                            })
                            .collect();
                    }
                    let cell = Line::from(cell);
                    let visible = cell.width();
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
        let mut spans = vec![Span::raw("│".to_string())];
        for (col, (cell, visible)) in row.iter().enumerate() {
            let target = col_widths[col];
            let (content, content_width) = if *visible > target {
                let truncated = truncate_line(cell, target);
                let measured = truncated.width();
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
            spans.push(Span::raw(format!(" {}", " ".repeat(left))));
            spans.extend(content.spans);
            spans.push(Span::raw(format!("{} │", " ".repeat(right))));
        }
        out.push(Line::from(spans));
        if row_index == 0 {
            out.push(table_border('├', '┼', '┤', &col_widths));
        }
    }
    out.push(table_border('└', '┴', '┘', &col_widths));
    out
}

fn table_border(left: char, middle: char, right: char, col_widths: &[usize]) -> Line<'static> {
    let mut out = String::new();
    out.push(left);
    for (index, width) in col_widths.iter().enumerate() {
        if index > 0 {
            out.push(middle);
        }
        out.push_str(&"─".repeat(width + 2));
    }
    out.push(right);
    Line::from(out)
}

/// Truncate a styled line to `max` visible columns, keeping span styles and
/// appending an ellipsis.
fn truncate_line(line: &Line<'static>, max: usize) -> Line<'static> {
    crate::truncate_line_spans(line, max)
}
