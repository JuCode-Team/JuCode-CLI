use flate2::read::GzDecoder;
use serde_json::Value;
use std::{
    fs,
    io::{self, Cursor, Read},
    path::{Component, Path, PathBuf},
};
use tar::Archive;
use zip::ZipArchive;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub package_url: Option<String>,
    pub package_sha256: Option<String>,
    pub package_type: Option<String>,
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
    if let Some(url) = skill
        .package_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        install_skill_package(&dir, skill, url)?;
    } else {
        install_inline_skill(&dir, skill)?;
    }
    Ok(())
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
    let content = read_string(value, "content").unwrap_or_default();
    let package_url = read_string(value, "package_url");
    if content.is_empty() && package_url.is_none() {
        return None;
    }
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
        package_url,
        package_sha256: read_string(value, "package_sha256"),
        package_type: read_string(value, "package_type"),
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

fn install_inline_skill(dir: &Path, skill: &MarketplaceSkill) -> io::Result<()> {
    replace_dir(dir)?;
    fs::write(dir.join("SKILL.md"), normalized_content(skill))
}

fn install_skill_package(dir: &Path, skill: &MarketplaceSkill, url: &str) -> io::Result<()> {
    let bytes = download_skill_package(url)?;
    if let Some(expected) = &skill.package_sha256 {
        verify_sha256(&bytes, expected)?;
    }
    let temp_dir = dir.with_extension("tmp-install");
    replace_dir(&temp_dir)?;
    let package_type = skill
        .package_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| infer_package_type(url));
    match package_type {
        "zip" => extract_zip(&bytes, &temp_dir)?,
        "tar.gz" | "tgz" => extract_tar_gz(&bytes, &temp_dir)?,
        other => {
            let _ = fs::remove_dir_all(&temp_dir);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported skill package type: {other}"),
            ));
        }
    }
    let root = find_skill_root(&temp_dir).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "skill package does not contain SKILL.md",
        )
    })?;
    replace_dir(dir)?;
    copy_dir_contents(&root, dir)?;
    let _ = fs::remove_dir_all(temp_dir);
    Ok(())
}

fn download_skill_package(url: &str) -> io::Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return fs::read(path);
    }
    if !url.contains("://") {
        return fs::read(url);
    }
    let response = ureq::get(url)
        .timeout(std::time::Duration::from_secs(60))
        .call()
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn verify_sha256(bytes: &[u8], expected: &str) -> io::Result<()> {
    use sha2::{Digest, Sha256};
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "skill package sha256 mismatch: expected {}, got {}",
                expected.trim(),
                actual
            ),
        ))
    }
}

fn infer_package_type(url: &str) -> &str {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        "tar.gz"
    } else {
        "zip"
    }
}

fn extract_zip(bytes: &[u8], dest: &Path) -> io::Result<()> {
    let cursor = Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).map_err(zip_err)?;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(zip_err)?;
        let Some(path) = safe_archive_path(file.name()) else {
            continue;
        };
        let out = dest.join(path);
        if file.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::File::create(&out)?;
        io::copy(&mut file, &mut output)?;
        apply_zip_permissions(&file, &out)?;
    }
    Ok(())
}

#[cfg(unix)]
fn apply_zip_permissions(file: &zip::read::ZipFile<'_>, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(mode) = file.unix_mode() {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_zip_permissions(_file: &zip::read::ZipFile<'_>, _path: &Path) -> io::Result<()> {
    Ok(())
}

fn extract_tar_gz(bytes: &[u8], dest: &Path) -> io::Result<()> {
    let gz = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(gz);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let Some(safe) = safe_path_components(&path) else {
            continue;
        };
        let out = dest.join(safe);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(out)?;
    }
    Ok(())
}

fn safe_archive_path(path: &str) -> Option<PathBuf> {
    safe_path_components(Path::new(path))
}

fn safe_path_components(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn find_skill_root(dir: &Path) -> Option<PathBuf> {
    let skill = dir.join("SKILL.md");
    if skill.exists() {
        return Some(dir.to_path_buf());
    }
    for entry in fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_skill_root(&path) {
                return Some(found);
            }
        }
    }
    None
}

fn copy_dir_contents(src: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_contents(&src_path, &dest_path)?;
        } else {
            fs::copy(&src_path, &dest_path)?;
            preserve_file_permissions(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

fn preserve_file_permissions(src: &Path, dest: &Path) -> io::Result<()> {
    fs::set_permissions(dest, fs::metadata(src)?.permissions())
}

fn replace_dir(dir: &Path) -> io::Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::create_dir_all(dir)
}

fn zip_err(error: zip::result::ZipError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn parses_enabled_marketplace_skills() {
        let marketplace = parse_marketplace(&json!({
            "skills": [
                { "id": "review", "name": "Review", "description": "Review code", "content": "body", "enabled": true },
                { "id": "off", "name": "Off", "description": "Hidden", "content": "body", "enabled": false },
                { "id": "pkg", "name": "Package", "description": "Package skill", "package_url": "https://example.com/skill.zip", "package_type": "zip", "enabled": true }
            ],
            "default_skill_ids": ["review", "off"]
        }))
        .unwrap();

        assert_eq!(marketplace.skills.len(), 2);
        assert_eq!(marketplace.skills[0].id, "review");
        assert_eq!(
            marketplace.skills[1].package_url.as_deref(),
            Some("https://example.com/skill.zip")
        );
        assert_eq!(marketplace.default_skill_ids, vec!["review", "off"]);
    }

    #[test]
    fn installs_skill_file() {
        let root = test_dir("jucode-marketplace-skill-test");
        let skill = MarketplaceSkill {
            id: "Code Review".to_string(),
            name: "Code Review".to_string(),
            description: "Review code".to_string(),
            content: "Be strict.".to_string(),
            package_url: None,
            package_sha256: None,
            package_type: None,
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

    #[test]
    fn extracts_zip_skill_package_contents() {
        let root = test_dir("jucode-zip-skill-test");
        let package = root.join("skill.zip");
        fs::create_dir_all(&root).unwrap();
        create_zip(
            &package,
            &[
                (
                    "bundle/SKILL.md",
                    "---\nname: packaged\ndescription: Packaged skill\n---\n\nUse script.",
                ),
                ("bundle/scripts/run.sh", "#!/bin/sh\necho ok\n"),
            ],
        );
        let skill = MarketplaceSkill {
            id: "packaged".to_string(),
            name: "Packaged".to_string(),
            description: "Packaged skill".to_string(),
            content: String::new(),
            package_url: Some(format!("file://{}", package.display())),
            package_sha256: None,
            package_type: Some("zip".to_string()),
            tags: vec![],
            enabled: true,
            updated_at: String::new(),
        };

        install_marketplace_skill(&root, &skill).unwrap();

        assert!(root.join("skills/packaged/SKILL.md").exists());
        assert_eq!(
            fs::read_to_string(root.join("skills/packaged/scripts/run.sh")).unwrap(),
            "#!/bin/sh\necho ok\n"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(root.join("skills/packaged/scripts/run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        let _ = fs::remove_dir_all(root);
    }

    fn test_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{}-{}",
            prefix,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn create_zip(path: &Path, files: &[(&str, &str)]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, content) in files {
            let opts = if name.ends_with(".sh") {
                zip::write::FileOptions::default().unix_permissions(0o755)
            } else {
                zip::write::FileOptions::default().unix_permissions(0o644)
            };
            zip.start_file(*name, opts).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
}
