//! Policy-driven provider login over the vendored omp catalog.
//!
//! This is the Rust port of oh-my-pi's declarative auth engines
//! (`packages/ai/src/registry/engine/`): the KDL rules compiled into
//! `omp-catalog.json` describe each provider's OAuth authorization-code,
//! device-code, or api-key flow; this module executes them.
//!
//! What is deliberately not ported: `login "custom"` hooks (github-copilot,
//! cursor, perplexity, …), `afterExchange`/`afterRefresh` identity hooks,
//! and env hooks. `manualOnly`/`nativeScheme` callbacks are served by the
//! manual-paste path (no native URI-handler registration). Providers needing
//! the rest are reported unsupported rather than half-working.

use crate::oauth;
use jucode_vendor::omp::{
    catalog, AuthProvider, AuthValue, CredentialField, CredentialMap, DeviceCodeLogin, ExpiresRule,
    LoginRule, OauthCodeLogin, TokenRequest, UserinfoRule,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::Path,
    sync::mpsc::Receiver,
    thread,
    time::{Duration, Instant},
};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Value supplied for `{claude_code_sdk_version}` in anthropic's refresh
/// headers; mirrors upstream's claude-code-fingerprint constant.
const CLAUDE_CODE_SDK_VERSION: &str = "0.112.1";
const XAI_OIDC_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
/// Far-future expiry for credentials that never expire (durable keys).
const NEVER_EXPIRES_MS: u64 = 8_640_000_000_000_000;

/// Stored OAuth credential for one provider, in epoch **milliseconds** to
/// match the upstream credential shape.
#[derive(Debug, Clone, Default)]
pub struct OmpCredential {
    pub access: String,
    pub refresh: String,
    pub expires_at_ms: u64,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub org_id: Option<String>,
    pub org_name: Option<String>,
    pub project_id: Option<String>,
    pub api_endpoint: Option<String>,
    pub enterprise_url: Option<String>,
}

pub enum OmpLoginOutcome {
    /// oauth-code / device-code produced a credential to store.
    Credentials(Box<OmpCredential>),
    /// api-key provider: no browser flow; the caller should surface these
    /// instructions and let the user configure the key themselves.
    ApiKeyInstructions {
        auth_url: Option<String>,
        instructions: Option<String>,
        prompt: Option<String>,
    },
}

/// Runs the declared login flow for `provider_id`. `on_auth` is invoked when
/// user action is required (browser opened / device code to enter) so the UI
/// can surface URL + instructions while the flow blocks. `paste_rx` carries
/// pasted redirect URLs/codes for `manualOnly` callbacks (fed by the
/// `/login-paste` command); ignored by other flows.
pub fn login(
    provider_id: &str,
    profile_dir: &Path,
    on_auth: &dyn Fn(&str, Option<&str>),
    on_notice: &dyn Fn(&str),
    paste_rx: Option<Receiver<String>>,
) -> Result<OmpLoginOutcome, String> {
    let provider = catalog()
        .auth_provider(provider_id)
        .ok_or_else(|| format!("unknown provider {provider_id}"))?;
    let login = provider.login.as_ref().ok_or_else(|| {
        format!(
            "{} has no login flow; configure a key via auth.json",
            provider.name
        )
    })?;
    match login {
        LoginRule::ApiKey(rule) => Ok(OmpLoginOutcome::ApiKeyInstructions {
            auth_url: rule.auth_url.clone(),
            instructions: rule.instructions.clone(),
            prompt: rule.prompt.clone(),
        }),
        LoginRule::OauthCode(rule) => {
            oauth_code_login(provider, rule, on_auth, on_notice, paste_rx)
                .map(|c| OmpLoginOutcome::Credentials(Box::new(c)))
        }
        LoginRule::DeviceCode(rule) => {
            device_code_login(provider, rule, profile_dir, on_auth, on_notice)
                .map(|c| OmpLoginOutcome::Credentials(Box::new(c)))
        }
        LoginRule::Custom { hook } => Err(format!(
            "{} uses a custom login flow ({hook}) that JuCode cannot run yet",
            provider.name
        )),
    }
}

/// Refreshes a stored credential per the provider's `refresh` rule. Returns
/// an error when the rule is `none` (re-login required) or a hook we can't
/// run. Unrotated refresh tokens are preserved from the stored credential.
pub fn refresh(
    provider_id: &str,
    stored: &OmpCredential,
    profile_dir: &Path,
) -> Result<OmpCredential, String> {
    let provider = catalog()
        .auth_provider(provider_id)
        .ok_or_else(|| format!("unknown provider {provider_id}"))?;
    let rule = provider.refresh.as_ref().ok_or_else(|| {
        format!(
            "{} cannot refresh; run /login {provider_id} again",
            provider.name
        )
    })?;
    match rule.kind.as_str() {
        "request" => {}
        "hook" => {
            return Err(format!(
                "{} refresh needs a custom hook; run /login {provider_id} again",
                provider.name
            ))
        }
        _ => {
            return Err(format!(
                "{} session expired; run /login {provider_id} again",
                provider.name
            ))
        }
    }
    for field in &rule.require {
        let present = match field.as_str() {
            "refresh" => !stored.refresh.is_empty(),
            "access" => !stored.access.is_empty(),
            "accountId" => stored.account_id.is_some(),
            "projectId" => stored.project_id.is_some(),
            _ => true,
        };
        if !present {
            return Err(format!(
                "{} credentials are missing {field}; run /login {provider_id} again",
                provider.name
            ));
        }
    }
    // Client identity comes from the provider's login rule, per upstream.
    let client = login_client(provider)?;
    let token = rule
        .token
        .as_ref()
        .ok_or_else(|| format!("{} refresh rule has no token request", provider.name))?;
    let vars = HashMap::from([
        ("refresh_token", stored.refresh.clone()),
        ("client_id", client.client_id.clone().unwrap_or_default()),
        (
            "client_secret",
            client.client_secret.clone().unwrap_or_default(),
        ),
        ("redirect_uri", client.redirect_uri.unwrap_or_default()),
        ("base", client.base_url.unwrap_or_default()),
        (
            "claude_code_sdk_version",
            CLAUDE_CODE_SDK_VERSION.to_string(),
        ),
    ]);
    let headers = hook_headers(rule.headers_hook.as_deref(), profile_dir)?;
    let body = post_token_request(
        provider_id,
        token,
        &[
            ("grant_type", Some("refresh_token".to_string())),
            ("client_id", client.client_id.clone()),
            ("client_secret", client.client_secret.clone()),
            ("refresh_token", Some(stored.refresh.clone())),
        ],
        &vars,
        &headers,
    )?;
    let credential_map = rule
        .credential
        .as_ref()
        .ok_or_else(|| format!("{} refresh rule has no credential map", provider.name))?;
    let mut refreshed = map_credentials(provider_id, credential_map, &body, Some(stored))?;
    apply_userinfo(rule.userinfo.as_ref(), &mut refreshed);
    Ok(refreshed)
}

