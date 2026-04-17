use std::borrow::Cow;
use std::collections::HashMap;

use serde::Serialize;

use super::messages::{Message, ThinkingLevel};

// ============================================================================
// Context
// ============================================================================

#[derive(Debug, Clone)]
pub struct Context<'a> {
    /// Provider-specific system prompt content.
    ///
    /// Uses [`Cow`] to borrow from `AgentConfig.system_prompt` on every turn without
    /// cloning.  Providers that need an owned `String` can call `.into_owned()`.
    pub system_prompt: Option<Cow<'a, str>>,
    /// Conversation history (user/assistant/tool results).
    pub messages: Cow<'a, [Message]>,
    /// Tool definitions available to the model for this request.
    pub tools: Cow<'a, [ToolDef]>,
}

impl Default for Context<'_> {
    fn default() -> Self {
        Self {
            system_prompt: None,
            messages: Cow::Owned(Vec::new()),
            tools: Cow::Owned(Vec::new()),
        }
    }
}

impl Context<'_> {
    /// Create a `Context` with fully-owned data (no borrowing).
    ///
    /// Convenient for tests and one-off callers that already have owned vectors.
    pub fn owned(
        system_prompt: Option<String>,
        messages: Vec<Message>,
        tools: Vec<ToolDef>,
    ) -> Context<'static> {
        Context {
            system_prompt: system_prompt.map(Cow::Owned),
            messages: Cow::Owned(messages),
            tools: Cow::Owned(tools),
        }
    }
}

// ============================================================================
// Tool Definition
// ============================================================================

/// A tool definition exposed to the model.
///
/// Providers translate this struct into the backend's tool/schema representation (typically JSON
/// Schema) so the model can emit tool calls that the host executes locally.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

// ============================================================================
// Stream Options
// ============================================================================

/// Options that control streaming completion behavior.
///
/// Most options are passed through to the provider request (temperature, max tokens, headers).
/// Some fields are provider-specific conveniences (e.g. `session_id` for logging/correlation).
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub api_key: Option<String>,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    pub headers: HashMap<String, String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// Cache retention policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheRetention {
    #[default]
    None,
    /// Provider-managed short-lived caching (provider-specific semantics).
    Short,
    /// Provider-managed long-lived caching (e.g. ~1 hour TTL on Anthropic).
    Long,
}

/// Custom thinking token budgets per level.
#[derive(Debug, Clone, Serialize)]
pub struct ThinkingBudgets {
    pub minimal: u32,
    pub low: u32,
    pub medium: u32,
    pub high: u32,
    pub xhigh: u32,
}

impl Default for ThinkingBudgets {
    fn default() -> Self {
        Self {
            minimal: 1024,
            low: 2048,
            medium: 8192,
            high: 16384,
            xhigh: 32768,
        }
    }
}
