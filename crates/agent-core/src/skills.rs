use flate2::read::GzDecoder;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Cursor, Read},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tar::Archive;
use zip::ZipArchive;

const MAX_PACKAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 100 * 1024 * 1024;
const MAX_PACKAGE_FILES: usize = 4096;
const SKILL_STATE_FILE: &str = "skills-state.json";
pub const ANTHROPIC_SKILLS_URL: &str = "https://github.com/anthropics/skills";
const ANTHROPIC_SKILLS_INDEX: &str = include_str!("anthropic-skills-index.json");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraSkill {
    pub id: String,
    pub path: String,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedSkill {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraSkillSource {
    pub name: String,
    pub repository: String,
    pub revision: String,
    pub skills: Vec<ExtraSkill>,
    pub excluded: Vec<ExcludedSkill>,
}

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

pub fn fetch_extra_skill_source(spec: &str) -> Result<Option<ExtraSkillSource>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(None);
    }
    if spec == "anthropic" || normalize_repository_url(spec) == ANTHROPIC_SKILLS_URL {
        let value = serde_json::from_str(ANTHROPIC_SKILLS_INDEX)
            .map_err(|error| format!("invalid bundled Anthropic skills index: {error}"))?;
        return parse_extra_skill_index(&value).map(Some);
    }

    let (owner, repository) = github_repository_parts(spec)?;
    let api_url = format!("https://api.github.com/repos/{owner}/{repository}/contents/skills");
    let response = ureq::get(&api_url)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "jucode-cli")
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|error| error.to_string())?;
    let value = response
        .into_json::<Value>()
        .map_err(|error| error.to_string())?;
    parse_github_skills_directory(&value, &format!("https://github.com/{owner}/{repository}"))
        .map(Some)
}

pub fn install_extra_skill(
    profile_dir: &Path,
    source: &ExtraSkillSource,
    skill: &ExtraSkill,
) -> io::Result<()> {
    validate_source_skill(skill)?;
    let (owner, repository) = github_repository_parts(&source.repository)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let revision = if source.revision.is_empty() {
        "HEAD"
    } else {
        validate_revision(&source.revision)?;
        source.revision.as_str()
    };
    let url = format!(
        "https://raw.githubusercontent.com/{owner}/{repository}/{revision}/{}",
        skill.path
    );
    let bytes = download_skill_package(&url)?;
    if let Some(expected) = skill.sha256.as_deref() {
        verify_sha256(&bytes, expected)?;
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "SKILL.md is not UTF-8"))?;
    if !content.trim_start().starts_with("---") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "downloaded SKILL.md is missing frontmatter",
        ));
    }
    let marketplace_skill = MarketplaceSkill {
        id: skill.id.clone(),
        name: skill.id.clone(),
        description: format!("Skill from {}", source.name),
        content,
        package_url: None,
        package_sha256: None,
        package_type: None,
        tags: Vec::new(),
        enabled: true,
        updated_at: String::new(),
    };
    install_marketplace_skill(profile_dir, &marketplace_skill)
}

