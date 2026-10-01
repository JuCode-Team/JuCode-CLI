use flate2::read::GzDecoder;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Cursor, Read},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tar::Archive;
use zip::ZipArchive;

/// Limit for one download: a package, a single skill file.
const MAX_PACKAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_TREE_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 100 * 1024 * 1024;
const MAX_PACKAGE_FILES: usize = 4096;
const SKILL_STATE_FILE: &str = "skills-state.json";
pub const ANTHROPIC_SKILLS_URL: &str = "https://github.com/anthropics/skills";
const ANTHROPIC_SKILLS_INDEX: &str = include_str!("anthropic-skills.json");

/// A skill in a GitHub repository's `skills/<id>/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub license: String,
    /// False for source-available skills whose terms forbid redistribution.
    pub redistributable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSource {
    pub name: String,
    pub repository: String,
    /// A commit SHA; empty follows the default branch.
    pub revision: String,
    pub skills: Vec<SourceSkill>,
}

impl SkillSource {
    pub fn homepage(&self, id: &str) -> String {
        format!("{}/tree/{}/skills/{id}", self.repository, self.git_ref())
    }

    fn git_ref(&self) -> &str {
        if self.revision.is_empty() {
            "HEAD"
        } else {
            &self.revision
        }
    }
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
    let mut request = ureq::get(&url).timeout(Duration::from_secs(30));
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        request = request.set("Authorization", &format!("Bearer {key}"));
    }
    let response = request.call().map_err(|error| error.to_string())?;
    let value = response
        .into_json::<Value>()
        .map_err(|error| error.to_string())?;
    parse_marketplace(&value)
}

/// The marketplace for the configured JuCode API. The endpoint is public, so
/// without a JuCode login it is asked without a token.
pub fn fetch_jucode_marketplace() -> Result<Marketplace, String> {
    let config = crate::config::Config::load_or_create().map_err(|error| error.to_string())?;
    let auth = crate::oauth::ensure_session(&config.jucode_api_url, config.encrypt_secrets).ok();
    fetch_marketplace(
        &config.jucode_api_url,
        auth.as_ref().and_then(|auth| auth.jucode_access_token()),
    )
}

/// Where JuCode loads installed user skills from: `~/.jucode/skills`.
pub fn profile_skills_dir() -> io::Result<PathBuf> {
    Ok(crate::config::profile_dir()?.join("skills"))
}

/// The bundled index of github.com/anthropics/skills, pinned to a reviewed
/// commit so listing needs no GitHub request.
pub fn anthropic_source() -> Result<SkillSource, String> {
    let value = serde_json::from_str(ANTHROPIC_SKILLS_INDEX)
        .map_err(|error| format!("invalid bundled Anthropic skills index: {error}"))?;
    parse_source_index(&value)
}

pub fn fetch_extra_skill_source(spec: &str) -> Result<Option<SkillSource>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(None);
    }
    if spec == "anthropic" || normalize_repository_url(spec) == ANTHROPIC_SKILLS_URL {
        return anthropic_source().map(Some);
    }

    let (owner, repository) = github_repository_parts(spec)?;
    let api_url = format!("https://api.github.com/repos/{owner}/{repository}/contents/skills");
    let response = ureq::get(&api_url)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "jucode-cli")
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|error| error.to_string())?;
    let value = response
        .into_json::<Value>()
        .map_err(|error| error.to_string())?;
    parse_github_skills_directory(&value, &format!("https://github.com/{owner}/{repository}"))
        .map(Some)
}

fn parse_source_index(value: &Value) -> Result<SkillSource, String> {
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
    let mut skills: Vec<SourceSkill> = Vec::with_capacity(skill_values.len());
    for item in skill_values {
        let id = read_string(item, "id").ok_or_else(|| "source skill missing id".to_string())?;
        validate_skill_id(&id).map_err(|error| error.to_string())?;
        let field = |key: &str| {
            read_string(item, key).ok_or_else(|| format!("source skill {id} missing {key}"))
        };
        let skill = SourceSkill {
            name: field("name")?,
            description: field("description")?,
            tags: read_strings(item, "tags"),
            license: field("license")?,
            redistributable: item
                .get("redistributable")
                .and_then(Value::as_bool)
                .ok_or_else(|| format!("source skill {id} missing redistributable"))?,
            id,
        };
        if skills.iter().any(|existing| existing.id == skill.id) {
            return Err(format!("duplicate source skill id: {}", skill.id));
        }
        skills.push(skill);
    }
    Ok(SkillSource {
        name,
        repository: normalize_repository_url(&repository),
        revision,
        skills,
    })
}

pub fn parse_github_skills_directory(
    value: &Value,
    repository: &str,
) -> Result<SkillSource, String> {
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
        skills.push(SourceSkill {
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            tags: Vec::new(),
            license: String::new(),
            redistributable: true,
        });
    }
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(SkillSource {
        name: github_repository_parts(repository)?.1,
        repository: normalize_repository_url(repository),
        revision: String::new(),
        skills,
    })
}

