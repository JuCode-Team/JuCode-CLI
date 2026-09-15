use std::borrow::Cow;
use std::path::Path;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::{
    truncate_line_spans, UiKind, UiLine, ACCENT, INPUT_SELECTION, TOOL_OUTPUT_PREVIEW_BYTES,
    TOOL_OUTPUT_PREVIEW_LINES,
};

const STRONG: Style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().fg(Color::DarkGray);
const BASH_COMMAND: Style = Style::new()
    .fg(Color::Rgb(220, 224, 232))
    .bg(Color::Rgb(44, 48, 58));
const ERROR: Style = Style::new().fg(Color::Red);

pub(crate) fn tool_output_preview(name: &str, output: &str, running: bool) -> Vec<UiLine> {
    if is_shell_tool(name) && running {
        return limited_preview(output);
    }
    // Any tool can fail with {"error": "…"}; show the message, not raw JSON.
    if let Some(error) = json_error(output) {
        return vec![UiLine::new(
            UiKind::Tool,
            Line::from(Span::styled(format!("error: {error}"), ERROR)),
        )];
    }
    if let Some(preview) = projected_tool_output(name, output) {
        return preview;
    }
    if is_edit_tool(name) {
        if let Some(diff) = diff_from_tool_output(output) {
            return with_rejected_hunks_line(edit_diff_view(&diff), output);
        }
    }
    if let Some(diff) = diff_from_tool_output(output) {
        return with_rejected_hunks_line(diff_preview(&diff), output);
    }
    limited_preview(output)
}

/// After a partial (hunk-subset) approval the tool's diff already contains
/// only the applied hunks; append a count of the user-rejected ones so the
/// preview does not read as the full requested change.
fn with_rejected_hunks_line(mut preview: Vec<UiLine>, output: &str) -> Vec<UiLine> {
    let rejected = serde_json::from_str::<serde_json::Value>(output)
        .ok()
        .and_then(|value| {
            value
                .get("rejected_hunks")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
        })
        .unwrap_or(0);
    if rejected == 0 {
        return preview;
    }
    preview.push(UiLine::new(
        UiKind::Tool,
        Line::from(Span::styled(
            format!("{rejected} hunk(s) rejected by user (not applied)"),
            DIM,
        )),
    ));
    preview
}

pub(crate) fn format_tool_header(
    name: &str,
    running: bool,
    preview_first: Option<&Line<'static>>,
    width: usize,
) -> Line<'static> {
    let action = tool_action_label(name);
    let mut spans = vec![
        Span::styled("●", ACCENT),
        Span::raw(" "),
        Span::styled(action.to_string(), STRONG),
    ];
    if running {
        spans.push(Span::styled(" running", DIM));
    }

    let Some(first) = preview_first.filter(|line| line.width() > 0) else {
        return Line::from(spans);
    };
    let head_width: usize = spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let available = width.saturating_sub(head_width).saturating_sub(2);
    if available == 0 {
        return Line::from(spans);
    }
    spans.push(Span::raw("  "));
    // The compact preview dimmer than the action; its own styled spans (e.g. the
    // command chip) still win.
    spans.extend(
        truncate_line_spans(first, available)
            .spans
            .into_iter()
            .map(|span| Span::styled(span.content, DIM.patch(span.style))),
    );
    Line::from(spans)
}

fn is_shell_tool(name: &str) -> bool {
    matches!(name, "bash" | "exec_command" | "execute" | "shell_command")
}

fn json_error(output: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(output)
        .ok()?
        .get("error")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn tool_action_label(name: &str) -> Cow<'_, str> {
    match name {
        "bash" | "exec_command" | "execute" | "shell_command" => "Ran".into(),
        "edit" | "str_replace" | "hashline_edit" | "write" | "apply_patch" => "Edit".into(),
        "read" => "Read".into(),
        "ls" => "Listed".into(),
        "rg" | "ripgrep" | "search" | "web_search" => "Searched".into(),
        "write_stdin" => "Sent".into(),
        "outline" => "Outlined".into(),
        "checkpoint" => "Checkpointed".into(),
        "browser_open" => "Opened".into(),
        "web_fetch" => "Fetched".into(),
        // Unknown tools (including MCP) keep their real name; `mcp__a__b` shows
        // as `a/b` instead of a useless "Tool".
        other => match other.strip_prefix("mcp__") {
            Some(rest) => Cow::Owned(rest.replacen("__", "/", 1)),
            None => Cow::Borrowed(other),
        },
    }
}

