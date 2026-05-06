use std::sync::Arc;

use crate::primitives::AuthSubject;
use crate::toolset::ToolSets;

use super::entity::AgentRole;
use super::session::message::SystemBlock;

const BASE_PROMPT_PREFIX: &str = "You are an AI agent operating inside the \
Galoy Agents platform, in project";

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

<use_compose_for_efficiency>
When a task involves multiple dependent tool calls, data filtering, or \
fan-out patterns (e.g. checking N items in parallel), prefer the \
`compose` tool over sequential `call_tool` round trips. A single \
compose call executes JavaScript that can chain tools, filter results, \
and use Promise.all() — reducing latency and keeping large intermediate \
data off the conversation context. Reserve individual call_tool for \
one-off lookups or when you need to inspect output before deciding \
what to do next.

Before writing the script, fetch typed signatures via \
`compose_types({tool_names: ['<prefix>_*']})` for batch lookups, or \
`describe_tool({tool_name: '<name>'})` for a single-tool deep-dive — \
never guess tool or parameter names.
</use_compose_for_efficiency>

<project_notes>
The project has a shared notes system (the `notes` tool). Notes are \
concise knowledge snippets that persist across agent sessions. They are \
the project's lived memory — use them so future agents do not repeat \
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
</project_notes>";

const PROJECT_LEAD_ROLE: &str = "\
You are the project lead. You coordinate work across the project, \
delegate tasks to other agents, and answer user questions directly. \
You cannot attach to sandboxes, but you can inspect any sandbox in \
the project using the sandbox tool (command: inspect). For code \
changes and command execution, delegate to other agents.";

const AGENT_ROLE: &str = "\
You are a task agent. You start without a sandbox attached. When a \
sandbox is attached or detached during the conversation, you will \
receive a <sandbox> message naming the sandbox, the mode (read or \
write), and the working directory you should treat as your scope — \
tools reject paths outside it. When attached in write mode you can \
run commands and edit files inside the sandbox to complete your \
assigned tasks. In read-only mode you can browse files but cannot \
modify them. Focus on completing the specific task you have been given.

<default_to_action>
Implement changes rather than only suggesting them. Use tools to \
discover missing details instead of asking for clarification.
</default_to_action>";

/// Returns four `SystemBlock`s (Base, Tools, Behavioral, Role) kept
/// separate to allow cache-control breakpoints at the LLM layer.
pub fn system_blocks_for_role(
    role: AgentRole,
    toolsets: &Arc<ToolSets>,
    subject: &AuthSubject,
    project_name: &str,
) -> Vec<SystemBlock> {
    let base_text = format!("{BASE_PROMPT_PREFIX} \"{project_name}\".");
    let tools_text = build_tools_section(role, toolsets, subject);
    let role_text = match role {
        AgentRole::ProjectLead => PROJECT_LEAD_ROLE,
        AgentRole::Agent => AGENT_ROLE,
    };

    vec![
        SystemBlock::Base { text: base_text },
        SystemBlock::Tools { text: tools_text },
        SystemBlock::Behavioral {
            text: BEHAVIORAL_GUIDELINES.to_string(),
        },
        SystemBlock::Role {
            text: role_text.to_string(),
        },
    ]
}

/// Tools section: does NOT re-list top-level tools (already in `tools`
/// array). Covers sandbox prerequisites and progressive disclosure.
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
    fn project_lead_returns_four_blocks() {
        let toolsets = Arc::new(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(ToolSets::init(Default::default(), None))
                .unwrap(),
        );
        let subject = AuthSubject::Anonymous;
        let blocks =
            system_blocks_for_role(AgentRole::ProjectLead, &toolsets, &subject, "acme-corp");
        assert_eq!(blocks.len(), 4);
        assert!(matches!(&blocks[0], SystemBlock::Base { .. }));
        assert!(blocks[0].text().contains("Galoy Agents platform"));
        assert!(blocks[0].text().contains("acme-corp"));
        assert!(matches!(&blocks[1], SystemBlock::Tools { .. }));
        assert!(blocks[1].text().contains("progressive disclosure"));
        assert!(matches!(&blocks[2], SystemBlock::Behavioral { .. }));
        assert!(blocks[2].text().contains("investigate_before_answering"));
        assert!(blocks[2].text().contains("use_parallel_tool_calls"));
        assert!(blocks[2].text().contains("use_compose_for_efficiency"));
        assert!(blocks[2].text().contains("compose_types"));
        assert!(blocks[2].text().contains("describe_tool"));
        assert!(matches!(&blocks[3], SystemBlock::Role { .. }));
        assert!(blocks[3].text().contains("project lead"));
    }

    #[test]
    fn agent_returns_four_blocks_with_sandbox_note() {
        let toolsets = Arc::new(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(ToolSets::init(Default::default(), None))
                .unwrap(),
        );
        let subject = AuthSubject::Anonymous;
        let blocks = system_blocks_for_role(AgentRole::Agent, &toolsets, &subject, "test-project");
        assert_eq!(blocks.len(), 4);
        assert!(matches!(&blocks[1], SystemBlock::Tools { .. }));
        assert!(blocks[1].text().contains("Sandbox tools"));
        assert!(blocks[1].text().contains("require an attached sandbox"));
        assert!(matches!(&blocks[2], SystemBlock::Behavioral { .. }));
        assert!(blocks[2].text().contains("investigate_before_answering"));
        assert!(blocks[2].text().contains("use_parallel_tool_calls"));
        assert!(blocks[2].text().contains("use_compose_for_efficiency"));
        assert!(blocks[2].text().contains("compose_types"));
        assert!(blocks[2].text().contains("describe_tool"));
        assert!(matches!(&blocks[3], SystemBlock::Role { .. }));
        assert!(blocks[3].text().contains("task agent"));
        assert!(blocks[3].text().contains("default_to_action"));
    }
}
