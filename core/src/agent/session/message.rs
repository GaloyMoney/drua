use serde::{Deserialize, Serialize};

use super::thread::SessionThreadId;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetThread {
    Main,
    Id(SessionThreadId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionMetadata {
    pub follows_from: SessionThreadId,
    pub tool_results_masked: usize,
    pub thinking_blocks_cleared: usize,
    pub sandbox_notifications_stripped: usize,
    pub estimated_tokens_saved: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub target_thread: TargetThread,
    pub model: String,
    pub max_tokens_per_response: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<SystemBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionMetadata>,
}

/// Each kind appears at most once per thread; updates match by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemBlockKind {
    Base,
    Tools,
    Behavioral,
    Role,
    Notes,
    Skills,
    Spaces,
}

impl SystemBlockKind {
    /// Canonical order shared between `system_prompt` and the session entity.
    pub const ORDER: &'static [SystemBlockKind] = &[
        SystemBlockKind::Base,
        SystemBlockKind::Tools,
        SystemBlockKind::Behavioral,
        SystemBlockKind::Role,
        SystemBlockKind::Notes,
        SystemBlockKind::Skills,
        SystemBlockKind::Spaces,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SystemBlock {
    Base { text: String },
    Tools { text: String },
    Behavioral { text: String },
    Role { text: String },
    Notes { text: String },
    Skills { text: String },
    Spaces { text: String },
}

impl SystemBlock {
    pub fn kind(&self) -> SystemBlockKind {
        match self {
            SystemBlock::Base { .. } => SystemBlockKind::Base,
            SystemBlock::Tools { .. } => SystemBlockKind::Tools,
            SystemBlock::Behavioral { .. } => SystemBlockKind::Behavioral,
            SystemBlock::Role { .. } => SystemBlockKind::Role,
            SystemBlock::Notes { .. } => SystemBlockKind::Notes,
            SystemBlock::Skills { .. } => SystemBlockKind::Skills,
            SystemBlock::Spaces { .. } => SystemBlockKind::Spaces,
        }
    }

    pub fn text(&self) -> &str {
        match self {
            SystemBlock::Base { text }
            | SystemBlock::Tools { text }
            | SystemBlock::Behavioral { text }
            | SystemBlock::Role { text }
            | SystemBlock::Notes { text }
            | SystemBlock::Skills { text }
            | SystemBlock::Spaces { text } => text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    /// Default-omitted; stale events hydrate as non-strict.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strict: bool,
    /// Default-omitted (`External`); stale events hydrate as
    /// `External` so MCP-routed tools keep working.
    #[serde(default, skip_serializing_if = "ToolKind::is_default")]
    pub kind: ToolKind,
}

/// Discriminator on `ToolDefinition`. `External` calls are routed to
/// MCP toolsets; `SubmitOutput` calls are intercepted by the agent
/// loop, validated against the tool's `input_schema`, and persisted as
/// the session's terminal `OutputSubmitted` event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    #[default]
    External,
    SubmitOutput,
}

impl ToolKind {
    fn is_default(&self) -> bool {
        matches!(self, ToolKind::External)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
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
    SandboxInfo {
        sandbox_name: String,
        sandbox_operation: SandboxOperation,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultBlock {
    Text { text: String },
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
pub struct ToolResultInput {
    pub tool_use_id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
}

impl From<Prompt> for llm::Prompt {
    fn from(p: Prompt) -> Self {
        llm::Prompt {
            chain: llm::ModelChain::new(p.model),
            max_tokens: Some(p.max_tokens_per_response),
            cache_key: p.cache_key,
            system: p
                .system
                .into_iter()
                .map(llm::prompt::SystemBlock::from)
                .collect(),
            tools: p.tools.into_iter().map(llm::prompt::Tool::from).collect(),
            tool_choice: None,
            messages: p
                .messages
                .into_iter()
                .map(llm::prompt::Message::from)
                .collect(),
        }
    }
}

impl From<SystemBlock> for llm::prompt::SystemBlock {
    fn from(b: SystemBlock) -> Self {
        // Kind is domain-level only; wire format is always `Text`.
        let text = match b {
            SystemBlock::Base { text }
            | SystemBlock::Tools { text }
            | SystemBlock::Behavioral { text }
            | SystemBlock::Role { text }
            | SystemBlock::Notes { text }
            | SystemBlock::Skills { text }
            | SystemBlock::Spaces { text } => text,
        };
        llm::prompt::SystemBlock::Text { text }
    }
}

impl From<ToolDefinition> for llm::prompt::Tool {
    fn from(t: ToolDefinition) -> Self {
        llm::prompt::Tool {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
            strict: t.strict,
        }
    }
}

impl From<Message> for llm::prompt::Message {
    fn from(m: Message) -> Self {
        match m {
            Message::User { content } => llm::prompt::Message::User {
                content: content
                    .into_iter()
                    .map(llm::prompt::UserBlock::from)
                    .collect(),
            },
            Message::Assistant { content } => llm::prompt::Message::Assistant {
                content: content
                    .into_iter()
                    .map(llm::prompt::AssistantBlock::from)
                    .collect(),
            },
        }
    }
}

impl From<UserBlock> for llm::prompt::UserBlock {
    fn from(b: UserBlock) -> Self {
        match b {
            UserBlock::Text { text } => llm::prompt::UserBlock::Text { text },
            UserBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => llm::prompt::UserBlock::ToolResult {
                tool_use_id,
                content: content
                    .into_iter()
                    .map(|tb| match tb {
                        ToolResultBlock::Text { text } => {
                            llm::prompt::ToolResultBlock::Text { text }
                        }
                    })
                    .collect(),
                is_error,
            },
            UserBlock::SandboxInfo {
                sandbox_name,
                sandbox_operation,
            } => llm::prompt::UserBlock::Text {
                text: sandbox_notification_text(&sandbox_name, &sandbox_operation),
            },
        }
    }
}

impl From<AssistantBlock> for llm::prompt::AssistantBlock {
    fn from(b: AssistantBlock) -> Self {
        match b {
            AssistantBlock::Text { text } => llm::prompt::AssistantBlock::Text { text },
            AssistantBlock::ToolUse { id, name, input } => {
                llm::prompt::AssistantBlock::ToolUse { id, name, input }
            }
            AssistantBlock::Thinking { text, signature } => {
                llm::prompt::AssistantBlock::Thinking { text, signature }
            }
        }
    }
}

impl From<llm::prompt::AssistantBlock> for AssistantBlock {
    fn from(b: llm::prompt::AssistantBlock) -> Self {
        match b {
            llm::prompt::AssistantBlock::Text { text, .. } => AssistantBlock::Text { text },
            llm::prompt::AssistantBlock::ToolUse {
                id, name, input, ..
            } => AssistantBlock::ToolUse { id, name, input },
            llm::prompt::AssistantBlock::Thinking { text, signature } => {
                AssistantBlock::Thinking { text, signature }
            }
        }
    }
}

impl From<llm::response::StopReason> for StopReason {
    fn from(s: llm::response::StopReason) -> Self {
        match s {
            llm::response::StopReason::EndTurn => StopReason::Stop,
            llm::response::StopReason::MaxTokens => StopReason::Length,
            llm::response::StopReason::StopSequence => StopReason::Stop,
            llm::response::StopReason::ToolUse => StopReason::ToolUse,
        }
    }
}

impl From<llm::ToolUseResult> for ToolResultInput {
    fn from(r: llm::ToolUseResult) -> Self {
        Self {
            tool_use_id: r.tool_use_id,
            content: r.content,
            is_error: r.is_error,
        }
    }
}

impl From<llm::prompt::Tool> for ToolDefinition {
    fn from(t: llm::prompt::Tool) -> Self {
        Self {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
            strict: t.strict,
            kind: ToolKind::External,
        }
    }
}

// No `From<llm::prompt::SystemBlock> for SystemBlock`: wire blocks carry
// no `kind`.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxOperation {
    /// `agent_mode` is the agent's permission (`read`/`write`).
    /// `kind` is the sandbox's bootstrap mode (`scratch`/`repo`).
    /// `cwd` is the inside-sandbox working directory recorded by the
    /// runtime — the notification renders it as the agent's only path
    /// of interest. `scope` is a short human label like
    /// `repo "my-app"`, used inside the `<sandbox>` text.
    Attach {
        agent_mode: String,
        kind: String,
        cwd: String,
        #[serde(default)]
        scope: Option<String>,
    },
    Detach,
}

/// XML matches the `<sandbox>` tag format described in the agent system prompt.
pub fn sandbox_notification_text(sandbox_name: &str, op: &SandboxOperation) -> String {
    match op {
        SandboxOperation::Attach {
            agent_mode,
            kind,
            cwd,
            scope,
        } => {
            let header = match scope {
                Some(label) => {
                    format!("Attached sandbox \"{sandbox_name}\" ({label}) in {agent_mode} mode.")
                }
                None => format!("Attached sandbox \"{sandbox_name}\" in {agent_mode} mode."),
            };
            let body = match kind.as_str() {
                "repo" => format!(
                    "Working directory: {cwd}\n\
                     The repository is checked out here at the requested branch. Stay inside \
                     this directory; tools reject paths outside it."
                ),
                _ => format!(
                    "Working directory: {cwd}\n\
                     Empty workspace — nothing is pre-populated. Stay inside this directory; \
                     tools reject paths outside it."
                ),
            };
            format!("<sandbox>\n{header}\n{body}\n</sandbox>")
        }
        SandboxOperation::Detach => {
            format!("<sandbox>\nDetached sandbox \"{sandbox_name}\".\n</sandbox>")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_default_kind_is_external() {
        let json = serde_json::json!({
            "name": "my_tool",
            "input_schema": {"type": "object"}
        });
        let def: ToolDefinition = serde_json::from_value(json).unwrap();
        assert!(matches!(def.kind, ToolKind::External));
        assert!(!def.strict);
    }

    #[test]
    fn tool_definition_external_kind_omitted_on_serialize() {
        let def = ToolDefinition {
            name: "foo".into(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            strict: false,
            kind: ToolKind::External,
        };
        let json = serde_json::to_value(&def).unwrap();
        assert!(json.get("kind").is_none());
        assert!(json.get("strict").is_none());
    }

    #[test]
    fn tool_definition_submit_output_roundtrips_through_serde() {
        let def = ToolDefinition {
            name: "submit_output".into(),
            description: Some("call once".into()),
            input_schema: serde_json::json!({"type": "object"}),
            strict: true,
            kind: ToolKind::SubmitOutput,
        };
        let json = serde_json::to_value(&def).unwrap();
        assert_eq!(json.get("kind"), Some(&serde_json::json!("submit_output")));
        assert_eq!(json.get("strict"), Some(&serde_json::json!(true)));
        let back: ToolDefinition = serde_json::from_value(json).unwrap();
        assert!(matches!(back.kind, ToolKind::SubmitOutput));
        assert!(back.strict);
    }

    #[test]
    fn tool_definition_to_llm_tool_preserves_strict() {
        let def = ToolDefinition {
            name: "submit_output".into(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            strict: true,
            kind: ToolKind::SubmitOutput,
        };
        let llm_tool: llm::prompt::Tool = def.into();
        assert!(llm_tool.strict);
    }
}
