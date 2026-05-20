#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlock {
    Text(String),
    ToolUse {
        name: String,
        input: String,
    },
    Thinking(String),
    ToolResult(String),
    Sandbox {
        sandbox_name: String,
        operation: String,
        mode: Option<String>,
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub blocks: Vec<ContentBlock>,
}

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

    /// Creates a new text block if the last block is a tool-use or empty.
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

    pub fn add_tool_activity(&mut self, name: impl Into<String>) {
        if let Some(msg) = self
            .messages
            .last_mut()
            .filter(|m| m.role == ChatRole::Assistant)
        {
            msg.blocks.push(ContentBlock::ToolUse {
                name: name.into(),
                input: String::new(),
            });
        }
    }

    pub fn add_error(&mut self, msg: impl Into<String>) {
        self.messages.push(ChatMessage {
            role: ChatRole::System,
            blocks: vec![ContentBlock::Text(format!("[Error: {}]", msg.into()))],
        });
        self.streaming = false;
    }

    pub fn finish_streaming(&mut self) {
        self.streaming = false;
    }

    pub fn load_history(&mut self, messages: Vec<ChatMessage>) {
        self.messages = messages;
        self.streaming = false;
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.streaming = false;
    }
}
