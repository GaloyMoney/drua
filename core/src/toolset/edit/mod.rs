//! Provider-aware schema translation for the in-sandbox file editor.
//!
//! drua's executor (the sandbox-tool-server's `/execute` endpoint) speaks the
//! Anthropic `text_editor_*` wire format; the canonical [`EditOp`] is its
//! one-to-one Rust mirror. Every provider adapter translates a model's
//! preferred schema into one or more [`EditOp`]s on the way *in*, and back
//! into the provider's expected tool-result envelope on the way *out*.
//!
//! - [`AnthropicAdapter`]   — identity (server tool, schema-less)
//! - [`OpenAIAdapter`]      — V4A `apply_patch` envelope + `read_file`
//! - [`GeminiAdapter`]      — `replace` / `write_file` / `read_file`
//! - [`FallbackAdapter`]    — single `edit_file` mega-tool
//!
//! [`adapter_for`] picks the right adapter from a model identifier
//! (e.g. `"claude-sonnet-4-20250514"`, `"gpt-5-codex"`, `"gemini-2.5-pro"`).

pub mod adapter;
mod adapters;
pub mod canonical;

pub use adapter::{ProviderEditAdapter, ToolDeclaration};
pub use adapters::{
    AnthropicAdapter, AnthropicTextEditorVersion, FallbackAdapter, GeminiAdapter, OpenAIAdapter,
};
pub use canonical::{EditExecError, EditOp, EditOpResult, EditParseError};

pub(crate) use adapters::edit_input_schema;

/// Pick the right [`ProviderEditAdapter`] for a model identifier.
///
/// Uses prefix sniffing on the model name — drua does not currently maintain
/// a structured `ModelId` enum (see open question #4 in the spec). The match
/// order matters: more specific prefixes first, fallback last.
pub fn adapter_for(model_id: &str) -> Box<dyn ProviderEditAdapter> {
    let lower = model_id.to_ascii_lowercase();

    if lower.starts_with("claude") || lower.starts_with("anthropic/") {
        return Box::new(AnthropicAdapter::new(anthropic_version_for(&lower)));
    }
    if lower.starts_with("gpt-")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
        || lower.starts_with("codex")
        || lower.starts_with("openai/")
    {
        return Box::new(OpenAIAdapter::new());
    }
    if lower.starts_with("gemini") || lower.starts_with("google/") {
        return Box::new(GeminiAdapter::new());
    }
    Box::new(FallbackAdapter::new())
}

/// Map a Claude model identifier to the right `text_editor_*` revision.
/// Defaults to the latest (`20250728`) on unknown Claude variants.
fn anthropic_version_for(lower_model_id: &str) -> AnthropicTextEditorVersion {
    if lower_model_id.contains("claude-3-5") || lower_model_id.contains("claude-sonnet-3-5") {
        AnthropicTextEditorVersion::V20241022
    } else if lower_model_id.contains("claude-3-7") || lower_model_id.contains("claude-sonnet-3-7")
    {
        AnthropicTextEditorVersion::V20250124
    } else {
        AnthropicTextEditorVersion::V20250728
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_claude_to_anthropic() {
        let a = adapter_for("claude-sonnet-4-20250514");
        assert_eq!(a.provider_name(), "anthropic");
    }

    #[test]
    fn routes_openrouter_anthropic_alias() {
        let a = adapter_for("anthropic/claude-opus-4");
        assert_eq!(a.provider_name(), "anthropic");
    }

    #[test]
    fn routes_gpt_to_openai() {
        assert_eq!(adapter_for("gpt-5-codex").provider_name(), "openai");
        assert_eq!(adapter_for("gpt-4.1").provider_name(), "openai");
        assert_eq!(adapter_for("o3-mini").provider_name(), "openai");
        assert_eq!(adapter_for("codex-mini").provider_name(), "openai");
    }

    #[test]
    fn routes_gemini_to_gemini() {
        assert_eq!(adapter_for("gemini-2.5-pro").provider_name(), "gemini");
        assert_eq!(
            adapter_for("google/gemini-2.5-flash").provider_name(),
            "gemini"
        );
    }

    #[test]
    fn unknown_falls_back() {
        assert_eq!(adapter_for("llama-3.3-70b").provider_name(), "fallback");
        assert_eq!(adapter_for("deepseek-v3").provider_name(), "fallback");
        assert_eq!(adapter_for("grok-2").provider_name(), "fallback");
    }

    #[test]
    fn anthropic_version_matrix() {
        match adapter_for("claude-3-5-sonnet-20241022")
            .declare_tools()
            .into_iter()
            .next()
            .unwrap()
            .provider_type
            .as_deref()
        {
            Some("text_editor_20241022") => {}
            other => panic!("expected text_editor_20241022, got {other:?}"),
        }
        match adapter_for("claude-3-7-sonnet-latest")
            .declare_tools()
            .into_iter()
            .next()
            .unwrap()
            .provider_type
            .as_deref()
        {
            Some("text_editor_20250124") => {}
            other => panic!("expected text_editor_20250124, got {other:?}"),
        }
        match adapter_for("claude-sonnet-4-20250514")
            .declare_tools()
            .into_iter()
            .next()
            .unwrap()
            .provider_type
            .as_deref()
        {
            Some("text_editor_20250728") => {}
            other => panic!("expected text_editor_20250728, got {other:?}"),
        }
    }
}
