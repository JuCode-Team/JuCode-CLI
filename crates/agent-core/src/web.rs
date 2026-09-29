//! Web tools served by the JuCode gateway: `web_search` (POST /tools/v1/search)
//! and, when `web_fetch_engine` names a gateway engine, `web_fetch`
//! (POST /tools/v1/fetch). The `local` fetch engine stays in `web_fetch`.
//! Calls authenticate with the JuCode login session and are billed to the
//! account like model calls.

use serde_json::{json, Value};
use std::time::Duration;

pub const SEARCH_ENGINES: &[&str] = &["auto", "parallel", "brave"];
pub const FETCH_ENGINES: &[&str] = &["local", "jina", "firecrawl", "parallel"];
pub const DEFAULT_SEARCH_ENGINE: &str = "auto";
pub const DEFAULT_FETCH_ENGINE: &str = "local";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The gateway gives a fetch vendor 30 s per page; this leaves room for it.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_SEARCH_RESULTS: u64 = 10;

/// What the web tools need from the engine's config and login state, set on
/// the tool state at the start of every turn.
#[derive(Debug, Clone)]
pub struct WebTools {
    pub api_url: String,
    pub encrypt_secrets: bool,
    pub search_engine: String,
    pub fetch_engine: String,
    /// A JuCode session exists; `web_search` is offered only then.
    pub signed_in: bool,
}

pub fn search_definition() -> Value {
    json!({
        "type": "function",
        "name": "web_search",
        "description": "Search the web and return ranked results: title, url, a snippet of the relevant page text, and the published date when known. Use it for current information, or to find pages you do not have a URL for; then read the pages you need in full with web_fetch. Write the query the way you would type it into a search engine. Each call is billed to the user's JuCode account.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query, at most 400 characters." },
                "max_results": { "type": "number", "description": "Number of results, 1-10. Defaults to 10." },
                "freshness": { "type": "string", "enum": ["day", "week", "month", "year"], "description": "Only return pages published within this period." }
            },
            "required": ["query"],
            "additionalProperties": false
        }
    })
}

pub fn run_search(args: &Value, web: Option<&WebTools>) -> Value {
    let Some(web) = web else {
        return json!({ "error": "web_search is not available in this session" });
    };
    let body = match search_body(args, &web.search_engine) {
        Ok(body) => body,
        Err(error) => return json!({ "error": error }),
    };
    match gateway_post(web, "/tools/v1/search", body) {
        Ok(value) => {
            let results = value
                .get("results")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            crate::log_info!(
                "web_search",
                "searched",
                engine = web.search_engine.clone(),
                results = results
            );
            value
        }
        Err(error) => {
            crate::log_warn!(
                "web_search",
                "search failed",
                engine = web.search_engine.clone(),
                error = error.clone()
            );
            json!({ "error": error })
        }
    }
}

/// Runs `web_fetch` on the configured engine: `local` fetches from this
/// machine, anything else goes through the gateway.
pub fn run_fetch(args: &Value, web: Option<&WebTools>) -> Value {
    let Some(web) = web.filter(|web| web.fetch_engine != "local") else {
        return crate::web_fetch::run(args);
    };
    let Some(url) = args.get("url").and_then(Value::as_str) else {
        return json!({ "error": "missing url" });
    };
    if let Err(error) = crate::web_fetch::validate_url(url) {
        return json!({ "url": url, "error": error });
    }
    let max_bytes = args.get("max_bytes").and_then(Value::as_u64);
    let body = json!({ "url": url, "engine": web.fetch_engine });
    match gateway_post(web, "/tools/v1/fetch", body) {
        Ok(value) => {
            crate::log_info!(
                "web_fetch",
                "fetched via gateway",
                engine = web.fetch_engine.clone()
            );
            fetch_result(&value, url, max_bytes)
        }
        Err(error) => {
            crate::log_warn!(
                "web_fetch",
                "gateway fetch failed",
                engine = web.fetch_engine.clone(),
                error = error.clone()
            );
            json!({ "url": url, "error": error })
        }
    }
}

