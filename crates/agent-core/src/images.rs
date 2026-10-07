//! `generate_image`: OpenAI-compatible image generation on the session's
//! provider. `POST {base_url}/images/generations` (JSON) draws from a prompt;
//! with input images, `POST {base_url}/images/edits` (multipart) edits them.
//! Results are saved into the workspace under the same rules as `write`.

use crate::config::Config;
use crate::tools::{ToolExecutionEvent, ToolState};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use llm_provider_kit::Protocol;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const TOOL_NAME: &str = "generate_image";
/// Shown when the engine has not configured the tool (no turn started yet).
pub const UNAVAILABLE: &str = "generate_image is not available in this session";
const MAX_IMAGES: u64 = 4;
/// OpenAI's limits for edit inputs: up to 16 images of under 50 MB each.
const MAX_INPUT_IMAGES: usize = 16;
const MAX_INPUT_IMAGE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
/// Generation takes 60–120 s; a short chat read timeout must not cut it off.
const MIN_READ_TIMEOUT: Duration = Duration::from_secs(180);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
const SLUG_MAX_CHARS: usize = 40;

/// Endpoint, credentials and default model for `generate_image`, set on the
/// tool state at the start of every turn.
#[derive(Clone)]
pub struct ImageTools {
    base_url: String,
    api_key: String,
    model: String,
    /// Extra request headers per model name (gateway routing choices).
    headers: HashMap<String, Vec<(String, String)>>,
    connect_timeout: Duration,
    read_timeout: Duration,
}

impl std::fmt::Debug for ImageTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageTools")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl ImageTools {
    /// The tool's settings for this session, or why it cannot be offered:
    /// no image model, a provider without an OpenAI-style images endpoint,
    /// or no API key.
    pub(crate) fn from_config(
        config: &Config,
        api_key: Option<String>,
        headers: &HashMap<String, Vec<(String, String)>>,
    ) -> Result<Self, String> {
        // The catalog lists chat models; one named "*image*" there draws
        // through chat, which /images/* does not serve. Only a gateway's own
        // models (gpt-image-2 on a gateway, say) are picked without asking.
        let catalog = llm_provider_kit::omp::catalog().models(&config.provider);
        let model = resolve_model(
            None,
            &config.image_model,
            config
                .models
                .iter()
                .map(|model| model.name.as_str())
                .filter(|name| !catalog.iter().any(|entry| entry.id == *name)),
        )?;
        check_endpoint(config, &model)?;
        let api_key = api_key
            .filter(|key| !key.trim().is_empty())
            .or_else(|| env::var(&config.api_key_env).ok())
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .ok_or_else(|| format!("generate_image needs an API key for {}", config.provider))?;
        Ok(Self {
            base_url: config.base_url.trim().trim_end_matches('/').to_string(),
            api_key,
            model,
            headers: headers.clone(),
            connect_timeout: Duration::from_secs(config.connect_timeout_seconds),
            read_timeout: Duration::from_secs(config.read_timeout_seconds).max(MIN_READ_TIMEOUT),
        })
    }
}

/// The image model: the call's `model`, else `image_model` from config.json,
/// else the first configured model whose name contains "image".
pub(crate) fn resolve_model<'a>(
    requested: Option<&str>,
    image_model: &str,
    models: impl IntoIterator<Item = &'a str>,
) -> Result<String, String> {
    [requested, Some(image_model)]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| {
            models
                .into_iter()
                .find(|name| name.to_ascii_lowercase().contains("image"))
                .map(str::to_string)
        })
        .ok_or_else(|| {
            "no image model: set image_model in config.json (for example \"gpt-image-2\")"
                .to_string()
        })
}

/// Only OpenAI-style endpoints (the Responses or Chat Completions dialects)
/// serve `/images/*` with Bearer auth: the ChatGPT (Codex) backend has no
/// images endpoint, Azure routes and authenticates differently, Anthropic has
/// none. A catalog provider speaks what the catalog says for the image model,
/// else for its own models; a gateway (JuCode, a custom base URL)
/// goes by the configured protocol and the image model's name.
fn check_endpoint(config: &Config, image_model: &str) -> Result<(), String> {
    if config.base_url.trim().is_empty() {
        return Err(format!("provider {} has no base_url", config.provider));
    }
    let catalog = llm_provider_kit::omp::catalog();
    let provider = config.provider.as_str();
    let dialect = catalog
        .protocol_for(provider, image_model)
        .or_else(|| catalog.protocol_for(provider, &config.model))
        .or_else(|| {
            let models = catalog.models(provider);
            catalog
                .supported_models(provider, models)
                .first()
                .and_then(|model| catalog.protocol_for(provider, &model.id))
        })
        .unwrap_or_else(|| crate::llm::protocol_for(provider, &config.protocol, image_model));
    match dialect {
        Protocol::OpenAiResponses | Protocol::OpenAiChatCompletions => Ok(()),
        other => Err(format!(
            "generate_image needs an OpenAI-compatible images endpoint; {provider} speaks the {} protocol",
            other.as_str()
        )),
    }
}

