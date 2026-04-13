use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::error::AgentError;
use super::AgentRole;

/// Every `AgentRole` variant that must be present in
/// [`AgentsConfig::builtin_roles`] for the service to start. Add new
/// variants to the enum AND to this list — the compiler won't help, but
/// [`AgentsConfig::validate`] will fail fast at startup.
const REQUIRED_ROLES: &[AgentRole] = &[AgentRole::WorkspaceLead];

/// Per-role defaults applied when an agent with that role is created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleConfig {
    pub model: String,
    pub system: Vec<llm::prompt::SystemBlock>,
    pub max_tokens: u32,
    /// If set, a new thread is started when a user message arrives more than
    /// this long after the previous user message in the current thread.
    /// `None` disables the auto-reset.
    #[serde(default)]
    pub reset_time_delta: Option<std::time::Duration>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsConfig {
    #[serde(default)]
    pub builtin_roles: HashMap<AgentRole, RoleConfig>,
}

impl AgentsConfig {
    /// Verify that every built-in `AgentRole` has a `RoleConfig`. Called
    /// from `App::init` so a misconfigured deployment fails loudly at
    /// startup rather than on the first agent-create.
    pub fn validate(&self) -> Result<(), AgentError> {
        for role in REQUIRED_ROLES {
            if !self.builtin_roles.contains_key(role) {
                return Err(AgentError::RoleNotConfigured(*role));
            }
        }
        Ok(())
    }
}
