use serde_json::Value;
use std::{fs, io, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub tags: Vec<String>,
    pub enabled: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marketplace {
    pub skills: Vec<MarketplaceSkill>,
    pub default_skill_ids: Vec<String>,
}

pub fn fetch_marketplace(api_url: &str, api_key: Option<&str>) -> Result<Marketplace, String> {
    let url = format!("{}/v1/skills/marketplace", api_url.trim_end_matches('/'));
    let mut request = ureq::get(&url).timeout(std::time::Duration::from_secs(30));
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        request = request.set("Authorization", &format!("Bearer {key}"));
    }
    let response = request.call().map_err(|error| error.to_string())?;
    let value = response
        .into_json::<Value>()
        .map_err(|error| error.to_string())?;
    parse_marketplace(&value)
}

pub fn install_marketplace_skill(profile_dir: &Path, skill: &MarketplaceSkill) -> io::Result<()> {
    let dir = profile_dir.join("skills").join(safe_skill_dir(&skill.id));
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("SKILL.md"), normalized_content(skill))
}

pub fn install_default_skills(profile_dir: &Path, marketplace: &Marketplace) -> io::Result<usize> {
    let mut installed = 0;
    for id in &marketplace.default_skill_ids {
        if let Some(skill) = marketplace.skills.iter().find(|skill| &skill.id == id) {
            install_marketplace_skill(profile_dir, skill)?;
            installed += 1;
        }
    }
    Ok(installed)
}

pub fn parse_marketplace(value: &Value) -> Result<Marketplace, String> {
    let skills_value = value
        .get("skills")
        .and_then(Value::as_array)
        .ok_or_else(|| "marketplace response missing skills".to_string())?;
    let skills = skills_value
        .iter()
        .filter_map(parse_skill)
        .filter(|skill| skill.enabled)
        .collect::<Vec<_>>();
    let default_skill_ids = value
        .get("default_skill_ids")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Marketplace {
        skills,
        default_skill_ids,
    })
}

fn parse_skill(value: &Value) -> Option<MarketplaceSkill> {
    let id = read_string(value, "id")?;
    let name = read_string(value, "name")?;
    let description = read_string(value, "description")?;
    let content = read_string(value, "content")?;
    let tags = value
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let enabled = value
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let updated_at = value
        .get("updated_at")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(MarketplaceSkill {
        id,
        name,
        description,
        content,
        tags,
        enabled,
        updated_at,
    })
}

fn read_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalized_content(skill: &MarketplaceSkill) -> String {
    let content = skill.content.trim_end();
    if content.starts_with("---") {
        format!("{content}\n")
    } else {
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n{content}\n",
            skill.name, skill.description
        )
    }
}

fn safe_skill_dir(id: &str) -> String {
    let mut output = String::new();
    let mut previous_dash = false;
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            output.push('-');
            previous_dash = true;
        }
    }
    let output = output.trim_matches('-').to_string();
    if output.is_empty() {
        "skill".to_string()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_enabled_marketplace_skills() {
        let marketplace = parse_marketplace(&json!({
            "skills": [
                { "id": "review", "name": "Review", "description": "Review code", "content": "body", "enabled": true },
                { "id": "off", "name": "Off", "description": "Hidden", "content": "body", "enabled": false }
            ],
            "default_skill_ids": ["review", "off"]
        }))
        .unwrap();

        assert_eq!(marketplace.skills.len(), 1);
        assert_eq!(marketplace.skills[0].id, "review");
        assert_eq!(marketplace.default_skill_ids, vec!["review", "off"]);
    }

    #[test]
    fn installs_skill_file() {
        let root = std::env::temp_dir().join(format!(
            "jucode-marketplace-skill-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skill = MarketplaceSkill {
            id: "Code Review".to_string(),
            name: "Code Review".to_string(),
            description: "Review code".to_string(),
            content: "Be strict.".to_string(),
            tags: vec![],
            enabled: true,
            updated_at: String::new(),
        };

        install_marketplace_skill(&root, &skill).unwrap();

        let content =
            fs::read_to_string(root.join("skills").join("code-review").join("SKILL.md")).unwrap();
        assert!(content.contains("name: Code Review"));
        assert!(content.contains("Be strict."));
        let _ = fs::remove_dir_all(root);
    }
}
