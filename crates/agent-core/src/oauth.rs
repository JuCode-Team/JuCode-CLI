//! JuCode gateway OAuth: the `/cli/oauth` authorization-code flow against the
//! JuCode web/API pair, plus the token refresh the LLM client uses.
//!
//! Provider-agnostic pieces (PKCE, loopback callbacks, URL encoding, browser
//! launch, JSON plumbing) live in `llm_provider_kit::oauth`; this module owns
//! what is JuCode's own: the gateway endpoints, the device label, and the
//! marketplace model list.

use llm_provider_kit::oauth::{
    open_browser, parse_callback_query, pkce_challenge, random_token, unix_now, url_encode,
    write_callback_response,
};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader},
    net::TcpListener,
    process::Command,
    thread,
    time::{Duration, Instant},
};

const CLIENT_ID: &str = "jucode-cli";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
/// Text the browser shows after landing on the CLI callback.
const CALLBACK_LOGIN_COMPLETE: &str = "JuCode CLI login complete. You can close this tab.";
const CALLBACK_LOGIN_FAILED: &str = "JuCode CLI login failed. Return to the terminal.";

#[derive(Debug)]
pub struct OAuthLoginResult {
    pub web_url: String,
    pub api_url: String,
    pub tokens: Tokens,
    pub models: Vec<OAuthModel>,
}

/// OAuth token bundle. Times are absolute unix seconds so the caller can
/// decide when to refresh without re-deriving from a relative TTL.
#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub access_expires_at: u64,
    pub refresh_expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthModel {
    pub id: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub reasoning_efforts: Option<Vec<String>>,
}

pub fn login(web_url: &str, api_url: &str) -> Result<OAuthLoginResult, String> {
    let web_url = web_url.trim().trim_end_matches('/').to_string();
    let api_url = api_url.trim().trim_end_matches('/').to_string();
    if web_url.is_empty() {
        return Err("JuCode web URL cannot be empty".to_string());
    }
    if api_url.is_empty() {
        return Err("JuCode API URL cannot be empty".to_string());
    }

    let verifier = random_token(32)?;
    let challenge = pkce_challenge(&verifier);
    let state = random_token(24)?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let authorize_url = format!(
        "{}/cli/oauth?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        web_url,
        url_encode(CLIENT_ID),
        url_encode(&redirect_uri),
        url_encode(&challenge),
        url_encode(&state),
    );
    open_browser(&authorize_url)
        .map_err(|error| format!("{error}. Open manually: {authorize_url}"))?;

    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err("timed out waiting for OAuth callback".to_string());
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    stream
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(CALLBACK_TIMEOUT))
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|error| error.to_string())?;
    let params = parse_callback_query(&request_line)?;
    let mut stream = reader.into_inner();

    if params.get("state") != Some(&state) {
        write_callback_response(&mut stream, CALLBACK_LOGIN_FAILED)?;
        return Err("OAuth state mismatch".to_string());
    }
    let Some(code) = params.get("code").filter(|value| !value.is_empty()) else {
        write_callback_response(&mut stream, CALLBACK_LOGIN_FAILED)?;
        return Err("OAuth callback did not include code".to_string());
    };
    write_callback_response(&mut stream, CALLBACK_LOGIN_COMPLETE)?;

    let tokens = exchange_code(&api_url, code, &redirect_uri, &verifier, &device_name())?;
    let models = fetch_models(&api_url, &tokens.access_token).unwrap_or_default();
    Ok(OAuthLoginResult {
        web_url,
        api_url,
        tokens,
        models,
    })
}

fn exchange_code(
    base_url: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    device_name: &str,
) -> Result<Tokens, String> {
    let url = format!("{}/v1/oauth/token", base_url);
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
            "device_name": device_name,
        }));
    parse_tokens(&json_response(response)?)
}

