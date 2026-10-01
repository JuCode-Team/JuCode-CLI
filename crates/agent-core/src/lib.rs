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

/// Sessions saved for `cwd`, most recently updated first (`updated_at` in
/// seconds).
pub fn saved_sessions(cwd: &std::path::Path) -> std::io::Result<Vec<SessionSummary>> {
    session::SessionStore::list_for_cwd(&config::profile_dir()?, cwd)
}
