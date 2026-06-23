use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
};

const LEGACY_DEFAULT_SYSTEM_PROMPT: &str = r#"You are JuCode, a focused coding agent.

Work with care before speed. Understand the task and the existing code before making changes. If the request is ambiguous or a key detail cannot be inferred safely, say so and ask a concise question. If there are multiple reasonable approaches, surface the tradeoff briefly.

Prefer the smallest change that correctly solves the problem. Avoid speculative features, unnecessary abstraction, broad refactors, and hidden compatibility layers unless they are explicitly needed. Match the project's existing structure, style, naming, and conventions.

Fix root causes rather than symptoms. Do not hide problems with silent fallback behavior or vague recovery paths. Use defensive programming only when the boundary is real and relevant.

Use tools when you need filesystem, search, shell, or verification access. Inspect before editing. Keep edits scoped to the affected files, and do not modify unrelated work.

Be accurate. Do not fabricate facts about APIs, tools, commands, or the codebase. When uncertain, verify from reliable sources or state the uncertainty clearly.

Verify before claiming completion. Use the smallest meaningful checks for the change, such as focused tests, builds, formatters, or linters. If verification is not possible, report that plainly.

For user-facing work, preserve the existing product language and design system. Build coherent, useful interfaces without fake data, decorative filler, or new visual styles unless requested.

Communicate directly and concisely. Report behavior-level changes, important risks, verification results, and any remaining gaps."#;

pub const DEFAULT_SYSTEM_PROMPT: &str = r#"You are JuCode, a focused coding agent.

Work with care before speed. Understand the task and the existing code before making changes. If the request is ambiguous or a key detail cannot be inferred safely, say so and ask a concise question. If there are multiple reasonable approaches, surface the tradeoff briefly.

Autonomy and persistence: for implementation, debugging, and evaluation tasks, assume the user wants the work completed end-to-end in the current turn whenever feasible. Do not stop at analysis, repo exploration, a partial patch, or a failed tool call. Continue through implementation, focused verification, and a clear final report unless the user explicitly asks only for a plan or redirects you.

Prefer the smallest change that correctly solves the problem. Avoid speculative features, unnecessary abstraction, broad refactors, and hidden compatibility layers unless they are explicitly needed. Match the project's existing structure, style, naming, and conventions.

Fix root causes rather than symptoms. Do not hide problems with silent fallback behavior or vague recovery paths. Use defensive programming only when the boundary is real and relevant.

Use tools when you need filesystem, search, shell, or verification access. Inspect before editing. Logically group related actions: when multiple read-only searches, listings, file reads, or shell inspections are independent, call them together in one assistant response; keep dependent edit-after-read and verify-after-edit steps ordered. Keep edits scoped to the affected files, and do not modify unrelated work. If a tool call fails, read the error, correct the call or use another suitable tool, and keep going when the task is still feasible.

For greenfield tasks in an empty or minimal repository, create the required project skeleton instead of stopping after inspection. Add the expected manifest/config, source entrypoints, and test files for the requested language or framework before verifying.

Be accurate. Do not fabricate facts about APIs, tools, commands, or the codebase. When uncertain, verify from reliable sources or state the uncertainty clearly.

Verify before claiming completion. Use the smallest meaningful checks for the change, such as focused tests, builds, formatters, or linters. If verification fails, inspect the failure, fix the likely cause, and rerun the focused check before ending. If verification is not possible, report that plainly.

Finish gate: do not end an implementation task after only listing or reading files, and do not report success without either relevant file changes or a clear reason no change was needed. Do not end while required files are missing, a required contract is unimplemented, or the last relevant verification failed.

For user-facing work, preserve the existing product language and design system. Build coherent, useful interfaces without fake data, decorative filler, or new visual styles unless requested.

