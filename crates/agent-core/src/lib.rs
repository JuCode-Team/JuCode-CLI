pub mod actions;
pub mod chat;
mod commands;
mod config;
mod core;
pub mod custom_commands;
pub mod event;
mod hooks;
pub mod host;
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
mod secrets;
mod session;
pub mod skills;
mod subagents;
mod tokens;
mod tools;
mod trust;
pub mod update;
mod web;
mod web_fetch;

pub use config::{
    builtin_providers, jucode_visible_models, models_for_provider, ApprovalMode, ModelConfig,
    SubagentModel,
};
pub use core::{title_completion, AgentCore};
pub use event::{
    AgentEvent, CommandView, GoalView, LoginProviderView, McpServerView, McpToolView,
    ModelOptionView, PlanItem, SessionListItemView, TranscriptItem, TreeNodeView,
};
pub use hunks::HunkView;
pub use session::SessionSummary;
pub use tools::{git_diff, terminate_tool_processes};

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
    gateway_credentials(config)
}

/// The same for each request of the daemon's local gateway: config.json is
/// read, never rewritten (concurrent requests would race on it).
pub fn jucode_gateway_token() -> Result<(String, String), String> {
    let config = config::Config::load_existing().map_err(|error| error.to_string())?;
    let (api, token, _) = gateway_credentials(config)?;
    Ok((api, token))
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

fn gateway_credentials(config: config::Config) -> Result<(String, String, u64), String> {
    let auth = oauth::ensure_session(&config.jucode_api_url, config.encrypt_secrets)?;
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
