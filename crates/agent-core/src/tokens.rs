use serde_json::Value;
use tiktoken_rs::bpe_for_model;

#[derive(Debug, Clone)]
pub(crate) struct TokenCount {
    pub tokens: usize,
    pub tokenizer: String,
}

pub(crate) fn count_text(model: &str, text: &str) -> TokenCount {
    let requested = tokenizer_model(model);
    let (bpe, tokenizer) = bpe_for_model(requested).map_or_else(
        |_| {
            // gpt-5/o-series models use o200k_base. If a future deployment suffix is
            // unknown to tiktoken-rs, falling back to gpt-5 keeps counting tokenizer
            // based instead of reverting to character estimates.
            (
                bpe_for_model("gpt-5").expect("gpt-5 tokenizer must be available"),
                "gpt-5",
            )
        },
        |bpe| (bpe, requested),
    );
    TokenCount {
        tokens: bpe.count_with_special_tokens(text),
        tokenizer: tokenizer.to_string(),
    }
}

pub(crate) fn count_value(model: &str, value: &Value) -> TokenCount {
    count_text(model, &value.to_string())
}

pub(crate) fn count_values<'a>(model: &str, values: impl Iterator<Item = &'a Value>) -> TokenCount {
    let mut tokens = 0usize;
    let mut tokenizer = tokenizer_model(model).to_string();
    for value in values {
        let count = count_value(model, value);
        tokenizer = count.tokenizer;
        tokens += count.tokens;
    }
    TokenCount { tokens, tokenizer }
}

fn tokenizer_model(model: &str) -> &str {
    if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.starts_with("codex")
    {
        model
    } else {
        // Anthropic and custom models do not expose a local tokenizer here.
        // Count with o200k_base and show the tokenizer name explicitly in
        // statistics so callers do not confuse it with provider-reported usage.
        "gpt-5"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_text_with_model_tokenizer() {
        let count = count_text("gpt-5", "hello world");

        assert!(count.tokens > 0);
        assert_eq!(count.tokenizer, "gpt-5");
    }

    #[test]
    fn counts_values_without_character_estimates() {
        let value =
            json!({ "role": "user", "content": [{ "type": "input_text", "text": "hello" }] });
        let count = count_value("gpt-5", &value);

        assert!(count.tokens > 0);
    }

    #[test]
    fn reports_fallback_tokenizer_for_custom_models() {
        let count = count_text("claude-sonnet", "hello");

        assert!(count.tokens > 0);
        assert_eq!(count.tokenizer, "gpt-5");
    }
}
