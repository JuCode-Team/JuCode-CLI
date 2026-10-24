//! Tool-owned API-key providers. Credentials never leave the local daemon.
use crate::store::Store;
use serde_json::{json, Value};

pub fn format(tool: &str) -> Result<&'static str, String> {
    match tool {
        "claude" => Ok("anthropic"),
        "codex" => Ok("responses"),
        _ => Err("custom gateway providers require Claude Code or Codex".into()),
    }
}

pub fn list(store: &Store, tool: &str) -> Result<Value, String> {
    format(tool)?;
    let mut config = store.setting(&format!("tool_providers_{tool}"));
    if !config.is_object() {
        config = json!({"selected": "", "providers": []});
    }
    if let Some(providers) = config["providers"].as_array_mut() {
        for provider in providers {
            with_models(provider);
            provider["has_key"] =
                json!(provider["key"].as_str().is_some_and(|key| !key.is_empty()));
            provider.as_object_mut().unwrap().remove("key");
        }
    }
    Ok(json!({"type": "tool_providers", "tool": tool, "config": config}))
}

pub fn handle(store: &Store, op: &Value) -> Result<Value, String> {
    static EDIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = EDIT.lock().unwrap_or_else(|p| p.into_inner());
    let tool = op["tool"].as_str().ok_or("tool is required")?;
    let protocol = format(tool)?;
    if op["op"] == "tool_providers" {
        return list(store, tool);
    }
    let setting = format!("tool_providers_{tool}");
    let mut config = store.setting(&setting);
    if !config.is_object() {
        config = json!({"selected": "", "providers": []});
    }
    let id = op["provider"]["id"]
        .as_str()
        .or(op["provider_id"].as_str())
        .unwrap_or("")
        .trim();
    match op["op"].as_str() {
        Some("tool_provider_save") => {
            let mut provider = op["provider"].clone();
            if id.is_empty()
                || id.len() > 100
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                return Err(
                    "provider ID must contain 1–100 letters, digits, hyphens or underscores".into(),
                );
            }
            let url = provider["base_url"]
                .as_str()
                .unwrap_or("")
                .trim()
                .trim_end_matches('/');
            let parsed: tungstenite::http::Uri = url.parse().map_err(|_| "invalid API URL")?;
            if !matches!(parsed.scheme_str(), Some("http" | "https"))
                || parsed.authority().is_none()
                || url.contains(['?', '#', '@'])
            {
                return Err(
                    "API URL must be an http(s) base URL without credentials, query or fragment"
                        .into(),
                );
            }
            if provider["format"] != protocol {
                return Err(format!("{tool} requires {protocol}"));
            }
            // The models it serves, the first as the default unless one is named.
            let mut models: Vec<String> = Vec::new();
            for model in provider["models"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(std::iter::once(&provider["model"]))
                .filter_map(|m| m.as_str().map(str::trim))
                .filter(|m| !m.is_empty())
            {
                if model.len() > 200 || model.chars().any(char::is_control) {
                    return Err(format!("invalid model ID: {model}"));
                }
                if !models.iter().any(|m| m == model) {
                    models.push(model.to_string());
                }
            }
            if models.is_empty() {
                return Err("model is required".into());
            }
            if models.len() > 500 {
                return Err("at most 500 models".into());
            }
            let model = provider["model"]
                .as_str()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or(&models[0])
                .to_string();
            let url = url.to_string();
            let key = provider["key"].as_str().unwrap_or("").trim();
            let previous = config["providers"]
                .as_array()
                .and_then(|ps| ps.iter().find(|p| p["id"] == id));
            let key = if key.is_empty() {
                previous.and_then(|p| p["key"].as_str()).unwrap_or("")
            } else {
                key
            };
            if key.is_empty() || key.contains(['\r', '\n']) {
                return Err("a valid API key is required".into());
            }
            provider = json!({"id":id,"name":provider["name"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or(id),"base_url":url,"format":protocol,"model":model,"models":models,"key":key});
            if !config["providers"].is_array() {
                config["providers"] = json!([]);
            }
            let providers = config["providers"].as_array_mut().unwrap();
            providers.retain(|p| p["id"] != id);
            providers.push(provider);
        }
        Some("tool_provider_remove") => {
            if let Some(ps) = config["providers"].as_array_mut() {
                ps.retain(|p| p["id"] != id);
            }
            if config["selected"] == id {
                config["selected"] = json!("");
            }
        }
        Some("tool_provider_select") => {
            if !id.is_empty()
                && !config["providers"]
                    .as_array()
                    .is_some_and(|ps| ps.iter().any(|p| p["id"] == id))
            {
                return Err("unknown provider for this tool".into());
            }
            config["selected"] = json!(id);
        }
        _ => return Err("unknown tool provider operation".into()),
    }
    store
        .set_setting(&setting, config)
        .map_err(|e| e.to_string())?;
    list(store, tool)
}

/// The provider `tool`'s new sessions start on (None: the tool's own
/// configuration).
pub fn selected(store: &Store, tool: &str) -> Option<Value> {
    let config = store.setting(&format!("tool_providers_{tool}"));
    let id = config["selected"].as_str().filter(|id| !id.is_empty())?;
    find(store, tool, id)
}

/// `tool`'s provider `id`, key included (for the local gateway only).
pub fn find(store: &Store, tool: &str, id: &str) -> Option<Value> {
    let mut provider = store.setting(&format!("tool_providers_{tool}"))["providers"]
        .as_array()?
        .iter()
        .find(|p| p["id"] == id)
        .cloned()?;
    with_models(&mut provider);
    Some(provider)
}

/// A provider saved with one `model` (before `models`) serves that one.
fn with_models(provider: &mut Value) {
    let listed = provider["models"]
        .as_array()
        .is_some_and(|models| !models.is_empty());
    if !listed {
        provider["models"] = json!([provider["model"].as_str().unwrap_or_default()]);
    }
}

/// What clients are told about the provider a session runs on: never its
/// key or URL.
pub fn view(provider: &Value) -> Value {
    json!({
        "id": provider["id"],
        "name": provider["name"],
        "model": provider["model"],
        "models": provider["models"],
    })
}

/// `tool_provider_models`: the models an API serves, from its model list
/// (`GET …/models`, in the OpenAI or Anthropic shape), for the form that
/// adds or edits a provider. The key is the one typed, else the saved one of
/// `provider_id`. Models the list says take other protocols than `tool`'s
/// are counted, not listed.
pub fn models(store: &Store, op: &Value) -> Result<Value, String> {
    let tool = op["tool"].as_str().ok_or("tool is required")?;
    let protocol = format(tool)?;
    let base = op["base_url"]
        .as_str()
        .unwrap_or("")
        .trim()
        .trim_end_matches('/');
    let parsed: tungstenite::http::Uri = base.parse().map_err(|_| "invalid API URL")?;
    if !matches!(parsed.scheme_str(), Some("http" | "https"))
        || parsed.authority().is_none()
        || base.contains(['?', '#', '@'])
    {
        return Err(
            "API URL must be an http(s) base URL without credentials, query or fragment".into(),
        );
    }
    let typed = op["key"].as_str().unwrap_or("").trim().to_string();
    let key = if typed.is_empty() {
        op["provider_id"]
            .as_str()
            .and_then(|id| find(store, tool, id))
            .and_then(|p| p["key"].as_str().map(str::to_string))
            .unwrap_or_default()
    } else {
        typed
    };
    if key.is_empty() || key.contains(['\r', '\n']) {
        return Err("a valid API key is required".into());
    }
    // Where the local gateway sends `/v1/messages`, `/v1/responses`: the
    // base with its own `/v1`, or `/v1` added.
    let url = if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(20))
        .build();
    let get = |page: Option<&str>, bearer: bool| {
        let mut call = agent.get(&url);
        if let Some(after) = page {
            call = call.query("limit", "1000").query("after_id", after);
        }
        if protocol == "anthropic" && !bearer {
            call = call
                .set("x-api-key", &key)
                .set("anthropic-version", "2023-06-01");
        } else {
            call = call.set("Authorization", &format!("Bearer {key}"));
        }
        match call.call() {
            Ok(response) => response
                .into_json::<Value>()
                .map_err(|e| (0, format!("the model list is not JSON: {e}"))),
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err((
                    status,
                    format!(
                        "HTTP {status}: {}",
                        body.chars().take(300).collect::<String>()
                    ),
                ))
            }
            Err(ureq::Error::Transport(error)) => Err((0, format!("cannot reach {url}: {error}"))),
        }
    };
    // Anthropic-format APIs take x-api-key; many relays only a bearer token.
    let mut bearer = protocol != "anthropic";
    let mut page = match get(None, bearer) {
        Err((401 | 403, _)) if !bearer => {
            bearer = true;
            get(None, true)
        }
        other => other,
    }
    .map_err(|(_, message)| message)?;
    let mut listed: Vec<Value> = Vec::new();
    let mut hidden = 0;
    // Anthropic's list comes in pages.
    for _ in 0..20 {
        for model in page["data"].as_array().into_iter().flatten() {
            let Some(id) = model["id"].as_str().filter(|id| !id.trim().is_empty()) else {
                continue;
            };
            if !serves(model, protocol) {
                hidden += 1;
                continue;
            }
            if listed.iter().any(|m| m["id"] == id) {
                continue;
            }
            let name = model["display_name"]
                .as_str()
                .or(model["name"].as_str())
                .filter(|name| *name != id);
            let window = model["context_length"]
                .as_u64()
                .or(model["context_window"].as_u64())
                .or(model["max_input_tokens"].as_u64());
            listed.push(json!({ "id": id, "name": name, "context_window": window }));
        }
        let next = page["last_id"]
            .as_str()
            .filter(|_| page["has_more"] == true);
        let Some(after) = next.map(str::to_string) else {
            break;
        };
        page = get(Some(&after), bearer).map_err(|(_, message)| message)?;
    }
    if page["data"].as_array().is_none() && listed.is_empty() {
        return Err(format!("{url} did not return a model list"));
    }
    Ok(json!({ "type": "tool_provider_models", "tool": tool, "models": listed, "hidden": hidden }))
}

