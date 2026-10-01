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
mod llm;
pub mod logging;
mod mcp;
mod oauth;
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
