//! `@` file mentions: typing `@` followed by a query in the input opens a
//! fuzzy file picker fed by `rg --files` (gitignore-aware) when available,
//! falling back to `git ls-files` and then a capped directory walk.

use std::{path::Path, process::Command};

/// Cap on the indexed file list; beyond this the picker still works but only
/// over the first entries the lister produced.
const MAX_INDEXED_FILES: usize = 5_000;
/// Rows shown in the picker.
pub(crate) const MAX_MENTION_MATCHES: usize = 8;

/// If the text before the cursor ends in an `@token`, returns
/// `(token_char_len, query)` where `token_char_len` counts the chars from the
/// `@` (inclusive) to the cursor. The `@` must start its whitespace-delimited
/// word so emails like `a@b` do not trigger the picker.
pub(crate) fn mention_token(text_before_cursor: &str) -> Option<(usize, String)> {
    let token_start = text_before_cursor
        .rfind(char::is_whitespace)
        .map(|index| {
            index
                + text_before_cursor[index..]
                    .chars()
                    .next()
                    .unwrap()
                    .len_utf8()
        })
        .unwrap_or(0);
    let token = &text_before_cursor[token_start..];
    let query = token.strip_prefix('@')?;
    Some((token.chars().count(), query.to_string()))
}

/// Case-insensitive fuzzy filter: every query char must appear in order.
/// Ranks basename prefix matches first, then basename substrings, then path
/// substrings, then plain subsequences; ties break on path length.
pub(crate) fn fuzzy_filter(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let query_lower = query.to_lowercase();
    let mut scored = files
        .iter()
        .filter_map(|path| {
            let score = fuzzy_score(path, &query_lower)?;
            Some((score, path.len(), path.clone()))
        })
        .collect::<Vec<_>>();
    scored.sort();
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, path)| path)
        .collect()
}

fn fuzzy_score(path: &str, query_lower: &str) -> Option<u8> {
    if query_lower.is_empty() {
        return Some(3);
    }
    let path_lower = path.to_lowercase();
    let basename = path_lower.rsplit(['/', '\\']).next().unwrap_or(&path_lower);
    if basename.starts_with(query_lower) {
        return Some(0);
    }
    if basename.contains(query_lower) {
        return Some(1);
    }
    if path_lower.contains(query_lower) {
        return Some(2);
    }
    if is_subsequence(&path_lower, query_lower) {
        return Some(3);
    }
    None
}

fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|needed| chars.any(|ch| ch == needed))
}

/// Project file list for the picker. Sources in order: `rg --files`
/// (gitignore-aware), `git ls-files` (tracked + untracked-but-not-ignored),
/// then a walk that skips dot-directories and common build outputs.
pub(crate) fn list_project_files(cwd: &Path) -> Vec<String> {
    if let Some(files) = command_file_list(cwd, "rg", &["--files"]) {
        return files;
    }
    if let Some(files) = command_file_list(
        cwd,
        "git",
        &["ls-files", "--cached", "--others", "--exclude-standard"],
    ) {
        return files;
    }
    let mut files = Vec::new();
    walk_files(cwd, cwd, &mut files);
    files
}

fn command_file_list(cwd: &Path, program: &str, args: &[&str]) -> Option<Vec<String>> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .take(MAX_INDEXED_FILES)
        .map(|line| line.replace('\\', "/"))
        .collect::<Vec<_>>();
    if files.is_empty() {
        None
    } else {
        Some(files)
    }
}

fn walk_files(root: &Path, dir: &Path, files: &mut Vec<String>) {
    if files.len() >= MAX_INDEXED_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_INDEXED_FILES {
            return;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if name.starts_with('.') || matches!(name, "target" | "node_modules") {
                continue;
            }
            walk_files(root, &path, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.display().to_string().replace('\\', "/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn mention_token_finds_the_at_word_before_the_cursor() {
        assert_eq!(mention_token("@"), Some((1, String::new())));
        assert_eq!(mention_token("@ma"), Some((3, "ma".to_string())));
        assert_eq!(
            mention_token("look at @src/li"),
            Some((7, "src/li".to_string()))
        );
        assert_eq!(mention_token("fix this"), None);
        // `@` mid-word (email-like) is not a mention
        assert_eq!(mention_token("mail me a@b"), None);
        assert_eq!(mention_token(""), None);
    }

    #[test]
    fn fuzzy_filter_ranks_basename_matches_first() {
        let files = files(&[
            "docs/main-notes.md",
            "src/main.rs",
            "src/domain/chain.rs",
            "README.md",
        ]);
        let matches = fuzzy_filter(&files, "main", 8);
        assert_eq!(matches[0], "src/main.rs");
        assert_eq!(matches[1], "docs/main-notes.md");
        assert!(!matches.contains(&"README.md".to_string()));
    }

    #[test]
    fn fuzzy_filter_accepts_subsequences_and_is_case_insensitive() {
        let files = files(&["crates/tui/src/lib.rs", "Cargo.toml"]);
        let matches = fuzzy_filter(&files, "TSL", 8);
        assert_eq!(matches, vec!["crates/tui/src/lib.rs".to_string()]);
        assert!(fuzzy_filter(&files, "zzz", 8).is_empty());
    }

    #[test]
    fn empty_query_lists_files() {
        let files = files(&["b.rs", "a.rs"]);
        assert_eq!(fuzzy_filter(&files, "", 1).len(), 1);
    }
}
