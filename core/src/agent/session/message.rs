use serde::{Deserialize, Serialize};

use crate::agent::config::ModelChain;

use super::thread::SessionThreadId;

/// Name of the runtime-provided `submit_output` tool. Used by:
/// - `crate::toolset::top_level::SubmitOutputTool` (the registered
///   global tool).
/// - `AgentSession::add_tool_results` to detect terminal turns.
/// - The workflow executor's per-step `ToolDefinition` override.
///
/// Defined here (alongside `ToolDefinition`) so the session layer
/// owns the constant; toolsets and workflow consume it.
pub const SUBMIT_OUTPUT_TOOL_NAME: &str = "submit_output";

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
    pub model_chain: ModelChain,
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
    Workspace,
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
        SystemBlockKind::Workspace,
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
    Workspace { text: String },
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
            SystemBlock::Workspace { .. } => SystemBlockKind::Workspace,
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
            | SystemBlock::Workspace { text }
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
    /// Default-omitted; stale events hydrate as non-strict. Mirrors
    /// the `strict: true` flag on `lib::llm::prompt::Tool` for
    /// providers that honour Anthropic / OpenAI strict tool use.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strict: bool,
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
        let max_tokens = Some(p.model_chain.primary.max_tokens_per_response);
        let mut chain = llm::ModelChain::new(
            llm::ModelSpec::new(p.model_chain.primary.model)
                .with_max_tokens(p.model_chain.primary.max_tokens_per_response),
        );
        for fallback in p.model_chain.fallbacks {
            chain = chain.with_fallback(
                llm::ModelSpec::new(fallback.model)
                    .with_max_tokens(fallback.max_tokens_per_response),
            );
        }
        llm::Prompt {
            chain,
            max_tokens,
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
            | SystemBlock::Workspace { text }
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
        /// One-line git-proxy push-policy description (e.g.
        /// `Push policy: only these refs are accepted on push: …`).
        /// Sourced from the allow-list at attach time so the agent
        /// knows which branch names will be accepted before its first
        /// push. `None` for scratch sandboxes or repos absent from the
        /// allow-list. Old events default to `None`.
        #[serde(default)]
        push_policy: Option<String>,
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
            push_policy,
        } => {
            let header = match scope {
                Some(label) => {
                    format!("Attached sandbox \"{sandbox_name}\" ({label}) in {agent_mode} mode.")
                }
                None => format!("Attached sandbox \"{sandbox_name}\" in {agent_mode} mode."),
            };
            let mut body = match kind.as_str() {
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
            if let Some(policy) = push_policy {
                body.push('\n');
                body.push_str(policy);
            }
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
    fn tool_definition_strict_default_omitted_on_serialize() {
        let def = ToolDefinition {
            name: "foo".into(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            strict: false,
        };
        let json = serde_json::to_value(&def).unwrap();
        assert!(json.get("strict").is_none());
    }

    #[test]
    fn tool_definition_strict_round_trips_through_serde() {
        let def = ToolDefinition {
            name: "submit_output".into(),
            description: Some("call once".into()),
            input_schema: serde_json::json!({"type": "object"}),
            strict: true,
        };
        let json = serde_json::to_value(&def).unwrap();
        assert_eq!(json.get("strict"), Some(&serde_json::json!(true)));
        let back: ToolDefinition = serde_json::from_value(json).unwrap();
        assert!(back.strict);
    }

    #[test]
    fn tool_definition_to_llm_tool_preserves_strict() {
        let def = ToolDefinition {
            name: "submit_output".into(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            strict: true,
        };
        let llm_tool: llm::prompt::Tool = def.into();
        assert!(llm_tool.strict);
    }
}