Communicate directly and concisely. Report behavior-level changes, important risks, verification results, and any remaining gaps."#;
const PROMPT_FILE_NAME: &str = "prompt.txt";
const DEFAULT_RETRY_ATTEMPTS: usize = 5;
const DEFAULT_CONNECT_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_READ_TIMEOUT_SECONDS: u64 = 300;
/// Absolute context-token cap that forces compaction regardless of window size.
/// 0 (default) disables it, so compaction is driven only by the per-model budget
/// (3/4 of the context window). Set a positive value to cap large-window models.
const DEFAULT_COMPACTION_THRESHOLD_TOKENS: u64 = 0;
const DEFAULT_COMPACT_REASONING_EFFORT: &str = "low";

#[derive(Debug, Clone)]
pub struct Config {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: String,
    pub compact_model: String,
    pub compact_reasoning_effort: String,
    pub models: Vec<ModelConfig>,
    pub base_url: String,
    pub jucode_web_url: String,
    pub jucode_api_url: String,
    pub api_key_env: String,
    pub retry_attempts: usize,
    pub connect_timeout_seconds: u64,
    pub read_timeout_seconds: u64,
    pub compaction_threshold_tokens: u64,
    pub include_project_instructions: bool,
    pub extensions: Vec<ExtensionConfig>,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ExtensionConfig {
    pub name: String,
    pub command: String,
    pub lazy: bool,
}

#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub name: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub reasoning_efforts: Vec<String>,
    /// USD price per 1M tokens. 0 means unknown, which suppresses cost display.
    pub input_cost: f64,
    pub cached_input_cost: f64,
    pub output_cost: f64,
}

impl ModelConfig {
    /// Cumulative USD cost for a turn's token usage. Returns 0 when no prices are
    /// configured for the model.
    pub fn cost_for(&self, input_tokens: u64, cached_input_tokens: u64, output_tokens: u64) -> f64 {
        let non_cached_input = input_tokens.saturating_sub(cached_input_tokens);
        (non_cached_input as f64 * self.input_cost
            + cached_input_tokens as f64 * self.cached_input_cost
            + output_tokens as f64 * self.output_cost)
            / 1_000_000.0
    }
}

#[derive(Debug, Clone)]
pub struct AuthStore {
    keys: BTreeMap<String, String>,
    path: PathBuf,
}

