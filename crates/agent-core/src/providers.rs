//! JuCode's own provider template, layered over the shared kit's built-ins.
//!
//! The gateway endpoint is JuCode's, so it lives here rather than in
//! `llm-provider-kit`; everything else comes from the kit's template list.

use llm_provider_kit::providers::GPT_MODELS;
use llm_provider_kit::{Protocol, ProviderTemplate};

/// Name this client presents where a provider expects one: the Codex
/// `originator` header and the Z.ai API-key label minted at login.
pub const CLIENT_NAME: &str = "jucode";

/// The JuCode gateway: the OpenAI Responses API serving the gpt-5 family (plus
/// claude-* models, which users configure by name).
pub const JUCODE_TEMPLATE: ProviderTemplate = ProviderTemplate {
    id: "jucode",
    base_url: "https://api.jucode.cn/v1",
    protocol: Protocol::OpenAiResponses,
    models: GPT_MODELS,
};

/// Template lookup: the JuCode gateway first, then the kit's built-ins.
pub fn template(id: &str) -> Option<&'static ProviderTemplate> {
    if id == JUCODE_TEMPLATE.id {
        return Some(&JUCODE_TEMPLATE);
    }
    llm_provider_kit::providers::template(id)
}

/// Every template in picker order — JuCode's gateway, then the kit's.
pub fn templates() -> impl Iterator<Item = &'static ProviderTemplate> {
    std::iter::once(&JUCODE_TEMPLATE).chain(llm_provider_kit::templates())
}
