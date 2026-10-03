use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantResponseMetadata {
    pub api: String,
    pub model: String,
    pub usage: Usage,
    pub cost: Cost,
    /// Upstream provider that actually served this turn (e.g. `"Anthropic"`,
    /// `"DeepInfra"` via OpenRouter). `None` when the client didn't report
    /// one (direct Anthropic/OpenAI, or a stored event from before this
    /// field existed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_provider: Option<String>,
    /// Raw wire finish reason (e.g. `"stop"`, `"error"`). `None` when the
    /// client didn't report one, or for a stored event from before this
    /// field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    /// Subset of `output` spent on reasoning (OpenAI o-series, OpenRouter
    /// `completion_tokens_details.reasoning_tokens`).
    #[serde(default)]
    pub reasoning: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
    /// OpenRouter BYOK only: the actual upstream provider cost (vs the
    /// gateway-charged credits in `total`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_inference_usd: Option<f64>,
}

impl From<llm::response::Usage> for AssistantResponseMetadata {
    fn from(usage: llm::response::Usage) -> Self {
        Self {
            api: String::new(),
            model: String::new(),
            usage: Usage {
                input: usage.input_tokens as u64,
                output: usage.output_tokens as u64,
                cache_read: usage.cache_read_input_tokens as u64,
                cache_write: usage.cache_creation_input_tokens as u64,
                total_tokens: (usage.input_tokens + usage.output_tokens) as u64,
                reasoning: usage.reasoning_output_tokens as u64,
            },
            cost: Cost {
                total: usage.cost_usd.unwrap_or(0.0),
                upstream_inference_usd: usage.upstream_inference_cost_usd,
                ..Cost::default()
            },
            // `Usage` (this impl's source) doesn't carry it — it lives on
            // `PromptResponse` itself. Callers with a full response set
            // `metadata.upstream_provider` after this conversion.
            upstream_provider: None,
            // Same reasoning as `upstream_provider`: `finish_reason` lives
            // on `PromptResponse`, not `Usage`.
            finish_reason: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_from_llm_usage_carries_cost_and_reasoning() {
        let llm_usage = llm::response::Usage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_input_tokens: 800,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 120,
            cost_usd: Some(0.0042),
            upstream_inference_cost_usd: Some(0.0038),
        };
        let metadata = AssistantResponseMetadata::from(llm_usage);
        assert_eq!(metadata.usage.input, 1000);
        assert_eq!(metadata.usage.cache_read, 800);
        assert_eq!(metadata.usage.reasoning, 120);
        assert_eq!(metadata.cost.total, 0.0042);
        assert_eq!(metadata.cost.upstream_inference_usd, Some(0.0038));
    }

    #[test]
    fn legacy_metadata_without_new_fields_round_trips() {
        // Persisted before this PR: no `reasoning` on usage, no
        // `upstream_inference_usd` on cost. Must hydrate with defaults.
        let json = r#"{
            "api": "anthropic",
            "model": "claude-sonnet-4",
            "usage": {
                "input": 100,
                "output": 50,
                "cache_read": 10,
                "cache_write": 5,
                "total_tokens": 150
            },
            "cost": {
                "input": 0.0,
                "output": 0.0,
                "cache_read": 0.0,
                "cache_write": 0.0,
                "total": 0.0
            }
        }"#;
        let m: AssistantResponseMetadata = serde_json::from_str(json).expect("legacy hydrates");
        assert_eq!(m.usage.reasoning, 0);
        assert!(m.cost.upstream_inference_usd.is_none());
        assert_eq!(m.upstream_provider, None);
        assert_eq!(m.finish_reason, None);
    }

    /// Handoff §3.4: mirrors what `Sessions::assistant_response_received`
    /// does — build metadata from `response.usage`, then set
    /// `upstream_provider` from the full response, since `Usage` alone
    /// doesn't carry it. A chunk that reported a provider must survive
    /// into the stored metadata.
    #[test]
    fn metadata_carries_upstream_provider_from_full_response() {
        let response = llm::PromptResponse {
            content: Vec::new(),
            usage: llm::response::Usage::default(),
            stop_reason: None,
            model_used: None,
            upstream_provider: Some("Anthropic".to_string()),
            finish_reason: None,
            upstream_error: None,
            malformed_tool_calls: Vec::new(),
        };
        let mut metadata = AssistantResponseMetadata::from(response.usage);
        metadata.upstream_provider = response.upstream_provider;
        assert_eq!(metadata.upstream_provider.as_deref(), Some("Anthropic"));
    }

    /// A response that never reported a provider (direct Anthropic/OpenAI)
    /// must not fabricate one.
    #[test]
    fn metadata_upstream_provider_is_none_when_response_did_not_report_one() {
        let response = llm::PromptResponse {
            content: Vec::new(),
            usage: llm::response::Usage::default(),
            stop_reason: None,
            model_used: None,
            upstream_provider: None,
            finish_reason: None,
            upstream_error: None,
            malformed_tool_calls: Vec::new(),
        };
        let mut metadata = AssistantResponseMetadata::from(response.usage);
        metadata.upstream_provider = response.upstream_provider;
        assert_eq!(metadata.upstream_provider, None);
    }

    /// The `upstream_provider` key must round-trip through JSON once set —
    /// this is what gets persisted on `AssistantResponseReceived.metadata`.
    #[test]
    fn upstream_provider_round_trips_through_json() {
        let mut metadata = AssistantResponseMetadata::from(llm::response::Usage::default());
        metadata.upstream_provider = Some("DeepInfra".to_string());
        let json = serde_json::to_string(&metadata).expect("serialize");
        let hydrated: AssistantResponseMetadata = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(hydrated.upstream_provider.as_deref(), Some("DeepInfra"));
    }

    /// Handoff §4.2 test 2 / D4: the `finish_reason` key must round-trip
    /// through JSON once set, the same way `upstream_provider` does — and a
    /// stored event predating this field (the `legacy_metadata_without_new_fields_round_trips`
    /// JSON above) hydrates it as `None`.
    #[test]
    fn finish_reason_round_trips_through_json() {
        let mut metadata = AssistantResponseMetadata::from(llm::response::Usage::default());
        metadata.finish_reason = Some("error".to_string());
        let json = serde_json::to_string(&metadata).expect("serialize");
        let hydrated: AssistantResponseMetadata = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(hydrated.finish_reason.as_deref(), Some("error"));
    }
}
