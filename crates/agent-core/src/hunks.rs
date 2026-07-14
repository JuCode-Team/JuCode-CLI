//! Hunk-level breakdown of gated edit tool calls for selective approval.
//!
//! When an edit tool call is gated on user approval, the engine computes the
//! would-be change as a list of hunks *before* anything is applied. The user
//! may then approve the whole call (as before) or only a subset of hunk ids;
//! the call is rewritten so that only the approved hunks are applied and the
//! rest are reported back to the model as rejected.
//!
//! Per-tool hunk semantics:
//! - `apply_patch`: standard `@@` hunks per file section, ids `f<file>h<n>`.
//! - `write`: always exactly one hunk (`f0h1`) — a full-file write is
//!   all-or-nothing.
//! - `str_replace`/`edit`: one replacement in the `edits` array = one hunk.
//! - `hashline_edit`: one edit (line-range) in the `edits` array = one hunk.

use crate::tools;
use serde_json::{json, Value};
use std::{collections::HashSet, fs, path::Path};

/// One selectable hunk of a gated edit tool call, for approval UIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkView {
    /// Stable id `f<file-index>h<hunk-number>` (file index 0-based in patch
    /// order, hunk number 1-based within the file).
    pub id: String,
    /// Path label of the file this hunk touches.
    pub file: String,
    /// The `@@` hunk header line.
    pub header: String,
    /// Raw unified-diff body lines for display.
    pub lines: Vec<String>,
}

/// An edit call rewritten to apply only the approved hunks.
#[derive(Debug)]
pub struct FilteredEdit {
    /// Rewritten tool arguments containing only the approved hunks.
    pub arguments: String,
    /// Hunk ids that will be applied, in call order.
    pub applied: Vec<String>,
    /// Hunk ids the user rejected, in call order.
    pub rejected: Vec<String>,
}

fn hunk_id(file_index: usize, hunk_number: usize) -> String {
    format!("f{file_index}h{hunk_number}")
}

/// Computes the hunk breakdown of a gated edit tool call without applying it.
/// Returns `None` for non-edit tools and whenever planning fails (unreadable
/// file, non-matching oldText, malformed patch, no-op edit): the request then
/// falls back to whole-call approval.
pub fn plan_edit_hunks(name: &str, arguments: &str, cwd: &Path) -> Option<Vec<HunkView>> {
    let args = serde_json::from_str::<Value>(arguments).ok()?;
    match name {
        "write" => plan_write(&args, cwd),
        "str_replace" | "edit" => plan_str_replace(&args, cwd),
        "hashline_edit" => plan_hashline_edit(&args, cwd),
        "apply_patch" => plan_apply_patch(&args),
        _ => None,
    }
}

/// Rewrites an approved edit call so only `approved` hunks are applied. Hunk
/// ids are derived deterministically from the arguments alone, so they match
/// the ids produced by [`plan_edit_hunks`] for the same call.
pub fn filter_edit_call(
    name: &str,
    arguments: &str,
    approved: &[String],
) -> Result<FilteredEdit, String> {
    let approved_set = approved.iter().map(String::as_str).collect::<HashSet<_>>();
    if approved_set.is_empty() {
        return Err("no hunks were approved; deny the call instead".to_string());
    }
    match name {
        "write" => {
            // A full-file write is a single all-or-nothing hunk.
            let only = hunk_id(0, 1);
            check_known_ids(&approved_set, std::slice::from_ref(&only))?;
            Ok(FilteredEdit {
                arguments: arguments.to_string(),
                applied: vec![only],
                rejected: Vec::new(),
            })
        }
        "str_replace" | "edit" | "hashline_edit" => filter_edits_array(arguments, &approved_set),
        "apply_patch" => filter_patch_call(arguments, &approved_set),
        other => Err(format!("tool {other} does not support hunk selection")),
    }
}

/// Merges the selective-approval outcome into an edit tool's JSON result so
/// the model knows precisely which hunks landed.
pub fn merge_selective_summary(result: &str, applied: &[String], rejected: &[String]) -> String {
    let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(result) else {
        // Edit tool results are always JSON objects; degrade gracefully anyway.
        return format!(
            "{result}\napplied_hunks: {} rejected_hunks: {}",
            applied.join(","),
            rejected.join(",")
        );
    };
    map.insert("applied_hunks".to_string(), json!(applied));
    map.insert("rejected_hunks".to_string(), json!(rejected));
    if !rejected.is_empty() {
        let mut note = format!(
            "user rejected {} of {} hunks; the rejected hunks were NOT applied",
            rejected.len(),
            applied.len() + rejected.len()
        );
        if let Some(existing) = map.get("note").and_then(Value::as_str) {
            note = format!("{note}; {existing}");
        }
        map.insert("note".to_string(), json!(note));
    }
    Value::Object(map).to_string()
}