/// Installs the whole `skills/<id>/` directory of `source` into
/// `skills_dir/<id>`, replacing an earlier install only once every file has
/// downloaded.
pub fn install_source_skill(
    skills_dir: &Path,
    source: &SkillSource,
    skill: &SourceSkill,
) -> io::Result<PathBuf> {
    install_github_skill(skills_dir, source, skill, &download)
}

fn install_github_skill(
    skills_dir: &Path,
    source: &SkillSource,
    skill: &SourceSkill,
    fetch: &dyn Fn(&str, usize) -> io::Result<Vec<u8>>,
) -> io::Result<PathBuf> {
    validate_skill_id(&skill.id)?;
    let (owner, repository) = github_repository_parts(&source.repository)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if !source.revision.is_empty() {
        validate_revision(&source.revision)?;
    }
    let git_ref = source.git_ref();
    let tree = fetch(
        &format!(
            "https://api.github.com/repos/{owner}/{repository}/git/trees/{git_ref}?recursive=1"
        ),
        MAX_TREE_BYTES,
    )?;
    let tree = serde_json::from_slice::<Value>(&tree)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if tree.get("truncated").and_then(Value::as_bool) == Some(true) {
        return Err(invalid_data(format!(
            "GitHub returned a truncated tree for {}",
            source.repository
        )));
    }

    let prefix = format!("skills/{}/", skill.id);
    let mut files = Vec::new();
    let mut declared_bytes = 0_u64;
    for entry in tree
        .get("tree")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_data("GitHub tree response missing tree".to_string()))?
    {
        let path = entry
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(relative) = path.strip_prefix(&prefix) else {
            continue;
        };
        let relative = safe_path_components(Path::new(relative))
            .ok_or_else(|| invalid_data(format!("unsafe path in GitHub tree: {path}")))?;
        let mode = entry
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match entry.get("type").and_then(Value::as_str) {
            Some("tree") => continue,
            Some("blob") if mode == "100644" || mode == "100755" => {}
            _ => {
                return Err(invalid_data(format!(
                    "unsupported GitHub tree entry: {path}"
                )))
            }
        }
        let size = entry
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_data(format!("GitHub tree entry has no size: {path}")))?;
        if size > MAX_PACKAGE_BYTES as u64 {
            return Err(invalid_data(format!(
                "skill file exceeds {MAX_PACKAGE_BYTES} byte limit: {path}"
            )));
        }
        declared_bytes = declared_bytes.saturating_add(size);
        if declared_bytes > MAX_EXTRACTED_BYTES {
            return Err(invalid_data(format!(
                "skill exceeds {MAX_EXTRACTED_BYTES} byte limit"
            )));
        }
        files.push((path.to_string(), relative, mode == "100755"));
        if files.len() > MAX_PACKAGE_FILES {
            return Err(invalid_data(format!(
                "skill exceeds {MAX_PACKAGE_FILES} file limit"
            )));
        }
    }
    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "skill files not found in {}: {}",
                source.repository, skill.id
            ),
        ));
    }

    let dir = skills_dir.join(&skill.id);
    let staging = staging_dir(&dir, "download");
    recreate_dir(&staging)?;
    let downloaded = (|| {
        let mut actual_bytes = 0_u64;
        for (path, relative, executable) in files {
            let url = format!(
                "https://raw.githubusercontent.com/{owner}/{repository}/{git_ref}/{}",
                encode_url_path(&path)
            );
            let bytes = fetch(&url, MAX_PACKAGE_BYTES)?;
            actual_bytes = actual_bytes.saturating_add(bytes.len() as u64);
            if actual_bytes > MAX_EXTRACTED_BYTES {
                return Err(invalid_data(format!(
                    "skill exceeds {MAX_EXTRACTED_BYTES} byte limit"
                )));
            }
            let out = staging.join(relative);
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&out, bytes)?;
            set_mode(&out, Some(if executable { 0o755 } else { 0o644 }))?;
        }
        if !staging.join("SKILL.md").is_file() {
            return Err(invalid_data(format!(
                "skill {} does not contain SKILL.md",
                skill.id
            )));
        }
        Ok(())
    })();
    if let Err(error) = downloaded {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    atomic_replace_dir(&staging, &dir)?;
    Ok(dir)
}

/// Installs a marketplace skill into `skills_dir`; its enabled state (a
/// JuCode profile's `skills-state.json`) is the caller's.
pub fn install_marketplace_skill(
    skills_dir: &Path,
    skill: &MarketplaceSkill,
) -> io::Result<PathBuf> {
    let dir = skills_dir.join(safe_skill_dir(&skill.id));
    if let Some(url) = skill
        .package_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        install_skill_package(&dir, skill, url)?;
    } else {
        install_inline_skill(&dir, skill)?;
    }
    Ok(dir)
}