pub fn parse_extra_skill_index(value: &Value) -> Result<ExtraSkillSource, String> {
    let name = read_string(value, "name").ok_or_else(|| "source index missing name".to_string())?;
    let repository = read_string(value, "repository")
        .ok_or_else(|| "source index missing repository".to_string())?;
    github_repository_parts(&repository)?;
    let revision = read_string(value, "revision").unwrap_or_default();
    if !revision.is_empty() {
        validate_revision(&revision).map_err(|error| error.to_string())?;
    }
    let skill_values = value
        .get("skills")
        .and_then(Value::as_array)
        .ok_or_else(|| "source index missing skills".to_string())?;
    let mut skills = Vec::with_capacity(skill_values.len());
    for item in skill_values {
        let id = read_string(item, "id").ok_or_else(|| "source skill missing id".to_string())?;
        let path =
            read_string(item, "path").ok_or_else(|| format!("source skill {id} missing path"))?;
        let skill = ExtraSkill {
            id,
            path,
            sha256: read_string(item, "sha256"),
        };
        validate_source_skill(&skill).map_err(|error| error.to_string())?;
        if skills
            .iter()
            .any(|existing: &ExtraSkill| existing.id == skill.id)
        {
            return Err(format!("duplicate source skill id: {}", skill.id));
        }
        skills.push(skill);
    }
    let excluded = value
        .get("excluded")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| {
            let id =
                read_string(item, "id").ok_or_else(|| "excluded skill missing id".to_string())?;
            validate_skill_id(&id).map_err(|error| error.to_string())?;
            let reason = read_string(item, "reason")
                .ok_or_else(|| format!("excluded skill {id} missing reason"))?;
            Ok(ExcludedSkill { id, reason })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if excluded
        .iter()
        .any(|item| skills.iter().any(|skill| skill.id == item.id))
    {
        return Err("a source skill cannot be both available and excluded".to_string());
    }
    Ok(ExtraSkillSource {
        name,
        repository: normalize_repository_url(&repository),
        revision,
        skills,
        excluded,
    })
}

pub fn parse_github_skills_directory(
    value: &Value,
    repository: &str,
) -> Result<ExtraSkillSource, String> {
    github_repository_parts(repository)?;
    let entries = value
        .as_array()
        .ok_or_else(|| "GitHub skills directory response is not an array".to_string())?;
    let mut skills = Vec::new();
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) != Some("dir") {
            continue;
        }
        let Some(id) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        validate_skill_id(id).map_err(|error| error.to_string())?;
        skills.push(ExtraSkill {
            id: id.to_string(),
            path: format!("skills/{id}/SKILL.md"),
            sha256: None,
        });
    }
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(ExtraSkillSource {
        name: github_repository_parts(repository)?.1,
        repository: normalize_repository_url(repository),
        revision: String::new(),
        skills,
        excluded: Vec::new(),
    })
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
    set_skill_enabled(profile_dir, &skill.id, true)?;
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

pub fn uninstall_skill(profile_dir: &Path, id: &str) -> io::Result<bool> {
    let skills_dir = profile_dir.join("skills");
    let target = skills_dir.join(safe_skill_dir(id));
    if !target.exists() {
        return Ok(false);
    }
    ensure_direct_child(&skills_dir, &target)?;
    fs::remove_dir_all(&target)?;
    set_skill_enabled(profile_dir, id, true)?;
    Ok(true)
}

pub fn set_skill_enabled(profile_dir: &Path, id: &str, enabled: bool) -> io::Result<()> {
    let id = safe_skill_dir(id);
    let mut disabled = read_disabled_skills(profile_dir)?;
    if enabled {
        disabled.remove(&id);
    } else {
        disabled.insert(id);
    }
    write_disabled_skills(profile_dir, &disabled)
}

pub fn is_skill_path_enabled(profile_dir: &Path, path: &Path) -> io::Result<bool> {
    let root = profile_dir.join("skills");
    let Ok(relative) = path.strip_prefix(&root) else {
        return Ok(true);
    };
    let Some(Component::Normal(id)) = relative.components().next() else {
        return Ok(true);
    };
    let Some(id) = id.to_str() else {
        return Ok(false);
    };
    Ok(!read_disabled_skills(profile_dir)?.contains(id))
}

pub fn installed_skill_ids(profile_dir: &Path) -> io::Result<Vec<String>> {
    let root = profile_dir.join("skills");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let disabled = read_disabled_skills(profile_dir)?;
    let mut installed = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.path().join("SKILL.md").exists() {
            let id = entry.file_name().to_string_lossy().to_string();
            installed.push(if disabled.contains(&id) {
                format!("{id} (disabled)")
            } else {
                id
            });
        }
    }
    installed.sort();
    Ok(installed)
}

pub fn skill_installed(profile_dir: &Path, id: &str) -> bool {
    profile_dir
        .join("skills")
        .join(safe_skill_dir(id))
        .join("SKILL.md")
        .exists()
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
    let staging = staging_dir(dir, "inline");
    recreate_dir(&staging)?;
    if let Err(error) = fs::write(staging.join("SKILL.md"), normalized_content(skill)) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    atomic_replace_dir(&staging, dir)
}