fn check_known_ids(approved: &HashSet<&str>, known: &[String]) -> Result<(), String> {
    for id in approved {
        if !known.iter().any(|known_id| known_id == id) {
            return Err(format!(
                "unknown hunk id '{id}'; valid ids: {}",
                known.join(", ")
            ));
        }
    }
    Ok(())
}

/// str_replace / hashline_edit: hunk `f0h<n>` maps to `edits[n-1]`; filtering
/// keeps only the approved entries of the `edits` array.
fn filter_edits_array(arguments: &str, approved: &HashSet<&str>) -> Result<FilteredEdit, String> {
    let mut args = serde_json::from_str::<Value>(arguments)
        .map_err(|error| format!("invalid JSON arguments: {error}"))?;
    let edits = args
        .get("edits")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "call has no edits array".to_string())?;
    let ids = (1..=edits.len())
        .map(|number| hunk_id(0, number))
        .collect::<Vec<_>>();
    check_known_ids(approved, &ids)?;

    let mut kept = Vec::new();
    let mut applied = Vec::new();
    let mut rejected = Vec::new();
    for (edit, id) in edits.into_iter().zip(ids) {
        if approved.contains(id.as_str()) {
            kept.push(edit);
            applied.push(id);
        } else {
            rejected.push(id);
        }
    }
    args["edits"] = json!(kept);
    Ok(FilteredEdit {
        arguments: args.to_string(),
        applied,
        rejected,
    })
}

/// apply_patch: reconstructs a unified diff containing only the approved
/// hunks (original file headers and hunk headers kept verbatim; files whose
/// hunks were all rejected are dropped). `git apply` matches the remaining
/// hunks by context, so the unadjusted line numbers stay usable.
fn filter_patch_call(arguments: &str, approved: &HashSet<&str>) -> Result<FilteredEdit, String> {
    let mut args = serde_json::from_str::<Value>(arguments)
        .map_err(|error| format!("invalid JSON arguments: {error}"))?;
    let patch = args
        .get("patch")
        .and_then(Value::as_str)
        .ok_or_else(|| "call has no patch".to_string())?;
    let files =
        parse_patch(patch).ok_or_else(|| "could not split the patch into hunks".to_string())?;
    let ids = files
        .iter()
        .enumerate()
        .flat_map(|(file_index, file)| {
            (1..=file.hunks.len()).map(move |number| hunk_id(file_index, number))
        })
        .collect::<Vec<_>>();
    check_known_ids(approved, &ids)?;

    let mut filtered = String::new();
    let mut applied = Vec::new();
    let mut rejected = Vec::new();
    for (file_index, file) in files.iter().enumerate() {
        let mut kept = Vec::new();
        for (offset, hunk) in file.hunks.iter().enumerate() {
            let id = hunk_id(file_index, offset + 1);
            if approved.contains(id.as_str()) {
                kept.push(hunk);
                applied.push(id);
            } else {
                rejected.push(id);
            }
        }
        if kept.is_empty() {
            continue;
        }
        for line in &file.header_lines {
            filtered.push_str(line);
            filtered.push('\n');
        }
        for hunk in kept {
            filtered.push_str(&hunk.header);
            filtered.push('\n');
            for line in &hunk.lines {
                filtered.push_str(line);
                filtered.push('\n');
            }
        }
    }
    args["patch"] = json!(filtered);
    Ok(FilteredEdit {
        arguments: args.to_string(),
        applied,
        rejected,
    })
}

fn plan_write(args: &Value, cwd: &Path) -> Option<Vec<HunkView>> {
    let path = args.get("path").and_then(Value::as_str)?;
    let content = args.get("content").and_then(Value::as_str)?;
    let path = tools::resolve_path(cwd, path);
    let original = fs::read_to_string(&path).unwrap_or_default();
    if original == content {
        return None;
    }
    let diff = tools::unified_diff_for_file(cwd, &path, &original, content)?;
    let (file, header, lines) = whole_diff_as_one_hunk(&diff)?;
    Some(vec![HunkView {
        id: hunk_id(0, 1),
        file,
        header,
        lines,
    }])
}

