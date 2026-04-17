use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSettings {
    pub model: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSimplificationSettings {
    pub simplify_after_idle_seconds: Option<u32>,
}