fn install_skill_package(dir: &Path, skill: &MarketplaceSkill, url: &str) -> io::Result<()> {
    let expected = skill.package_sha256.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "marketplace package is missing required package_sha256",
        )
    })?;
    let bytes = download_skill_package(url)?;
    verify_sha256(&bytes, expected)?;
    let temp_dir = staging_dir(dir, "extract");
    recreate_dir(&temp_dir)?;
    let package_type = skill
        .package_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| infer_package_type(url));
    let extract_result = match package_type {
        "zip" => extract_zip(&bytes, &temp_dir),
        "tar.gz" | "tgz" => extract_tar_gz(&bytes, &temp_dir),
        other => {
            let _ = fs::remove_dir_all(&temp_dir);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported skill package type: {other}"),
            ));
        }
    };
    if let Err(error) = extract_result {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(error);
    }
    let Some(root) = find_skill_root(&temp_dir) else {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "skill package does not contain SKILL.md",
        ));
    };
    let staging = if root == temp_dir {
        temp_dir
    } else {
        let ready = staging_dir(dir, "ready");
        recreate_dir(&ready)?;
        if let Err(error) = copy_dir_contents(&root, &ready) {
            let _ = fs::remove_dir_all(&temp_dir);
            let _ = fs::remove_dir_all(&ready);
            return Err(error);
        }
        let _ = fs::remove_dir_all(&temp_dir);
        ready
    };
    atomic_replace_dir(&staging, dir)
}

fn download_skill_package(url: &str) -> io::Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return read_bounded(fs::File::open(path)?);
    }
    if !url.contains("://") {
        return read_bounded(fs::File::open(url)?);
    }
    let response = ureq::get(url)
        .timeout(std::time::Duration::from_secs(60))
        .call()
        .map_err(|error| io::Error::other(error.to_string()))?;
    read_bounded(response.into_reader())
}

fn read_bounded(reader: impl Read) -> io::Result<Vec<u8>> {
    let mut reader = reader.take((MAX_PACKAGE_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PACKAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("skill package exceeds {MAX_PACKAGE_BYTES} byte limit"),
        ));
    }
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
    if archive.len() > MAX_PACKAGE_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("skill package exceeds {MAX_PACKAGE_FILES} file limit"),
        ));
    }
    let mut extracted_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(zip_err)?;
        if let Some(mode) = file.unix_mode() {
            let file_type = mode & 0o170000;
            if file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "skill package links and special files are not allowed",
                ));
            }
        }
        extracted_bytes = extracted_bytes.saturating_add(file.size());
        if extracted_bytes > MAX_EXTRACTED_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("extracted skill exceeds {MAX_EXTRACTED_BYTES} byte limit"),
            ));
        }
        let path = safe_archive_path(file.name()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsafe path in skill package: {}", file.name()),
            )
        })?;
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
    let mut extracted_bytes = 0_u64;
    for (index, entry) in archive.entries()?.enumerate() {
        if index >= MAX_PACKAGE_FILES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("skill package exceeds {MAX_PACKAGE_FILES} file limit"),
            ));
        }
        let mut entry = entry?;
        let path = entry.path()?;
        let safe = safe_path_components(&path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsafe path in skill package: {}", path.display()),
            )
        })?;
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() && !entry_type.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "skill package links and special files are not allowed",
            ));
        }
        extracted_bytes = extracted_bytes.saturating_add(entry.header().size()?);
        if extracted_bytes > MAX_EXTRACTED_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("extracted skill exceeds {MAX_EXTRACTED_BYTES} byte limit"),
            ));
        }
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

fn recreate_dir(dir: &Path) -> io::Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::create_dir_all(dir)
}

fn staging_dir(dir: &Path, label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("skill");
    dir.with_file_name(format!(".{name}-{label}-{nonce}"))
}

fn atomic_replace_dir(staging: &Path, destination: &Path) -> io::Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "skill has no parent"))?;
    fs::create_dir_all(parent)?;
    let backup = staging_dir(destination, "backup");
    let had_destination = destination.exists();
    if had_destination {
        fs::rename(destination, &backup)?;
    }
    if let Err(error) = fs::rename(staging, destination) {
        if had_destination {
            let _ = fs::rename(&backup, destination);
        }
        let _ = fs::remove_dir_all(staging);
        return Err(error);
    }
    if had_destination {
        let _ = fs::remove_dir_all(backup);
    }
    Ok(())
}

fn ensure_direct_child(parent: &Path, child: &Path) -> io::Result<()> {
    if child.parent() == Some(parent) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill path escapes skills directory",
        ))
    }
}