fn plan_str_replace(args: &Value, cwd: &Path) -> Option<Vec<HunkView>> {
    let path = args.get("path").and_then(Value::as_str)?;
    let edits = args.get("edits").and_then(Value::as_array)?;
    if edits.is_empty() {
        return None;
    }
    let path = tools::resolve_path(cwd, path);
    let original = fs::read_to_string(&path).ok()?;

    let mut views = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        // Mirrors the tool's matching rule (unique oldText occurrence); when a
        // preview cannot be built, the whole call degrades to all-or-nothing.
        let old_text = edit.get("oldText").and_then(Value::as_str)?;
        let new_text = edit.get("newText").and_then(Value::as_str)?;
        if old_text.is_empty() {
            return None;
        }
        let matches = original.match_indices(old_text).collect::<Vec<_>>();
        if matches.len() != 1 {
            return None;
        }
        let start = matches[0].0;
        let updated = format!(
            "{}{}{}",
            &original[..start],
            new_text,
            &original[start + old_text.len()..]
        );
        views.push(single_edit_hunk(cwd, &path, &original, &updated, index)?);
    }
    Some(views)
}

fn plan_hashline_edit(args: &Value, cwd: &Path) -> Option<Vec<HunkView>> {
    let path = args.get("path").and_then(Value::as_str)?;
    let edits = args.get("edits").and_then(Value::as_array)?;
    if edits.is_empty() {
        return None;
    }
    let path = tools::resolve_path(cwd, path);
    let original = fs::read_to_string(&path).ok()?;

    let mut views = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let updated =
            tools::apply_hashline_edits_preview(&original, std::slice::from_ref(edit)).ok()?;
        views.push(single_edit_hunk(cwd, &path, &original, &updated, index)?);
    }
    Some(views)
}

/// Diffs one edit applied in isolation and wraps it as hunk `f0h<index+1>`.
fn single_edit_hunk(
    cwd: &Path,
    path: &Path,
    original: &str,
    updated: &str,
    index: usize,
) -> Option<HunkView> {
    if original == updated {
        return None;
    }
    let diff = tools::unified_diff_for_file(cwd, path, original, updated)?;
    let (file, header, lines) = whole_diff_as_one_hunk(&diff)?;
    Some(HunkView {
        id: hunk_id(0, index + 1),
        file,
        header,
        lines,
    })
}

/// Collapses a single-file unified diff into one displayable hunk: the first
/// `@@` header plus every following diff line (later `@@` headers inline).
fn whole_diff_as_one_hunk(diff: &str) -> Option<(String, String, Vec<String>)> {
    let files = parse_patch(diff)?;
    let file = files.first()?;
    let first = file.hunks.first()?;
    let mut lines = first.lines.clone();
    for hunk in &file.hunks[1..] {
        lines.push(hunk.header.clone());
        lines.extend(hunk.lines.iter().cloned());
    }
    Some((file.label.clone(), first.header.clone(), lines))
}

struct PatchHunk {
    header: String,
    lines: Vec<String>,
}

struct PatchFile {
    header_lines: Vec<String>,
    label: String,
    hunks: Vec<PatchHunk>,
}

/// Path label from a file-header line, preferring the post-image name.
fn label_from_header_line(line: &str) -> Option<String> {
    let strip = |path: &str| {
        let path = path.trim();
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
            .to_string()
    };
    if let Some(rest) = line.strip_prefix("diff --git ") {
        let mut parts = rest.split_whitespace();
        let _old = parts.next()?;
        return parts.next().map(strip);
    }
    for prefix in ["+++ ", "--- "] {
        if let Some(path) = line.strip_prefix(prefix) {
            let path = path.trim();
            if path == "/dev/null" {
                return None;
            }
            return Some(strip(path));
        }
    }
    None
}

/// `@@ -a[,b] +c[,d] @@` → (b, d) with omitted counts defaulting to 1.
fn parse_hunk_counts(header: &str) -> Option<(usize, usize)> {
    let rest = header.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let mut ranges = rest[..end].split(' ');
    let old = ranges.next()?.strip_prefix('-')?;
    let new = ranges.next()?.strip_prefix('+')?;
    let count = |range: &str| match range.split_once(',') {
        Some((_, count)) => count.parse::<usize>().ok(),
        None => Some(1),
    };
    Some((count(old)?, count(new)?))
}