fn search_body(args: &Value, engine: &str) -> Result<Value, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .ok_or("missing query")?;
    let mut body = json!({ "query": query, "engine": engine });
    if let Some(value) = args.get("max_results") {
        let count = value
            .as_u64()
            .filter(|count| (1..=MAX_SEARCH_RESULTS).contains(count))
            .ok_or("max_results must be an integer from 1 to 10")?;
        body["max_results"] = json!(count);
    }
    if let Some(freshness) = args.get("freshness").and_then(Value::as_str) {
        body["freshness"] = json!(freshness);
    }
    Ok(body)
}

/// Shapes a gateway fetch like a local one (`url`, `status`, `text`), capped
/// at `max_bytes` when the caller asked for less.
fn fetch_result(value: &Value, requested_url: &str, max_bytes: Option<u64>) -> Value {
    let mut text = value
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut result = json!({
        "url": value.get("url").and_then(Value::as_str).unwrap_or(requested_url),
        "engine": value.get("engine").cloned().unwrap_or(Value::Null),
    });
    if let Some(status) = value.get("status").and_then(Value::as_u64) {
        result["status"] = json!(status);
    }
    if let Some(title) = value
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        result["title"] = json!(title);
    }
    if let Some(published) = value
        .get("published_at")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    {
        result["published_at"] = json!(published);
    }
    if let Some(cap) = max_bytes
        .map(|cap| cap as usize)
        .filter(|cap| text.len() > *cap)
    {
        let mut end = cap;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        result["truncated"] = json!(true);
    }
    result["text"] = json!(text);
    result
}

fn gateway_post(web: &WebTools, path: &str, body: Value) -> Result<Value, String> {
    let auth = crate::oauth::ensure_session(&web.api_url, web.encrypt_secrets)?;
    let token = auth
        .jucode_access_token()
        .ok_or("not logged in to JuCode. Run /login.")?;
    let url = format!("{}{}", web.api_url.trim_end_matches('/'), path);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .build();
    match agent
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body)
    {
        Ok(response) => response
            .into_json::<Value>()
            .map_err(|error| format!("invalid gateway response: {error}")),
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_json::<Value>().unwrap_or(Value::Null);
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            Err(format!("JuCode gateway HTTP {code}: {message}"))
        }
        Err(ureq::Error::Transport(transport)) => {
            Err(format!("could not reach the JuCode gateway: {transport}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_body_carries_engine_and_validates() {
        let body = search_body(
            &json!({ "query": " rust ", "max_results": 3, "freshness": "week" }),
            "brave",
        )
        .unwrap();
        assert_eq!(
            body,
            json!({ "query": "rust", "engine": "brave", "max_results": 3, "freshness": "week" })
        );
        assert_eq!(
            search_body(&json!({ "query": "  " }), "auto").unwrap_err(),
            "missing query"
        );
        assert!(search_body(&json!({ "query": "x", "max_results": 11 }), "auto").is_err());
        assert!(search_body(&json!({ "query": "x", "max_results": 2.5 }), "auto").is_err());
    }

    #[test]
    fn fetch_result_matches_local_shape_and_caps_text() {
        let gateway = json!({
            "object": "web.fetch", "engine": "jina", "url": "https://example.com/",
            "title": "Example", "content": "héllo world", "status": 200
        });
        let result = fetch_result(&gateway, "https://example.com", Some(2));
        assert_eq!(result["url"], "https://example.com/");
        assert_eq!(result["status"], 200);
        assert_eq!(result["title"], "Example");
        // The cap backs off to a char boundary instead of splitting "é".
        assert_eq!(result["text"], "h");
        assert_eq!(result["truncated"], true);

        let full = fetch_result(&gateway, "https://example.com", None);
        assert_eq!(full["text"], "héllo world");
        assert!(full.get("truncated").is_none());
    }

    #[test]
    fn local_engine_and_missing_config_fetch_locally() {
        let local = WebTools {
            api_url: "https://api.jucode.cn".to_string(),
            encrypt_secrets: false,
            search_engine: "auto".to_string(),
            fetch_engine: "local".to_string(),
            signed_in: true,
        };
        for web in [None, Some(&local)] {
            let result = run_fetch(&json!({ "url": "ftp://example.com" }), web);
            assert!(
                result["error"].as_str().unwrap().contains("http"),
                "{result}"
            );
        }
    }

    #[test]
    fn search_without_config_reports_unavailable() {
        let result = run_search(&json!({ "query": "x" }), None);
        assert!(result["error"].as_str().unwrap().contains("not available"));
    }
}