fn read_disabled_skills(profile_dir: &Path) -> io::Result<BTreeSet<String>> {
    let path = profile_dir.join(SKILL_STATE_FILE);
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let content = fs::read_to_string(path)?;
    let value = serde_json::from_str::<Value>(&content).unwrap_or(Value::Null);
    Ok(value
        .get("disabled")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect())
}

fn write_disabled_skills(profile_dir: &Path, disabled: &BTreeSet<String>) -> io::Result<()> {
    fs::create_dir_all(profile_dir)?;
    let path = profile_dir.join(SKILL_STATE_FILE);
    let temp = profile_dir.join(format!(".{SKILL_STATE_FILE}.tmp"));
    let value = serde_json::json!({ "disabled": disabled });
    fs::write(
        &temp,
        format!("{}\n", serde_json::to_string_pretty(&value)?),
    )?;
    match fs::rename(&temp, &path) {
        Ok(()) => Ok(()),
        Err(error) if path.exists() => {
            let backup = profile_dir.join(format!(".{SKILL_STATE_FILE}.backup"));
            let _ = fs::remove_file(&backup);
            fs::rename(&path, &backup)?;
            if let Err(move_error) = fs::rename(&temp, &path) {
                let _ = fs::rename(&backup, &path);
                let _ = fs::remove_file(&temp);
                return Err(io::Error::new(
                    move_error.kind(),
                    format!("{error}; replacement failed: {move_error}"),
                ));
            }
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(temp);
            Err(error)
        }
    }
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

fn validate_source_skill(skill: &ExtraSkill) -> io::Result<()> {
    validate_skill_id(&skill.id)?;
    let expected = format!("skills/{}/SKILL.md", skill.id);
    if skill.path != expected
        || safe_archive_path(&skill.path).as_deref() != Some(Path::new(&expected))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe source path for skill {}: {}", skill.id, skill.path),
        ));
    }
    if let Some(hash) = skill.sha256.as_deref() {
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid sha256 for source skill {}", skill.id),
            ));
        }
    }
    Ok(())
}

fn validate_skill_id(id: &str) -> io::Result<()> {
    if id.is_empty()
        || id.starts_with('-')
        || id.ends_with('-')
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe source skill id: {id}"),
        ));
    }
    Ok(())
}

fn validate_revision(revision: &str) -> io::Result<&str> {
    if revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(revision)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source revision must be a 40-character Git commit SHA",
        ))
    }
}

