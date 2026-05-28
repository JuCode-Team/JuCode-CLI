use std::{
    fs, io,
    path::{Path, PathBuf},
};

const TOOL_GUIDANCE: &str = "prefer read/ls/ripgrep for exploration; use bash for commands and verification; use edit/apply_patch for scoped file changes.";
const PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct PromptContext {
    pub date: String,
    pub cwd: PathBuf,
    pub tools: Vec<&'static str>,
    pub project_instructions: Vec<ProjectInstruction>,
    pub skills: Vec<SkillPromptItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstruction {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPromptItem {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommand {
    pub command: String,
    pub skill: SkillPromptItem,
}

pub fn build_system_prompt(base: &str, context: &PromptContext) -> String {
    let mut prompt = base.trim_end().to_string();
    prompt.push_str("\n\n<runtime_context>\n");
    prompt.push_str(&format!("Current date: {}\n", context.date));
    prompt.push_str(&format!(
        "Current working directory: {}\n",
        context.cwd.display()
    ));
    prompt.push_str(&format!("Available tools: {}\n", context.tools.join(", ")));
    prompt.push_str(&format!("Tool guidance: {TOOL_GUIDANCE}\n"));
    prompt.push_str("</runtime_context>");

    if !context.project_instructions.is_empty() {
        prompt.push_str("\n\n<project_context>\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for instruction in &context.project_instructions {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                escape_xml(&instruction.path.display().to_string()),
                instruction.content.trim_end()
            ));
        }
        prompt.push_str("</project_context>");
    }

    if !context.skills.is_empty() {
        prompt.push_str(
            "\n\nThe following skills provide specialized instructions for specific tasks.\n",
        );
        prompt.push_str("Read the full skill file when the task matches its description.\n");
        prompt.push_str("When a skill file references a relative path, resolve it against the skill directory.\n\n");
        prompt.push_str("<available_skills>\n");
        for skill in &context.skills {
            prompt.push_str("  <skill>\n");
            prompt.push_str(&format!("    <name>{}</name>\n", escape_xml(&skill.name)));
            prompt.push_str(&format!(
                "    <description>{}</description>\n",
                escape_xml(&skill.description)
            ));
            prompt.push_str(&format!(
                "    <location>{}</location>\n",
                escape_xml(&skill.path.display().to_string())
            ));
            prompt.push_str("  </skill>\n");
        }
        prompt.push_str("</available_skills>");
    }

    prompt
}

pub fn discover_skills(profile_dir: &Path, cwd: &Path) -> io::Result<Vec<SkillPromptItem>> {
    let mut skills = Vec::new();
    read_skills_dir(&profile_dir.join("skills"), &mut skills)?;
    read_skills_dir(&cwd.join(".jucode").join("skills"), &mut skills)?;
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills.dedup_by(|left, right| left.name == right.name && left.path == right.path);
    Ok(skills)
}

pub fn skill_commands(profile_dir: &Path, cwd: &Path) -> io::Result<Vec<SkillCommand>> {
    let mut commands = discover_skills(profile_dir, cwd)?
        .into_iter()
        .map(|skill| SkillCommand {
            command: format!("/{}", skill_command_name(&skill.name)),
            skill,
        })
        .collect::<Vec<_>>();
    commands.retain(|entry| entry.command != "/");
    commands.sort_by(|left, right| left.command.cmp(&right.command));
    Ok(commands)
}

pub fn skill_message(skill: &SkillPromptItem, request: &str) -> io::Result<String> {
    let content = fs::read_to_string(&skill.path)?;
    let mut message = format!(
        "Use the following skill instructions for this request.\n\n<skill name=\"{}\" path=\"{}\">\n{}\n</skill>",
        escape_xml(&skill.name),
        escape_xml(&skill.path.display().to_string()),
        content.trim_end()
    );
    if !request.trim().is_empty() {
        message.push_str("\n\nUser request:\n");
        message.push_str(request.trim());
    }
    Ok(message)
}

pub fn discover_project_instructions(cwd: &Path) -> io::Result<Vec<ProjectInstruction>> {
    let dirs = project_instruction_dirs(cwd);

    let mut instructions = Vec::new();
    let mut remaining = PROJECT_INSTRUCTIONS_MAX_BYTES;
    for dir in dirs {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let path = dir.join(name);
            if path.exists() {
                if remaining == 0 {
                    return Ok(instructions);
                }
                let (content, bytes_read) = read_limited_utf8(&path, remaining)?;
                remaining = remaining.saturating_sub(bytes_read);
                instructions.push(ProjectInstruction {
                    path: path.clone(),
                    content,
                });
            }
        }
    }
    Ok(instructions)
}