/// Splits a unified diff into file sections and `@@` hunks. Hunk bodies are
/// consumed by the counts in the header, so `---`/`+++`-looking removal lines
/// inside a hunk cannot be mistaken for a new file section. Returns `None`
/// for anything malformed.
fn parse_patch(patch: &str) -> Option<Vec<PatchFile>> {
    let mut files: Vec<PatchFile> = Vec::new();
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("@@") {
            let (mut old_left, mut new_left) = parse_hunk_counts(line)?;
            let file = files.last_mut()?;
            let mut body = Vec::new();
            while old_left > 0 || new_left > 0 {
                let body_line = lines.next()?;
                match body_line.as_bytes().first() {
                    // An empty line is an empty context line (trailing space stripped).
                    Some(b' ') | None => {
                        old_left = old_left.checked_sub(1)?;
                        new_left = new_left.checked_sub(1)?;
                    }
                    Some(b'-') => old_left = old_left.checked_sub(1)?,
                    Some(b'+') => new_left = new_left.checked_sub(1)?,
                    Some(b'\\') => {}
                    _ => return None,
                }
                body.push(body_line.to_string());
            }
            if lines.peek().is_some_and(|next| next.starts_with('\\')) {
                body.push(lines.next()?.to_string());
            }
            file.hunks.push(PatchHunk {
                header: line.to_string(),
                lines: body,
            });
            continue;
        }
        if line.trim().is_empty() {
            // Blank separator between concatenated file sections.
            continue;
        }
        let starts_new_file = line.starts_with("diff --git ")
            || files.last().is_none_or(|file| !file.hunks.is_empty());
        if starts_new_file {
            files.push(PatchFile {
                header_lines: Vec::new(),
                label: String::new(),
                hunks: Vec::new(),
            });
        }
        let file = files.last_mut()?;
        file.header_lines.push(line.to_string());
        if file.label.is_empty() {
            if let Some(label) = label_from_header_line(line) {
                file.label = label;
            }
        }
    }
    if files.is_empty() || files.iter().any(|file| file.hunks.is_empty()) {
        return None;
    }
    Some(files)
}