pub fn definition() -> Value {
    json!({
        "type": "function",
        "name": TOOL_NAME,
        "description": "Generate an image from a prompt, or edit workspace images, with the provider's image model, and save it in the workspace. Returns the saved paths. Takes one to two minutes.",
        "parameters": {
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "What to draw, or how to change the input images." },
                "images": { "type": "array", "items": { "type": "string" }, "description": "Workspace images (png, jpg, webp) to edit or combine. Omit to draw from the prompt alone." },
                "path": { "type": "string", "description": "File or directory to save to, relative to the workspace. Defaults to images/<timestamp>-<prompt>.png. The extension follows the image format; existing files are never overwritten (a numeric suffix is added)." },
                "size": { "type": "string", "description": "For example 1024x1024, 1536x1024, 1024x1536 or auto." },
                "n": { "type": "integer", "description": "Number of images, 1-4. Defaults to 1." },
                "model": { "type": "string", "description": "Image model. Defaults to the configured one." }
            },
            "required": ["prompt"],
            "additionalProperties": false
        }
    })
}

#[derive(Debug, Clone, PartialEq)]
struct ImageRequest {
    prompt: String,
    model: Option<String>,
    size: Option<String>,
    n: u64,
    path: Option<String>,
    images: Vec<String>,
}

fn parse_args(args: &Value) -> Result<ImageRequest, String> {
    let text = |key: &str| -> Result<Option<String>, String> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => {
                Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty()))
            }
            Some(_) => Err(format!("{key} must be a string")),
        }
    };
    let prompt = text("prompt")?.ok_or("missing prompt")?;
    let size = text("size")?;
    if let Some(size) = &size {
        let valid = size == "auto"
            || size.split_once('x').is_some_and(|(width, height)| {
                [width, height]
                    .iter()
                    .all(|side| !side.is_empty() && side.bytes().all(|b| b.is_ascii_digit()))
            });
        if !valid {
            return Err(format!(
                "size must be WIDTHxHEIGHT (for example 1024x1024) or auto, got \"{size}\""
            ));
        }
    }
    let n = crate::tools::optional_u64(args, "n")?.unwrap_or(1);
    if !(1..=MAX_IMAGES).contains(&n) {
        return Err(format!("n must be an integer from 1 to {MAX_IMAGES}"));
    }
    let images = match args.get("images") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| "images must be an array of file paths".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("images must be an array of file paths".to_string()),
    };
    if images.len() > MAX_INPUT_IMAGES {
        return Err(format!("at most {MAX_INPUT_IMAGES} input images"));
    }
    Ok(ImageRequest {
        prompt,
        model: text("model")?,
        size,
        n,
        path: text("path")?,
        images,
    })
}

/// An edit input read from the workspace.
struct InputImage {
    file_name: String,
    mime: &'static str,
    bytes: Vec<u8>,
}

/// What the endpoint returned: decoded image bytes and the prompt the model
/// actually used, when it reports one.
struct Generated {
    images: Vec<Vec<u8>>,
    revised_prompt: Option<String>,
}