fn project_instruction_dirs(cwd: &Path) -> Vec<&Path> {
    let mut dirs = cwd.ancestors().collect::<Vec<_>>();
    dirs.reverse();
    if let Some(index) = dirs.iter().position(|dir| dir.join(".git").exists()) {
        return dirs[index..].to_vec();
    }
    if let Some(index) = dirs
        .iter()
        .position(|dir| dir.join("AGENTS.md").exists() || dir.join("CLAUDE.md").exists())
    {
        return dirs[index..].to_vec();
    }
    vec![cwd]
}

fn read_limited_utf8(path: &Path, max_bytes: usize) -> io::Result<(String, usize)> {
    let bytes = fs::read(path)?;
    let truncated = bytes.len() > max_bytes;
    let mut end = bytes.len().min(max_bytes);
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    let mut content = String::from_utf8_lossy(&bytes[..end]).to_string();
    if truncated {
        content.push_str("\n\n[project instructions truncated by JuCode budget]\n");
    }
    Ok((content, end))
}

fn skill_command_name(name: &str) -> String {
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

fn read_skills_dir(dir: &Path, skills: &mut Vec<SkillPromptItem>) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let skill_path = path.join("SKILL.md");
            if skill_path.exists() {
                if let Some(skill) = read_skill_file(&skill_path)? {
                    skills.push(skill);
                }
            }
        } else if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
            if let Some(skill) = read_skill_file(&path)? {
                skills.push(skill);
            }
        }
    }
    Ok(())
}

fn read_skill_file(path: &Path) -> io::Result<Option<SkillPromptItem>> {
    let content = fs::read_to_string(path)?;
    let name = read_frontmatter_field(&content, "name")
        .or_else(|| {
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "skill".to_string());
    let description = read_frontmatter_field(&content, "description")
        .or_else(|| first_non_empty_body_line(&content))
        .unwrap_or_default();
    if description.is_empty() {
        return Ok(None);
    }
    Ok(Some(SkillPromptItem {
        name,
        description,
        path: path.to_path_buf(),
    }))
}

fn read_frontmatter_field(content: &str, key: &str) -> Option<String> {
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

fn first_non_empty_body_line(content: &str) -> Option<String> {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "---")
        .map(|line| line.trim_start_matches('#').trim().to_string())
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn prompt_includes_runtime_context_and_skills() {
        let prompt = build_system_prompt(
            "Base prompt",
            &PromptContext {
                date: "2026-05-27".to_string(),
                cwd: PathBuf::from("C:/repo"),
                tools: vec!["read", "bash"],
                project_instructions: vec![ProjectInstruction {
                    path: PathBuf::from("C:/repo/AGENTS.md"),
                    content: "Follow project rules.".to_string(),
                }],
                skills: vec![SkillPromptItem {
                    name: "review".to_string(),
                    description: "Review <code> & tests".to_string(),
                    path: PathBuf::from("C:/skills/review/SKILL.md"),
                }],
            },
        );

        assert!(prompt.contains("<runtime_context>"));
        assert!(prompt.contains("Current date: 2026-05-27"));
        assert!(prompt.contains("Available tools: read, bash"));
        assert!(prompt.contains("<project_context>"));
        assert!(prompt.contains("Follow project rules."));
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("Review &lt;code&gt; &amp; tests"));
    }

    #[test]
    fn discovers_project_instructions_from_root_to_cwd() {
        let root = std::env::temp_dir().join(format!(
            "jucode-instruction-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("repo");
        let nested = project.join("crates").join("cli");
        fs::create_dir_all(&nested).unwrap();
        fs::write(project.join("AGENTS.md"), "root agents").unwrap();
        fs::write(nested.join("CLAUDE.md"), "nested claude").unwrap();

        let instructions = discover_project_instructions(&nested).unwrap();

        assert_eq!(instructions.len(), 2);
        assert_eq!(
            instructions[0]
                .path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("AGENTS.md")
        );
        assert_eq!(
            instructions[1]
                .path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("CLAUDE.md")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovers_frontmatter_skill_files() {
        let root = std::env::temp_dir().join(format!(
            "jucode-skill-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skill_dir = root.join("profile").join("skills").join("review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: review\ndescription: Review code carefully\n---\nbody\n",
        )
        .unwrap();

        let skills = discover_skills(&root.join("profile"), &root.join("cwd")).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "review");
        assert_eq!(skills[0].description, "Review code carefully");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skill_commands_slugify_names() {
        let root = std::env::temp_dir().join(format!(
            "jucode-skill-command-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skill_dir = root.join("profile").join("skills").join("code-review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Code Review\ndescription: Review code\n---\nbody\n",
        )
        .unwrap();

        let commands = skill_commands(&root.join("profile"), &root.join("cwd")).unwrap();

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "/code-review");

        let _ = fs::remove_dir_all(root);
    }
}