pub fn install_default_skills(profile_dir: &Path, marketplace: &Marketplace) -> io::Result<usize> {
    let mut installed = 0;
    for id in &marketplace.default_skill_ids {
        if let Some(skill) = marketplace.skills.iter().find(|skill| &skill.id == id) {
            install_marketplace_skill(&profile_dir.join("skills"), skill)?;
            set_skill_enabled(profile_dir, &skill.id, true)?;
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

pub fn skill_installed(skills_dir: &Path, id: &str) -> bool {
    skills_dir
        .join(safe_skill_dir(id))
        .join("SKILL.md")
        .is_file()
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
    Ok(Marketplace {
        skills,
        default_skill_ids: read_strings(value, "default_skill_ids"),
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
        tags: read_strings(value, "tags"),
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

fn read_strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
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
    let bytes = download(url, MAX_PACKAGE_BYTES)?;
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

fn download(url: &str, limit: usize) -> io::Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return read_bounded(fs::File::open(path)?, limit);
    }
    if !url.contains("://") {
        return read_bounded(fs::File::open(url)?, limit);
    }
    let response = ureq::get(url)
        .set("User-Agent", "jucode")
        .timeout(Duration::from_secs(60))
        .call()
        .map_err(|error| io::Error::other(error.to_string()))?;
    read_bounded(response.into_reader(), limit)
}

fn read_bounded(reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut reader = reader.take((limit + 1) as u64);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid_data(format!("download exceeds {limit} byte limit")));
    }
    Ok(bytes)
}

fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
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
        let path = safe_path_components(Path::new(file.name())).ok_or_else(|| {
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
        set_mode(&out, file.unix_mode())?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>) -> io::Result<()> {
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

fn encode_url_path(path: &str) -> String {
    let mut output = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
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
    fn bundled_anthropic_index_is_pinned_and_marks_document_skills() {
        let source = anthropic_source().unwrap();

        assert_eq!(source.name, "anthropic");
        assert_eq!(source.repository, ANTHROPIC_SKILLS_URL);
        assert_eq!(source.revision.len(), 40);
        let skill = |id: &str| source.skills.iter().find(|skill| skill.id == id).unwrap();
        assert!(skill("mcp-builder").redistributable);
        assert!(!skill("mcp-builder").description.is_empty());
        let restricted = source
            .skills
            .iter()
            .filter(|skill| !skill.redistributable)
            .map(|skill| skill.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(restricted, ["docx", "pdf", "pptx", "xlsx"]);
        assert_eq!(
            source.homepage("pdf"),
            format!("{ANTHROPIC_SKILLS_URL}/tree/{}/skills/pdf", source.revision)
        );
    }

    #[test]
    fn source_index_rejects_unsafe_ids_and_incomplete_entries() {
        let base = json!({
            "name": "test",
            "repository": "https://github.com/example/skills",
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "skills": [{
                "id": "safe-skill", "name": "Safe", "description": "Safe skill",
                "license": "MIT", "redistributable": true
            }]
        });
        assert!(parse_source_index(&base).is_ok());

        for id in ["../escape", "nested/escape", "UPPER", "-dash"] {
            let mut unsafe_index = base.clone();
            unsafe_index["skills"][0]["id"] = json!(id);
            assert!(parse_source_index(&unsafe_index).is_err(), "{id}");
        }
        let mut no_license_flag = base.clone();
        no_license_flag["skills"][0]
            .as_object_mut()
            .unwrap()
            .remove("redistributable");
        assert!(parse_source_index(&no_license_flag).is_err());
        let mut branch = base;
        branch["revision"] = json!("main");
        assert!(parse_source_index(&branch).is_err());
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
        assert_eq!(source.skills[0].id, "review");
        assert_eq!(
            source.homepage("review"),
            "https://github.com/example/skills/tree/HEAD/skills/review"
        );

        assert!(parse_github_skills_directory(
            &json!([{ "name": "../escape", "type": "dir" }]),
            "https://github.com/example/skills",
        )
        .is_err());
    }

    #[test]
    fn installs_the_whole_github_skill_directory_at_the_pinned_revision() {
        let root = test_dir("jucode-github-skill-test");
        let source = github_source();
        let tree = json!({ "truncated": false, "tree": [
            { "path": "skills/demo", "type": "tree", "mode": "040000" },
            { "path": "skills/demo/SKILL.md", "type": "blob", "mode": "100644", "size": 30 },
            { "path": "skills/demo/scripts", "type": "tree", "mode": "040000" },
            { "path": "skills/demo/scripts/run me.sh", "type": "blob", "mode": "100755", "size": 9 },
            { "path": "skills/other/SKILL.md", "type": "blob", "mode": "100644", "size": 5 }
        ]});
        let revision = source.revision.clone();
        let fetched = std::cell::RefCell::new(Vec::new());
        let fetch = |url: &str, _limit: usize| -> io::Result<Vec<u8>> {
            fetched.borrow_mut().push(url.to_string());
            let raw = format!("https://raw.githubusercontent.com/example/skills/{revision}/");
            match url.strip_prefix(&raw) {
                None => Ok(tree.to_string().into_bytes()),
                Some("skills/demo/SKILL.md") => Ok(b"---\nname: demo\n---\n".to_vec()),
                Some("skills/demo/scripts/run%20me.sh") => Ok(b"echo ok\n".to_vec()),
                Some(other) => panic!("unexpected download {other}"),
            }
        };

        let dir = install_github_skill(&root, &source, &source.skills[0], &fetch).unwrap();

        assert_eq!(dir, root.join("demo"));
        assert_eq!(
            fetched.borrow()[0],
            format!("https://api.github.com/repos/example/skills/git/trees/{revision}?recursive=1")
        );
        assert_eq!(fetched.borrow().len(), 3);
        assert!(skill_installed(&root, "demo"));
        assert_eq!(
            fs::read_to_string(dir.join("scripts/run me.sh")).unwrap(),
            "echo ok\n"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(dir.join("scripts/run me.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn github_install_rejects_escaping_paths_and_keeps_the_existing_skill() {
        let root = test_dir("jucode-github-escape-test");
        let installed = root.join("demo");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("SKILL.md"), "old content").unwrap();
        let source = github_source();
        for (path, kind) in [
            ("skills/demo/../../escape", "blob"),
            ("skills/demo/link", "commit"),
        ] {
            let tree = json!({ "tree": [
                { "path": "skills/demo/SKILL.md", "type": "blob", "mode": "100644", "size": 3 },
                { "path": path, "type": kind, "mode": "100644", "size": 3 }
            ]});
            let fetch = |_: &str, _: usize| Ok(tree.to_string().into_bytes());

            let error = install_github_skill(&root, &source, &source.skills[0], &fetch)
                .unwrap_err()
                .to_string();

            assert!(error.contains(path), "{error}");
        }
        let no_skill_file = json!({ "tree": [
            { "path": "skills/demo/README.md", "type": "blob", "mode": "100644", "size": 3 }
        ]});
        let fetch = |url: &str, _: usize| {
            Ok(if url.contains("api.github.com") {
                no_skill_file.to_string().into_bytes()
            } else {
                b"doc".to_vec()
            })
        };
        let error = install_github_skill(&root, &source, &source.skills[0], &fetch).unwrap_err();
        assert!(error.to_string().contains("SKILL.md"), "{error}");
        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            "old content"
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        let _ = fs::remove_dir_all(root);
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

        let dir = install_marketplace_skill(&root.join("skills"), &skill).unwrap();

        assert_eq!(dir, root.join("skills").join("code-review"));
        let content = fs::read_to_string(dir.join("SKILL.md")).unwrap();
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

        install_marketplace_skill(&root.join("skills"), &skill).unwrap();

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

        let error = install_marketplace_skill(&root.join("skills"), &skill).unwrap_err();

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

        let error = install_marketplace_skill(&root.join("skills"), &skill).unwrap_err();

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

        assert!(skill_installed(&root.join("skills"), "review"));
        assert!(is_skill_path_enabled(&root, &installed.join("SKILL.md")).unwrap());
        set_skill_enabled(&root, "review", false).unwrap();
        assert!(!is_skill_path_enabled(&root, &installed.join("SKILL.md")).unwrap());
        assert_eq!(installed_skill_ids(&root).unwrap(), ["review (disabled)"]);
        set_skill_enabled(&root, "review", true).unwrap();
        assert!(is_skill_path_enabled(&root, &installed.join("SKILL.md")).unwrap());
        assert!(uninstall_skill(&root, "review").unwrap());
        assert!(!skill_installed(&root.join("skills"), "review"));
        assert!(!uninstall_skill(&root, "review").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_download_has_a_hard_size_limit() {
        let error = read_bounded(
            Cursor::new(vec![0_u8; MAX_PACKAGE_BYTES + 1]),
            MAX_PACKAGE_BYTES,
        )
        .unwrap_err();
        assert!(error.to_string().contains("byte limit"));
    }

    fn github_source() -> SkillSource {
        SkillSource {
            name: "example".to_string(),
            repository: "https://github.com/example/skills".to_string(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
            skills: vec![SourceSkill {
                id: "demo".to_string(),
                name: "Demo".to_string(),
                description: "Demo skill".to_string(),
                tags: Vec::new(),
                license: "MIT".to_string(),
                redistributable: true,
            }],
        }
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
