//! Non-image files attached to a user message. They are not inlined: each
//! is announced to the model as one line naming its path, apart from the
//! user's text, and the model opens it with its own tools. ACP agents get a
//! `resource_link` block instead (`file_uri`).

use std::path::Path;

const PREFIX: &str = "[Attached file \"";
const MIDDLE: &str = "\" is saved at: ";

/// The last component of `path` (the path itself when it has none).
pub fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// `[Attached file "<name>" is saved at: <path>]`.
pub fn file_line(path: &str) -> String {
    format!("{PREFIX}{}{MIDDLE}{path}]", file_name(path))
}

/// The path a `file_line` names. The name must be the path's own, so a name
/// holding quotes or the separator still parses.
pub fn line_path(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(PREFIX)?.strip_suffix(']')?;
    rest.match_indices(MIDDLE).find_map(|(at, _)| {
        let path = &rest[at + MIDDLE.len()..];
        (!path.is_empty() && file_name(path) == rest[..at]).then(|| path.to_string())
    })
}

/// `text`, then a blank line and one `file_line` per file.
pub fn with_files(text: &str, files: &[String]) -> String {
    if files.is_empty() {
        return text.to_string();
    }
    let lines: Vec<String> = files.iter().map(|path| file_line(path)).collect();
    if text.is_empty() {
        lines.join("\n")
    } else {
        format!("{text}\n\n{}", lines.join("\n"))
    }
}

/// The inverse of `with_files`: the text without its trailing file lines
/// (and the blank line before them), and the files they name.
pub fn split_files(text: &str) -> (String, Vec<String>) {
    let mut lines: Vec<&str> = text.trim_end().split('\n').collect();
    let mut files = Vec::new();
    while let Some(path) = lines.last().and_then(|line| line_path(line)) {
        files.push(path);
        lines.pop();
    }
    if files.is_empty() {
        return (text.to_string(), files);
    }
    files.reverse();
    if lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    (lines.join("\n"), files)
}

/// `path` as a `file://` URI, percent-encoded.
pub fn file_uri(path: &str) -> String {
    let path = if cfg!(windows) {
        format!("/{}", path.replace('\\', "/"))
    } else {
        path.to_string()
    };
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// The path of a local `file://` URI.
pub fn uri_path(uri: &str) -> Option<String> {
    let encoded = uri.strip_prefix("file://")?;
    if !encoded.starts_with('/') {
        return None;
    }
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(out).ok()?;
    Some(if cfg!(windows) {
        path.trim_start_matches('/').replace('/', "\\")
    } else {
        path
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_line_names_the_file_and_its_path() {
        assert_eq!(
            file_line("/tmp/report.pdf"),
            "[Attached file \"report.pdf\" is saved at: /tmp/report.pdf]"
        );
    }

    #[test]
    fn split_files_round_trips_odd_names() {
        let files = vec![
            "/tmp/my report.pdf".to_string(),
            "/tmp/say \"hi\".txt".to_string(),
            "/tmp/a\" is saved at: b.md".to_string(),
            "/tmp/资料/会议 记录.docx".to_string(),
            "/tmp/clip].mp4".to_string(),
        ];
        let text = with_files("look at these\n\nplease", &files);
        assert_eq!(
            split_files(&text),
            ("look at these\n\nplease".to_string(), files.clone())
        );
        assert_eq!(split_files(&with_files("", &files)), (String::new(), files));
    }

    #[test]
    fn split_files_takes_lines_joined_without_a_blank_line() {
        let text = format!("hi\n{}\n{}\n", file_line("/a/x.pdf"), file_line("/b/y.zip"));
        assert_eq!(
            split_files(&text),
            (
                "hi".to_string(),
                vec!["/a/x.pdf".to_string(), "/b/y.zip".to_string()]
            )
        );
    }

    #[test]
    fn split_files_keeps_text_without_file_lines() {
        for text in [
            "",
            "plain\n",
            "[Attached file \"x.pdf\" is saved at: /a/other.pdf]",
        ] {
            assert_eq!(split_files(text), (text.to_string(), Vec::new()));
        }
        // Only trailing lines are attachments.
        let text = format!("{}\nthen text", file_line("/a/x.pdf"));
        assert_eq!(split_files(&text), (text.clone(), Vec::new()));
    }

    #[test]
    fn with_files_without_files_is_the_text() {
        assert_eq!(with_files("hi\n", &[]), "hi\n");
    }

    #[cfg(not(windows))]
    #[test]
    fn file_uri_round_trips() {
        let path = "/tmp/资料/my report #1%.pdf";
        let uri = file_uri(path);
        assert_eq!(
            uri,
            "file:///tmp/%E8%B5%84%E6%96%99/my%20report%20%231%25.pdf"
        );
        assert_eq!(uri_path(&uri).as_deref(), Some(path));
        assert_eq!(uri_path("https://x/y"), None);
    }
}