/// Exchange a refresh token for a fresh access+refresh pair (rotation).
/// Used by the LLM client when the access token has expired or is rejected.
pub fn refresh(api_url: &str, refresh_token: &str) -> Result<Tokens, String> {
    let api_url = api_url.trim().trim_end_matches('/');
    if api_url.is_empty() {
        return Err("JuCode API URL cannot be empty".to_string());
    }
    let url = format!("{}/v1/oauth/token", api_url);
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }));
    parse_tokens(&json_response(response)?)
}

fn parse_tokens(value: &Value) -> Result<Tokens, String> {
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "OAuth token response did not include access_token".to_string())?;
    let refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "OAuth token response did not include refresh_token".to_string())?;
    let now = unix_now();
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    let refresh_expires_in = value
        .get("refresh_expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(90 * 24 * 3600);
    Ok(Tokens {
        access_token,
        refresh_token,
        access_expires_at: now.saturating_add(expires_in),
        refresh_expires_at: now.saturating_add(refresh_expires_in),
    })
}

/// GET an OAuth-protected JSON endpoint (e.g. /v1/oauth/userinfo) with the
/// device access token. Used by the `/usage` command.
pub fn get_json(api_url: &str, path: &str, access_token: &str) -> Result<Value, String> {
    let url = format!("{}{}", api_url.trim_end_matches('/'), path);
    json_response(
        ureq::get(&url)
            .set("Authorization", &format!("Bearer {access_token}"))
            .call(),
    )
}

fn fetch_models(base_url: &str, access_token: &str) -> Result<Vec<OAuthModel>, String> {
    let url = format!("{}/v1/models", base_url);
    let value = json_response(
        ureq::get(&url)
            .set("Authorization", &format!("Bearer {access_token}"))
            .call(),
    )?;
    Ok(parse_models_response(&value))
}

/// A human-facing device label shown under 授权设备管理. Best-effort
/// hostname + OS; never fails (falls back to a generic label).
fn device_name() -> String {
    let host = hostname().unwrap_or_else(|| "unknown-host".to_string());
    format!("JuCode CLI · {host} ({})", std::env::consts::OS)
}

fn hostname() -> Option<String> {
    if cfg!(windows) {
        return std::env::var("COMPUTERNAME")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

fn parse_models_response(value: &Value) -> Vec<OAuthModel> {
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_model)
        .collect()
}

fn parse_model(item: &Value) -> Option<OAuthModel> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    Some(OAuthModel {
        id,
        context_window: read_u64_field(item, &["context_window", "context_length"]),
        max_output_tokens: read_u64_field(item, &["max_output_tokens", "max_output"]),
        reasoning_efforts: item
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
            .filter(|values| !values.is_empty()),
    })
}

fn read_u64_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find_map(Value::as_u64)
}

/// Gateway errors name the service; the kit's helper reports the bare status.
fn json_response(response: Result<ureq::Response, ureq::Error>) -> Result<Value, String> {
    llm_provider_kit::oauth::json_response(response)
        .map_err(|error| format!("JuCode OAuth returned {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_metadata_from_models_response() {
        let value = json!({
            "data": [{
                "id": "gpt-5.5",
                "context_window": 1050000,
                "max_output_tokens": 128000,
                "reasoning_efforts": ["low", "medium"]
            }]
        });

        assert_eq!(
            parse_models_response(&value),
            vec![OAuthModel {
                id: "gpt-5.5".to_string(),
                context_window: Some(1_050_000),
                max_output_tokens: Some(128_000),
                reasoning_efforts: Some(vec!["low".to_string(), "medium".to_string()])
            }]
        );
    }

    #[test]
    fn parses_legacy_id_only_models_response() {
        let value = json!({ "data": [{ "id": "gpt-5.4-mini" }] });

        assert_eq!(
            parse_models_response(&value),
            vec![OAuthModel {
                id: "gpt-5.4-mini".to_string(),
                context_window: None,
                max_output_tokens: None,
                reasoning_efforts: None
            }]
        );
    }
}
