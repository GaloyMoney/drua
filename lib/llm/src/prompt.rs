use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<SystemBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User { content: Vec<UserBlock> },
    Assistant { content: Vec<AssistantBlock> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultBlock>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultBlock {
    Text { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

/// Anthropic prompt-caching marker. Placed on the last block you want
/// included in the cache; everything from the start of the prompt up
/// to that block becomes a cache breakpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheControl {
    Ephemeral {
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl: Option<CacheTtl>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

/// Recursively sort all object keys in a JSON value so that
/// semantically equal values produce identical serialized bytes.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize(v)))
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .collect();
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(canonicalize).collect())
        }
        other => other.clone(),
    }
}

impl Prompt {
    /// Returns a hex-encoded SHA-256 hash of the prompt's canonical JSON form.
    ///
    /// Deterministic across processes: two `Prompt` values that serialize to
    /// semantically equal JSON will always produce the same hash, regardless
    /// of object-key ordering in embedded `serde_json::Value` fields.
    ///
    /// Hex-encoded `String` is returned (rather than `[u8; 32]`) because the
    /// primary use-case is cache keys — strings are directly usable in file
    /// names, database columns, and log output without further conversion.
    pub fn hash(&self) -> String {
        let value = serde_json::to_value(self).expect("Prompt is always serializable");
        let canonical = canonicalize(&value);
        let bytes = serde_json::to_vec(&canonical).expect("canonical Value is always serializable");
        let digest = Sha256::digest(&bytes);
        digest.iter().fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write;
            write!(s, "{b:02x}").expect("writing to String never fails");
            s
        })
    }

    /// Estimates the token count of this prompt using the cl100k_base BPE encoding.
    ///
    /// This is an approximation — it does not replicate any specific provider's
    /// exact billing logic. It counts tokens across system blocks, all message
    /// content (text, tool-use inputs, tool-result text, thinking), and tool
    /// definitions (name + description + schema).
    pub fn estimate_tokens(&self) -> usize {
        let enc = tiktoken::get_encoding("cl100k_base").expect("cl100k_base must be available");
        let mut total = 0usize;

        // System blocks
        for block in &self.system {
            match block {
                SystemBlock::Text { text, .. } => total += enc.encode(text).len(),
            }
        }

        // Messages
        for msg in &self.messages {
            match msg {
                Message::User { content } => {
                    for block in content {
                        match block {
                            UserBlock::Text { text, .. } => {
                                total += enc.encode(text).len();
                            }
                            UserBlock::ToolResult { content, .. } => {
                                for tb in content {
                                    match tb {
                                        ToolResultBlock::Text { text } => {
                                            total += enc.encode(text).len();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Message::Assistant { content } => {
                    for block in content {
                        match block {
                            AssistantBlock::Text { text, .. } => {
                                total += enc.encode(text).len();
                            }
                            AssistantBlock::ToolUse { input, name, .. } => {
                                total += enc.encode(name).len();
                                let json = serde_json::to_string(input)
                                    .unwrap_or_default();
                                total += enc.encode(&json).len();
                            }
                            AssistantBlock::Thinking { text, .. } => {
                                total += enc.encode(text).len();
                            }
                        }
                    }
                }
            }
        }

        // Tool definitions
        for tool in &self.tools {
            total += enc.encode(&tool.name).len();
            if let Some(desc) = &tool.description {
                total += enc.encode(desc).len();
            }
            let schema = serde_json::to_string(&tool.input_schema).unwrap_or_default();
            total += enc.encode(&schema).len();
        }

        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_prompt(msg_text: &str) -> Prompt {
        Prompt {
            model: "claude-sonnet-4-20250514".to_string(),
            system: vec![SystemBlock::Text {
                text: "You are a helpful assistant.".to_string(),
                cache_control: None,
            }],
            messages: vec![Message::User {
                content: vec![UserBlock::Text {
                    text: msg_text.to_string(),
                    cache_control: None,
                }],
            }],
            tools: vec![Tool {
                name: "get_weather".to_string(),
                description: Some("Get the current weather for a location.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "location": { "type": "string" }
                    },
                    "required": ["location"]
                }),
                cache_control: None,
            }],
            tool_choice: None,
            max_tokens: Some(1024),
        }
    }

    #[test]
    fn hash_is_stable() {
        let a = sample_prompt("Hello, world!");
        let b = sample_prompt("Hello, world!");
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn hash_differs_on_content_change() {
        let a = sample_prompt("Hello, world!");
        let b = sample_prompt("Goodbye, world!");
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn hash_is_64_hex_chars() {
        let h = sample_prompt("test").hash();
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_stable_despite_json_key_order() {
        // Build two prompts with tool_use inputs whose keys are inserted
        // in different order — the canonical hash must still match.
        let input_a = serde_json::json!({"alpha": 1, "beta": 2});
        let input_b = serde_json::json!({"beta": 2, "alpha": 1});

        let make = |input: serde_json::Value| Prompt {
            model: "test".to_string(),
            system: vec![],
            messages: vec![Message::Assistant {
                content: vec![AssistantBlock::ToolUse {
                    id: "tu_1".to_string(),
                    name: "fn".to_string(),
                    input,
                    cache_control: None,
                }],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: None,
        };

        assert_eq!(make(input_a).hash(), make(input_b).hash());
    }

    #[test]
    fn estimate_tokens_nonzero_for_nonempty_prompt() {
        let p = sample_prompt("Hello, world!");
        assert!(p.estimate_tokens() > 0);
    }

    #[test]
    fn estimate_tokens_proportional_to_length() {
        let short = sample_prompt("Hi");
        let long = sample_prompt(&"word ".repeat(500));
        assert!(long.estimate_tokens() > short.estimate_tokens());
    }

    #[test]
    fn estimate_tokens_counts_tool_definitions() {
        let with_tools = sample_prompt("Hi");
        let mut without_tools = sample_prompt("Hi");
        without_tools.tools.clear();
        assert!(with_tools.estimate_tokens() > without_tools.estimate_tokens());
    }
}