/// Extra headers a provider's declared `headersHook` contributes to auth
/// requests. Only hooks JuCode implements resolve; unknown hooks error.
fn hook_headers(hook: Option<&str>, profile_dir: &Path) -> Result<Vec<(String, String)>, String> {
    match hook {
        None => Ok(Vec::new()),
        Some("kimi-fingerprint") => Ok(kimi_headers(profile_dir)),
        Some(name) => Err(format!(
            "auth rule needs unsupported headers hook \"{name}\""
        )),
    }
}

/// Kimi's telemetry headers (`kimi-fingerprint`): a stable per-install
/// device id plus platform markers, ported from upstream's kimi.ts.
fn kimi_headers(profile_dir: &Path) -> Vec<(String, String)> {
    let sanitize = |s: &str, fallback: &str| {
        let cleaned: String = s.chars().filter(|c| (' '..='~').contains(c)).collect();
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            fallback.to_string()
        } else {
            cleaned.to_string()
        }
    };
    let device_id_path = profile_dir.join("kimi-device-id");
    let device_id = std::fs::read_to_string(&device_id_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let id = oauth::random_token(16).ok()?.replace('-', "");
            let _ = std::fs::create_dir_all(profile_dir);
            let _ = std::fs::write(&device_id_path, format!("{id}\n"));
            Some(id)
        })
        .unwrap_or_else(|| "unknown".to_string());
    let version = env!("CARGO_PKG_VERSION");
    vec![
        ("User-Agent".to_string(), format!("KimiCLI/{version}")),
        ("X-Msh-Platform".to_string(), "kimi_cli".to_string()),
        ("X-Msh-Version".to_string(), version.to_string()),
        (
            "X-Msh-Device-Name".to_string(),
            sanitize(&hostname(), "unknown"),
        ),
        (
            "X-Msh-Device-Id".to_string(),
            sanitize(&device_id, "unknown"),
        ),
        (
            "X-Msh-Os-Version".to_string(),
            sanitize(std::env::consts::OS, "unknown"),
        ),
    ]
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

// ---- oauth-code -----------------------------------------------------------

fn oauth_code_login(
    provider: &AuthProvider,
    rule: &OauthCodeLogin,
    on_auth: &dyn Fn(&str, Option<&str>),
    on_notice: &dyn Fn(&str),
    paste_rx: Option<Receiver<String>>,
) -> Result<OmpCredential, String> {
    let callback = &rule.callback;
    // `manualOnly` skips the loopback server entirely; `nativeScheme` targets
    // a custom URI scheme we can't register — for both, the documented path
    // is the user pasting the redirect URL or code back.
    let manual = callback.manual_only || callback.native_scheme;
    let client_id = rule.client_id.as_ref().map(resolve_value).transpose()?;
    let client_secret = rule.client_secret.as_ref().map(resolve_value).transpose()?;
    let authorize_url = resolve_value(&rule.authorize_url)?;

    let (listeners, redirect_uri, path) = if manual {
        let uri = resolve_value(
            callback
                .redirect_uri
                .as_ref()
                .ok_or_else(|| format!("{} manual flow has no redirectUri", provider.name))?,
        )?;
        (Vec::new(), uri, String::new())
    } else {
        // Redirect override: env/value wins; only loopback HTTP is receivable.
        let (bind_host, mut port, path) = match &callback.redirect_uri {
            Some(value) => {
                let uri = resolve_value(value)?;
                parse_loopback_uri(&uri).ok_or_else(|| {
                    format!(
                        "{} redirect override must be loopback http://",
                        provider.name
                    )
                })?
            }
            None => (
                callback.hostname.clone(),
                callback.port,
                callback.path.clone(),
            ),
        };

        // Browsers resolve `localhost` to ::1 first on dual-stack hosts, so a
        // v4-only listener never sees the callback. Bind both stacks.
        let bind_hosts: Vec<String> = if bind_host == "localhost" || bind_host == "[::1]" {
            vec!["127.0.0.1".to_string(), "[::1]".to_string()]
        } else {
            vec![bind_host.clone()]
        };
        let mut listeners = Vec::new();
        for host in &bind_hosts {
            if let Ok(l) = TcpListener::bind(format!("{host}:{port}")) {
                listeners.push(l);
            }
        }
        if listeners.is_empty() {
            if !callback.port_fallback {
                return Err(format!("cannot bind OAuth callback port {port}"));
            }
            let l = TcpListener::bind(format!("{}:0", bind_hosts[0]))
                .map_err(|e| format!("cannot bind OAuth callback (port {port} busy; {e})"))?;
            port = l.local_addr().map_err(|e| e.to_string())?.port();
            listeners.push(l);
            for host in &bind_hosts[1..] {
                if let Ok(l) = TcpListener::bind(format!("{host}:{port}")) {
                    listeners.push(l);
                }
            }
        }
        for listener in &listeners {
            listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        }
        (listeners, format!("http://{bind_host}:{port}{path}"), path)
    };

    let (verifier, challenge) = if rule.pkce {
        let v = oauth::random_token(32)?;
        (v.clone(), Some(oauth::pkce_challenge(&v)))
    } else {
        (String::new(), None)
    };
    let state = match rule.state.as_str() {
        "none" => String::new(),
        "uuid" => uuid(),
        _ => oauth::random_token(24)?,
    };
    let scope = if rule.scopes.is_empty() {
        None
    } else {
        Some(rule.scopes.join(&rule.scope_separator))
    };

    let vars: HashMap<&str, String> = [
        ("client_id", client_id.clone().unwrap_or_default()),
        ("redirect_uri", redirect_uri.clone()),
        ("scope", scope.clone().unwrap_or_default()),
        ("state", state.clone()),
        ("code_challenge", challenge.clone().unwrap_or_default()),
    ]
    .into_iter()
    .collect();
    let mut params: Vec<(String, String)> = Vec::new();
    if rule.standard_authorize_params {
        if let Some(id) = &client_id {
            params.push(("client_id".into(), id.clone()));
        }
        params.push(("response_type".into(), "code".into()));
        params.push(("redirect_uri".into(), redirect_uri.clone()));
        if let Some(scope) = &scope {
            params.push(("scope".into(), scope.clone()));
        }
        if let Some(challenge) = &challenge {
            params.push(("code_challenge".into(), challenge.clone()));
            params.push(("code_challenge_method".into(), "S256".into()));
        }
        if !state.is_empty() {
            params.push(("state".into(), state.clone()));
        }
    }
    for (k, v) in &rule.authorize_params {
        params.push((k.clone(), template(v, &vars)));
    }
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", oauth::url_encode(k), oauth::url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let url = format!("{authorize_url}?{query}");
    oauth::open_browser(&url).map_err(|e| format!("{e}. Open manually: {url}"))?;
    let instructions = if manual {
        let hint = "then run: /login-paste <redirect-url-or-code>";
        match rule.instructions.as_deref() {
            Some(i) => Some(format!("{i}\n{hint}")),
            None => Some(hint.to_string()),
        }
    } else {
        rule.instructions.clone()
    };
    on_auth(&url, instructions.as_deref());

    let code = if manual {
        wait_for_pasted_code(
            paste_rx
                .as_ref()
                .ok_or_else(|| format!("{} login needs a paste channel", provider.name))?,
            &state,
            on_notice,
        )?
    } else {
        wait_for_callback(&listeners, &path, &state)?
    };
    // Providers may echo `code#state`; the fragment wins over callback state.
    let (exchange_code, exchange_state) = match code.split_once('#') {
        Some((c, s)) => (
            c.to_string(),
            if s.is_empty() {
                state.clone()
            } else {
                s.to_string()
            },
        ),
        None => (code, state.clone()),
    };
    let vars: HashMap<&str, String> = [
        ("code", exchange_code.clone()),
        ("state", exchange_state),
        ("redirect_uri", redirect_uri.clone()),
        ("code_verifier", verifier.clone()),
        ("client_id", client_id.clone().unwrap_or_default()),
        ("client_secret", client_secret.clone().unwrap_or_default()),
    ]
    .into_iter()
    .collect();
    let body = post_token_request(
        &provider.id,
        &rule.token,
        &[
            ("grant_type", Some("authorization_code".to_string())),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", Some(exchange_code)),
            ("redirect_uri", Some(redirect_uri)),
            (
                "code_verifier",
                if rule.pkce { Some(verifier) } else { None },
            ),
        ],
        &vars,
        &[],
    )
    // Authorization codes are single-use and short-lived; a stale or
    // already-consumed paste surfaces as an opaque server error.
    .map_err(|e| format!("{e} — the code may be expired or already used; run /login again"))?;
    let mut credentials = map_credentials(&provider.id, &rule.credential, &body, None)?;
    apply_userinfo(rule.userinfo.as_ref(), &mut credentials);
    apply_after_exchange(rule.after_exchange.as_deref(), &mut credentials, on_notice)?;
    Ok(credentials)
}