impl Config {
    pub fn load_or_create() -> io::Result<Self> {
        let path = config_path()?;
        ensure_system_prompt_file()?;
        if !path.exists() {
            let config = Self {
                provider: "openai".to_string(),
                model: "gpt-5".to_string(),
                reasoning_effort: "medium".to_string(),
                compact_model: "gpt-5".to_string(),
                compact_reasoning_effort: DEFAULT_COMPACT_REASONING_EFFORT.to_string(),
                models: default_model_configs(),
                base_url: "https://api.openai.com/v1".to_string(),
                jucode_web_url: "https://api.jucode.cn".to_string(),
                jucode_api_url: "https://api.jucode.cn".to_string(),
                api_key_env: "OPENAI_API_KEY".to_string(),
                retry_attempts: DEFAULT_RETRY_ATTEMPTS,
                connect_timeout_seconds: DEFAULT_CONNECT_TIMEOUT_SECONDS,
                read_timeout_seconds: DEFAULT_READ_TIMEOUT_SECONDS,
                compaction_threshold_tokens: DEFAULT_COMPACTION_THRESHOLD_TOKENS,
                include_project_instructions: true,
                extensions: Vec::new(),
                path,
            };
            config.save()?;
            return Ok(config);
        }

        let content = fs::read_to_string(&path)?;
        let value = serde_json::from_str::<Value>(&content).unwrap_or_else(|_| json!({}));
        let model = read_string(&value, "model", "gpt-5");
        let mut models = read_model_configs(&value);
        if !models.iter().any(|entry| entry.name == model) {
            models.insert(0, default_model_config(&model));
        }
        let reasoning_effort =
            read_reasoning_effort(&value, "reasoning_effort", "medium", &model, &models);
        let compact_model = read_string(&value, "compact_model", &model);
        let compact_reasoning_effort = read_reasoning_effort(
            &value,
            "compact_reasoning_effort",
            DEFAULT_COMPACT_REASONING_EFFORT,
            &compact_model,
            &models,
        );
        let legacy_jucode_url = read_string(&value, "jucode_base_url", "");
        let default_jucode_web_url =
            if legacy_jucode_url.is_empty() || legacy_jucode_url == "http://localhost:8090" {
                "https://api.jucode.cn"
            } else {
                &legacy_jucode_url
            };
        let default_jucode_api_url = if legacy_jucode_url.is_empty() {
            "https://api.jucode.cn"
        } else {
            &legacy_jucode_url
        };
        let config = Self {
            provider: read_string(&value, "provider", "openai"),
            model,
            reasoning_effort,
            compact_model,
            compact_reasoning_effort,
            models,
            base_url: normalize_base_url(&read_string(
                &value,
                "base_url",
                "https://api.openai.com/v1",
            )),
            jucode_web_url: normalize_base_url(&read_string(
                &value,
                "jucode_web_url",
                default_jucode_web_url,
            )),
            jucode_api_url: normalize_base_url(&read_string(
                &value,
                "jucode_api_url",
                default_jucode_api_url,
            )),
            api_key_env: read_api_key_env(&value),
            retry_attempts: read_usize(&value, "retry_attempts", DEFAULT_RETRY_ATTEMPTS),
            connect_timeout_seconds: read_u64(
                &value,
                "connect_timeout_seconds",
                DEFAULT_CONNECT_TIMEOUT_SECONDS,
            ),
            read_timeout_seconds: read_u64(
                &value,
                "read_timeout_seconds",
                DEFAULT_READ_TIMEOUT_SECONDS,
            ),
            compaction_threshold_tokens: read_u64(
                &value,
                "compaction_threshold_tokens",
                DEFAULT_COMPACTION_THRESHOLD_TOKENS,
            ),
            include_project_instructions: read_bool(&value, "include_project_instructions", true),
            extensions: read_extensions(&value),
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
            "reasoning_effort": self.reasoning_effort,
            "compact_model": self.compact_model,
            "compact_reasoning_effort": self.compact_reasoning_effort,
            "models": self.models.iter().map(model_config_value).collect::<Vec<_>>(),
            "base_url": normalize_base_url(&self.base_url),
            "jucode_web_url": normalize_base_url(&self.jucode_web_url),
            "jucode_api_url": normalize_base_url(&self.jucode_api_url),
            "api_key_env": self.api_key_env,
            "retry_attempts": self.retry_attempts,
            "connect_timeout_seconds": self.connect_timeout_seconds,
            "read_timeout_seconds": self.read_timeout_seconds,
            "compaction_threshold_tokens": self.compaction_threshold_tokens,
            "include_project_instructions": self.include_project_instructions,
            "extensions": self.extensions.iter().map(extension_config_value).collect::<Vec<_>>()
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

    pub fn current_model_config(&self) -> ModelConfig {
        self.model_config(&self.model)
    }

    pub fn compact_model_config(&self) -> ModelConfig {
        self.model_config(&self.compact_model)
    }

    fn model_config(&self, model: &str) -> ModelConfig {
        self.models
            .iter()
            .find(|entry| entry.name == model)
            .cloned()
            .unwrap_or_else(|| default_model_config(model))
    }

    pub fn system_prompt(&self) -> io::Result<String> {
        fs::read_to_string(system_prompt_path()?)
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

    pub fn set_key_for(&mut self, provider: &str, key: String) {
        self.keys.insert(provider.to_string(), key);
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

fn read_usize(value: &Value, key: &str, default: usize) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
}

fn read_u64(value: &Value, key: &str, default: u64) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn read_bool(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn read_f64(value: &Value, key: &str, default: f64) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(default)
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

fn read_reasoning_effort(
    value: &Value,
    key: &str,
    default: &str,
    model: &str,
    models: &[ModelConfig],
) -> String {
    let effort = read_string(value, key, default);
    let supported = models
        .iter()
        .find(|entry| entry.name == model)
        .map(|entry| entry.reasoning_efforts.as_slice())
        .unwrap_or(&[]);
    if supported.iter().any(|entry| entry == &effort) {
        effort
    } else if supported.iter().any(|entry| entry == "medium") {
        "medium".to_string()
    } else {
        supported
            .first()
            .cloned()
            .unwrap_or_else(|| "medium".to_string())
    }
}

fn read_model_configs(value: &Value) -> Vec<ModelConfig> {
    let Some(models) = value.get("models").and_then(Value::as_array) else {
        return default_model_configs();
    };

    let mut configs = Vec::new();
    for model in models {
        let Some(name) = model
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let reasoning_efforts = model
            .get("reasoning_efforts")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|values| !values.is_empty())
            .unwrap_or_else(default_reasoning_efforts);

        configs.push(ModelConfig {
            name: name.to_string(),
            context_window: model
                .get("context_window")
                .and_then(Value::as_u64)
                .unwrap_or(400_000),
            max_output_tokens: model
                .get("max_output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(128_000),
            reasoning_efforts,
            input_cost: read_f64(model, "input_cost", 0.0),
            cached_input_cost: read_f64(model, "cached_input_cost", 0.0),
            output_cost: read_f64(model, "output_cost", 0.0),
        });
    }

    if configs.is_empty() {
        default_model_configs()
    } else {
        configs
    }
}

fn read_extensions(value: &Value) -> Vec<ExtensionConfig> {
    let Some(extensions) = value.get("extensions").and_then(Value::as_array) else {
        return Vec::new();
    };
    extensions
        .iter()
        .filter_map(|extension| {
            let name = extension
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())?;
            let command = extension
                .get("command")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())?;
            Some(ExtensionConfig {
                name: name.to_string(),
                command: command.to_string(),
                lazy: read_bool(extension, "lazy", false),
            })
        })
        .collect()
}

fn model_config_value(model: &ModelConfig) -> Value {
    json!({
        "name": model.name,
        "context_window": model.context_window,
        "max_output_tokens": model.max_output_tokens,
        "reasoning_efforts": model.reasoning_efforts,
        "input_cost": model.input_cost,
        "cached_input_cost": model.cached_input_cost,
        "output_cost": model.output_cost,
    })
}

fn extension_config_value(extension: &ExtensionConfig) -> Value {
    json!({
        "name": extension.name,
        "command": extension.command,
        "lazy": extension.lazy,
    })
}

fn default_reasoning_efforts() -> Vec<String> {
    ["none", "low", "medium", "high", "xhigh"]
        .iter()
        .map(|value| value.to_string())
        .collect()
}

fn default_model_configs() -> Vec<ModelConfig> {
    [
        (
            "gpt-5.5",
            1_050_000,
            128_000,
            &["none", "low", "medium", "high", "xhigh"][..],
        ),
        (
            "gpt-5.4",
            1_050_000,
            128_000,
            &["none", "low", "medium", "high", "xhigh"],
        ),
        (
            "gpt-5.4-mini",
            400_000,
            128_000,
            &["none", "low", "medium", "high", "xhigh"],
        ),
        (
            "gpt-5.3-codex",
            400_000,
            128_000,
            &["low", "medium", "high", "xhigh"],
        ),
        (
            "gpt-5.2",
            400_000,
            128_000,
            &["none", "low", "medium", "high", "xhigh"],
        ),
    ]
    .iter()
    .map(
        |(name, context_window, max_output_tokens, reasoning_efforts)| ModelConfig {
            name: (*name).to_string(),
            context_window: *context_window,
            max_output_tokens: *max_output_tokens,
            reasoning_efforts: reasoning_efforts
                .iter()
                .map(|value| value.to_string())
                .collect(),
            input_cost: 0.0,
            cached_input_cost: 0.0,
            output_cost: 0.0,
        },
    )
    .collect()
}

fn default_model_config(name: &str) -> ModelConfig {
    default_model_configs()
        .into_iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| ModelConfig {
            name: name.to_string(),
            context_window: 400_000,
            max_output_tokens: 128_000,
            reasoning_efforts: default_reasoning_efforts(),
            input_cost: 0.0,
            cached_input_cost: 0.0,
            output_cost: 0.0,
        })
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

fn system_prompt_path() -> io::Result<PathBuf> {
    Ok(jucode_dir()?.join(PROMPT_FILE_NAME))
}

fn ensure_system_prompt_file() -> io::Result<()> {
    let path = system_prompt_path()?;
    if path.exists() {
        let content = fs::read_to_string(&path)?;
        if content.trim() == LEGACY_DEFAULT_SYSTEM_PROMPT.trim() {
            fs::write(path, format!("{DEFAULT_SYSTEM_PROMPT}\n"))?;
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{DEFAULT_SYSTEM_PROMPT}\n"))
}

fn jucode_dir() -> io::Result<PathBuf> {
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    Ok(PathBuf::from(home).join(".jucode"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_for_prices_cached_input_separately() {
        let model = ModelConfig {
            name: "m".to_string(),
            context_window: 1,
            max_output_tokens: 1,
            reasoning_efforts: vec![],
            input_cost: 2.0,
            cached_input_cost: 0.5,
            output_cost: 8.0,
        };
        // 1M non-cached input @2 + 1M cached @0.5 + 1M output @8 = 10.5
        let cost = model.cost_for(2_000_000, 1_000_000, 1_000_000);
        assert!((cost - 10.5).abs() < 1e-9, "cost was {cost}");
    }

    #[test]
    fn cost_for_zero_prices_is_free() {
        let model = default_model_config("gpt-5.4");
        assert_eq!(model.cost_for(1_000, 100, 1_000), 0.0);
    }

    #[test]
    fn compact_model_config_uses_compact_model() {
        let config = Config {
            provider: "openai".to_string(),
            model: "chat-model".to_string(),
            reasoning_effort: "medium".to_string(),
            compact_model: "compact-model".to_string(),
            compact_reasoning_effort: "low".to_string(),
            models: vec![
                ModelConfig {
                    name: "chat-model".to_string(),
                    context_window: 100,
                    max_output_tokens: 10,
                    reasoning_efforts: vec!["medium".to_string()],
                    input_cost: 0.0,
                    cached_input_cost: 0.0,
                    output_cost: 0.0,
                },
                ModelConfig {
                    name: "compact-model".to_string(),
                    context_window: 200,
                    max_output_tokens: 20,
                    reasoning_efforts: vec!["low".to_string()],
                    input_cost: 0.0,
                    cached_input_cost: 0.0,
                    output_cost: 0.0,
                },
            ],
            base_url: "https://api.openai.com/v1".to_string(),
            jucode_web_url: "https://api.jucode.cn".to_string(),
            jucode_api_url: "https://api.jucode.cn".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            connect_timeout_seconds: DEFAULT_CONNECT_TIMEOUT_SECONDS,
            read_timeout_seconds: DEFAULT_READ_TIMEOUT_SECONDS,
            compaction_threshold_tokens: DEFAULT_COMPACTION_THRESHOLD_TOKENS,
            include_project_instructions: true,
            extensions: Vec::new(),
            path: PathBuf::from("config.json"),
        };

        assert_eq!(config.current_model_config().max_output_tokens, 10);
        assert_eq!(config.compact_model_config().max_output_tokens, 20);
    }
}
