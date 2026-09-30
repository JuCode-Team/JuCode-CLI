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
pub use core::AgentCore;
pub use event::{
    AgentEvent, CommandView, GoalView, LoginProviderView, McpServerView, McpToolView,
    ModelOptionView, PlanItem, SessionListItemView, TranscriptItem, TreeNodeView,
};
pub use hunks::HunkView;
pub use session::SessionSummary;
pub use tools::{git_diff, terminate_tool_processes};

/// Sessions saved for `cwd`, most recently updated first (`updated_at` in
/// seconds).
pub fn saved_sessions(cwd: &std::path::Path) -> std::io::Result<Vec<SessionSummary>> {
    session::SessionStore::list_for_cwd(&config::profile_dir()?, cwd)
}