/// Accepts one loopback callback on any bound listener, validates state,
/// returns the auth code.
fn wait_for_callback(
    listeners: &[TcpListener],
    expected_path: &str,
    expected_state: &str,
) -> Result<String, String> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    let stream = 'accept: loop {
        for listener in listeners {
            match listener.accept() {
                Ok((stream, _)) => break 'accept stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for OAuth callback".to_string());
        }
        thread::sleep(Duration::from_millis(50));
    };
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(CALLBACK_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| e.to_string())?;
    let params = oauth::parse_callback_query(&request_line)?;
    let mut stream = reader.into_inner();
    // Wrong-path requests (favicon etc.) get a quiet 404 and no result.
    if !request_line
        .split_whitespace()
        .nth(1)
        .map(|p| p.split('?').next() == Some(expected_path))
        .unwrap_or(false)
    {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return wait_for_callback(listeners, expected_path, expected_state);
    }
    if !expected_state.is_empty() && params.get("state").map(String::as_str) != Some(expected_state)
    {
        oauth::write_callback_response(&mut stream, false)?;
        return Err("OAuth state mismatch".to_string());
    }
    let code = params
        .get("code")
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| params.get("token").filter(|v| !v.is_empty()).cloned());
    match code {
        Some(code) => {
            oauth::write_callback_response(&mut stream, true)?;
            Ok(code)
        }
        None => {
            oauth::write_callback_response(&mut stream, false)?;
            let error = params
                .get("error_description")
                .or_else(|| params.get("error"))
                .cloned()
                .unwrap_or_else(|| "OAuth callback did not include code".to_string());
            Err(error)
        }
    }
}

/// Blocks until the user pastes a redirect URL or code through
/// `/login-paste` (upstream `onManualCodeInput`). Rejected pastes are
/// reported so the user can retry; channel close means the flow was
/// abandoned.
fn wait_for_pasted_code(
    rx: &Receiver<String>,
    expected_state: &str,
    on_notice: &dyn Fn(&str),
) -> Result<String, String> {
    loop {
        let input = rx
            .recv()
            .map_err(|_| "login cancelled while waiting for pasted code".to_string())?;
        let (code, state) = parse_callback_input(&input);
        let Some(code) = code else {
            on_notice("that doesn't look like a redirect URL or code — paste the full zcode://… address from the browser");
            continue;
        };
        if !expected_state.is_empty() && state.as_deref().is_some_and(|s| s != expected_state) {
            on_notice("state mismatch — that redirect URL is from a different login attempt; authorize again and paste the new URL");
            continue;
        }
        return Ok(code);
    }
}

/// Extracts `code`/`state` from a pasted redirect URL, a `?code=…` query
/// string, or a bare code (port of upstream `parseCallbackInput`).
fn parse_callback_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    let query = if let Some(pos) = value.find('?') {
        value[pos + 1..].split('#').next().unwrap_or_default()
    } else if value.contains("code=") {
        value.trim_start_matches(['?', '#'])
    } else {
        return (Some(value.to_string()), None);
    };
    let pairs: HashMap<String, String> = query
        .split('&')
        .filter_map(|part| {
            let (key, val) = part.split_once('=')?;
            Some((oauth::url_decode(key), oauth::url_decode(val)))
        })
        .collect();
    (pairs.get("code").cloned(), pairs.get("state").cloned())
}

