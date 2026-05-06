use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantResponseMetadata {
    pub api: String,
    pub model: String,
    pub usage: Usage,
    pub cost: Cost,
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
    }
}
