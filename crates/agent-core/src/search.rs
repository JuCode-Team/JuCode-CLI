//! The `ripgrep` tool's search when `rg` is not installed (a stock Windows has
//! none). It prints what `rg --line-number --no-heading` prints for the options
//! the tool takes, so the model reads either the same way. Like rg's defaults it
//! skips hidden entries and binary files; for want of .gitignore parsing it also
//! skips the usual ignored directories.

use regex::{Regex, RegexBuilder};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Directories a .gitignore nearly always lists; rg would skip them there.
const SKIPPED_DIRS: [&str; 4] = ["node_modules", "target", "__pycache__", "venv"];
/// A file whose first 8 KB hold a NUL byte is binary (rg's heuristic).
const BINARY_PROBE_BYTES: usize = 8 * 1024;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

pub(crate) struct Options<'a> {
    pub ignore_case: bool,
    pub literal: bool,
    pub context: usize,
    pub glob: Option<&'a str>,
    pub timeout: Duration,
}

pub(crate) struct Output {
    /// 0: matches; 1: none; 2: a bad pattern or glob (rg's exit codes).
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
}

pub(crate) fn search(pattern: &str, root: &Path, options: &Options) -> Output {
    let source = if options.literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let matcher = match RegexBuilder::new(&source)
        .case_insensitive(options.ignore_case)
        .build()
    {
        Ok(matcher) => matcher,
        Err(error) => return failed(format!("regex parse error: {error}")),
    };
    let glob = match options.glob.map(Glob::new).transpose() {
        Ok(glob) => glob,
        Err(error) => return failed(error),
    };
    let started = Instant::now();
    let mut out = Output {
        exit_code: 1,
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
        timed_out: false,
    };
    // A single file named on the command line is printed without its path.
    let single_file = root.is_file();
    for file in files(root, glob.as_ref()) {
        if started.elapsed() > options.timeout {
            out.timed_out = true;
            break;
        }
        if out.stdout.len() > MAX_OUTPUT_BYTES {
            out.truncated = true;
            break;
        }
        let Ok(bytes) = fs::read(&file) else { continue };
        if bytes[..bytes.len().min(BINARY_PROBE_BYTES)].contains(&0) {
            continue;
        }
        let prefix = (!single_file).then(|| file.display().to_string());
        if print_matches(
            &String::from_utf8_lossy(&bytes),
            &matcher,
            options.context,
            prefix.as_deref(),
            &mut out.stdout,
        ) {
            out.exit_code = 0;
        }
    }
    out
}

fn failed(stderr: String) -> Output {
    Output {
        exit_code: 2,
        stdout: String::new(),
        stderr,
        truncated: false,
        timed_out: false,
    }
}

/// Files under `root` (or `root` itself), in name order, that pass `glob`.
fn files(root: &Path, glob: Option<&Glob>) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        let mut subdirs = Vec::new();
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if !SKIPPED_DIRS.contains(&name.as_str()) {
                    subdirs.push(path);
                }
            } else if kind.is_file() {
                let relative = path.strip_prefix(root).unwrap_or(&path);
                if glob.is_none_or(|glob| glob.allows(relative)) {
                    found.push(path);
                }
            }
        }
        // Depth first, in name order, as rg prints a directory.
        pending.extend(subdirs.into_iter().rev());
    }
    found
}

/// Appends `text`'s matching lines, with `context` lines around them, as rg
/// prints them: `path:line:text` for a match, `path-line-text` for context, and
/// `--` between groups that do not touch. True when anything matched.
fn print_matches(
    text: &str,
    matcher: &Regex,
    context: usize,
    path: Option<&str>,
    out: &mut String,
) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let hits: Vec<usize> = (0..lines.len())
        .filter(|&i| matcher.is_match(lines[i]))
        .collect();
    if hits.is_empty() {
        return false;
    }
    let mut last_printed: Option<usize> = None;
    let mut printed_any = !out.is_empty();
    for &hit in &hits {
        let from = hit.saturating_sub(context);
        let to = (hit + context).min(lines.len() - 1);
        let start = match last_printed {
            Some(last) if last + 1 >= from => last + 1,
            _ => {
                if context > 0 && printed_any {
                    out.push_str("--\n");
                }
                from
            }
        };
        for (i, line) in lines.iter().enumerate().take(to + 1).skip(start) {
            let separator = if hits.binary_search(&i).is_ok() {
                ':'
            } else {
                '-'
            };
            if let Some(path) = path {
                out.push_str(path);
                out.push(separator);
            }
            out.push_str(&(i + 1).to_string());
            out.push(separator);
            out.push_str(line);
            out.push('\n');
        }
        last_printed = Some(to.max(last_printed.unwrap_or(0)));
        printed_any = true;
    }
    true
}