pub(crate) fn run(
    args: &Value,
    cwd: &Path,
    extra_read_roots: &[PathBuf],
    state: &ToolState,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Value {
    match run_inner(args, cwd, extra_read_roots, state, emit) {
        Ok(value) => value,
        Err(error) => json!({ "error": error }),
    }
}

fn run_inner(
    args: &Value,
    cwd: &Path,
    extra_read_roots: &[PathBuf],
    state: &ToolState,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Result<Value, String> {
    let tools = state.images()?;
    let request = parse_args(args)?;
    let model = resolve_model(request.model.as_deref(), &tools.model, [])?;
    // Check where the images go before paying for them.
    let base = output_base(cwd, request.path.as_deref(), &request.prompt, now_secs());
    crate::tools::write_target(cwd, &base.to_string_lossy(), state)?;
    let inputs = request
        .images
        .iter()
        .map(|path| read_input_image(cwd, path, extra_read_roots))
        .collect::<Result<Vec<_>, _>>()?;

    let generated = generate_with_progress(&tools, &model, &request, inputs, emit)?;
    let paths = allocate_paths(&base, &generated.images)?;
    for path in &paths {
        crate::tools::write_target(cwd, &path.to_string_lossy(), state)?;
    }
    // Every target is new: rewinding the turn deletes them again.
    let _ = crate::tools::create_checkpoint(cwd, "auto-generate-image", &paths);
    for (path, bytes) in paths.iter().zip(&generated.images) {
        write_new(path, bytes)?;
    }
    crate::log_info!(
        "generate_image",
        "saved images",
        model = model.clone(),
        count = paths.len()
    );
    let mut result = json!({
        "paths": paths.iter().map(|path| crate::tools::diff_label(cwd, path)).collect::<Vec<_>>(),
        "model": model,
    });
    if let Some(revised) = generated.revised_prompt {
        result["revised_prompt"] = json!(revised);
    }
    Ok(result)
}

/// Runs the request on its own thread and reports progress meanwhile, so an
/// interrupt ends the call instead of waiting out a minutes-long request.
fn generate_with_progress(
    tools: &ImageTools,
    model: &str,
    request: &ImageRequest,
    inputs: Vec<InputImage>,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Result<Generated, String> {
    let action = if inputs.is_empty() {
        "generating"
    } else {
        "editing"
    };
    let label = format!(
        "{action} {} image{} with {model}",
        request.n,
        if request.n == 1 { "" } else { "s" }
    );
    emit(ToolExecutionEvent::Update(label.clone()))?;
    let (tx, rx) = mpsc::channel();
    let (tools, model, request) = (tools.clone(), model.to_string(), request.clone());
    thread::spawn(move || {
        let _ = tx.send(generate(&tools, &model, &request, &inputs));
    });
    let started = Instant::now();
    loop {
        match rx.recv_timeout(PROGRESS_INTERVAL) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => emit(ToolExecutionEvent::Update(format!(
                "{label} ({}s)",
                started.elapsed().as_secs()
            )))?,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the image request ended without a result".to_string())
            }
        }
    }
}

fn generate(
    tools: &ImageTools,
    model: &str,
    request: &ImageRequest,
    inputs: &[InputImage],
) -> Result<Generated, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(tools.connect_timeout)
        .timeout_read(tools.read_timeout)
        .build();
    // gpt-image models always answer in base64 and may reject the field.
    let response_format = (!is_gpt_image(model)).then_some("b64_json");
    let endpoint = if inputs.is_empty() {
        "generations"
    } else {
        "edits"
    };
    let mut call = agent
        .post(&format!("{}/images/{endpoint}", tools.base_url))
        .set("Authorization", &format!("Bearer {}", tools.api_key))
        .set("Accept", "application/json");
    for (name, value) in tools.headers.get(model).into_iter().flatten() {
        call = call.set(name, value);
    }
    let response = if inputs.is_empty() {
        let mut body = json!({ "model": model, "prompt": request.prompt, "n": request.n });
        if let Some(size) = &request.size {
            body["size"] = json!(size);
        }
        if let Some(format) = response_format {
            body["response_format"] = json!(format);
        }
        call.send_json(body)
    } else {
        let mut fields = vec![
            ("model", model.to_string()),
            ("prompt", request.prompt.clone()),
            ("n", request.n.to_string()),
        ];
        if let Some(size) = &request.size {
            fields.push(("size", size.clone()));
        }
        if let Some(format) = response_format {
            fields.push(("response_format", format.to_string()));
        }
        let boundary = multipart_boundary();
        let body = multipart_body(&boundary, &fields, inputs);
        call.set(
            "Content-Type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .send_bytes(&body)
    };
    let body = read_limited(response.map_err(request_error)?, MAX_RESPONSE_BYTES)?;
    let value = serde_json::from_slice::<Value>(&body)
        .map_err(|error| format!("the image API returned an unreadable response: {error}"))?;
    parse_response(&value, &agent)
}

fn is_gpt_image(model: &str) -> bool {
    model.to_ascii_lowercase().contains("gpt-image")
}

fn request_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| error_message(&value))
                .unwrap_or_else(|| body.trim().chars().take(300).collect());
            format!("the image API returned HTTP {code}: {message}")
        }
        ureq::Error::Transport(transport) => format!("the image request failed: {transport}"),
    }
}

fn error_message(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_string)
}