fn projected_tool_output(name: &str, output: &str) -> Option<Vec<UiLine>> {
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
            let suffix = if truncated { " …" } else { "" };
            Some(vec![plain_preview_line(format!(
                "read {}: {lines} lines from line {offset}{suffix}",
                display_path_name(path)
            ))])
        }
        "ls" => {
            let path = value.get("path").and_then(serde_json::Value::as_str)?;
            Some(vec![plain_preview_line(format!("ls {}", path))])
        }
        _ if is_shell_tool(name) || name == "write_stdin" => Some(project_bash_output(&value)),
        _ => None,
    }
}

fn plain_preview_line(text: String) -> UiLine {
    UiLine::new(UiKind::Tool, Line::from(text))
}

fn project_bash_output(value: &serde_json::Value) -> Vec<UiLine> {
    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
        return vec![UiLine::new(
            UiKind::Tool,
            Line::from(Span::styled(format!("error: {error}"), ERROR)),
        )];
    }
    let command = value
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("bash");
    // A long-running command reported mid-flight carries a session instead of
    // an exit code (write_stdin polls share this shape).
    let status = if value
        .get("running")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        let session = value
            .get("session_id")
            .and_then(serde_json::Value::as_u64)
            .map(|id| id.to_string())
            .unwrap_or_else(|| "?".to_string());
        format!("session {session} running")
    } else {
        let exit_code = value
            .get("exit_code")
            .and_then(serde_json::Value::as_i64)
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let timed_out = value
            .get("timed_out")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        format!(
            "exit {exit_code}{}",
            if timed_out { ", timed out" } else { "" }
        )
    };
    let stdout = value
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let stderr = value
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mut lines = vec![UiLine::new(
        UiKind::Tool,
        Line::from(vec![
            Span::styled(command.to_string(), BASH_COMMAND),
            Span::styled(format!("  {status}"), DIM),
        ]),
    )];
    for (label, body) in [("stdout:", stdout), ("stderr:", stderr)] {
        if !body.trim().is_empty() {
            lines.push(plain_preview_line(label.to_string()));
            lines.extend(
                tail_lines(body, 8)
                    .lines()
                    .map(|line| plain_preview_line(line.to_string())),
            );
        }
    }
    lines
}