/// One `--glob`: `*.rs`, `src/**/*.ts`, `*.{js,ts}`, or `!` to exclude. A glob
/// without `/` matches the file name at any depth, as in rg.
struct Glob {
    matcher: Regex,
    whole_path: bool,
    exclude: bool,
}

impl Glob {
    fn new(glob: &str) -> Result<Self, String> {
        let (exclude, glob) = match glob.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, glob),
        };
        let whole_path = glob.contains('/');
        let mut source = String::from("^");
        let mut chars = glob.trim_start_matches("./").chars().peekable();
        let mut in_braces = false;
        while let Some(c) = chars.next() {
            match c {
                '*' if chars.peek() == Some(&'*') => {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        source.push_str("(?:.*/)?");
                    } else {
                        source.push_str(".*");
                    }
                }
                '*' => source.push_str("[^/]*"),
                '?' => source.push_str("[^/]"),
                '{' => {
                    in_braces = true;
                    source.push_str("(?:");
                }
                '}' if in_braces => {
                    in_braces = false;
                    source.push(')');
                }
                ',' if in_braces => source.push('|'),
                '[' => {
                    source.push('[');
                    for c in chars.by_ref() {
                        source.push(c);
                        if c == ']' {
                            break;
                        }
                    }
                }
                c => source.push_str(&regex::escape(&c.to_string())),
            }
        }
        source.push('$');
        let matcher =
            Regex::new(&source).map_err(|error| format!("invalid glob {glob}: {error}"))?;
        Ok(Self {
            matcher,
            whole_path,
            exclude,
        })
    }

    fn allows(&self, relative: &Path) -> bool {
        let subject = if self.whole_path {
            relative.to_string_lossy().replace('\\', "/")
        } else {
            relative
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        };
        self.matcher.is_match(&subject) != self.exclude
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Options<'static> {
        Options {
            ignore_case: false,
            literal: false,
            context: 0,
            glob: None,
            timeout: Duration::from_secs(30),
        }
    }

    fn tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jucode-search-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (path, text) in [
            ("src/a.rs", "fn main() {\n    let needle = 1;\n}\n"),
            ("src/b.ts", "const Needle = 2;\r\n"),
            ("docs/c.md", "one\ntwo\nneedle\nthree\nfour\nfive\nneedle\n"),
            (".git/config", "needle\n"),
            ("node_modules/x/index.js", "needle\n"),
        ] {
            let file = dir.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }
        fs::write(dir.join("blob.bin"), b"needle\0\x01").unwrap();
        dir
    }

    #[test]
    fn prints_like_rg_and_skips_hidden_ignored_and_binary_files() {
        let dir = tree("rg-like");
        let out = search("needle", &dir, &options());
        let path = |rel: &str| dir.join(rel).display().to_string();
        assert_eq!(out.exit_code, 0);
        assert_eq!(
            out.stdout,
            format!(
                "{}:3:needle\n{}:7:needle\n{}:2:    let needle = 1;\n",
                path("docs/c.md"),
                path("docs/c.md"),
                path("src/a.rs")
            )
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn honours_case_literal_glob_and_context() {
        let dir = tree("rg-options");
        let ignore_case = Options {
            ignore_case: true,
            glob: Some("*.ts"),
            ..options()
        };
        assert!(search("needle", &dir, &ignore_case)
            .stdout
            .ends_with("b.ts:1:const Needle = 2;\n"));
        let excluded = Options {
            glob: Some("!*.md"),
            ..options()
        };
        assert!(!search("needle", &dir, &excluded).stdout.contains("c.md"));
        let in_src = Options {
            glob: Some("src/**/*.{rs,ts}"),
            ..options()
        };
        assert!(search("needle", &dir, &in_src).stdout.contains("a.rs:2:"));
        let literal = Options {
            literal: true,
            ..options()
        };
        assert_eq!(search("main()", &dir, &literal).exit_code, 0);
        assert_eq!(search("a(", &dir, &options()).exit_code, 2);

        let file = dir.join("docs/c.md");
        let around = Options {
            context: 1,
            ..options()
        };
        assert_eq!(
            search("needle", &file, &around).stdout,
            "2-two\n3:needle\n4-three\n--\n6-five\n7:needle\n"
        );
        assert_eq!(search("absent", &dir, &options()).exit_code, 1);
        let _ = fs::remove_dir_all(dir);
    }
}
