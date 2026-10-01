//! The skills marketplace for the desktop: the JuCode marketplace and
//! github.com/anthropics/skills in one catalog, installed into the personal
//! skills directory of the session's engine.

use crate::engines;
use jucode_agent_core::skills;
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn handle(name: &str, op: &Value) -> Result<Value, String> {
    let dir = install_dir(op["backend"].as_str().unwrap_or("jucode"))?;
    match name {
        "skills_catalog" => catalog(dir),
        "skill_install" => {
            let text = |key: &str| {
                op[key]
                    .as_str()
                    .ok_or_else(|| format!("skill_install requires {key}"))
            };
            install(dir, text("source")?, text("skill")?)
                .map(|path| json!({ "type": "skill_installed", "path": path }))
        }
        _ => Err(format!("unknown op {name}")),
    }
}

/// Claude Code reads `~/.claude/skills`; every other engine runs with the
/// JuCode profile's skills.
fn install_dir(backend: &str) -> Result<PathBuf, String> {
    if backend != "claude" {
        return skills::profile_skills_dir().map_err(|error| error.to_string());
    }
    let home = engines::home();
    if home.as_os_str().is_empty() {
        return Err("home directory not found".to_string());
    }
    Ok(home.join(".claude").join("skills"))
}

/// A JuCode marketplace failure is only a warning: the Anthropic catalog is
/// bundled and stays installable.
fn catalog(dir: PathBuf) -> Result<Value, String> {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    match skills::fetch_jucode_marketplace() {
        Ok(market) => entries.extend(market.skills.iter().map(|skill| {
            json!({
                "id": skill.id,
                "name": skill.name,
                "description": skill.description,
                "tags": skill.tags,
                "source": "jucode",
                "isDefault": market.default_skill_ids.contains(&skill.id),
                "installed": skills::skill_installed(&dir, &skill.id),
                "license": "",
                "redistributable": true,
                "homepage": "",
            })
        })),
        Err(error) => warnings.push(format!("JuCode marketplace: {error}")),
    }
    let anthropic = skills::anthropic_source()?;
    entries.extend(anthropic.skills.iter().map(|skill| {
        json!({
            "id": skill.id,
            "name": skill.name,
            "description": skill.description,
            "tags": skill.tags,
            "source": "anthropic",
            "isDefault": false,
            "installed": skills::skill_installed(&dir, &skill.id),
            "license": skill.license,
            "redistributable": skill.redistributable,
            "homepage": anthropic.homepage(&skill.id),
        })
    }));
    let key = |entry: &Value| {
        (
            entry["source"].as_str().unwrap_or_default().to_string(),
            entry["name"].as_str().unwrap_or_default().to_string(),
        )
    };
    entries.sort_by_key(key);
    Ok(json!({
        "type": "skills_catalog",
        "skills": entries,
        "warnings": warnings,
        "installDir": dir,
    }))
}

/// The skill is looked up again here rather than trusting anything the
/// client sent beyond its source and id. Source-available Anthropic skills
/// install too: they come from Anthropic's repository, not from JuCode.
fn install(dir: PathBuf, source: &str, id: &str) -> Result<PathBuf, String> {
    match source {
        "anthropic" => {
            let anthropic = skills::anthropic_source()?;
            let skill = anthropic
                .skills
                .iter()
                .find(|skill| skill.id == id)
                .ok_or_else(|| format!("Anthropic skill not found: {id}"))?;
            skills::install_source_skill(&dir, &anthropic, skill)
        }
        "jucode" => {
            let market = skills::fetch_jucode_marketplace()?;
            let skill = market
                .skills
                .iter()
                .find(|skill| skill.id == id)
                .ok_or_else(|| format!("JuCode skill not found: {id}"))?;
            skills::install_marketplace_skill(&dir, skill)
        }
        other => return Err(format!("unknown skill source: {other}")),
    }
    .map_err(|error| error.to_string())
}
