//! Plan mode (`ApprovalMode::Plan`): the agent investigates with read-only
//! tools and delivers a plan through `propose_plan`; nothing else runs until
//! the user approves it (`approve_plan`). This module decides which calls
//! may run, and holds the prompt and tool text the mode adds.

use serde_json::{json, Value};

pub const TOOL_NAME: &str = "propose_plan";

/// Added to the main agent's system prompt for a turn that starts in plan mode.
pub const PROMPT_ADDENDUM: &str = r#"<plan_mode>
You are in plan mode. Only read-only tools run: reading, listing and searching files, read-only shell commands (for example git status/diff/log, ls, cat, grep, find), web search and fetch, and subagents (which are in plan mode too). Edits, writes and other changes are refused until the user approves a plan. If the user asks you to make changes, plan them instead.

1. Investigate first: read the relevant code, configuration and tests until you understand the current state. Do not ask the user anything you can find out yourself; ask only when you are blocked by a decision only they can make.
2. Deliver the plan by calling propose_plan with a short title and the plan in Markdown. Then stop: the user approves the plan or asks for changes.

Write the plan for scanning, with short sections and tables:
- **Goal & principles**: what the change achieves and the rules it follows.
- **Changes**: a table with the columns File | Change | Why.
- **Steps**: numbered, in the order you will do them.
- **Risks**: what could break, and how you avoid it.
- **Verification**: the tests and checks that prove it works.
Keep it decision-complete and concise: whoever executes it must not have to make new decisions. A revised plan replaces the earlier one completely.
</plan_mode>"#;

/// Added to a subagent's context when it starts in plan mode.
pub const SUBAGENT_NOTE: &str = " Plan mode applies to you too: only read-only tools run. Report your findings to the parent; do not propose a plan.";

/// The value of string field `key` in a JSON object still being written:
/// what has arrived of it so far, unescaped; None until the value starts.
pub fn partial_string_field(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\"");
    let mut from = 0;
    while let Some(found) = json[from..].find(&pattern) {
        let after = from + found + pattern.len();
        let rest = json[after..].trim_start();
        if let Some(value) = rest.strip_prefix(':').map(str::trim_start) {
            return value.strip_prefix('"').map(unescape_partial);
        }
        from = after;
    }
    None
}

/// A JSON string body up to its closing quote, or to the end when the rest
/// has not arrived; an escape cut off at the end is left out.
fn unescape_partial(body: &str) -> String {
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some(c @ ('"' | '\\' | '/')) => out.push(c),
                Some('u') => {
                    let Some(high) = hex4(&mut chars) else { break };
                    let code = if (0xD800..0xDC00).contains(&high) {
                        // A surrogate pair: its low half follows as \uXXXX.
                        if chars.next() != Some('\\') || chars.next() != Some('u') {
                            break;
                        }
                        let Some(low) = hex4(&mut chars) else { break };
                        0x10000 + ((high - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF)
                    } else {
                        high
                    };
                    if let Some(c) = char::from_u32(code) {
                        out.push(c);
                    }
                }
                _ => break,
            },
            c => out.push(c),
        }
    }
    out
}

fn hex4(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<u32> {
    let digits: String = chars.by_ref().take(4).collect();
    (digits.len() == 4)
        .then(|| u32::from_str_radix(&digits, 16).ok())
        .flatten()
}

pub fn propose_plan_definition() -> Value {
    json!({
        "type": "function",
        "name": TOOL_NAME,
        "description": "Plan mode only: present your finished plan to the user for approval. This ends your turn; the user approves the plan (then you implement it) or asks for changes (then you propose a complete revised plan).",
        "parameters": {
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Short title of the plan, in the user's language." },
                "plan": { "type": "string", "description": "The full plan in Markdown: goal & principles, a File | Change | Why table, numbered steps, risks, verification." }
            },
            "required": ["title", "plan"],
            "additionalProperties": false
        }
    })
}

/// Tools that never change anything.
const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "ls",
    "ripgrep",
    "outline",
    "web_fetch",
    "web_search",
    "get_goal",
    // Subagents share the session's live mode, so they are in plan mode too.
    "spawn_agent",
    "wait_agent",
    "list_agents",
    "send_message",
    "close_agent",
    TOOL_NAME,
];

