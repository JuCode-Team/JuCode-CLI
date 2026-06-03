use std::path::Path;

use crate::{
    truncate_with_ellipsis, visible_width, UiKind, INVERSE_OFF, INVERSE_ON,
    TOOL_OUTPUT_PREVIEW_BYTES, TOOL_OUTPUT_PREVIEW_LINES,
};

pub(crate) fn tool_output_preview(name: &str, output: &str, running: bool) -> String {
    if name == "bash" && running {
        return limited_preview(output);
    }
    if let Some(preview) = projected_tool_output(name, output) {
        return preview;
    }
    if let Some(diff) = diff_from_tool_output(output) {
        return diff_preview(&diff);
    }
    limited_preview(output)
}

pub(crate) fn compact_tool_preview(name: &str, output: &str, running: bool) -> String {
    let preview = tool_output_preview(name, output, running);
    preview.lines().next().unwrap_or_default().to_string()
}

pub(crate) fn format_tool_header(name: &str, running: bool, preview: &str, width: usize) -> String {
    let suffix = if running { " running" } else { "" };
    let prefix = format!("* tool:{name}{suffix}");
    let compact = preview.lines().next().unwrap_or_default().to_string();
    if compact.is_empty() {
        return prefix;
    }

    let separator = "  ";
    let available = width
        .saturating_sub(visible_width(&prefix))
        .saturating_sub(visible_width(separator));
    if available == 0 {
        return prefix;
    }

    let compact_width = visible_width(&compact);
    if visible_width(&prefix)
        .saturating_add(visible_width(separator))
        .saturating_add(compact_width)
        <= width
    {
        return format!("{prefix}{separator}{compact}");
    }

    let truncated = truncate_with_ellipsis(&compact, available);
    if truncated.is_empty() {
        prefix
    } else {
        format!("{prefix}{separator}{truncated}")
    }
}

fn projected_tool_output(name: &str, output: &str) -> Option<String> {
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
            Some(format!(
                "read {}: {lines} lines from line {offset}{suffix}",
                display_path_name(path)
            ))
        }
        "ls" => {
            let path = value.get("path").and_then(serde_json::Value::as_str)?;
            Some(format!("ls {}", path))
        }
        "bash" => Some(project_bash_output(&value)),
        _ => None,
    }
}

fn project_bash_output(value: &serde_json::Value) -> String {
    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
        return format!("error: {error}");
    }
    let command = value
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("bash");
    let exit_code = value
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let timed_out = value
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let stdout = value
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let stderr = value
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mut lines = vec![format!(
        "bash: {command} (exit {exit_code}{})",
        if timed_out { ", timed out" } else { "" }
    )];
    if !stdout.trim().is_empty() {
        lines.push(format!("stdout:\n{}", tail_lines(stdout, 8)));
    }
    if !stderr.trim().is_empty() {
        lines.push(format!("stderr:\n{}", tail_lines(stderr, 8)));
    }
    lines.join("\n")
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

fn diff_preview(diff: &str) -> String {
    let mut preview = Vec::new();
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
        truncated |= !push_preview_line(&mut preview, &mut preview_bytes, label);
    }
    truncated |= !push_preview_line(&mut preview, &mut preview_bytes, header);

    let line_budget = TOOL_OUTPUT_PREVIEW_LINES.saturating_sub(preview.len());
    let selected = balanced_diff_lines(&change_lines, line_budget);
    truncated |= selected.len() < change_lines.len();
    for line in render_intra_line_diff(&selected) {
        truncated |= !push_preview_line(&mut preview, &mut preview_bytes, &line);
    }

    if truncated {
        preview.push("…".to_string());
    }

    preview.join("\n")
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

fn render_intra_line_diff(lines: &[&str]) -> Vec<String> {
    let mut rendered = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        if !lines[index].starts_with('-') {
            rendered.push(lines[index].to_string());
            index += 1;
            continue;
        }

        let removed_start = index;
        while index < lines.len() && lines[index].starts_with('-') {
            index += 1;
        }
        let added_start = index;
        while index < lines.len() && lines[index].starts_with('+') {
            index += 1;
        }

        let removed = &lines[removed_start..added_start];
        let added = &lines[added_start..index];
        if removed.len() == 1 && added.len() == 1 {
            let (old_line, new_line) = render_intra_line_pair(removed[0], added[0]);
            rendered.push(old_line);
            rendered.push(new_line);
        } else {
            rendered.extend(removed.iter().map(|line| (*line).to_string()));
            rendered.extend(added.iter().map(|line| (*line).to_string()));
        }
    }

    rendered
}

fn render_intra_line_pair(old_line: &str, new_line: &str) -> (String, String) {
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
        format!(
            "-{}",
            highlight_changed_range(old_content, prefix, old_suffix)
        ),
        format!(
            "+{}",
            highlight_changed_range(new_content, prefix, new_suffix)
        ),
    )
}

fn highlight_changed_range(text: &str, start: usize, end: usize) -> String {
    if start >= end {
        return text.to_string();
    }

    let mut output = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index == start {
            output.push_str(INVERSE_ON);
        }
        output.push(ch);
        if index + 1 == end {
            output.push_str(INVERSE_OFF);
        }
    }
    output
}

fn push_preview_line(preview: &mut Vec<String>, preview_bytes: &mut usize, line: &str) -> bool {
    if preview.len() >= TOOL_OUTPUT_PREVIEW_LINES {
        return false;
    }

    let next_bytes = preview_bytes
        .saturating_add(line.len())
        .saturating_add(usize::from(!preview.is_empty()));
    if next_bytes > TOOL_OUTPUT_PREVIEW_BYTES {
        return false;
    }

    preview.push(line.to_string());
    *preview_bytes = next_bytes;
    true
}

fn limited_preview(output: &str) -> String {
    let mut preview = String::new();
    let mut lines = 0usize;
    let mut truncated = false;

    for line in output.lines() {
        if lines >= TOOL_OUTPUT_PREVIEW_LINES
            || preview.len().saturating_add(line.len()) > TOOL_OUTPUT_PREVIEW_BYTES
        {
            truncated = true;
            break;
        }
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str(line);
        lines += 1;
    }

    if output.is_empty() {
        preview.push_str("(empty output)");
    } else if output.lines().count() > lines {
        truncated = true;
    }

    if truncated {
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push('…');
    }

    preview
}

pub(crate) fn diff_line_kind(line: &str) -> UiKind {
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