fn normalize_repository_url(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

fn github_repository_parts(url: &str) -> Result<(String, String), String> {
    let normalized = normalize_repository_url(url);
    let path = normalized
        .strip_prefix("https://github.com/")
        .ok_or_else(|| "extra_skills_source must be an HTTPS GitHub repository URL".to_string())?;
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err("extra_skills_source must name one GitHub owner/repository".to_string());
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
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
    use sha2::Digest;
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
    fn parses_bundled_anthropic_index_and_excludes_document_skills() {
        let value = serde_json::from_str(ANTHROPIC_SKILLS_INDEX).unwrap();
        let source = parse_extra_skill_index(&value).unwrap();

        assert_eq!(source.name, "anthropic");
        assert_eq!(source.repository, ANTHROPIC_SKILLS_URL);
        assert_eq!(source.revision.len(), 40);
        assert!(source.skills.iter().any(|skill| skill.id == "mcp-builder"));
        for id in ["docx", "pdf", "pptx", "xlsx"] {
            assert!(!source.skills.iter().any(|skill| skill.id == id));
            assert!(source.excluded.iter().any(|skill| skill.id == id));
        }
    }

    #[test]
    fn source_index_rejects_unsafe_ids_paths_and_hashes() {
        let base = json!({
            "name": "test",
            "repository": "https://github.com/example/skills",
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "skills": [{ "id": "safe-skill", "path": "skills/safe-skill/SKILL.md" }]
        });
        assert!(parse_extra_skill_index(&base).is_ok());

        for (id, path) in [
            ("../escape", "skills/../escape/SKILL.md"),
            ("safe-skill", "../SKILL.md"),
            ("safe-skill", "skills/other/SKILL.md"),
            ("UPPER", "skills/UPPER/SKILL.md"),
        ] {
            let mut unsafe_index = base.clone();
            unsafe_index["skills"][0]["id"] = json!(id);
            unsafe_index["skills"][0]["path"] = json!(path);
            assert!(
                parse_extra_skill_index(&unsafe_index).is_err(),
                "{id}: {path}"
            );
        }

        let mut bad_hash = base;
        bad_hash["skills"][0]["sha256"] = json!("not-a-sha");
        assert!(parse_extra_skill_index(&bad_hash).is_err());
    }

    #[test]
    fn parses_github_directory_entries_without_accepting_unsafe_names() {
        let source = parse_github_skills_directory(
            &json!([
                { "name": "review", "type": "dir" },
                { "name": "README.md", "type": "file" }
            ]),
            "https://github.com/example/skills",
        )
        .unwrap();
        assert_eq!(source.name, "skills");
        assert_eq!(source.skills.len(), 1);
        assert_eq!(source.skills[0].path, "skills/review/SKILL.md");

        assert!(parse_github_skills_directory(
            &json!([{ "name": "../escape", "type": "dir" }]),
            "https://github.com/example/skills",
        )
        .is_err());
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
        let package_hash = format!("{:x}", sha2::Sha256::digest(fs::read(&package).unwrap()));
        let skill = MarketplaceSkill {
            id: "packaged".to_string(),
            name: "Packaged".to_string(),
            description: "Packaged skill".to_string(),
            content: String::new(),
            package_url: Some(format!("file://{}", package.display())),
            package_sha256: Some(package_hash),
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

    #[test]
    fn package_install_requires_sha256_and_preserves_existing_skill() {
        let root = test_dir("jucode-package-sha-test");
        let package = root.join("skill.zip");
        let installed = root.join("skills/packaged");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("SKILL.md"), "old content").unwrap();
        create_zip(
            &package,
            &[(
                "SKILL.md",
                "---\nname: packaged\ndescription: New\n---\nnew",
            )],
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

        let error = install_marketplace_skill(&root, &skill).unwrap_err();

        assert!(error.to_string().contains("package_sha256"));
        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            "old content"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsafe_archive_path_is_rejected_without_replacing_existing_skill() {
        let root = test_dir("jucode-package-escape-test");
        let package = root.join("skill.zip");
        let installed = root.join("skills/packaged");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("SKILL.md"), "old content").unwrap();
        create_zip(
            &package,
            &[
                (
                    "SKILL.md",
                    "---\nname: packaged\ndescription: New\n---\nnew",
                ),
                ("../escape.sh", "bad"),
            ],
        );
        let hash = format!("{:x}", sha2::Sha256::digest(fs::read(&package).unwrap()));
        let skill = MarketplaceSkill {
            id: "packaged".to_string(),
            name: "Packaged".to_string(),
            description: "Packaged skill".to_string(),
            content: String::new(),
            package_url: Some(format!("file://{}", package.display())),
            package_sha256: Some(hash),
            package_type: Some("zip".to_string()),
            tags: vec![],
            enabled: true,
            updated_at: String::new(),
        };

        let error = install_marketplace_skill(&root, &skill).unwrap_err();

        assert!(error.to_string().contains("unsafe path"));
        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            "old content"
        );
        assert!(!root.join("escape.sh").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lifecycle_enable_disable_and_uninstall_updates_state() {
        let root = test_dir("jucode-skill-lifecycle-test");
        let installed = root.join("skills/review");
        fs::create_dir_all(&installed).unwrap();
        fs::write(
            installed.join("SKILL.md"),
            "---\nname: review\ndescription: Review\n---\n",
        )
        .unwrap();

        assert!(skill_installed(&root, "review"));
        assert!(is_skill_path_enabled(&root, &installed.join("SKILL.md")).unwrap());
        set_skill_enabled(&root, "review", false).unwrap();
        assert!(!is_skill_path_enabled(&root, &installed.join("SKILL.md")).unwrap());
        assert_eq!(installed_skill_ids(&root).unwrap(), ["review (disabled)"]);
        set_skill_enabled(&root, "review", true).unwrap();
        assert!(is_skill_path_enabled(&root, &installed.join("SKILL.md")).unwrap());
        assert!(uninstall_skill(&root, "review").unwrap());
        assert!(!skill_installed(&root, "review"));
        assert!(!uninstall_skill(&root, "review").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_download_has_a_hard_size_limit() {
        let error = read_bounded(Cursor::new(vec![0_u8; MAX_PACKAGE_BYTES + 1])).unwrap_err();
        assert!(error.to_string().contains("byte limit"));
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
