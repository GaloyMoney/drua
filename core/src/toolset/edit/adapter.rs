//! [`ProviderEditAdapter`] — translates between a provider's preferred
//! file-edit tool schema and the canonical [`EditOp`](super::canonical::EditOp).
//!
//! Each adapter owns three things:
//! 1. [`declare_tools`](ProviderEditAdapter::declare_tools) — the tool
//!    declarations the model expects (`apply_patch` for OpenAI, three tools for
//!    Gemini, the schema-less server tool for Anthropic, etc.);
//! 2. [`parse_call`](ProviderEditAdapter::parse_call) — turn a model-emitted
//!    tool-call payload into one or more [`EditOp`]s;
//! 3. [`format_result`](ProviderEditAdapter::format_result) — serialise the
//!    canonical [`EditOpResult`] / [`EditExecError`] into the wire shape the
//!    provider expects on the next turn.

use serde::{Deserialize, Serialize};

use super::canonical::{EditExecError, EditOp, EditOpResult, EditParseError};

/// One advertised tool. Several adapters declare > 1 (e.g. Gemini: `replace`,
/// `write_file`, `read_file`; OpenAI: `apply_patch` + `read_file`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input. Empty `{}` for schema-less server
    /// tools (Anthropic's `text_editor_*`).
    pub input_schema: serde_json::Value,
    /// Set for Anthropic's schema-less server tool. When `Some`, the LLM
    /// caller should emit a `{"type": <provider_type>, "name": <name>}`
    /// envelope instead of declaring `input_schema` over the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
}

pub trait ProviderEditAdapter: Send + Sync {
    /// Stable name for tracing / debugging.
    fn provider_name(&self) -> &'static str;

    /// Tools to advertise to the model. Multiple entries are valid — Gemini's
    /// adapter exposes three; OpenAI's exposes two; Anthropic's exposes one.
    fn declare_tools(&self) -> Vec<ToolDeclaration>;

    /// Translate a provider-emitted tool-call into 1+ canonical [`EditOp`]s.
    /// `tool_name` lets adapters with multiple tools (Gemini, OpenAI) dispatch.
    /// V4A `apply_patch` returns a `Vec` because one patch can touch many files.
    fn parse_call(
        &self,
        tool_name: &str,
        raw_input: &serde_json::Value,
    ) -> Result<Vec<EditOp>, EditParseError>;

    /// Serialise an executed batch's outcome back into the provider's
    /// expected tool-result envelope. `tool_use_id` is the model's id for
    /// the call (echoed back per the Anthropic / OpenAI conventions).
    fn format_result(
        &self,
        tool_use_id: &str,
        results: &[EditOpResult],
        errors: &[EditExecError],
    ) -> serde_json::Value;
}
