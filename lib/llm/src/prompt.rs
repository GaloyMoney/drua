use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::spec::ModelChain;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub chain: ModelChain,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<SystemBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            chain: ModelChain::new(""),
            messages: Vec::new(),
            system: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            cache_key: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemBlock {
    Text { text: String },
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
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultBlock>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
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
    /// Anthropic + OpenAI honour `strict: true`; providers that don't
    /// silently ignore it. Schema-violation defence is the runtime's
    /// post-hoc validation in `core::agent`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub strict: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

/// Recursively sort object keys so semantically equal values serialize identically.
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
    /// Content-only SHA-256: excludes `cache_key` and the fallback list
    /// (routing intent shouldn't bust the cache).
    pub fn hash(&self) -> String {
        #[derive(Serialize)]
        struct Hashable<'a> {
            primary_model: &'a str,
            primary_max_tokens: Option<u32>,
            messages: &'a [Message],
            system: &'a [SystemBlock],
            tools: &'a [Tool],
            tool_choice: Option<&'a ToolChoice>,
        }
        let hashable = Hashable {
            primary_model: &self.chain.primary.name,
            primary_max_tokens: self.chain.primary.max_tokens,
            messages: &self.messages,
            system: &self.system,
            tools: &self.tools,
            tool_choice: self.tool_choice.as_ref(),
        };
        let value = serde_json::to_value(&hashable).expect("hashable is always serializable");
        let canonical = canonicalize(&value);
        let bytes = serde_json::to_vec(&canonical).expect("canonical Value is always serializable");
        let digest = Sha256::digest(&bytes);
        digest.iter().fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write;
            write!(s, "{b:02x}").expect("writing to String never fails");
            s
        })
    }

    /// cl100k_base BPE estimate. Approximation — does not replicate any
    /// specific provider's billing.
    pub fn estimate_tokens(&self) -> usize {
        let enc = tiktoken::get_encoding("cl100k_base").expect("cl100k_base must be available");
        let mut total = 0usize;

        for block in &self.system {
            match block {
                SystemBlock::Text { text } => total += enc.encode(text).len(),
            }
        }

        for msg in &self.messages {
            match msg {
                Message::User { content } => {
                    for block in content {
                        match block {
                            UserBlock::Text { text } => {
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
                            AssistantBlock::Text { text } => {
                                total += enc.encode(text).len();
                            }
                            AssistantBlock::ToolUse { input, name, .. } => {
                                total += enc.encode(name).len();
                                let json = serde_json::to_string(input).unwrap_or_default();
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
    use crate::spec::ModelSpec;

    fn sample_prompt(msg_text: &str) -> Prompt {
        Prompt {
            chain: ModelChain::new(
                ModelSpec::new("claude-sonnet-4-20250514").with_max_tokens(1024),
            ),
            system: vec![SystemBlock::Text {
                text: "You are a helpful assistant.".to_string(),
            }],
            messages: vec![Message::User {
                content: vec![UserBlock::Text {
                    text: msg_text.to_string(),
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
                strict: false,
            }],
            tool_choice: None,
            cache_key: None,
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
        let input_a = serde_json::json!({"alpha": 1, "beta": 2});
        let input_b = serde_json::json!({"beta": 2, "alpha": 1});

        let make = |input: serde_json::Value| Prompt {
            chain: ModelChain::new("test"),
            system: vec![],
            messages: vec![Message::Assistant {
                content: vec![AssistantBlock::ToolUse {
                    id: "tu_1".to_string(),
                    name: "fn".to_string(),
                    input,
                }],
            }],
            tools: vec![],
            tool_choice: None,
            cache_key: None,
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

    #[test]
    fn hash_ignores_cache_key() {
        let mut a = sample_prompt("hello");
        let mut b = sample_prompt("hello");
        a.cache_key = Some("session-a".to_string());
        b.cache_key = Some("session-b".to_string());

        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn tool_strict_roundtrips_through_serde() {
        let strict_tool = Tool {
            name: "submit_output".into(),
            description: Some("call once".into()),
            input_schema: serde_json::json!({"type": "object"}),
            strict: true,
        };
        let json = serde_json::to_value(&strict_tool).unwrap();
        assert_eq!(json.get("strict"), Some(&serde_json::json!(true)));
        let back: Tool = serde_json::from_value(json).unwrap();
        assert!(back.strict);

        let lax_tool = Tool {
            name: "x".into(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            strict: false,
        };
        let json = serde_json::to_value(&lax_tool).unwrap();
        assert!(
            json.get("strict").is_none(),
            "strict: false should be omitted from wire JSON"
        );
        let back: Tool = serde_json::from_value(json).unwrap();
        assert!(!back.strict);
    }

    #[test]
    fn hash_ignores_fallbacks() {
        let mut a = sample_prompt("hello");
        let mut b = sample_prompt("hello");
        a.chain = ModelChain::new("a").with_fallback("b");
        b.chain = ModelChain::new("a").with_fallback("c").with_fallback("d");
        assert_eq!(a.hash(), b.hash());
    }
}
