//! Provider-agnostic model identification and fallback chain types.

use serde::{Deserialize, Serialize};

/// Provider-agnostic reasoning level; each client maps it to its own wire
/// shape (OpenAI `reasoning_effort`, OpenRouter `reasoning.effort`,
/// Anthropic thinking `budget_tokens`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    #[default]
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

impl ReasoningEffort {
    pub fn is_low(effort: &Self) -> bool {
        *effort == Self::Low
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
}

impl ModelSpec {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            max_tokens: None,
            effort: None,
        }
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn with_effort(mut self, effort: ReasoningEffort) -> Self {
        self.effort = Some(effort);
        self
    }
}

impl<S: Into<String>> From<S> for ModelSpec {
    fn from(name: S) -> Self {
        Self::new(name)
    }
}

/// Always non-empty: `primary` is required.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelChain {
    pub primary: ModelSpec,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<ModelSpec>,
}

impl ModelChain {
    pub fn new(primary: impl Into<ModelSpec>) -> Self {
        Self {
            primary: primary.into(),
            fallbacks: Vec::new(),
        }
    }

    pub fn with_fallback(mut self, spec: impl Into<ModelSpec>) -> Self {
        self.fallbacks.push(spec.into());
        self
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModelSpec> {
        std::iter::once(&self.primary).chain(self.fallbacks.iter())
    }

    pub fn len(&self) -> usize {
        1 + self.fallbacks.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

impl From<ModelSpec> for ModelChain {
    fn from(primary: ModelSpec) -> Self {
        Self::new(primary)
    }
}

impl From<String> for ModelChain {
    fn from(name: String) -> Self {
        Self::new(ModelSpec::new(name))
    }
}

impl From<&str> for ModelChain {
    fn from(name: &str) -> Self {
        Self::new(ModelSpec::new(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iter_yields_primary_then_fallbacks() {
        let chain = ModelChain::new("a")
            .with_fallback("b")
            .with_fallback(ModelSpec::new("c").with_max_tokens(2048));

        let names: Vec<_> = chain.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(chain.len(), 3);
    }

    #[test]
    fn round_trips_via_serde() {
        let chain = ModelChain::new(ModelSpec::new("anthropic/sonnet").with_max_tokens(8192))
            .with_fallback("openai/gpt-4o");
        let json = serde_json::to_string(&chain).unwrap();
        let back: ModelChain = serde_json::from_str(&json).unwrap();
        assert_eq!(chain, back);
    }

    #[test]
    fn from_string_yields_length_one_chain() {
        let chain: ModelChain = "anthropic/haiku".into();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.primary.name, "anthropic/haiku");
        assert!(chain.fallbacks.is_empty());
    }
}
