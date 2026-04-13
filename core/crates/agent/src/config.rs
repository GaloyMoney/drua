use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::AgentRole;

/// Per-role defaults applied when an agent with that role is created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleConfig {
    pub model: String,
    pub system: Vec<llm::prompt::SystemBlock>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsConfig {
    #[serde(default)]
    pub builtin_roles: HashMap<AgentRole, RoleConfig>,
}
