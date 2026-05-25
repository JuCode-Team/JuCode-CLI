use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Config {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AuthStore {
    keys: BTreeMap<String, String>,
    path: PathBuf,
}

impl Config {
    pub fn load_or_create() -> io::Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            let config = Self {
                provider: "openai".to_string(),
                model: "gpt-5".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                api_key_env: "OPENAI_API_KEY".to_string(),
                path,
            };
            config.save()?;
            return Ok(config);
        }

        let content = fs::read_to_string(&path)?;
        let value = serde_json::from_str::<Value>(&content).unwrap_or_else(|_| json!({}));
        let config = Self {
            provider: read_string(&value, "provider", "openai"),
            model: read_string(&value, "model", "gpt-5"),
            base_url: normalize_base_url(&read_string(
                &value,
                "base_url",
                "https://api.openai.com/v1",
            )),
            api_key_env: read_api_key_env(&value),
            path,
        };
        config.save()?;
        Ok(config)
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let value = json!({
            "provider": self.provider,
            "model": self.model,
            "base_url": normalize_base_url(&self.base_url),
            "api_key_env": self.api_key_env
        });
        fs::write(
            &self.path,
            format!("{}\n", serde_json::to_string_pretty(&value)?),
        )
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn profile_dir(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }
}

impl AuthStore {
    pub fn load_or_create() -> io::Result<Self> {
        let path = auth_path()?;
        if !path.exists() {
            let auth = Self {
                keys: BTreeMap::new(),
                path,
            };
            auth.save()?;
            return Ok(auth);
        }

        let content = fs::read_to_string(&path)?;
        let value = serde_json::from_str::<Value>(&content).unwrap_or_else(|_| json!({}));
        let keys = value
            .get("providers")
            .and_then(Value::as_object)
            .map(read_provider_keys)
            .unwrap_or_default();

        let auth = Self { keys, path };
        auth.save()?;
        Ok(auth)
    }

    pub fn key_for(&self, provider: &str) -> Option<&str> {
        self.keys.get(provider).map(String::as_str)
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let value = json!({ "providers": self.keys });
        fs::write(
            &self.path,
            format!("{}\n", serde_json::to_string_pretty(&value)?),
        )
    }
}

pub fn profile_dir() -> io::Result<PathBuf> {
    jucode_dir()
}

pub fn normalize_base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

fn read_string(value: &Value, key: &str, default: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(default)
        .to_string()
}

fn read_api_key_env(value: &Value) -> String {
    let raw = value
        .get("api_key_env")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("OPENAI_API_KEY");

    if raw.is_empty() || raw.starts_with("sk-") {
        "OPENAI_API_KEY".to_string()
    } else {
        raw.to_string()
    }
}

fn read_provider_keys(providers: &Map<String, Value>) -> BTreeMap<String, String> {
    let mut keys = BTreeMap::new();
    for (provider, value) in providers {
        if let Some(key) = value.as_str().map(str::trim).filter(|key| !key.is_empty()) {
            keys.insert(provider.to_string(), key.to_string());
        }
    }
    keys
}

fn config_path() -> io::Result<PathBuf> {
    Ok(jucode_dir()?.join("config.json"))
}

fn auth_path() -> io::Result<PathBuf> {
    Ok(jucode_dir()?.join("auth.json"))
}

fn jucode_dir() -> io::Result<PathBuf> {
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    Ok(PathBuf::from(home).join(".jucode"))
}
