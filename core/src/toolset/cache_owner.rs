//! Adapter from drua's `AuthSubject` discrimination into the
//! `drua-tool-cache` crate's flat `InvocationOwner` shape. Lives at
//! the boundary so the cache crate stays auth-agnostic.
//!
//! `Anonymous` / `WorkflowExecutor` subjects yield `None` — the
//! dispatcher short-circuits to raw passthrough rather than
//! persisting; callers gate on the `Option` themselves.

use drua_tool_cache::{InvocationOwner, ToolInvocationOwnerId};

use crate::auth::AuthSubject;
use crate::primitives::{AgentId, UserId};

impl From<AgentId> for ToolInvocationOwnerId {
    fn from(id: AgentId) -> Self {
        ToolInvocationOwnerId(id.into())
    }
}

impl From<UserId> for ToolInvocationOwnerId {
    fn from(id: UserId) -> Self {
        ToolInvocationOwnerId(id.into())
    }
}

/// Derive a fetch-scope owner from an `AuthSubject`.
///
/// - `Agent` (no user attribution) keys off the agent only.
/// - `AgentOnBehalfOfUser` keys off BOTH agent_id AND user_id so
///   the same user can fetch the invocation later via an
///   `ExportedAgent` bearer token (which only supplies user_id).
///   Cursor #3212630890 caught the missing user-side link.
/// - `User` and `ExportedAgent` key off the user — covers
///   mcp-gateway external callers, IDE-driven dispatch, exported-agent
///   bearer tokens.
/// - `Anonymous` and `WorkflowExecutor` yield `None`; the dispatcher
///   short-circuits to raw passthrough so the workflow step's typed
///   `structured_content` reaches its caller untouched.
pub fn invocation_owner(subject: &AuthSubject) -> Option<InvocationOwner> {
    match subject {
        AuthSubject::Agent(_, agent_id, _) => Some(InvocationOwner::agent(*agent_id)),
        AuthSubject::AgentOnBehalfOfUser(user_id, _, agent_id, _) => Some(InvocationOwner {
            agent_id: Some((*agent_id).into()),
            user_id: Some((*user_id).into()),
        }),
        AuthSubject::User(user_id) => Some(InvocationOwner::user(*user_id)),
        AuthSubject::ExportedAgent(user_id, _, _) => Some(InvocationOwner::user(*user_id)),
        AuthSubject::WorkflowExecutor(_, _, _, _) | AuthSubject::Anonymous => None,
    }
}