fn plan_apply_patch(args: &Value) -> Option<Vec<HunkView>> {
    let patch = args.get("patch").and_then(Value::as_str)?;
    let files = parse_patch(patch)?;
    let mut views = Vec::new();
    for (file_index, file) in files.iter().enumerate() {
        for (offset, hunk) in file.hunks.iter().enumerate() {
            views.push(HunkView {
                id: hunk_id(file_index, offset + 1),
                file: file.label.clone(),
                header: hunk.header.clone(),
                lines: hunk.lines.clone(),
            });
        }
    }
    Some(views)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        path::PathBuf,
        process::{Command, Stdio},
        time::{SystemTime, UNIX_EPOCH},
    };

    fn test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("jucode-hunks-test-{name}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const TWO_HUNK_PATCH: &str = "\
diff --git a/sample.txt b/sample.txt
--- a/sample.txt
+++ b/sample.txt
@@ -1,6 +1,6 @@
 a1
 a2
-a3
+A3
 a4
 a5
 a6
@@ -14,6 +14,6 @@
 a14
 a15
 a16
-a17
+A17
 a18
 a19
";

    fn sample_content() -> String {
        (1..=20).map(|n| format!("a{n}\n")).collect()
    }

    fn git_apply(dir: &Path, patch: &str) -> bool {
        use std::io::Write;
        let mut child = Command::new("git")
            .args(["apply", "--whitespace=nowarn", "-"])
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(patch.as_bytes())
            .unwrap();
        child.wait().unwrap().success()
    }

    #[test]
    fn apply_patch_plan_splits_multi_file_patch_with_stable_ids() {
        let patch = format!(
            "{TWO_HUNK_PATCH}diff --git a/other.txt b/other.txt\n--- a/other.txt\n+++ b/other.txt\n@@ -1,3 +1,3 @@\n b1\n-b2\n+B2\n b3\n"
        );
        let args = json!({ "patch": patch }).to_string();

        let hunks = plan_edit_hunks("apply_patch", &args, Path::new(".")).unwrap();

        let ids = hunks
            .iter()
            .map(|hunk| hunk.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["f0h1", "f0h2", "f1h1"]);
        assert_eq!(hunks[0].file, "sample.txt");
        assert_eq!(hunks[1].file, "sample.txt");
        assert_eq!(hunks[2].file, "other.txt");
        assert_eq!(hunks[0].header, "@@ -1,6 +1,6 @@");
        assert!(hunks[0].lines.contains(&"-a3".to_string()));
        assert!(hunks[2].lines.contains(&"+B2".to_string()));
    }

    #[test]
    fn filtered_patch_keeps_headers_and_only_approved_hunks() {
        let args = json!({ "patch": TWO_HUNK_PATCH }).to_string();

        let filtered = filter_edit_call("apply_patch", &args, &["f0h2".to_string()]).unwrap();

        assert_eq!(filtered.applied, ["f0h2"]);
        assert_eq!(filtered.rejected, ["f0h1"]);
        let patch = serde_json::from_str::<Value>(&filtered.arguments).unwrap()["patch"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(patch.starts_with("diff --git a/sample.txt b/sample.txt\n"));
        assert!(patch.contains("+++ b/sample.txt\n"));
        assert!(patch.contains("@@ -14,6 +14,6 @@"));
        assert!(!patch.contains("@@ -1,6 +1,6 @@"));
        assert!(!patch.contains("+A3"));
    }

    #[test]
    fn applying_a_filtered_subset_changes_only_approved_hunks() {
        let dir = test_dir("subset-apply");
        fs::write(dir.join("sample.txt"), sample_content()).unwrap();
        let args = json!({ "patch": TWO_HUNK_PATCH }).to_string();

        let filtered = filter_edit_call("apply_patch", &args, &["f0h2".to_string()]).unwrap();
        let patch = serde_json::from_str::<Value>(&filtered.arguments).unwrap()["patch"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(git_apply(&dir, &patch), "filtered patch must apply cleanly");

        let content = fs::read_to_string(dir.join("sample.txt")).unwrap();
        assert!(content.contains("a3\n"), "rejected hunk must not apply");
        assert!(content.contains("A17\n"), "approved hunk must apply");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn approving_all_hunks_equals_a_normal_apply() {
        let full_dir = test_dir("full-apply");
        let all_dir = test_dir("all-hunks-apply");
        fs::write(full_dir.join("sample.txt"), sample_content()).unwrap();
        fs::write(all_dir.join("sample.txt"), sample_content()).unwrap();
        let args = json!({ "patch": TWO_HUNK_PATCH }).to_string();

        let filtered = filter_edit_call(
            "apply_patch",
            &args,
            &["f0h1".to_string(), "f0h2".to_string()],
        )
        .unwrap();
        assert!(filtered.rejected.is_empty());
        let patch = serde_json::from_str::<Value>(&filtered.arguments).unwrap()["patch"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(git_apply(&full_dir, TWO_HUNK_PATCH));
        assert!(git_apply(&all_dir, &patch));

        assert_eq!(
            fs::read_to_string(full_dir.join("sample.txt")).unwrap(),
            fs::read_to_string(all_dir.join("sample.txt")).unwrap()
        );
        let _ = fs::remove_dir_all(full_dir);
        let _ = fs::remove_dir_all(all_dir);
    }

    #[test]
    fn write_plans_a_single_all_or_nothing_hunk() {
        let dir = test_dir("write-plan");
        fs::write(dir.join("notes.txt"), "old\n").unwrap();
        let args = json!({ "path": "notes.txt", "content": "new\n" }).to_string();

        let hunks = plan_edit_hunks("write", &args, &dir).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].id, "f0h1");
        assert_eq!(hunks[0].file, "notes.txt");
        assert!(hunks[0].lines.contains(&"-old".to_string()));
        assert!(hunks[0].lines.contains(&"+new".to_string()));

        // Filtering the single hunk keeps the call unchanged with nothing rejected.
        let filtered = filter_edit_call("write", &args, &["f0h1".to_string()]).unwrap();
        assert_eq!(filtered.arguments, args);
        assert_eq!(filtered.applied, ["f0h1"]);
        assert!(filtered.rejected.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn str_replace_hunks_follow_edit_order_and_filter_keeps_subset() {
        let dir = test_dir("str-replace-plan");
        fs::write(dir.join("code.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let args = json!({
            "path": "code.txt",
            "edits": [
                { "oldText": "alpha", "newText": "ALPHA" },
                { "oldText": "gamma", "newText": "GAMMA" },
            ],
        })
        .to_string();

        let hunks = plan_edit_hunks("str_replace", &args, &dir).unwrap();
        let ids = hunks
            .iter()
            .map(|hunk| hunk.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["f0h1", "f0h2"]);
        assert!(hunks[0].lines.contains(&"+ALPHA".to_string()));
        assert!(hunks[1].lines.contains(&"+GAMMA".to_string()));

        let filtered = filter_edit_call("str_replace", &args, &["f0h2".to_string()]).unwrap();
        assert_eq!(filtered.applied, ["f0h2"]);
        assert_eq!(filtered.rejected, ["f0h1"]);
        let value = serde_json::from_str::<Value>(&filtered.arguments).unwrap();
        let edits = value["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["oldText"], "gamma");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hashline_edit_plans_one_hunk_per_edit() {
        let dir = test_dir("hashline-plan");
        fs::write(dir.join("data.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let anchor =
            |line: usize, text: &str| format!("{line}#{}", tools::compute_line_hash(line, text));
        let args = json!({
            "path": "data.txt",
            "edits": [
                { "op": "replace", "pos": anchor(1, "alpha"), "lines": ["ALPHA"] },
                { "op": "replace", "pos": anchor(3, "gamma"), "lines": ["GAMMA"] },
            ],
        })
        .to_string();

        let hunks = plan_edit_hunks("hashline_edit", &args, &dir).unwrap();
        let ids = hunks
            .iter()
            .map(|hunk| hunk.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["f0h1", "f0h2"]);
        assert!(hunks[0].lines.contains(&"+ALPHA".to_string()));
        assert!(hunks[1].lines.contains(&"+GAMMA".to_string()));

        let filtered = filter_edit_call("hashline_edit", &args, &["f0h1".to_string()]).unwrap();
        assert_eq!(filtered.applied, ["f0h1"]);
        assert_eq!(filtered.rejected, ["f0h2"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn filter_rejects_unknown_hunk_ids_and_unsupported_tools() {
        let args = json!({ "patch": TWO_HUNK_PATCH }).to_string();
        let error = filter_edit_call("apply_patch", &args, &["f9h9".to_string()]).unwrap_err();
        assert!(error.contains("unknown hunk id 'f9h9'"), "{error}");
        assert!(
            error.contains("f0h1"),
            "error should list valid ids: {error}"
        );

        let error = filter_edit_call("bash", "{}", &["f0h1".to_string()]).unwrap_err();
        assert!(error.contains("does not support hunk selection"), "{error}");

        let error = filter_edit_call("apply_patch", &args, &[]).unwrap_err();
        assert!(error.contains("no hunks were approved"), "{error}");
    }

    #[test]
    fn non_edit_tools_have_no_hunk_plan() {
        assert!(plan_edit_hunks("bash", "{\"command\":\"ls\"}", Path::new(".")).is_none());
        assert!(plan_edit_hunks("read", "{\"path\":\"x\"}", Path::new(".")).is_none());
    }

    #[test]
    fn merge_selective_summary_reports_applied_and_rejected_hunks() {
        let result = json!({ "path": "a.txt", "diff": "…" }).to_string();

        let merged = merge_selective_summary(
            &result,
            &["f0h2".to_string()],
            &["f0h1".to_string(), "f0h3".to_string()],
        );

        let value = serde_json::from_str::<Value>(&merged).unwrap();
        assert_eq!(value["applied_hunks"], json!(["f0h2"]));
        assert_eq!(value["rejected_hunks"], json!(["f0h1", "f0h3"]));
        let note = value["note"].as_str().unwrap();
        assert!(note.contains("rejected 2 of 3 hunks"), "{note}");
        assert_eq!(value["path"], "a.txt");
    }

    #[test]
    fn merge_selective_summary_omits_note_when_nothing_was_rejected() {
        let merged = merge_selective_summary(
            &json!({ "ok": true }).to_string(),
            &["f0h1".to_string()],
            &[],
        );
        let value = serde_json::from_str::<Value>(&merged).unwrap();
        assert_eq!(value["applied_hunks"], json!(["f0h1"]));
        assert_eq!(value["rejected_hunks"], json!([]));
        assert!(value.get("note").is_none());
    }

    #[test]
    fn merge_selective_summary_prepends_to_an_existing_note() {
        let result = json!({ "note": "large diff summarized" }).to_string();
        let merged = merge_selective_summary(&result, &[], &["f0h1".to_string()]);
        let value = serde_json::from_str::<Value>(&merged).unwrap();
        let note = value["note"].as_str().unwrap();
        assert!(note.starts_with("user rejected 1 of 1 hunks"), "{note}");
        assert!(note.ends_with("large diff summarized"), "{note}");
    }
}
