//! User-defined slash commands loaded from Markdown prompt files.
//!
//! Two sources, mirroring skills: `~/.jucode/commands/*.md` (always loaded —
//! the profile dir is user-owned) and `<project>/.jucode/commands/*.md`
//! (loaded only after the project is trusted, same gate as project skills).
//! Invoking `/name args` injects the file body as the user prompt, with
//! `$ARGUMENTS` replaced by the raw argument string.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";
const DESCRIPTION_MAX_CHARS: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// Canonical invocation including the leading slash, e.g. `/deploy`.
    pub command: String,
    pub path: PathBuf,
    pub description: String,
    /// True for project-local commands (`.jucode/commands`), false for user
    /// commands (`~/.jucode/commands`).
    pub project_scoped: bool,
}

/// Discovers custom commands. Project commands are only included when the
/// project is trusted. Ordered user-first, then by command name; the first
/// entry wins on name collisions (user commands shadow project commands).
pub fn discover_custom_commands(
    profile_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> io::Result<Vec<CustomCommand>> {
    let mut commands = Vec::new();
    read_commands_dir(&profile_dir.join("commands"), false, &mut commands)?;
    if project_trusted {
        read_commands_dir(&cwd.join(".jucode").join("commands"), true, &mut commands)?;
    }
    commands.sort_by(|left, right| {
        (left.project_scoped, &left.command).cmp(&(right.project_scoped, &right.command))
    });
    commands.dedup_by(|next, kept| next.command == kept.command);
    Ok(commands)
}

/// Builds the prompt injected for `command` with the raw `args` string.
/// `$ARGUMENTS` occurrences are substituted; when the template has no
/// placeholder and args are present, they are appended as a trailing block.
pub fn command_message(command: &CustomCommand, args: &str) -> io::Result<String> {
    let content = fs::read_to_string(&command.path)?;
    Ok(render_command_template(
        strip_frontmatter(&content),
        args.trim(),
    ))
}

fn render_command_template(template: &str, args: &str) -> String {
    let body = template.trim();
    if body.contains(ARGUMENTS_PLACEHOLDER) {
        return body.replace(ARGUMENTS_PLACEHOLDER, args);
    }
    if args.is_empty() {
        body.to_string()
    } else {
        format!("{body}\n\n{args}")
    }
}

fn read_commands_dir(
    dir: &Path,
    project_scoped: bool,
    commands: &mut Vec<CustomCommand>,
) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let name = slugify(stem);
        if name.is_empty() {
            continue;
        }
        let description = fs::read_to_string(&path)
            .ok()
            .and_then(|content| describe(&content))
            .unwrap_or_else(|| "custom command".to_string());
        commands.push(CustomCommand {
            command: format!("/{name}"),
            path,
            description,
            project_scoped,
        });
    }
    Ok(())
}

fn describe(content: &str) -> Option<String> {
    let description = frontmatter_field(content, "description").or_else(|| {
        strip_frontmatter(content)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.trim_start_matches('#').trim().to_string())
    })?;
    if description.is_empty() {
        return None;
    }
    Some(if description.chars().count() > DESCRIPTION_MAX_CHARS {
        let mut short = description
            .chars()
            .take(DESCRIPTION_MAX_CHARS)
            .collect::<String>();
        short.push('…');
        short
    } else {
        description
    })
}

fn frontmatter_field(content: &str, key: &str) -> Option<String> {
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }
    for line in lines {
        if line == "---" {
            return None;
        }
        let (field, value) = line.split_once(':')?;
        if field.trim() == key {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn strip_frontmatter(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content;
    };
    match rest.find("\n---") {
        Some(index) => {
            let after = &rest[index + 4..];
            after.strip_prefix('\n').unwrap_or(after)
        }
        None => content,
    }
}

fn slugify(name: &str) -> String {
    let mut output = String::new();
    let mut previous_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            output.push('-');
            previous_dash = true;
        }
    }
    output.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "jucode-custom-cmd-{tag}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn discovers_user_and_trusted_project_commands() {
        let root = temp_root("discover");
        let profile = root.join("profile");
        let cwd = root.join("repo");
        fs::create_dir_all(profile.join("commands")).unwrap();
        fs::create_dir_all(cwd.join(".jucode").join("commands")).unwrap();
        fs::write(
            profile.join("commands").join("Deploy Fast.md"),
            "---\ndescription: Ship it\n---\nDeploy the app.\n",
        )
        .unwrap();
        fs::write(
            cwd.join(".jucode").join("commands").join("review.md"),
            "# Review checklist\nDo a review.\n",
        )
        .unwrap();

        let commands = discover_custom_commands(&profile, &cwd, true).unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].command, "/deploy-fast");
        assert_eq!(commands[0].description, "Ship it");
        assert!(!commands[0].project_scoped);
        assert_eq!(commands[1].command, "/review");
        assert_eq!(commands[1].description, "Review checklist");
        assert!(commands[1].project_scoped);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn untrusted_project_commands_are_hidden() {
        let root = temp_root("trust");
        let profile = root.join("profile");
        let cwd = root.join("repo");
        fs::create_dir_all(cwd.join(".jucode").join("commands")).unwrap();
        fs::write(
            cwd.join(".jucode").join("commands").join("evil.md"),
            "run something\n",
        )
        .unwrap();

        let commands = discover_custom_commands(&profile, &cwd, false).unwrap();
        assert!(commands.is_empty());

        let trusted = discover_custom_commands(&profile, &cwd, true).unwrap();
        assert_eq!(trusted.len(), 1);
        assert_eq!(trusted[0].command, "/evil");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn user_command_shadows_project_command_with_same_name() {
        let root = temp_root("shadow");
        let profile = root.join("profile");
        let cwd = root.join("repo");
        fs::create_dir_all(profile.join("commands")).unwrap();
        fs::create_dir_all(cwd.join(".jucode").join("commands")).unwrap();
        fs::write(profile.join("commands").join("go.md"), "user prompt\n").unwrap();
        fs::write(
            cwd.join(".jucode").join("commands").join("go.md"),
            "project prompt\n",
        )
        .unwrap();

        let commands = discover_custom_commands(&profile, &cwd, true).unwrap();
        assert_eq!(commands.len(), 1);
        assert!(!commands[0].project_scoped);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn command_message_substitutes_arguments_placeholder() {
        assert_eq!(
            render_command_template("Fix the bug in $ARGUMENTS now.", "src/main.rs"),
            "Fix the bug in src/main.rs now."
        );
        assert_eq!(
            render_command_template("Run the checklist.", "extra context"),
            "Run the checklist.\n\nextra context"
        );
        assert_eq!(
            render_command_template("Run the checklist.", ""),
            "Run the checklist."
        );
    }

    #[test]
    fn command_message_strips_frontmatter() {
        let root = temp_root("frontmatter");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("cmd.md");
        fs::write(&path, "---\ndescription: X\n---\nBody with $ARGUMENTS.\n").unwrap();
        let command = CustomCommand {
            command: "/cmd".to_string(),
            path,
            description: "X".to_string(),
            project_scoped: false,
        };
        assert_eq!(
            command_message(&command, "42").unwrap(),
            "Body with 42.".to_string()
        );
        let _ = fs::remove_dir_all(root);
    }
}