/// Whether a listed model takes `protocol`, when the list says which
/// endpoints it serves (`supported_endpoints`: paths; new-api's
/// `supported_endpoint_types`: names). Unsaid: assumed.
fn serves(model: &Value, protocol: &str) -> bool {
    let named = |key: &str| {
        model[key].as_array().map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
    };
    if let Some(paths) = named("supported_endpoints") {
        let suffix = if protocol == "anthropic" {
            "/messages"
        } else {
            "/responses"
        };
        return paths
            .iter()
            .any(|p| p.trim_end_matches('/').ends_with(suffix));
    }
    if let Some(types) = named("supported_endpoint_types") {
        let wanted = if protocol == "anthropic" {
            "anthropic"
        } else {
            "openai-response"
        };
        return types.iter().any(|t| t == wanted);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn providers_are_tool_owned_and_keys_are_redacted_and_preserved() {
        let dir = std::env::temp_dir().join(format!(
            "jucode-provider-test-{}-{}",
            std::process::id(),
            crate::store::now()
        ));
        let store = Store::open(dir.clone()).unwrap();
        let mut op = json!({"op":"tool_provider_save","tool":"claude","provider":{"id":"custom","name":"My API","base_url":"https://example.com/v1/","format":"anthropic","model":"claude-test","key":"secret"}});
        let reply = handle(&store, &op).unwrap();
        assert_eq!(reply["config"]["providers"][0]["has_key"], true);
        assert!(reply["config"]["providers"][0].get("key").is_none());
        assert!(!reply.to_string().contains("secret"));
        assert_eq!(
            list(&store, "codex").unwrap()["config"]["providers"],
            json!([])
        );
        // One saved before `models` serves its model.
        assert_eq!(
            reply["config"]["providers"][0]["models"],
            json!(["claude-test"])
        );
        op["provider"]["key"] = json!("");
        op["provider"]["model"] = json!("new-model");
        op["provider"]["models"] = json!([" a ", "new-model", "a", ""]);
        let reply = handle(&store, &op).unwrap();
        assert_eq!(
            reply["config"]["providers"][0]["models"],
            json!(["a", "new-model"])
        );
        assert_eq!(reply["config"]["providers"][0]["model"], "new-model");
        assert_eq!(
            view(&find(&store, "claude", "custom").unwrap())["models"],
            json!(["a", "new-model"])
        );
        assert_eq!(
            store.setting("tool_providers_claude")["providers"][0]["key"],
            "secret"
        );
        assert!(handle(
            &store,
            &json!({"op":"tool_provider_select","tool":"codex","provider_id":"custom"})
        )
        .is_err());
        handle(
            &store,
            &json!({"op":"tool_provider_select","tool":"claude","provider_id":"custom"}),
        )
        .unwrap();
        handle(
            &store,
            &json!({"op":"tool_provider_remove","tool":"claude","provider_id":"custom"}),
        )
        .unwrap();
        assert_eq!(list(&store, "claude").unwrap()["config"]["selected"], "");
        op["provider"]["key"] = json!("secret");
        op["provider"]["format"] = json!("responses");
        assert!(handle(&store, &op).is_err());
        op["provider"]["format"] = json!("anthropic");
        for url in [
            "file:///etc/passwd",
            "https://user:pass@example.com",
            "https://example.com?token=secret",
            "https://example.com#frag",
            "invalid",
        ] {
            op["provider"]["base_url"] = json!(url);
            assert!(handle(&store, &op).is_err(), "{url}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join("settings.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn listed_models_keep_the_ones_that_take_the_tool_s_protocol() {
        let claude = json!({"id":"c","supported_endpoints":["/messages"]});
        let gpt = json!({"id":"g","supported_endpoints":["/chat/completions","/responses"]});
        let relay = json!({"id":"r","supported_endpoint_types":["anthropic","openai"]});
        let plain = json!({"id":"p"});
        assert!(serves(&claude, "anthropic") && !serves(&claude, "responses"));
        assert!(!serves(&gpt, "anthropic") && serves(&gpt, "responses"));
        assert!(serves(&relay, "anthropic") && !serves(&relay, "responses"));
        assert!(serves(&plain, "anthropic") && serves(&plain, "responses"));
    }
}