fn parse_response(value: &Value, agent: &ureq::Agent) -> Result<Generated, String> {
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| match error_message(value) {
            Some(message) => format!("the image API returned an error: {message}"),
            None => "the image API returned no images".to_string(),
        })?;
    let images = items
        .iter()
        .map(|item| {
            if let Some(data) = item.get("b64_json").and_then(Value::as_str) {
                decode_base64(data)
            } else if let Some(url) = item.get("url").and_then(Value::as_str) {
                fetch_url(url, agent)
            } else {
                Err("an image in the response has neither b64_json nor url".to_string())
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let revised_prompt = items
        .iter()
        .filter_map(|item| item.get("revised_prompt").and_then(Value::as_str))
        .map(str::trim)
        .find(|prompt| !prompt.is_empty())
        .map(str::to_string);
    Ok(Generated {
        images,
        revised_prompt,
    })
}

fn decode_base64(data: &str) -> Result<Vec<u8>, String> {
    let compact = data
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect::<String>();
    BASE64_STANDARD
        .decode(compact)
        .map_err(|error| format!("the image API returned invalid base64: {error}"))
}

/// Downloads an image URL. The provider's key is not sent: the URL is
/// usually a signed link on another host.
fn fetch_url(url: &str, agent: &ureq::Agent) -> Result<Vec<u8>, String> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (_, data) = rest
            .split_once(";base64,")
            .ok_or("the image API returned a data URL without base64 content")?;
        return decode_base64(data);
    }
    crate::web_fetch::validate_url(url)?;
    match agent.get(url).call() {
        Ok(response) => read_limited(response, MAX_DOWNLOAD_BYTES),
        Err(ureq::Error::Status(code, _)) => {
            Err(format!("downloading the image failed: HTTP {code}"))
        }
        Err(ureq::Error::Transport(transport)) => {
            Err(format!("downloading the image failed: {transport}"))
        }
    }
}

fn read_limited(response: ureq::Response, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading the image response failed: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!(
            "the image response is larger than {} MB",
            limit / (1024 * 1024)
        ));
    }
    Ok(bytes)
}

fn read_input_image(
    cwd: &Path,
    path: &str,
    extra_read_roots: &[PathBuf],
) -> Result<InputImage, String> {
    let resolved = crate::tools::readable_path(cwd, path, extra_read_roots)?;
    let mime = crate::tools::image_mime(&resolved)
        .filter(|mime| matches!(*mime, "image/png" | "image/jpeg" | "image/webp"))
        .ok_or_else(|| format!("{path}: input images must be png, jpg or webp"))?;
    let size = fs::metadata(&resolved)
        .map_err(|error| format!("{path}: {error}"))?
        .len();
    if size > MAX_INPUT_IMAGE_BYTES {
        return Err(format!("{path}: input images must be under 50 MB"));
    }
    let bytes = fs::read(&resolved).map_err(|error| format!("{path}: {error}"))?;
    let file_name = resolved
        .file_name()
        .map(|name| name.to_string_lossy().replace(['"', '\r', '\n'], "_"))
        .unwrap_or_else(|| "image".to_string());
    Ok(InputImage {
        file_name,
        mime,
        bytes,
    })
}

fn multipart_boundary() -> String {
    let mut bytes = [0u8; 12];
    let _ = getrandom::getrandom(&mut bytes);
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("jucode-{hex}")
}

fn multipart_body(boundary: &str, fields: &[(&str, String)], files: &[InputImage]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        let _ = write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        );
    }
    for file in files {
        let _ = write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image[]\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
            file.file_name, file.mime
        );
        body.extend_from_slice(&file.bytes);
        body.extend_from_slice(b"\r\n");
    }
    let _ = write!(body, "--{boundary}--\r\n");
    body
}

/// Where to save: the requested path, or `images/<timestamp>-<slug>`; a
/// requested directory gets that default name inside it. The extension is
/// set later from the image format.
fn output_base(cwd: &Path, requested: Option<&str>, prompt: &str, now: u64) -> PathBuf {
    let default_name = format!("{}-{}", timestamp(now), slug(prompt));
    match requested {
        None => cwd.join("images").join(default_name),
        Some(path) => {
            let resolved = crate::tools::resolve_path(cwd, path);
            if path.ends_with(['/', '\\']) || resolved.is_dir() {
                resolved.join(default_name)
            } else {
                resolved
            }
        }
    }
}

fn timestamp(secs: u64) -> String {
    let (year, month, day) = crate::logging::civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// ASCII words of the prompt joined with dashes, at most 40 characters;
/// "image" when the prompt has none (a prompt in Chinese, say).
fn slug(prompt: &str) -> String {
    let mut slug = String::new();
    for word in prompt
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let extra = usize::from(!slug.is_empty()) + word.len();
        if slug.len() + extra > SLUG_MAX_CHARS {
            if slug.is_empty() {
                slug = word[..SLUG_MAX_CHARS].to_ascii_lowercase();
            }
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word.to_ascii_lowercase());
    }
    if slug.is_empty() {
        "image".to_string()
    } else {
        slug
    }
}

fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else {
        None
    }
}