/// Why plan mode refuses this call, or None when it may run. `mcp_read_only`
/// is the MCP server's readOnlyHint for the tool, when it gave one.
pub fn refusal(name: &str, arguments: &str, mcp_read_only: Option<bool>) -> Option<String> {
    if READ_ONLY_TOOLS.contains(&name) {
        return None;
    }
    let args = serde_json::from_str::<Value>(arguments).unwrap_or(Value::Null);
    let field = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    let allowed = match name {
        name if name.starts_with("mcp__") => mcp_read_only == Some(true),
        "bash" | "execute" | "exec_command" | "shell_command" => {
            let command = args
                .get("command")
                .or_else(|| args.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let escalated = args.get("escalate").and_then(Value::as_bool) == Some(true);
            if escalated || !is_read_only_command(command) {
                return Some(format!(
                    "plan mode: `{}` was not run because it is not a known read-only command. Only read-only commands run in plan mode (git status/diff/log/show, ls, cat, head, tail, grep, rg, find, wc and similar, optionally piped together). Use read, ls and ripgrep to investigate, and put the commands that change things into the plan you deliver with {TOOL_NAME}.",
                    command.trim()
                ));
            }
            true
        }
        // Polling a running shell writes nothing.
        "write_stdin" => field("text").is_empty() && field("chars").is_empty(),
        "checkpoint" => field("action") == "list",
        _ => false,
    };
    (!allowed).then(|| {
        format!(
            "plan mode: {name} was not run. You are in plan mode, where only read-only tools run and nothing in the workspace may change. Finish investigating, then call {TOOL_NAME} with your plan; the user approves it before anything is changed."
        )
    })
}

/// Programs that only read, whatever their arguments (apart from the
/// options `forbidden_option` rejects).
const READ_ONLY_PROGRAMS: &[&str] = &[
    "cat", "head", "tail", "wc", "ls", "pwd", "echo", "printf", "grep", "egrep", "fgrep", "rg",
    "find", "tree", "file", "stat", "du", "df", "which", "whereis", "sort", "cut", "tr", "diff",
    "cmp", "comm", "nl", "basename", "dirname", "realpath", "readlink", "uname", "whoami", "id",
    "jq", "true", "false", "cd",
];

/// git subcommands that only read.
const READ_ONLY_GIT: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "ls-files",
    "ls-tree",
    "rev-parse",
    "describe",
    "shortlog",
    "grep",
    "cat-file",
    "rev-list",
    "show-ref",
    "merge-base",
    "name-rev",
    "reflog",
];

