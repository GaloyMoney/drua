use std::sync::Arc;

use crate::primitives::AuthSubject;
use crate::toolset::ToolSets;

use super::entity::AgentRole;
use super::session::message::SystemBlock;

/// Identity line shared by every agent role. The workspace name is
/// interpolated at construction time via [`build_base_block`].
const BASE_PROMPT_PREFIX: &str = "You are an AI agent operating inside the \
Galoy Agents platform, in workspace";

/// Behavioral guidelines shared by every agent role. These are
/// high-ROI directives drawn from Anthropic's official prompting
/// best-practices and published system-prompt patterns.
const BEHAVIORAL_GUIDELINES: &str = "\
<investigate_before_answering>
Never speculate about sandbox contents you have not read. Always use \
read, grep, or ls tools before answering questions about code in a \
sandbox. If you are unsure about something, look it up rather than \
guessing.
</investigate_before_answering>

<use_parallel_tool_calls>
If you intend to call multiple tools and there are no dependencies \
between the tool calls, make all of the independent tool calls in \
parallel. For example, when reading several files, read them all at \
once rather than one at a time.
</use_parallel_tool_calls>

<workspace_notes>
The workspace has a shared notes system (the `notes` tool). Notes are \
concise knowledge snippets that persist across agent sessions. They are \
the workspace's lived memory — use them so future agents do not repeat \
discoveries or mistakes.

Before starting work: read any pinned notes in your system prompt, then \
search notes for your task topic. Prior agents may have left relevant \
context.

When to store a note:
- Findings a future agent needs: recurring bugs, flaky tests, environment \
quirks, error patterns, workarounds.
- Decisions and their rationale: \"chose X over Y because Z.\"
- Task outcomes and summaries: what was done, what remains, what to watch.
- Conventions or patterns discovered in the codebase.

When NOT to store a note:
- Ephemeral session state (use your conversation context instead).
- Information already in the codebase, documentation, or pinned notes.
- Speculative or unverified conclusions.

Keep notes short (under 4000 characters). A note should answer one \
question for the next agent. If you need to write a full document, \
that belongs in the library, not in notes.

Pinning: pin a note when it is critical active context that every agent \
must see immediately — ongoing incidents, active conventions, critical \
warnings. Pinned notes appear in every agent's system prompt, so pin \
sparingly. Unpin when the context is no longer urgent; the note remains \
searchable.
</workspace_notes>";

/// Role-specific context for the workspace lead.
const WORKSPACE_LEAD_ROLE: &str = "\
You are the workspace lead. You coordinate work across the workspace, \
delegate tasks to other agents, and answer user questions directly. \
You cannot attach to sandboxes, but you can inspect any sandbox in \
the workspace using the sandbox tool (command: inspect). For code \
changes and command execution, delegate to other agents.";

/// Role-specific context for a task agent.
const AGENT_ROLE: &str = "\
You are a task agent. You start without a sandbox attached. When a \
sandbox is attached or detached during the conversation, you will \
receive a <sandbox> message announcing the change and the mode \
(read or write). When attached in write mode you can run commands \
and edit files inside the sandbox to complete your assigned tasks. \
In read-only mode you can browse files but cannot modify them. \
Focus on completing the specific task you have been given.

<default_to_action>
Implement changes rather than only suggesting them. Use tools to \
discover missing details instead of asking for clarification.
</default_to_action>";

/// Build the system blocks for a given agent role.
///
/// Returns four [`SystemBlock`]s:
/// 1. **Base** — identity line (cacheable).
/// 2. **Tools** — dynamic list of top-level tools and the progressive
///    disclosure pattern for upstream toolsets.
/// 3. **Behavioral** — shared behavioral guidelines (cacheable).
/// 4. **Role** — role-specific instructions.
///
/// Keeping them as separate blocks (rather than one concatenated
/// string) allows cache-control breakpoints at the LLM layer.
pub fn system_blocks_for_role(
    role: AgentRole,
    toolsets: &Arc<ToolSets>,
    subject: &AuthSubject,
    workspace_name: &str,
) -> Vec<SystemBlock> {
    let base_text = format!("{BASE_PROMPT_PREFIX} \"{workspace_name}\".");
    let tools_text = build_tools_section(role, toolsets, subject);
    let role_text = match role {
        AgentRole::WorkspaceLead => WORKSPACE_LEAD_ROLE,
        AgentRole::Agent => AGENT_ROLE,
    };

    vec![
        SystemBlock::Text { text: base_text },
        SystemBlock::Text { text: tools_text },
        SystemBlock::Text {
            text: BEHAVIORAL_GUIDELINES.to_string(),
        },
        SystemBlock::Text {
            text: role_text.to_string(),
        },
    ]
}

/// Build the tools-context section. Does NOT re-list top-level tools
/// (those are already in the prompt's `tools` array with full schemas).
/// Instead, explains things the model can't infer from the tools array:
/// sandbox-tool prerequisites (for task agents) and the progressive-
/// disclosure pattern for upstream toolsets.
fn build_tools_section(
    role: AgentRole,
    toolsets: &Arc<ToolSets>,
    _subject: &AuthSubject,
) -> String {
    let mut section = String::new();

    if matches!(role, AgentRole::Agent) {
        section.push_str(
            "Sandbox tools (bash, text_editor, read, ls, grep, glob) \
             require an attached sandbox.\n",
        );
    }

    // Progressive disclosure for upstream/searchable toolsets.
    let gateway_info = toolsets.mcp_gateway_info();
    if !gateway_info.is_empty() {
        section.push_str("\n# Additional tools (progressive disclosure)\n\n");
        section.push_str(&gateway_info);
        section.push('\n');
    }

    section
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_lead_returns_four_blocks() {
        let toolsets = Arc::new(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(ToolSets::init(Default::default()))
                .unwrap(),
        );
        let subject = AuthSubject::Anonymous;
        let blocks =
            system_blocks_for_role(AgentRole::WorkspaceLead, &toolsets, &subject, "acme-corp");
        assert_eq!(blocks.len(), 4);
        match &blocks[0] {
            SystemBlock::Text { text } => {
                assert!(text.contains("Galoy Agents platform"));
                assert!(text.contains("acme-corp"));
            }
        }
        match &blocks[1] {
            SystemBlock::Text { text } => assert!(text.contains("progressive disclosure")),
        }
        match &blocks[2] {
            SystemBlock::Text { text } => {
                assert!(text.contains("investigate_before_answering"));
                assert!(text.contains("use_parallel_tool_calls"));
            }
        }
        match &blocks[3] {
            SystemBlock::Text { text } => assert!(text.contains("workspace lead")),
        }
    }

    #[test]
    fn agent_returns_four_blocks_with_sandbox_note() {
        let toolsets = Arc::new(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(ToolSets::init(Default::default()))
                .unwrap(),
        );
        let subject = AuthSubject::Anonymous;
        let blocks =
            system_blocks_for_role(AgentRole::Agent, &toolsets, &subject, "test-workspace");
        assert_eq!(blocks.len(), 4);
        match &blocks[1] {
            SystemBlock::Text { text } => {
                assert!(text.contains("Sandbox tools"));
                assert!(text.contains("require an attached sandbox"));
            }
        }
        match &blocks[2] {
            SystemBlock::Text { text } => {
                assert!(text.contains("investigate_before_answering"));
                assert!(text.contains("use_parallel_tool_calls"));
            }
        }
        match &blocks[3] {
            SystemBlock::Text { text } => {
                assert!(text.contains("task agent"));
                assert!(text.contains("default_to_action"));
            }
        }
    }
}