/// One free path per image: `base` with the image's extension, then `-2`,
/// `-3`, ... for files that exist or were taken by an earlier image of this
/// call.
fn allocate_paths(base: &Path, images: &[Vec<u8>]) -> Result<Vec<PathBuf>, String> {
    let mut taken = HashSet::new();
    let mut paths = Vec::new();
    for bytes in images {
        let extension = image_extension(bytes)
            .ok_or("the image API returned data that is not a png, jpg, webp or gif image")?;
        let path = free_path(&with_extension(base, extension), &taken);
        taken.insert(path.clone());
        paths.push(path);
    }
    Ok(paths)
}

/// `path` named for its format: an image extension is replaced (a png saved
/// as .jpg would mislead viewers), any other suffix kept (`v1.2` → `v1.2.png`).
fn with_extension(path: &Path, extension: &str) -> PathBuf {
    let current = path
        .extension()
        .map(|current| current.to_string_lossy().to_ascii_lowercase());
    match current.as_deref() {
        Some(current) if current == extension || (current == "jpeg" && extension == "jpg") => {
            path.to_path_buf()
        }
        Some(_) if crate::tools::image_mime(path).is_some() => path.with_extension(extension),
        _ => {
            let mut name = path.as_os_str().to_owned();
            name.push(format!(".{extension}"));
            PathBuf::from(name)
        }
    }
}