fn display_path_name(path: &str) -> String {
    let path = path.trim_end_matches(['/', '\\']);
    path.rsplit(['/', '\\'])
        .find(|name| !name.is_empty())
        .or_else(|| Path::new(path).file_name().and_then(|name| name.to_str()))
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

fn is_edit_tool(name: &str) -> bool {
    matches!(name, "edit" | "str_replace" | "hashline_edit" | "write")
}

fn render_full_diff(diff: &str) -> Vec<UiLine> {
    render_intra_line_diff(&diff.lines().collect::<Vec<_>>())
}

fn edit_diff_view(diff: &str) -> Vec<UiLine> {
    let Some(parsed) = parse_unified_diff(diff) else {
        return render_full_diff(diff);
    };
    let mut lines = vec![plain_preview_line(format!(
        "{} (+{} -{})",
        parsed.path, parsed.additions, parsed.removals
    ))];
    lines.extend(parsed.lines.iter().map(EditDiffLine::render));
    lines
}

struct ParsedEditDiff {
    path: String,
    additions: usize,
    removals: usize,
    lines: Vec<EditDiffLine>,
}

enum EditDiffLine {
    Context { line: Option<usize>, text: String },
    Add { line: usize, text: String },
    Remove { line: usize, text: String },
    Gap,
}

impl EditDiffLine {
    fn render(&self) -> UiLine {
        let (kind, text) = match self {
            EditDiffLine::Context { line, text } => match line {
                Some(line) => (UiKind::Tool, format!("{line:>6}    {text}")),
                None => (UiKind::Tool, format!("{:>6}    {text}", "")),
            },
            EditDiffLine::Add { line, text } => (UiKind::DiffAdd, format!("{line:>6} +  {text}")),
            EditDiffLine::Remove { line, text } => {
                (UiKind::DiffRemove, format!("{line:>6} -  {text}"))
            }
            EditDiffLine::Gap => (UiKind::Tool, format!("{:>6}    …", "")),
        };
        UiLine::new(kind, Line::from(text))
    }
}

fn parse_unified_diff(diff: &str) -> Option<ParsedEditDiff> {
    let mut path = None;
    let mut lines = Vec::new();
    let mut old_line = 0usize;
    let mut new_line = 0usize;
    let mut in_hunk = false;
    let mut additions = 0usize;
    let mut removals = 0usize;

    for raw in diff.lines() {
        if raw.starts_with("diff --git ") {
            path.get_or_insert_with(|| diff_file_path(raw));
            if in_hunk {
                lines.push(EditDiffLine::Gap);
                in_hunk = false;
            }
            continue;
        }
        if raw.starts_with("--- ") || raw.starts_with("+++ ") || raw.starts_with("index ") {
            continue;
        }
        if raw.starts_with("@@") {
            let (old_start, new_start) = parse_hunk_header(raw)?;
            if in_hunk {
                lines.push(EditDiffLine::Gap);
            }
            old_line = old_start;
            new_line = new_start;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(text) = raw.strip_prefix('+') {
            additions += 1;
            lines.push(EditDiffLine::Add {
                line: new_line,
                text: text.to_string(),
            });
            new_line += 1;
        } else if let Some(text) = raw.strip_prefix('-') {
            removals += 1;
            lines.push(EditDiffLine::Remove {
                line: old_line,
                text: text.to_string(),
            });
            old_line += 1;
        } else if let Some(text) = raw.strip_prefix(' ') {
            lines.push(EditDiffLine::Context {
                line: Some(new_line),
                text: text.to_string(),
            });
            old_line += 1;
            new_line += 1;
        } else if raw == r"\ No newline at end of file" {
            lines.push(EditDiffLine::Context {
                line: None,
                text: raw.to_string(),
            });
        }
    }

    Some(ParsedEditDiff {
        path: path?,
        additions,
        removals,
        lines,
    })
}

fn diff_file_path(line: &str) -> String {
    let label = diff_file_label(line);
    label
        .strip_prefix("diff ")
        .unwrap_or(label.as_str())
        .to_string()
}

fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "@@" {
        return None;
    }
    let old = parts.next()?;
    let new = parts.next()?;
    if !old.starts_with('-') || !new.starts_with('+') {
        return None;
    }
    Some((parse_hunk_start(&old[1..])?, parse_hunk_start(&new[1..])?))
}

fn parse_hunk_start(value: &str) -> Option<usize> {
    value
        .split_once(',')
        .map(|(start, _)| start)
        .unwrap_or(value)
        .parse()
        .ok()
}

fn diff_preview(diff: &str) -> Vec<UiLine> {
    let mut preview: Vec<UiLine> = Vec::new();
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
        truncated |= !push_preview_text(&mut preview, &mut preview_bytes, label);
    }
    truncated |= !push_preview_text(&mut preview, &mut preview_bytes, header);

    let line_budget = TOOL_OUTPUT_PREVIEW_LINES.saturating_sub(preview.len());
    let selected = balanced_diff_lines(&change_lines, line_budget);
    truncated |= selected.len() < change_lines.len();
    for line in render_intra_line_diff(&selected) {
        truncated |= !push_preview_line(&mut preview, &mut preview_bytes, line);
    }

    if truncated {
        preview.push(plain_preview_line("…".to_string()));
    }

    preview
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

fn render_intra_line_diff(lines: &[&str]) -> Vec<UiLine> {
    let mut rendered = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        if !is_diff_change_line(lines[index]) {
            rendered.push(UiLine::new(
                diff_line_kind(lines[index]),
                Line::from(lines[index].to_string()),
            ));
            index += 1;
            continue;
        }

        let removed_start = index;
        while index < lines.len()
            && is_diff_change_line(lines[index])
            && lines[index].starts_with('-')
        {
            index += 1;
        }
        let added_start = index;
        while index < lines.len()
            && is_diff_change_line(lines[index])
            && lines[index].starts_with('+')
        {
            index += 1;
        }

        let removed = &lines[removed_start..added_start];
        let added = &lines[added_start..index];
        if removed.len() == 1 && added.len() == 1 {
            let (old_line, new_line) = render_intra_line_pair(removed[0], added[0]);
            rendered.push(old_line);
            rendered.push(new_line);
        } else {
            rendered.extend(
                removed
                    .iter()
                    .map(|line| UiLine::new(UiKind::DiffRemove, Line::from((*line).to_string()))),
            );
            rendered.extend(
                added
                    .iter()
                    .map(|line| UiLine::new(UiKind::DiffAdd, Line::from((*line).to_string()))),
            );
        }
    }

    rendered
}

