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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

impl From<llm::response::Usage> for AssistantResponseMetadata {
    fn from(usage: llm::response::Usage) -> Self {
        Self {
            api: String::new(),
            model: String::new(),
            usage: Usage {
                input: usage.input_tokens as u64,
                output: usage.output_tokens as u64,
                cache_read: 0,
                cache_write: 0,
                total_tokens: (usage.input_tokens + usage.output_tokens) as u64,
            },
            cost: Cost::default(),
        }
    }
}