/// Parses `http://host:port/path` loopback URIs for redirect overrides.
fn parse_loopback_uri(uri: &str) -> Option<(String, u16, String)> {
    let rest = uri.strip_prefix("http://")?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 80),
    };
    if host != "localhost" && host != "127.0.0.1" && host != "[::1]" {
        return None;
    }
    Some((host, port, path))
}

// ---- device-code ----------------------------------------------------------

fn device_code_login(
    provider: &AuthProvider,
    rule: &DeviceCodeLogin,
    profile_dir: &Path,
    on_auth: &dyn Fn(&str, Option<&str>),
    on_notice: &dyn Fn(&str),
) -> Result<OmpCredential, String> {
    let client_id = resolve_value(&rule.client_id)?;
    let base = rule.base_url.as_ref().map(resolve_value).transpose()?;
    let scope = if rule.scopes.is_empty() {
        None
    } else {
        Some(rule.scopes.join(&rule.scope_separator))
    };
    let headers = hook_headers(rule.headers_hook.as_deref(), profile_dir)?;
    let vars: HashMap<&str, String> = [
        ("client_id", client_id.clone()),
        ("scope", scope.clone().unwrap_or_default()),
        ("base", base.clone().unwrap_or_default()),
    ]
    .into_iter()
    .collect();
    let device = post_token_request_raw(
        &provider.id,
        &rule.device,
        &[
            ("client_id", Some(client_id.clone())),
            ("scope", scope.clone()),
        ],
        &vars,
        &headers,
    )?;
    if device.status >= 400 {
        return Err(format!(
            "{} device authorization failed: HTTP {} {}",
            provider.name, device.status, device.body
        ));
    }
    let body = &device.body;
    let user_code = json_path_str(body, rule.response.user_code.as_deref())
        .ok_or_else(|| format!("{} device response missing user code", provider.name))?;
    let device_code = json_path_str(body, rule.response.device_code.as_deref())
        .ok_or_else(|| format!("{} device response missing device code", provider.name))?;
    let verification_uri = json_path_str(body, rule.response.verification_uri.as_deref())
        .ok_or_else(|| format!("{} device response missing verification uri", provider.name))?;
    let verification_complete =
        json_path_str(body, rule.response.verification_uri_complete.as_deref());
    let interval = json_path_u64(body, rule.response.interval.as_deref()).unwrap_or(5);
    let expires_in = json_path_u64(body, rule.response.expires_in.as_deref()).unwrap_or(900);

    let display_url = verification_complete.unwrap_or_else(|| verification_uri.clone());
    let instructions = template(
        &rule.instructions,
        &HashMap::from([("user_code", user_code.clone())]),
    );
    on_auth(&display_url, Some(&instructions));

    let poll_vars: HashMap<&str, String> = [
        ("client_id", client_id.clone()),
        ("device_code", device_code.clone()),
        ("base", base.unwrap_or_default()),
    ]
    .into_iter()
    .collect();
    let mut wait = Duration::from_secs(interval.max(1));
    let deadline = Instant::now() + Duration::from_secs(expires_in);
    loop {
        thread::sleep(wait);
        if Instant::now() >= deadline {
            return Err(format!(
                "{} device code expired; run /login again",
                provider.name
            ));
        }
        let poll = post_token_request_raw(
            &provider.id,
            &rule.token,
            &[
                (
                    "grant_type",
                    Some("urn:ietf:params:oauth:grant-type:device_code".to_string()),
                ),
                ("client_id", Some(client_id.clone())),
                ("device_code", Some(device_code.clone())),
            ],
            &poll_vars,
            &headers,
        )?;
        if poll.status < 400 {
            let mut credentials =
                map_credentials(&provider.id, &rule.credential, &poll.body, None)?;
            apply_userinfo(rule.userinfo.as_ref(), &mut credentials);
            apply_after_exchange(rule.after_exchange.as_deref(), &mut credentials, on_notice)?;
            return Ok(credentials);
        }
        match json_path_str(&poll.body, Some("error")).as_deref() {
            Some("authorization_pending") | None => {}
            Some("slow_down") => wait += Duration::from_secs(5),
            Some("expired_token") => {
                return Err(format!(
                    "{} device code expired; run /login again",
                    provider.name
                ))
            }
            Some("access_denied") => {
                return Err(format!("{} device authorization was denied", provider.name))
            }
            Some(error) => {
                let detail = json_path_str(&poll.body, Some("error_description"))
                    .unwrap_or_else(|| error.to_string());
                return Err(format!(
                    "{} device token request failed: HTTP {} {}",
                    provider.name, poll.status, detail
                ));
            }
        }
    }
}

// ---- shared machinery (port of engine/common.ts) --------------------------

/// Resolves a KDL value node: env override, hook, then literal (+base64).
fn resolve_value(value: &AuthValue) -> Result<String, String> {
    for name in &value.env {
        if let Ok(v) = std::env::var(name) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    if let Some(hook) = &value.hook {
        return value_hook(hook);
    }
    let literal = value.value.clone().unwrap_or_default();
    if value.base64 {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&literal)
            .map_err(|e| format!("bad base64 auth value: {e}"))?;
        return String::from_utf8(bytes).map_err(|e| e.to_string());
    }
    Ok(literal)
}

/// Value hooks that have a Rust port. Unknown hooks error out the flow.
fn value_hook(name: &str) -> Result<String, String> {
    match name {
        "xai-token-endpoint" => {
            let discovery = oauth::json_response(
                ureq::get(XAI_OIDC_DISCOVERY_URL)
                    .timeout(Duration::from_secs(15))
                    .call(),
            )?;
            json_path_str(&discovery, Some("token_endpoint"))
                .ok_or_else(|| "xAI discovery response missing token_endpoint".to_string())
        }
        other => Err(format!(
            "auth rule needs unsupported value hook \"{other}\""
        )),
    }
}

/// Client identity for refresh: taken from the login rule (upstream
/// `loginClient`), not the refresh rule.
#[derive(Default)]
struct LoginClient {
    client_id: Option<String>,
    client_secret: Option<String>,
    redirect_uri: Option<String>,
    base_url: Option<String>,
}

