use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// Top-level request body for `POST /v1/messages`.
#[derive(Serialize)]
pub(crate) struct MessagesRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub system: &'a str,
    pub tools: &'a [ToolDefinition],
    pub messages: &'a [Message],
}

/// A tool definition in the Anthropic Messages API format.
#[derive(Serialize, Clone, Debug)]
pub(crate) struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// A single message in the conversation (internally tagged by `role`).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "role", rename_all = "lowercase")]
pub(crate) enum Message {
    User { content: MessageContent },
    Assistant { content: Vec<ContentBlock> },
}

/// User message content — either a plain string or an array of content blocks.
///
/// The Anthropic API accepts both forms for user messages. We use
/// `Text` for the initial prompt and `Blocks` when sending tool results.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub(crate) enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

// ---------------------------------------------------------------------------
// Content blocks (discriminated by `type`)
// ---------------------------------------------------------------------------

/// A single content block within a message.
///
/// Used for both request (serialization) and response (deserialization).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Response from `POST /v1/messages`.
#[derive(Deserialize)]
pub(crate) struct MessagesResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
}

/// Reason the model stopped generating.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

/// Token usage statistics from a single API call.
#[derive(Deserialize, Clone, Debug)]
pub(crate) struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_message_serializes_with_string_content() {
        let msg = Message::User {
            content: MessageContent::Text("hello".to_string()),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn user_tool_result_message_serializes_as_array() {
        let msg = Message::User {
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "result text".to_string(),
                is_error: None,
            }]),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_1");
        assert_eq!(blocks[0]["content"], "result text");
        assert!(blocks[0].get("is_error").is_none());
    }

    #[test]
    fn tool_result_with_error_includes_is_error() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "call_2".to_string(),
            content: "something failed".to_string(),
            is_error: Some(true),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["is_error"], true);
    }

    #[test]
    fn assistant_message_serializes_content_blocks() {
        let msg = Message::Assistant {
            content: vec![
                ContentBlock::Text {
                    text: "Let me help.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call_3".to_string(),
                    name: "search_tools".to_string(),
                    input: serde_json::json!({"query": "test"}),
                },
            ],
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "assistant");
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_3");
    }

    #[test]
    fn stop_reason_deserializes() {
        let json = r#""tool_use""#;
        let reason: StopReason = serde_json::from_str(json).unwrap();
        assert_eq!(reason, StopReason::ToolUse);

        let json = r#""end_turn""#;
        let reason: StopReason = serde_json::from_str(json).unwrap();
        assert_eq!(reason, StopReason::EndTurn);
    }

    #[test]
    fn request_serializes_correctly() {
        let tools = vec![ToolDefinition {
            name: "search_tools".to_string(),
            description: "Search for tools".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let messages = vec![Message::User {
            content: MessageContent::Text("hi".to_string()),
        }];
        let req = MessagesRequest {
            model: "claude-sonnet-4-6",
            max_tokens: 4096,
            system: "You are helpful.",
            tools: &tools,
            messages: &messages,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-6");
        assert_eq!(json["max_tokens"], 4096);
        assert_eq!(json["system"], "You are helpful.");
        assert!(json["tools"].is_array());
        assert!(json["messages"].is_array());
    }

    #[test]
    fn response_deserializes_text_and_tool_use() {
        let json = serde_json::json!({
            "id": "msg_123",
            "type": "message",
            "content": [
                {"type": "text", "text": "I'll search for that."},
                {"type": "tool_use", "id": "call_1", "name": "search_tools", "input": {"query": "test"}}
            ],
            "model": "claude-sonnet-4-6",
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let resp: MessagesResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(resp.usage.input_tokens, 100);
        assert_eq!(resp.usage.output_tokens, 50);
    }
}
