use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::error::AgentError;
use super::session::CompactionConfig;
use super::AgentRole;

/// Roles that must be present in `builtin_roles`. New variants must be
/// added here too — `validate` fails fast at startup if missing.
const REQUIRED_ROLES: &[AgentRole] = &[AgentRole::WorkspaceLead, AgentRole::Agent];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleConfig {
    pub model: String,
    #[serde(default)]
    pub compaction: CompactionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDefaults {
    pub model: String,
    pub max_tokens_per_response: u32,
    pub context_window_tokens: u64,
}

impl Default for ModelDefaults {
    fn default() -> Self {
        Self {
            model: String::new(),
            max_tokens_per_response: 4096,
            context_window_tokens: 200_000,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsConfig {
    #[serde(default)]
    pub builtin_roles: HashMap<AgentRole, RoleConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelDefaults>,
}

impl AgentsConfig {
    /// Called from `App::init` to fail loudly at startup.
    pub fn validate(&self) -> Result<(), AgentError> {
        for role in REQUIRED_ROLES {
            if !self.builtin_roles.contains_key(role) {
                return Err(AgentError::RoleNotConfigured(*role));
            }
        }
        for role_config in self.builtin_roles.values() {
            if !self.models.contains_key(&role_config.model) {
                return Err(AgentError::ModelNotConfigured(role_config.model.clone()));
            }
        }
        Ok(())
    }
}