fn free_path(path: &Path, taken: &HashSet<PathBuf>) -> PathBuf {
    let free = |candidate: &Path| !candidate.exists() && !taken.contains(candidate);
    if free(path) {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    (2u64..)
        .map(|number| path.with_file_name(format!("{stem}-{number}{extension}")))
        .find(|candidate| free(candidate))
        .expect("an unbounded range always yields a free name")
}

/// Writes a file that must not exist yet: a file created since the path was
/// picked is left alone.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-png";
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0fake-jpeg";

    fn test_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "jucode-images-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tools(base_url: &str) -> ImageTools {
        ImageTools {
            base_url: base_url.to_string(),
            api_key: "test-key".to_string(),
            model: "gpt-image-2".to_string(),
            headers: HashMap::from([(
                "gpt-image-2".to_string(),
                vec![("X-JuCode-Group".to_string(), "g-7".to_string())],
            )]),
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(5),
        }
    }

    fn state_with(tools: ImageTools) -> ToolState {
        let state = ToolState::default();
        state.set_images(Ok(tools));
        state
    }

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn bytes_response(body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// Serves `responses` in order, one per connection, and hands back each
    /// request (headers and body) it read.
    fn serve(responses: Vec<Vec<u8>>) -> (String, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_request(&mut stream).unwrap_or_default();
                let _ = stream.write_all(&response);
                let _ = stream.flush();
                let _ = tx.send(request);
            }
        });
        (format!("http://{addr}"), rx)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<Vec<u8>> {
        let mut received = Vec::new();
        let mut chunk = [0_u8; 4096];
        let (mut header_end, mut content_length) = (None, 0_usize);
        loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Ok(received);
            }
            received.extend_from_slice(&chunk[..read]);
            if header_end.is_none() {
                if let Some(position) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = Some(position + 4);
                    content_length = String::from_utf8_lossy(&received[..position])
                        .to_ascii_lowercase()
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse().ok())
                        .unwrap_or(0);
                }
            }
            if header_end.is_some_and(|end| received.len() >= end + content_length) {
                return Ok(received);
            }
        }
    }

    fn run_tool(args: Value, cwd: &Path, state: &ToolState) -> Value {
        run(&args, cwd, &[], state, &mut |_| Ok(()))
    }

    fn body_json(request: &[u8]) -> Value {
        let text = String::from_utf8_lossy(request);
        let (_, body) = text.split_once("\r\n\r\n").unwrap();
        serde_json::from_str(body).unwrap()
    }

    #[test]
    fn arguments_default_and_validate() {
        let request = parse_args(&json!({ "prompt": " a red fox " })).unwrap();
        assert_eq!(
            request,
            ImageRequest {
                prompt: "a red fox".to_string(),
                model: None,
                size: None,
                n: 1,
                path: None,
                images: Vec::new(),
            }
        );
        let request = parse_args(&json!({
            "prompt": "fox", "model": "gpt-image-2", "size": "1536x1024", "n": 3.0,
            "path": "art/fox.png", "images": ["a.png", " b.jpg "]
        }))
        .unwrap();
        assert_eq!(request.model.as_deref(), Some("gpt-image-2"));
        assert_eq!(request.size.as_deref(), Some("1536x1024"));
        assert_eq!(request.n, 3);
        assert_eq!(request.path.as_deref(), Some("art/fox.png"));
        assert_eq!(request.images, ["a.png", "b.jpg"]);
        assert_eq!(
            parse_args(&json!({ "prompt": "x", "size": "auto" }))
                .unwrap()
                .size
                .as_deref(),
            Some("auto")
        );

        assert_eq!(parse_args(&json!({})).unwrap_err(), "missing prompt");
        assert_eq!(
            parse_args(&json!({ "prompt": "  " })).unwrap_err(),
            "missing prompt"
        );
        for n in [json!(0), json!(5), json!(1.5), json!("2")] {
            assert!(
                parse_args(&json!({ "prompt": "x", "n": n })).is_err(),
                "{n}"
            );
        }
        for size in ["large", "1024", "1024x", "x1024", "1024*1024"] {
            assert!(
                parse_args(&json!({ "prompt": "x", "size": size })).is_err(),
                "{size}"
            );
        }
        assert!(parse_args(&json!({ "prompt": "x", "images": "a.png" })).is_err());
        assert!(parse_args(&json!({ "prompt": "x", "images": [1] })).is_err());
        assert!(parse_args(&json!({ "prompt": 3 })).is_err());
    }

    #[test]
    fn model_resolution_prefers_argument_then_config_then_listed_image_model() {
        let models = ["gpt-5.5", "gpt-image-2", "flux-image"];
        assert_eq!(
            resolve_model(Some("dall-e-3"), "gpt-image-1", models).unwrap(),
            "dall-e-3"
        );
        assert_eq!(
            resolve_model(Some(" "), " gpt-image-1 ", models).unwrap(),
            "gpt-image-1"
        );
        assert_eq!(resolve_model(None, "", models).unwrap(), "gpt-image-2");
        assert_eq!(
            resolve_model(None, "", ["GPT-IMAGE-1.5"]).unwrap(),
            "GPT-IMAGE-1.5"
        );
        let error = resolve_model(None, "", ["gpt-5.5", "claude-opus-5-5"]).unwrap_err();
        assert!(error.contains("image_model"), "{error}");
    }

    fn config(provider: &str, protocol: &str, model: &str, models: &[&str]) -> Config {
        let models = models
            .iter()
            .map(|name| json!({ "name": name }))
            .collect::<Vec<_>>();
        Config::from_value(
            &json!({
                "provider": provider,
                "protocol": protocol,
                "model": model,
                "models": models,
                "base_url": "https://gateway.example/v1",
                "read_timeout_seconds": 30,
            })
            .to_string(),
            PathBuf::from("config.json"),
        )
        .unwrap()
    }

    #[test]
    fn offered_only_on_openai_style_endpoints_with_an_image_model() {
        let key = || Some("key".to_string());
        let headers = HashMap::new();
        let gateway = config("gateway", "chat", "gpt-5.5", &["gpt-5.5", "gpt-image-2"]);
        let tools = ImageTools::from_config(&gateway, key(), &headers).unwrap();
        assert_eq!(tools.model, "gpt-image-2");
        assert_eq!(tools.base_url, "https://gateway.example/v1");
        // A short chat read timeout is raised for minutes-long generations.
        assert_eq!(tools.read_timeout, MIN_READ_TIMEOUT);

        let responses = config("custom", "responses", "gpt-5.5", &["gpt-image-1"]);
        assert!(ImageTools::from_config(&responses, key(), &headers).is_ok());

        let mut configured = config("gateway", "chat", "gpt-5.5", &["gpt-5.5"]);
        assert!(ImageTools::from_config(&configured, key(), &headers)
            .unwrap_err()
            .contains("image_model"));
        configured.image_model = "seedream-4".to_string();
        assert_eq!(
            ImageTools::from_config(&configured, key(), &headers)
                .unwrap()
                .model,
            "seedream-4"
        );

        for protocol in ["anthropic", "codex", "azure"] {
            let mut blocked = config("custom", protocol, "gpt-5.5", &["gpt-image-2"]);
            blocked.image_model = "gpt-image-2".to_string();
            let error = ImageTools::from_config(&blocked, key(), &headers).unwrap_err();
            assert!(error.contains(protocol), "{protocol}: {error}");
        }
        // The ChatGPT logins have no images endpoint whatever the config says.
        for provider in ["openai-codex", "openai-codex-device"] {
            let mut codex = config(provider, "", "gpt-5.5", &["gpt-5.5"]);
            codex.image_model = "gpt-image-2".to_string();
            let error = ImageTools::from_config(&codex, key(), &headers).unwrap_err();
            assert!(error.contains("codex"), "{provider}: {error}");
        }

        let mut no_key = gateway.clone();
        no_key.api_key_env = "JUCODE_IMAGES_TEST_UNSET_KEY".to_string();
        assert!(ImageTools::from_config(&no_key, None, &headers)
            .unwrap_err()
            .contains("API key"));
    }

    #[test]
    fn file_names_use_timestamp_slug_and_never_overwrite() {
        // 2026-10-08 09:05:07 UTC
        let now = 1_791_450_307;
        assert_eq!(timestamp(now), "20261008-090507");
        assert_eq!(slug("A red fox, in the snow!"), "a-red-fox-in-the-snow");
        assert_eq!(slug("一只狐狸"), "image");
        assert_eq!(slug(&"word ".repeat(30)).len(), 39);
        assert_eq!(slug(&"x".repeat(60)).len(), SLUG_MAX_CHARS);

        let dir = test_dir("names");
        assert_eq!(
            output_base(&dir, None, "Red fox", now),
            dir.join("images").join("20261008-090507-red-fox")
        );
        assert_eq!(
            output_base(&dir, Some("art/"), "fox", now),
            dir.join("art").join("20261008-090507-fox")
        );
        fs::create_dir_all(dir.join("existing")).unwrap();
        assert_eq!(
            output_base(&dir, Some("existing"), "fox", now),
            dir.join("existing").join("20261008-090507-fox")
        );
        assert_eq!(
            output_base(&dir, Some("art/fox.png"), "fox", now),
            dir.join("art/fox.png")
        );

        let base = dir.join("fox");
        fs::write(dir.join("fox.png"), b"old").unwrap();
        fs::write(dir.join("fox-2.png"), b"old").unwrap();
        let paths = allocate_paths(&base, &[PNG.to_vec(), PNG.to_vec(), JPEG.to_vec()]).unwrap();
        assert_eq!(
            paths,
            [
                dir.join("fox-3.png"),
                dir.join("fox-4.png"),
                dir.join("fox.jpg")
            ]
        );
        // The extension follows the data; a non-image suffix is kept.
        assert_eq!(
            allocate_paths(&dir.join("cat.jpeg"), &[PNG.to_vec()]).unwrap(),
            [dir.join("cat.png")]
        );
        assert_eq!(
            allocate_paths(&dir.join("cat.jpeg"), &[JPEG.to_vec()]).unwrap(),
            [dir.join("cat.jpeg")]
        );
        assert_eq!(
            allocate_paths(&dir.join("v1.2"), &[PNG.to_vec()]).unwrap(),
            [dir.join("v1.2.png")]
        );
        assert!(allocate_paths(&base, &[b"not an image".to_vec()]).is_err());

        assert!(write_new(&dir.join("fox.png"), PNG).is_err());
        assert_eq!(fs::read(dir.join("fox.png")).unwrap(), b"old");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn generations_save_b64_images_and_send_model_headers() {
        let body = json!({
            "data": [
                { "b64_json": BASE64_STANDARD.encode(PNG), "revised_prompt": "a red fox in snow" },
                { "b64_json": BASE64_STANDARD.encode(PNG) }
            ]
        });
        let (url, requests) = serve(vec![json_response("200 OK", &body.to_string()).into_bytes()]);
        let dir = test_dir("b64");
        fs::create_dir_all(dir.join("art")).unwrap();
        fs::write(dir.join("art/fox.png"), b"keep").unwrap();
        let state = state_with(tools(&url));

        let result = run_tool(
            json!({ "prompt": "a red fox", "n": 2, "size": "1024x1024", "path": "art/fox.png" }),
            &dir,
            &state,
        );
        assert_eq!(
            result,
            json!({
                "paths": ["art/fox-2.png", "art/fox-3.png"],
                "model": "gpt-image-2",
                "revised_prompt": "a red fox in snow"
            })
        );
        assert_eq!(fs::read(dir.join("art/fox.png")).unwrap(), b"keep");
        assert_eq!(fs::read(dir.join("art/fox-2.png")).unwrap(), PNG);
        assert_eq!(fs::read(dir.join("art/fox-3.png")).unwrap(), PNG);

        let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        let head = String::from_utf8_lossy(&request).to_ascii_lowercase();
        assert!(head.starts_with("post /images/generations "), "{head}");
        assert!(head.contains("authorization: bearer test-key"));
        assert!(head.contains("x-jucode-group: g-7"));
        // gpt-image models are not sent response_format.
        assert_eq!(
            body_json(&request),
            json!({ "model": "gpt-image-2", "prompt": "a red fox", "n": 2, "size": "1024x1024" })
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn url_images_are_downloaded_and_other_models_ask_for_b64() {
        let (image_url, image_requests) = serve(vec![bytes_response(JPEG)]);
        let body = json!({ "data": [{ "url": format!("{image_url}/files/fox.jpg") }] });
        let (url, requests) = serve(vec![json_response("200 OK", &body.to_string()).into_bytes()]);
        let dir = test_dir("url");
        let state = state_with(tools(&url));

        let result = run_tool(
            json!({ "prompt": "Fox!", "model": "dall-e-3" }),
            &dir,
            &state,
        );
        let path = result["paths"][0].as_str().unwrap();
        assert!(
            path.starts_with("images/") && path.ends_with("-fox.jpg"),
            "{result}"
        );
        assert_eq!(result["model"], "dall-e-3");
        assert!(result.get("revised_prompt").is_none());
        assert_eq!(fs::read(dir.join(path)).unwrap(), JPEG);

        let request = body_json(&requests.recv_timeout(Duration::from_secs(5)).unwrap());
        assert_eq!(request["response_format"], "b64_json");
        assert_eq!(request["model"], "dall-e-3");
        // The download does not carry the provider key.
        let download = image_requests.recv_timeout(Duration::from_secs(5)).unwrap();
        let head = String::from_utf8_lossy(&download).to_ascii_lowercase();
        assert!(head.starts_with("get /files/fox.jpg "), "{head}");
        assert!(!head.contains("authorization"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn api_errors_are_reported_and_nothing_is_written() {
        let error =
            json!({ "error": { "message": "model not found", "type": "invalid_request_error" } });
        let (url, _requests) = serve(vec![
            json_response("400 Bad Request", &error.to_string()).into_bytes(),
            json_response("200 OK", r#"{"data":[]}"#).into_bytes(),
        ]);
        let dir = test_dir("errors");
        let state = state_with(tools(&url));

        let result = run_tool(json!({ "prompt": "fox" }), &dir, &state);
        assert_eq!(
            result["error"],
            "the image API returned HTTP 400: model not found"
        );
        let result = run_tool(json!({ "prompt": "fox" }), &dir, &state);
        assert_eq!(result["error"], "the image API returned no images");
        assert!(!dir.join("images").exists());

        // Bad arguments and an unconfigured session fail before any request.
        let result = run_tool(json!({ "prompt": "fox", "n": 9 }), &dir, &state);
        assert!(result["error"].as_str().unwrap().contains("n must be"));
        let result = run_tool(json!({ "prompt": "fox" }), &dir, &ToolState::default());
        assert_eq!(result["error"], UNAVAILABLE);
        let unconfigured = ToolState::default();
        unconfigured.set_images(Err("no image model".to_string()));
        let result = run_tool(json!({ "prompt": "fox" }), &dir, &unconfigured);
        assert_eq!(result["error"], "no image model");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_outside_the_workspace_is_refused_before_the_request() {
        let dir = test_dir("escape");
        // No server: a request would fail with a connection error instead.
        let state = state_with(tools("http://127.0.0.1:9"));
        let result = run_tool(
            json!({ "prompt": "fox", "path": "../outside.png" }),
            &dir,
            &state,
        );
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("escapes the workspace"),
            "{result}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edits_send_multipart_with_each_input_image() {
        let body = json!({ "data": [{ "b64_json": BASE64_STANDARD.encode(PNG) }] });
        let (url, requests) = serve(vec![json_response("200 OK", &body.to_string()).into_bytes()]);
        let dir = test_dir("edits");
        fs::write(dir.join("a.png"), PNG).unwrap();
        fs::write(dir.join("b.jpg"), JPEG).unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        let state = state_with(tools(&url));

        let result = run_tool(
            json!({ "prompt": "combine", "images": ["notes.txt"] }),
            &dir,
            &state,
        );
        assert!(result["error"]
            .as_str()
            .unwrap()
            .contains("png, jpg or webp"));

        let result = run_tool(
            json!({ "prompt": "combine them", "images": ["a.png", "b.jpg"], "path": "out" }),
            &dir,
            &state,
        );
        assert_eq!(result["paths"], json!(["out.png"]), "{result}");

        let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        let text = String::from_utf8_lossy(&request);
        let lower = text.to_ascii_lowercase();
        assert!(lower.starts_with("post /images/edits "), "{lower}");
        let boundary = lower
            .lines()
            .find_map(|line| line.strip_prefix("content-type: multipart/form-data; boundary="))
            .unwrap()
            .trim()
            .to_string();
        assert!(text.contains("name=\"model\"\r\n\r\ngpt-image-2\r\n"));
        assert!(text.contains("name=\"prompt\"\r\n\r\ncombine them\r\n"));
        assert!(text.contains("name=\"n\"\r\n\r\n1\r\n"));
        assert!(!text.contains("response_format"));
        assert!(text
            .contains("name=\"image[]\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n"));
        assert!(text
            .contains("name=\"image[]\"; filename=\"b.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n"));
        assert!(request.windows(PNG.len()).any(|window| window == PNG));
        assert!(text.trim_end().ends_with(&format!("--{boundary}--")));
        let _ = fs::remove_dir_all(dir);
    }
}
