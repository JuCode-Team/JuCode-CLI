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
            let model = provider["model"].as_str().unwrap_or("").trim();
            if model.is_empty() {
                return Err("model is required".into());
            }
            let model = model.to_string();
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
            provider = json!({"id":id,"name":provider["name"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or(id),"base_url":url,"format":protocol,"model":model,"key":key});
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
    store.setting(&format!("tool_providers_{tool}"))["providers"]
        .as_array()?
        .iter()
        .find(|p| p["id"] == id)
        .cloned()
}

/// What clients are told about the provider a session runs on: never its
/// key or URL.
pub fn view(provider: &Value) -> Value {
    json!({ "id": provider["id"], "name": provider["name"], "model": provider["model"] })
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
        op["provider"]["key"] = json!("");
        op["provider"]["model"] = json!("new-model");
        handle(&store, &op).unwrap();
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
}