fn login_client(provider: &AuthProvider) -> Result<LoginClient, String> {
    match &provider.login {
        Some(LoginRule::OauthCode(rule)) => Ok(LoginClient {
            client_id: rule.client_id.as_ref().map(resolve_value).transpose()?,
            client_secret: rule.client_secret.as_ref().map(resolve_value).transpose()?,
            redirect_uri: rule
                .callback
                .redirect_uri
                .as_ref()
                .map(resolve_value)
                .transpose()?
                .or_else(|| {
                    Some(format!(
                        "http://{}:{}{}",
                        rule.callback.hostname, rule.callback.port, rule.callback.path
                    ))
                }),
            ..LoginClient::default()
        }),
        Some(LoginRule::DeviceCode(rule)) => Ok(LoginClient {
            client_id: Some(resolve_value(&rule.client_id)?),
            base_url: rule.base_url.as_ref().map(resolve_value).transpose()?,
            ..LoginClient::default()
        }),
        _ => Ok(LoginClient::default()),
    }
}

/// `{name}` placeholder substitution; unknown keys resolve to empty.
fn template(text: &str, vars: &HashMap<&str, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        match rest[start..].find('}') {
            Some(len) => {
                let key = &rest[start + 1..start + len];
                if key.chars().all(|c| c.is_ascii_lowercase() || c == '_') && !key.is_empty() {
                    out.push_str(vars.get(key).map(String::as_str).unwrap_or_default());
                } else {
                    out.push_str(&rest[start..=start + len]);
                }
                rest = &rest[start + len + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Dotted-path JSON lookup (`data.user.email`).
fn json_path<'a>(body: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = body;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

fn json_path_str(body: &Value, path: Option<&str>) -> Option<String> {
    let value = json_path(body, path?)?;
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn json_path_u64(body: &Value, path: Option<&str>) -> Option<u64> {
    json_path(body, path?)?.as_u64()
}

/// Decodes a JWT payload without verifying; None when malformed.
fn decode_jwt_payload(token: &str) -> Option<Value> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Epoch ms of a JWT `exp` claim minus `skew_ms`.
fn jwt_expiry_ms(token: &str, skew_ms: u64) -> Option<u64> {
    let exp = decode_jwt_payload(token)?.get("exp")?.as_u64()?;
    Some(exp.saturating_mul(1000).saturating_sub(skew_ms))
}

fn now_ms() -> u64 {
    oauth::unix_now().saturating_mul(1000)
}

fn read_credential_field(
    field: Option<&CredentialField>,
    body: &Value,
    claims: Option<&Value>,
) -> Option<String> {
    let field = field?;
    if let Some(path) = &field.path {
        if let Some(value) = json_path_str(body, Some(path)) {
            return Some(value);
        }
    }
    // Claims are flat JWT payload keys, not dot paths (claim names can
    // themselves contain dots, e.g. "https://api.openai.com/auth").
    if let Some(claims) = claims {
        for claim in &field.claims {
            if let Some(value) = claims.get(claim) {
                match value {
                    Value::String(s) if !s.is_empty() => return Some(s.clone()),
                    Value::Number(n) => return Some(n.to_string()),
                    _ => {}
                }
            }
        }
    }
    field.literal.clone()
}

/// Projects a token response onto `OmpCredential` per the rule's credential
/// map. A missing refresh token keeps `previous.refresh`.
fn map_credentials(
    provider: &str,
    map: &CredentialMap,
    body: &Value,
    previous: Option<&OmpCredential>,
) -> Result<OmpCredential, String> {
    let access_no_claims = read_credential_field(Some(&map.access), body, None);
    let claims = access_no_claims.as_deref().and_then(decode_jwt_payload);
    let access = access_no_claims
        .or_else(|| read_credential_field(Some(&map.access), body, claims.as_ref()));
    let Some(access) = access else {
        let excerpt = body.to_string();
        return Err(format!(
            "{provider} token response missing access token: {}",
            &excerpt[..excerpt.len().min(500)]
        ));
    };
    let expires_at_ms = match &map.expires {
        ExpiresRule::Never => NEVER_EXPIRES_MS,
        ExpiresRule::Jwt {
            skew_ms,
            fallback_ms,
        } => jwt_expiry_ms(&access, *skew_ms)
            .unwrap_or_else(|| now_ms() + fallback_ms.unwrap_or(3_600_000)),
        ExpiresRule::Seconds {
            path,
            from_path,
            skew_ms,
            fallback_ms,
        } => {
            let seconds = json_path(body, path).and_then(Value::as_u64);
            match seconds {
                Some(seconds) => {
                    let base = from_path
                        .as_deref()
                        .and_then(|p| json_path(body, p))
                        .and_then(Value::as_u64)
                        .map(|from| from.saturating_mul(1000))
                        .unwrap_or_else(now_ms);
                    base.saturating_add(seconds.saturating_mul(1000))
                        .saturating_sub(*skew_ms)
                }
                None => match fallback_ms {
                    Some(fallback) => now_ms() + fallback,
                    None => return Err(format!("{provider} token response missing {path}")),
                },
            }
        }
    };
    let claims_ref = claims.as_ref();
    let read = |f: &Option<CredentialField>| read_credential_field(f.as_ref(), body, claims_ref);
    Ok(OmpCredential {
        access,
        refresh: read(&map.refresh)
            .or_else(|| previous.map(|p| p.refresh.clone()))
            .unwrap_or_default(),
        expires_at_ms,
        email: read(&map.email),
        account_id: read(&map.account_id),
        org_id: read(&map.org_id),
        org_name: read(&map.org_name),
        project_id: read(&map.project_id),
        api_endpoint: read(&map.api_endpoint),
        enterprise_url: read(&map.enterprise_url),
    })
}

// ---- afterExchange hooks ---------------------------------------------------

const ZAI_BIZ_BASE: &str = "https://api.z.ai";
/// Our label for the durable Z.ai API key minted during login.
const ZAI_KEY_NAME: &str = "jucode";
const MUSE_KEY_URL: &str = "https://api.meta.ai/muse-code/key";

/// Runs the declared `afterExchange` hook when it mints the real bearer
/// (zai's durable `id.secret`, muse's model api key). Enrichment-only hooks
/// (identity/project/cache lookups) are skipped — the bearer works without
/// them.
fn apply_after_exchange(
    hook: Option<&str>,
    credentials: &mut OmpCredential,
    on_notice: &dyn Fn(&str),
) -> Result<(), String> {
    match hook {
        Some("zai-mint-key") => {
            on_notice("provisioning a Z.AI API key…");
            credentials.access = zai_mint_key(&credentials.access)?;
            Ok(())
        }
        Some("muse-code-key") => {
            on_notice("provisioning a Muse Code API key…");
            muse_code_attach_key(credentials)
        }
        Some(_) | None => Ok(()),
    }
}

/// The value sent as the request bearer for a stored credential. muse-code
/// wraps `{oauthAccessToken, apiKey}` in JSON; everything else is verbatim.
pub fn bearer_token(credential: &OmpCredential) -> String {
    serde_json::from_str::<Value>(&credential.access)
        .ok()
        .and_then(|v| v.get("apiKey").and_then(Value::as_str).map(str::to_string))
        .filter(|key| !key.is_empty())
        .unwrap_or_else(|| credential.access.clone())
}

fn get_json(url: &str, bearer: &str) -> Result<Value, String> {
    let response = ureq::get(url)
        .timeout(DEFAULT_REQUEST_TIMEOUT)
        .set("Authorization", &format!("Bearer {bearer}"))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(status, response) => {
                let body = response.into_string().unwrap_or_default();
                format!("GET {url} failed ({status}): {}", truncate(&body, 160))
            }
            ureq::Error::Transport(t) => format!("GET {url} failed: {t}"),
        })?;
    response
        .into_json::<Value>()
        .map_err(|e| format!("GET {url} returned invalid JSON: {e}"))
}

fn post_json(url: &str, body: Value, bearer: Option<&str>) -> Result<Value, String> {
    let mut call = ureq::post(url)
        .timeout(DEFAULT_REQUEST_TIMEOUT)
        .set("Content-Type", "application/json");
    if let Some(bearer) = bearer {
        call = call.set("Authorization", &format!("Bearer {bearer}"));
    }
    let response = call.send_json(body).map_err(|e| match e {
        ureq::Error::Status(status, response) => {
            let body = response.into_string().unwrap_or_default();
            format!("POST {url} failed ({status}): {}", truncate(&body, 160))
        }
        ureq::Error::Transport(t) => format!("POST {url} failed: {t}"),
    })?;
    response
        .into_json::<Value>()
        .map_err(|e| format!("POST {url} returned invalid JSON: {e}"))
}

/// Z.ai's `{code, msg, data, success}` envelope: success when code is
/// absent/0/200; `data` is unwrapped when present (upstream unwrapEnvelope).
fn zai_unwrap(body: Value, operation: &str) -> Result<Value, String> {
    let Some(obj) = body.as_object() else {
        return Ok(body);
    };
    if !obj.contains_key("code") && !obj.contains_key("success") {
        return Ok(body);
    }
    let code_ok = match obj.get("code") {
        None => true,
        Some(Value::Number(n)) => matches!(n.as_i64(), Some(0 | 200)),
        Some(Value::String(s)) => s == "0" || s == "200",
        _ => false,
    };
    if obj.get("success").and_then(Value::as_bool) == Some(false) || !code_ok {
        let msg = obj
            .get("msg")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("code {}", obj.get("code").unwrap_or(&Value::Null)));
        return Err(format!("z.ai {operation} failed: {msg}"));
    }
    Ok(obj.get("data").cloned().unwrap_or(body))
}

/// `zai-mint-key`: exchange the short-lived OAuth token for a durable
/// `apiKey.secretKey` via the business APIs (port of upstream zai.ts).
fn zai_mint_key(oauth_access: &str) -> Result<String, String> {
    let login = post_json(
        &format!("{ZAI_BIZ_BASE}/api/auth/z/login"),
        json!({ "token": oauth_access }),
        None,
    )?;
    let biz_token = zai_unwrap(login, "business login").and_then(|data| {
        json_path_str(&data, Some("access_token"))
            .or_else(|| json_path_str(&data, Some("accessToken")))
            .ok_or_else(|| "z.ai business login returned no access token".to_string())
    })?;

    let customer = zai_unwrap(
        get_json(
            &format!("{ZAI_BIZ_BASE}/api/biz/customer/getCustomerInfo"),
            &biz_token,
        )?,
        "customer lookup",
    )?;
    let orgs = json_path(&customer, "organizations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let org = orgs
        .iter()
        .find(|o| o.get("isDefault").and_then(Value::as_bool) == Some(true))
        .or_else(|| orgs.first());
    let projects = org
        .and_then(|o| o.get("projects"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let project = projects
        .iter()
        .find(|p| p.get("isDefault").and_then(Value::as_bool) == Some(true))
        .or_else(|| projects.first());
    let org_id = org
        .and_then(|o| o.get("organizationId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let project_id = project
        .and_then(|p| p.get("projectId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (org_id, project_id) = match (org_id, project_id) {
        (Some(o), Some(p)) => (o.to_string(), p.to_string()),
        _ => {
            return Err(
                "z.ai key provisioning failed: no organization/project on account".to_string(),
            )
        }
    };

    let keys_url =
        format!("{ZAI_BIZ_BASE}/api/biz/v1/organization/{org_id}/projects/{project_id}/api_keys");
    let list = zai_unwrap(get_json(&keys_url, &biz_token)?, "api key list")?;
    let entries: Vec<Value> = match &list {
        Value::Array(items) => items.clone(),
        Value::Object(obj) => ["list", "keys", "apiKeys", "records"]
            .iter()
            .find_map(|key| obj.get(*key).and_then(Value::as_array).cloned())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let record = match entries
        .into_iter()
        .find(|key| key.get("name").and_then(Value::as_str) == Some(ZAI_KEY_NAME))
    {
        Some(record) => record,
        None => zai_unwrap(
            post_json(&keys_url, json!({ "name": ZAI_KEY_NAME }), Some(&biz_token))?,
            "api key create",
        )?,
    };
    let api_key = record
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "z.ai key provisioning returned no apiKey".to_string())?;

    // The copy endpoint always returns the full secret; list entries mask it.
    let copied = zai_unwrap(
        get_json(
            &format!("{keys_url}/copy/{}", oauth::url_encode(api_key)),
            &biz_token,
        )?,
        "api key copy",
    )?;
    let secret = copied
        .get("secretKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "z.ai key provisioning returned no secretKey".to_string())?;
    Ok(format!("{api_key}.{secret}"))
}

/// `muse-code-key`: exchange the Meta OAuth token for the model API key a
/// Muse subscription entitles (port of upstream muse-code.ts). The minted
/// credential stores `{"oauthAccessToken","apiKey"}` — `bearer_token`
/// unwraps it at request time. Reuses an already-minted key since the
/// endpoint is aggressively rate-limited.
fn muse_code_attach_key(credentials: &mut OmpCredential) -> Result<(), String> {
    if let Ok(existing) = serde_json::from_str::<Value>(&credentials.access) {
        let has_key = existing
            .get("apiKey")
            .and_then(Value::as_str)
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false);
        if has_key {
            return Ok(());
        }
    }
    let response = ureq::post(MUSE_KEY_URL)
        .timeout(DEFAULT_REQUEST_TIMEOUT)
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {}", credentials.access))
        .set("Content-Type", "application/json")
        .set("x-api-version", "1.0.0")
        .send_json(json!({ "onboard": true }))
        .map_err(|e| match e {
            ureq::Error::Status(status, response) => {
                let body = response.into_string().unwrap_or_default();
                format!(
                    "muse-code key exchange failed ({status}): {}",
                    truncate(&body, 160)
                )
            }
            ureq::Error::Transport(t) => format!("muse-code key exchange failed: {t}"),
        })?;
    let payload: Value = response
        .into_json()
        .map_err(|e| format!("muse-code key exchange returned invalid JSON: {e}"))?;
    if payload.get("is_subs_active").and_then(Value::as_bool) == Some(false) {
        return Err("invalid_grant: Muse Code subscription is inactive".to_string());
    }
    let api_key = payload
        .get("api_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let api_key = match api_key {
        Some(key) => key,
        None => {
            let action_url = payload
                .get("action_url")
                .or_else(|| payload.get("require_payment_action_url"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if payload.get("require_payment").and_then(Value::as_bool) == Some(true)
                || action_url.is_some()
            {
                return Err(match action_url {
                    Some(url) => format!("Muse Code subscription is required: {url}"),
                    None => "Muse Code subscription is required".to_string(),
                });
            }
            return Err("muse-code key response is missing api_key".to_string());
        }
    };
    let email = payload
        .get("user_email")
        .and_then(Value::as_str)
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty());
    let account_id = payload
        .get("user_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| email.clone())
        .ok_or_else(|| "muse-code key response is missing a stable account identity".to_string())?;
    credentials.access = json!({
        "oauthAccessToken": credentials.access,
        "apiKey": api_key,
    })
    .to_string();
    credentials.account_id = Some(account_id);
    credentials.email = email.or(credentials.email.take());
    Ok(())
}

/// Bearer GET declared by `userinfo`; failures leave identity fields unset.
fn apply_userinfo(rule: Option<&UserinfoRule>, credentials: &mut OmpCredential) {
    let Some(rule) = rule else { return };
    let Ok(body) = oauth::json_response(
        ureq::get(&rule.url)
            .set("Authorization", &format!("Bearer {}", credentials.access))
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .call(),
    ) else {
        return;
    };
    if credentials.email.is_none() {
        credentials.email = rule
            .email
            .as_deref()
            .and_then(|path| json_path_str(&body, Some(path)));
    }
    if credentials.account_id.is_none() {
        credentials.account_id = rule
            .account_id
            .as_deref()
            .and_then(|path| json_path_str(&body, Some(path)));
    }
}

struct TokenResponse {
    status: u16,
    body: Value,
}

/// POST a declared token request; non-2xx surfaces as `Err` (use the `_raw`
/// variant for device polling, where errors are protocol values).
fn post_token_request(
    provider: &str,
    request: &TokenRequest,
    standard: &[(&str, Option<String>)],
    vars: &HashMap<&str, String>,
    extra_headers: &[(String, String)],
) -> Result<Value, String> {
    let response = post_token_request_raw(provider, request, standard, vars, extra_headers)?;
    if response.status >= 400 {
        return Err(format!(
            "{provider} token request failed: HTTP {} {}",
            response.status,
            truncate(&response.body.to_string(), 500)
        ));
    }
    Ok(response.body)
}

fn post_token_request_raw(
    provider: &str,
    request: &TokenRequest,
    standard: &[(&str, Option<String>)],
    vars: &HashMap<&str, String>,
    extra_headers: &[(String, String)],
) -> Result<TokenResponse, String> {
    let url = template(&resolve_value(&request.url)?, vars);
    let mut params: Vec<(String, String)> = Vec::new();
    if request.standard {
        for (key, value) in standard {
            if let Some(value) = value {
                params.push((key.to_string(), value.clone()));
            }
        }
    }
    for (key, value) in &request.params {
        params.push((key.clone(), template(value, vars)));
    }
    let timeout = Duration::from_millis(request.timeout_ms.unwrap_or(30_000));
    let mut call = ureq::post(&url).timeout(timeout);
    for (key, value) in extra_headers {
        call = call.set(key, value);
    }
    for (key, value) in &request.headers {
        call = call.set(key, &template(value, vars));
    }
    let response = if request.body == "json" {
        let object: serde_json::Map<String, Value> = params
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        call.set("Content-Type", "application/json")
            .send_json(Value::Object(object))
    } else {
        let encoded = params
            .iter()
            .map(|(k, v)| format!("{}={}", oauth::url_encode(k), oauth::url_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        call.set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&encoded)
    };
    match response {
        Ok(response) => {
            let status = response.status();
            let text = response.into_string().map_err(|e| e.to_string())?;
            Ok(TokenResponse {
                status,
                body: parse_body(&text),
            })
        }
        Err(ureq::Error::Status(status, response)) => {
            let text = response.into_string().unwrap_or_default();
            Ok(TokenResponse {
                status,
                body: parse_body(&text),
            })
        }
        Err(ureq::Error::Transport(t)) => Err(format!("{provider} request to {url} failed: {t}")),
    }
}

fn parse_body(text: &str) -> Value {
    if text.is_empty() {
        return Value::Null;
    }
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

fn uuid() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        return oauth::random_token(16).unwrap_or_default();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn template_substitutes_known_and_blank_unknowns() {
        let vars = HashMap::from([("user_code", "ABCD".to_string())]);
        assert_eq!(
            template("Enter code: {user_code}", &vars),
            "Enter code: ABCD"
        );
        assert_eq!(template("{missing}!", &vars), "!");
        assert_eq!(template("{NotAVar}", &vars), "{NotAVar}");
    }

    #[test]
    fn json_path_walks_dotted_segments() {
        let body = json!({"a": {"b": {"c": "x"}}, "n": 3});
        assert_eq!(json_path(&body, "a.b.c"), Some(&json!("x")));
        assert_eq!(json_path(&body, "n"), Some(&json!(3)));
        assert_eq!(json_path(&body, "a.b.missing"), None);
    }

    #[test]
    fn map_credentials_seconds_expiry_keeps_previous_refresh() {
        let map = CredentialMap {
            access: CredentialField {
                path: Some("access_token".into()),
                ..Default::default()
            },
            refresh: Some(CredentialField {
                path: Some("refresh_token".into()),
                ..Default::default()
            }),
            expires: ExpiresRule::Seconds {
                path: "expires_in".into(),
                from_path: None,
                skew_ms: 0,
                fallback_ms: None,
            },
            ..Default::default()
        };
        let body = json!({"access_token": "new", "expires_in": 60});
        let previous = OmpCredential {
            access: "old".into(),
            refresh: "keep-me".into(),
            expires_at_ms: 1,
            email: None,
            account_id: None,
            org_id: None,
            org_name: None,
            project_id: None,
            api_endpoint: None,
            enterprise_url: None,
        };
        let cred = map_credentials("p", &map, &body, Some(&previous)).unwrap();
        assert_eq!(cred.access, "new");
        assert_eq!(cred.refresh, "keep-me");
        assert!(cred.expires_at_ms > now_ms());
    }

    #[test]
    fn map_credentials_jwt_expiry_reads_exp_claim() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(json!({"exp": 4_000_000_000_u64}).to_string());
        let token = format!("header.{payload}.sig");
        let map = CredentialMap {
            access: CredentialField {
                path: Some("access_token".into()),
                ..Default::default()
            },
            expires: ExpiresRule::Jwt {
                skew_ms: 0,
                fallback_ms: None,
            },
            ..Default::default()
        };
        let body = json!({"access_token": token});
        let cred = map_credentials("p", &map, &body, None).unwrap();
        assert_eq!(cred.expires_at_ms, 4_000_000_000_000);
    }

    #[test]
    fn wait_for_callback_accepts_on_any_bound_listener() {
        // Dual-stack callback: browsers may hit either 127.0.0.1 or ::1 for
        // `localhost`, so every bound listener must be polled.
        let l4 = TcpListener::bind("127.0.0.1:0").unwrap();
        l4.set_nonblocking(true).unwrap();
        let mut listeners = vec![l4];
        if let Ok(l6) = TcpListener::bind("[::1]:0") {
            l6.set_nonblocking(true).unwrap();
            listeners.push(l6);
        }
        let target = listeners.last().unwrap().local_addr().unwrap();
        let handle = thread::spawn(move || wait_for_callback(&listeners, "/cb", "s1"));
        let mut stream = std::net::TcpStream::connect(target).unwrap();
        stream
            .write_all(b"GET /cb?code=abc&state=s1 HTTP/1.0\r\n\r\n")
            .unwrap();
        assert_eq!(handle.join().unwrap().unwrap(), "abc");
    }

    #[test]
    fn parse_callback_input_accepts_url_query_and_bare_code() {
        assert_eq!(
            parse_callback_input("zcode://zai-auth/callback?code=abc&state=xyz"),
            (Some("abc".to_string()), Some("xyz".to_string()))
        );
        assert_eq!(
            parse_callback_input("code=abc%20d&state=xyz"),
            (Some("abc d".to_string()), Some("xyz".to_string()))
        );
        assert_eq!(
            parse_callback_input("  bare-code  "),
            (Some("bare-code".to_string()), None)
        );
        assert_eq!(
            parse_callback_input("https://x/cb?state=s1#code=frag"),
            (None, Some("s1".to_string()))
        );
        assert_eq!(parse_callback_input(""), (None, None));
    }

    #[test]
    fn pasted_code_state_mismatch_keeps_waiting() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send("https://x/cb?code=bad&state=wrong".to_string())
            .unwrap();
        tx.send("https://x/cb?code=good&state=expected".to_string())
            .unwrap();
        let notices = std::cell::RefCell::new(Vec::new());
        let notice = |msg: &str| notices.borrow_mut().push(msg.to_string());
        assert_eq!(
            wait_for_pasted_code(&rx, "expected", &notice).unwrap(),
            "good"
        );
        assert_eq!(notices.borrow().len(), 1);
        assert!(notices.borrow()[0].contains("state mismatch"));
    }

    #[test]
    fn credential_field_falls_back_to_claims_then_literal() {
        let field = CredentialField {
            claims: vec!["sub".into()],
            literal: Some("lit".into()),
            ..Default::default()
        };
        let body = json!({});
        let claims = json!({"sub": "from-claim"});
        assert_eq!(
            read_credential_field(Some(&field), &body, Some(&claims)),
            Some("from-claim".to_string())
        );
        assert_eq!(
            read_credential_field(Some(&field), &body, None),
            Some("lit".to_string())
        );
    }

    #[test]
    fn bearer_token_unwraps_muse_envelope() {
        let mut credential = OmpCredential {
            access: r#"{"oauthAccessToken":"oauth-tok","apiKey":"muse-key"}"#.to_string(),
            ..OmpCredential::default()
        };
        assert_eq!(bearer_token(&credential), "muse-key");
        credential.access = "plain-token".to_string();
        assert_eq!(bearer_token(&credential), "plain-token");
        credential.access = r#"{"oauthAccessToken":"oauth-tok"}"#.to_string();
        assert_eq!(bearer_token(&credential), credential.access);
    }

    #[test]
    fn zai_unwrap_accepts_zero_and_200_codes() {
        let ok_zero = zai_unwrap(json!({"code": 0, "data": {"a": 1}}), "op").unwrap();
        assert_eq!(ok_zero, json!({"a": 1}));
        let ok_200 = zai_unwrap(json!({"code": "200", "success": true, "data": 2}), "op").unwrap();
        assert_eq!(ok_200, json!(2));
        assert!(zai_unwrap(json!({"code": 401, "msg": "denied"}), "op").is_err());
        assert!(zai_unwrap(json!({"success": false, "msg": "nope"}), "op").is_err());
        let bare = zai_unwrap(json!({"no": "envelope"}), "op").unwrap();
        assert_eq!(bare, json!({"no": "envelope"}));
    }
}
