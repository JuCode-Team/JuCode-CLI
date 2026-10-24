pub mod actions;
pub mod chat;
mod commands;
mod config;
mod core;
pub mod custom_commands;
pub mod event;
mod hooks;
mod hunks;
mod images;
mod llm;
pub mod logging;
pub mod machine;
mod mcp;
mod oauth;
mod plan_mode;
mod prompt;
pub mod protocol;
mod providers;
pub mod sandbox;
mod search;
mod secrets;
mod session;
pub mod skills;
mod subagent_trace;
mod subagents;
mod tokens;
mod tools;
mod trust;
pub mod update;
pub mod user_tools;
mod web;
mod web_fetch;

pub use config::{
    builtin_providers, jucode_visible_models, models_for_provider, ApprovalMode, ModelConfig,
    SubagentModel,
};
pub use core::{queue_index, queue_move_target, title_completion, AgentCore};
pub use event::{
    AgentEvent, CommandView, GoalView, LoginProviderView, McpServerView, McpToolView,
    ModelOptionView, PlanItem, SessionListItemView, TranscriptItem, TreeNodeView,
};
pub use hunks::HunkView;
pub use session::SessionSummary;
pub use tools::{
    git_diff, signal_process_group, terminate_tool_processes, track_process_group, HostTools,
};

/// The JuCode gateway URL and an access token good for at least two more
/// minutes (refreshed first when needed), for tools spawned to call the
/// gateway on the user's JuCode login.
pub fn jucode_gateway_credentials() -> Result<(String, String), String> {
    let (api, token, _) = jucode_session()?;
    Ok((api, token))
}

/// `jucode_gateway_credentials` plus when the token expires (unix seconds).
pub fn jucode_session() -> Result<(String, String, u64), String> {
    let config = config::Config::load_or_create().map_err(|error| error.to_string())?;
    gateway_credentials(config, None)
}

/// The same for each request of the daemon's local gateway: config.json is
/// read, never rewritten (concurrent requests would race on it).
pub fn jucode_gateway_token() -> Result<(String, String), String> {
    let config = config::Config::load_existing().map_err(|error| error.to_string())?;
    let (api, token, _) = gateway_credentials(config, None)?;
    Ok((api, token))
}

/// `jucode_gateway_token` after a server refused `refused`: a refreshed token
/// (or the one another process saved meanwhile).
pub fn jucode_gateway_token_refreshed(refused: &str) -> Result<(String, String), String> {
    let config = config::Config::load_existing().map_err(|error| error.to_string())?;
    let (api, token, _) = gateway_credentials(config, Some(refused))?;
    Ok((api, token))
}

/// The JuCode access token saved on this computer, None when signed out (no
/// network, no refresh): tells whether auth.json changed since a token was
/// refused.
pub fn jucode_saved_access_token() -> Option<String> {
    let config = config::Config::load_existing().ok()?;
    let auth = config::AuthStore::load_or_create(config.encrypt_secrets).ok()?;
    auth.jucode_access_token().map(str::to_string)
}

/// This computer's host name, None when it cannot be read.
pub fn hostname() -> Option<String> {
    oauth::hostname()
}

/// Which kind of channel a provider id is, for usage records: `jucode`
/// (the JuCode gateway), `third_party` (a provider from the built-in
/// catalog, on the user's key or plan) or `local` (anything the user set up
/// themselves: their own Claude / ChatGPT login, a custom provider, a local
/// model server).
pub fn provider_channel_kind(provider: &str) -> &'static str {
    match provider {
        "jucode" | "jucode_gateway" => "jucode",
        // The same subscriptions Claude Code and Codex sign in to.
        "anthropic" | "openai-codex" | "openai-codex-device" => "local",
        _ if llm_provider_kit::omp::catalog()
            .auth_provider(provider)
            .is_some() =>
        {
            "third_party"
        }
        _ => "local",
    }
}

/// Whether this computer is signed in to JuCode (no network, no refresh).
pub fn jucode_signed_in() -> bool {
    config::Config::load_existing()
        .and_then(|config| config::AuthStore::load_or_create(config.encrypt_secrets))
        .is_ok_and(|auth| auth.jucode_tokens().is_some())
}

/// Signs this computer out of JuCode and revokes its device login.
pub fn jucode_logout() -> Result<(), String> {
    let config = config::Config::load_existing().map_err(|error| error.to_string())?;
    oauth::logout(&config.jucode_api_url, config.encrypt_secrets)
}

