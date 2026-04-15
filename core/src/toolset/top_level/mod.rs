//! Built-in [`TopLevelTool`](super::traits::TopLevelTool) implementations.

mod bash;
mod catalog;
mod glob;
mod grep;
mod log;
mod ls;
mod ping;
mod read;
mod text_editor;

pub use bash::Bash;
pub use catalog::{CallCatalogTool, DescribeCatalogTool, SearchCatalog};
pub use glob::GlobTool;
pub use grep::Grep;
pub use log::{AllLogs, WorkspaceLog};
pub use ls::Ls;
pub use ping::Ping;
pub use read::Read;
pub use text_editor::TextEditor;

// ---------------------------------------------------------------------------
// Shared authz helpers for sandbox-backed tools
// ---------------------------------------------------------------------------

use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId};

/// True for `Agent` and `AgentOnBehalfOfUser`. Other subjects (User,
/// ExportedAgent, Anonymous) shouldn't see sandbox tools.
pub(super) fn is_agent_subject(subject: &AuthSubject) -> bool {
    matches!(
        subject,
        AuthSubject::Agent(_, _, _) | AuthSubject::AgentOnBehalfOfUser(_, _, _, _)
    )
}

/// First sandbox the subject can read from — `SandboxUseAll` always implies
/// `SandboxUseReadOnly`, so we accept either. Used by read-only tools
/// (Grep, Glob, Read, LS) and the `view` command of the text editor.
pub(super) fn readable_sandbox_id(subject: &AuthSubject) -> Option<SandboxId> {
    subject.scopes().iter().find_map(|s| match s {
        AuthScope::SandboxUseAll(id) | AuthScope::SandboxUseReadOnly(id) => Some(*id),
        _ => None,
    })
}
