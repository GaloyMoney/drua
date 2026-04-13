use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::AgentRole;

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