/// Names the app in the device label of the next JuCode login.
pub fn set_login_client_label(label: &'static str) {
    oauth::set_client_label(label);
}

fn gateway_credentials(
    config: config::Config,
    refused: Option<&str>,
) -> Result<(String, String, u64), String> {
    let auth = match refused {
        Some(refused) => {
            oauth::refresh_refused(&config.jucode_api_url, config.encrypt_secrets, refused)?
        }
        None => oauth::ensure_session(&config.jucode_api_url, config.encrypt_secrets)?,
    };
    let tokens = auth
        .jucode_tokens()
        .filter(|tokens| !tokens.access_token.is_empty())
        .ok_or("not logged in to JuCode. Run /login.")?;
    Ok((
        config.jucode_api_url,
        tokens.access_token.clone(),
        tokens.access_expires_at,
    ))
}

/// The user's enabled MCP servers as an ACP agent takes them in
/// `session/new` (`McpServer`): stdio ones by command, http ones by URL
/// (`"type": "http"`, for agents that take those). Those signing in by OAuth
/// are left out: their tokens are JuCode's to keep.
pub fn acp_mcp_servers() -> Vec<serde_json::Value> {
    let Ok(config) = config::Config::load_or_create() else {
        return Vec::new();
    };
    let pairs = |map: &std::collections::BTreeMap<String, String>| {
        map.iter()
            .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
            .collect::<Vec<_>>()
    };
    config
        .mcp_servers
        .iter()
        .filter(|server| server.enabled && server.oauth.is_none())
        .map(|server| match server.transport {
            config::McpTransportKind::Stdio => serde_json::json!({
                "name": server.name, "command": server.command, "args": server.args, "env": pairs(&server.env),
            }),
            config::McpTransportKind::Http => serde_json::json!({
                "type": "http", "name": server.name, "url": server.url, "headers": pairs(&server.headers),
            }),
        })
        .collect()
}

/// Saves an MCP server change to config.json: `mcp_set` (`server`, a config
/// entry), `mcp_remove` (`name`) or `mcp_toggle` (`name`, `enabled`), the
/// session ops, for a client with no session to send it to. Running sessions
/// apply it from the same op.
pub fn change_mcp_config(op: &serde_json::Value) -> Result<(), String> {
    let mut config = config::Config::load_or_create().map_err(|error| error.to_string())?;
    let name = op["name"].as_str().unwrap_or_default();
    let unknown = || format!("unknown MCP server: {name}");
    match op["op"].as_str().unwrap_or_default() {
        "mcp_set" => {
            let server = config::parse_mcp_server_value(&op["server"])?;
            match config
                .mcp_servers
                .iter_mut()
                .find(|s| s.name == server.name)
            {
                Some(existing) => *existing = server,
                None => config.mcp_servers.push(server),
            }
        }
        "mcp_remove" => {
            if !config.mcp_servers.iter().any(|s| s.name == name) {
                return Err(unknown());
            }
            config.mcp_servers.retain(|s| s.name != name);
        }
        "mcp_toggle" => {
            let enabled = op["enabled"]
                .as_bool()
                .ok_or("mcp_toggle requires enabled")?;
            config
                .mcp_servers
                .iter_mut()
                .find(|s| s.name == name)
                .ok_or_else(unknown)?
                .enabled = enabled;
        }
        other => return Err(format!("not an MCP change: {other}")),
    }
    config
        .save()
        .map_err(|error| format!("failed to save config: {error}"))
}

/// Sessions saved for `cwd`, most recently updated first (`updated_at` in
/// seconds).
pub fn saved_sessions(cwd: &std::path::Path) -> std::io::Result<Vec<SessionSummary>> {
    session::SessionStore::list_for_cwd(&config::profile_dir()?, cwd)
}

/// Forks session `from` of `from_cwd` into a new session saved for `cwd`
/// (another directory when the fork works in its own worktree) that holds
/// its first `keep` user turns (all when None); the new session's id.
pub fn fork_session(
    from_cwd: &std::path::Path,
    cwd: &std::path::Path,
    from: &str,
    keep: Option<usize>,
) -> std::io::Result<String> {
    let profile = config::profile_dir()?;
    let mut fork = session::SessionStore::load_for_cwd(&profile, from_cwd, from)?.forked(keep);
    fork.save_for_cwd(&profile, cwd)?;
    Ok(fork.session_id().to_string())
}