fn render_intra_line_pair(old_line: &str, new_line: &str) -> (UiLine, UiLine) {
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
        UiLine::new(
            UiKind::DiffRemove,
            diff_change_line('-', old_content, prefix, old_suffix),
        ),
        UiLine::new(
            UiKind::DiffAdd,
            diff_change_line('+', new_content, prefix, new_suffix),
        ),
    )
}

fn diff_change_line(prefix: char, text: &str, start: usize, end: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(prefix.to_string())];
    if start >= end {
        spans.push(Span::raw(text.to_string()));
        return Line::from(spans);
    }
    let before: String = text.chars().take(start).collect();
    let changed: String = text.chars().skip(start).take(end - start).collect();
    let after: String = text.chars().skip(end).collect();
    if !before.is_empty() {
        spans.push(Span::raw(before));
    }
    spans.push(Span::styled(changed, INPUT_SELECTION));
    if !after.is_empty() {
        spans.push(Span::raw(after));
    }
    Line::from(spans)
}

fn push_preview_line(preview: &mut Vec<UiLine>, preview_bytes: &mut usize, line: UiLine) -> bool {
    if preview.len() >= TOOL_OUTPUT_PREVIEW_LINES {
        return false;
    }

    let line_len: usize = line.line.spans.iter().map(|span| span.content.len()).sum();
    let next_bytes = preview_bytes
        .saturating_add(line_len)
        .saturating_add(usize::from(!preview.is_empty()));
    if next_bytes > TOOL_OUTPUT_PREVIEW_BYTES {
        return false;
    }

    preview.push(line);
    *preview_bytes = next_bytes;
    true
}

fn push_preview_text(preview: &mut Vec<UiLine>, preview_bytes: &mut usize, line: &str) -> bool {
    push_preview_line(
        preview,
        preview_bytes,
        UiLine::new(diff_line_kind(line), Line::from(line.to_string())),
    )
}

fn limited_preview(output: &str) -> Vec<UiLine> {
    let mut preview = Vec::new();
    let mut preview_bytes = 0usize;
    let mut truncated = false;

    for line in output.lines() {
        if !push_preview_text(&mut preview, &mut preview_bytes, line) {
            truncated = true;
            break;
        }
    }

    if output.is_empty() {
        preview.push(plain_preview_line("(empty output)".to_string()));
    } else if output.lines().count() > preview.len() {
        truncated = true;
    }

    if truncated {
        preview.push(plain_preview_line("…".to_string()));
    }

    preview
}

pub(crate) fn diff_line_kind(line: &str) -> UiKind {
    if let Some(kind) = edit_view_line_kind(line) {
        return kind;
    }
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

fn edit_view_line_kind(line: &str) -> Option<UiKind> {
    let trimmed = line.trim_start();
    let digits = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = &trimmed[digits..];
    if rest.starts_with(" +") {
        Some(UiKind::DiffAdd)
    } else if rest.starts_with(" -") {
        Some(UiKind::DiffRemove)
    } else {
        None
    }
}
