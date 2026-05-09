//! `AuthSubject` → `InvocationOwner` adapter.

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

// cursor #3212630890: AgentOnBehalfOfUser populates BOTH agent_id and user_id
// so the same user can fetch later via an ExportedAgent token.
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