/// Whether `command` provably only reads: a pipeline or list of known
/// read-only programs, with no redirection to files, no command
/// substitution and no background jobs. Anything else is not read-only.
pub fn is_read_only_command(command: &str) -> bool {
    let mut text = command.trim().to_string();
    if text.is_empty() {
        return false;
    }
    // Redirections that write nothing.
    for harmless in ["2>&1", "&>/dev/null", "2>/dev/null", ">/dev/null"] {
        text = text.replace(harmless, " ");
    }
    if text.contains(['>', '`', '\n', '\r'])
        || text.contains("$(")
        || text.contains("<(")
        || text.contains("${")
    {
        return false;
    }
    let text = text.replace("&&", ";").replace("||", ";");
    if text.contains('&') {
        return false;
    }
    text.split([';', '|'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .all(segment_is_read_only)
        && !text
            .split([';', '|'])
            .all(|segment| segment.trim().is_empty())
}

fn segment_is_read_only(segment: &str) -> bool {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let Some((&program, args)) = words.split_first() else {
        return false;
    };
    // An environment assignment or a path could run anything.
    if program.contains(['=', '/']) {
        return false;
    }
    if program == "git" {
        return git_is_read_only(args);
    }
    READ_ONLY_PROGRAMS.contains(&program) && !args.iter().any(|arg| forbidden_option(program, arg))
}

/// Options that make an otherwise read-only program write or run something.
fn forbidden_option(program: &str, arg: &str) -> bool {
    match program {
        "find" => matches!(
            arg,
            "-exec"
                | "-execdir"
                | "-ok"
                | "-okdir"
                | "-delete"
                | "-fprint"
                | "-fprint0"
                | "-fprintf"
                | "-fls"
        ),
        "sort" => {
            arg == "-o" || arg.starts_with("--output") || (arg.starts_with("-o") && arg.len() > 2)
        }
        "rg" => arg.starts_with("--pre"),
        "tree" => arg == "-o",
        _ => false,
    }
}

fn git_is_read_only(args: &[&str]) -> bool {
    let mut rest = args;
    // Global options before the subcommand.
    loop {
        match rest {
            ["-C", _, tail @ ..] => rest = tail,
            ["--no-pager" | "-P", tail @ ..] => rest = tail,
            _ => break,
        }
    }
    let Some((&subcommand, options)) = rest.split_first() else {
        return false;
    };
    if options
        .iter()
        .any(|arg| *arg == "-o" || arg.starts_with("--output") || arg.starts_with("--ext-diff"))
    {
        return false;
    }
    match subcommand {
        "branch" => options.iter().all(|arg| {
            matches!(
                *arg,
                "-a" | "-r"
                    | "-v"
                    | "-vv"
                    | "--list"
                    | "--all"
                    | "--remotes"
                    | "--show-current"
                    | "--verbose"
            )
        }),
        "remote" => options.iter().all(|arg| matches!(*arg, "-v" | "--verbose")),
        "tag" => options.iter().all(|arg| matches!(*arg, "-l" | "--list")),
        "stash" => options.first() == Some(&"list"),
        "config" => options.first().is_some_and(|arg| {
            matches!(
                *arg,
                "--get" | "--get-all" | "--get-regexp" | "--list" | "-l"
            )
        }),
        other => READ_ONLY_GIT.contains(&other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_tools_run_and_changes_are_refused() {
        for tool in [
            "read",
            "ls",
            "ripgrep",
            "outline",
            "web_fetch",
            "web_search",
            "spawn_agent",
            "wait_agent",
            TOOL_NAME,
        ] {
            assert_eq!(refusal(tool, "{}", None), None, "{tool}");
        }
        for tool in [
            "write",
            "str_replace",
            "hashline_edit",
            "apply_patch",
            "generate_image",
            "update_plan",
            "create_goal",
            "report",
        ] {
            let reason = refusal(tool, "{}", None).expect(tool);
            assert!(reason.contains("plan mode"), "{reason}");
            assert!(reason.contains(TOOL_NAME), "{reason}");
        }
        assert_eq!(refusal("checkpoint", r#"{"action":"list"}"#, None), None);
        assert!(refusal("checkpoint", r#"{"action":"create","paths":["a"]}"#, None).is_some());
        assert_eq!(refusal("write_stdin", r#"{"session_id":1}"#, None), None);
        assert!(refusal(
            "write_stdin",
            r#"{"session_id":1,"text":"rm -rf x\n"}"#,
            None
        )
        .is_some());
    }

    #[test]
    fn mcp_tools_run_only_when_marked_read_only() {
        assert_eq!(refusal("mcp__docs__search", "{}", Some(true)), None);
        assert!(refusal("mcp__docs__search", "{}", None).is_some());
        assert!(refusal("mcp__db__drop", "{}", Some(false)).is_some());
    }

    #[test]
    fn shell_commands_run_only_when_provably_read_only() {
        for command in [
            "git status",
            "git -C crates/x --no-pager log --oneline -5",
            "git diff HEAD~1 -- src/lib.rs | head -50",
            "ls -la && cat Cargo.toml",
            "rg -n 'fn main' src | wc -l",
            "find . -name '*.rs' -type f 2>/dev/null | sort",
            "cd crates && grep -rn TODO .",
            "git branch -a",
            "git config --get user.name",
        ] {
            assert!(is_read_only_command(command), "{command}");
            assert_eq!(
                refusal("bash", &json!({ "command": command }).to_string(), None),
                None,
                "{command}"
            );
        }
        for command in [
            "",
            "cargo test",
            "rm -rf target",
            "echo hi > notes.txt",
            "cat a >> b",
            "ls; touch x",
            "git commit -m x",
            "git checkout main",
            "git branch new-branch",
            "git diff --output=patch.diff",
            "find . -name x -delete",
            "find . -exec rm {} ;",
            "sort -o out.txt in.txt",
            "rg --pre ./script x",
            "echo $(rm -rf x)",
            "cat `ls`",
            "sleep 10 &",
            "FOO=1 ls",
            "/bin/rm x",
            "xargs rm",
            "sed -i s/a/b/ f",
            "python -c 'print(1)'",
            "git stash",
        ] {
            assert!(!is_read_only_command(command), "{command:?}");
        }
        let escalated = json!({ "command": "ls", "escalate": true }).to_string();
        assert!(refusal("bash", &escalated, None).is_some());
        let refused = refusal("exec_command", r#"{"cmd":"cargo build"}"#, None).unwrap();
        assert!(refused.contains("`cargo build`"), "{refused}");
    }

    #[test]
    fn reads_a_string_field_while_it_is_still_being_written() {
        let whole =
            r###"{"title": "Web 版", "plan": "## Goal\n\n- \"one\" \u4e2d \ud83d\ude00\n"}"###;
        assert_eq!(partial_string_field(whole, "title").unwrap(), "Web 版");
        assert_eq!(
            partial_string_field(whole, "plan").unwrap(),
            "## Goal\n\n- \"one\" 中 😀\n"
        );
        // Cut anywhere: what arrived so far, an escape cut off left out.
        let cut = |json: &str| partial_string_field(json, "plan");
        assert_eq!(cut(r###"{"title":"T","plan":"## Go"###).unwrap(), "## Go");
        assert_eq!(cut(r###"{"title":"T","plan":"line\"###).unwrap(), "line");
        assert_eq!(cut(r###"{"title":"T","plan":"a\u4e"###).unwrap(), "a");
        assert_eq!(cut(r###"{"title":"T","pl"###), None);
        assert_eq!(cut(r###"{"title":"T","plan": "###), None);
        // A quoted "plan" inside the title is not the field.
        assert_eq!(cut(r###"{"title":"the \"plan\" one"}"###), None);
    }
}
