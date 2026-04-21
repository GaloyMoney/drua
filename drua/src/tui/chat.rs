/// Structured chat content: interleaved text and tool-use blocks.
#[derive(Clone, PartialEq, Eq)]
pub enum ContentBlock {
    Text(String),
    ToolUse(String),
    Thinking(String),
    ToolResult(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    System,
}

#[derive(Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub blocks: Vec<ContentBlock>,
}

/// Manages the conversation with a workspace lead agent.
///
/// Streaming flow:
/// 1. `add_user_message(text)` → pushes user msg + empty assistant msg, sets streaming
/// 2. `append_text(text)` / `add_tool_activity(name)` → fills the current assistant msg
/// 3. `finish_streaming()` or `add_error(msg)` → clears streaming flag
#[derive(Default)]
pub struct AssistantChat {
    pub messages: Vec<ChatMessage>,
    pub streaming: bool,
}

impl AssistantChat {
    /// Push a user message and prepare an empty assistant reply.
    pub fn add_user_message(&mut self, text: impl Into<String>) {
        self.messages.push(ChatMessage {
            role: ChatRole::User,
            blocks: vec![ContentBlock::Text(text.into())],
        });
        self.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            blocks: Vec::new(),
        });
        self.streaming = true;
    }

    /// Append text to the current assistant message's last text block,
    /// creating a new text block if the last block is a tool-use or empty.
    pub fn append_text(&mut self, text: &str) {
        let msg = match self.messages.last_mut() {
            Some(m) if m.role == ChatRole::Assistant => m,
            _ => {
                self.messages.push(ChatMessage {
                    role: ChatRole::Assistant,
                    blocks: Vec::new(),
                });
                self.messages.last_mut().unwrap()
            }
        };

        match msg.blocks.last_mut() {
            Some(ContentBlock::Text(existing)) => existing.push_str(text),
            _ => msg.blocks.push(ContentBlock::Text(text.to_string())),
        }
    }

    /// Record a tool invocation inline in the current assistant message.
    pub fn add_tool_activity(&mut self, name: impl Into<String>) {
        if let Some(msg) = self
            .messages
            .last_mut()
            .filter(|m| m.role == ChatRole::Assistant)
        {
            msg.blocks.push(ContentBlock::ToolUse(name.into()));
        }
    }

    /// Append an error as a system message and stop streaming.
    pub fn add_error(&mut self, msg: impl Into<String>) {
        self.messages.push(ChatMessage {
            role: ChatRole::System,
            blocks: vec![ContentBlock::Text(format!("[Error: {}]", msg.into()))],
        });
        self.streaming = false;
    }

    /// Mark the current stream as complete.
    pub fn finish_streaming(&mut self) {
        self.streaming = false;
    }

    /// Replace the conversation with pre-fetched history messages.
    pub fn load_history(&mut self, messages: Vec<ChatMessage>) {
        self.messages = messages;
        self.streaming = false;
    }

    /// Reset all conversation state.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.streaming = false;
    }
}
