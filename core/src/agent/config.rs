use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::error::AgentError;
use super::session::CompactionConfig;
use super::AgentRole;

/// Every `AgentRole` variant that must be present in
/// [`AgentsConfig::builtin_roles`] for the service to start. Add new
/// variants to the enum AND to this list — the compiler won't help, but
/// [`AgentsConfig::validate`] will fail fast at startup.
const REQUIRED_ROLES: &[AgentRole] = &[AgentRole::WorkspaceLead, AgentRole::Agent];

/// Per-role defaults applied when an agent with that role is created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleConfig {
    pub model: String,
    #[serde(default)]
    pub compaction: CompactionConfig,
}

/// Model-level defaults populated from the top-level `providers` config.
/// Carries the model name alongside token limits so it can be threaded
/// through session and thread entities as a single struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDefaults {
    pub model: String,
    pub max_tokens_per_response: u32,
    pub context_window_tokens: u64,
    /// Provider prompt-cache TTL in seconds. Anthropic ≈ 300 s; providers
    /// without caching should set this to 0.
    #[serde(default = "default_cache_ttl_seconds")]
    pub cache_ttl_seconds: u64,
}

fn default_cache_ttl_seconds() -> u64 {
    300
}

impl Default for ModelDefaults {
    fn default() -> Self {
        Self {
            model: String::new(),
            max_tokens_per_response: 4096,
            context_window_tokens: 200_000,
            cache_ttl_seconds: default_cache_ttl_seconds(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsConfig {
    #[serde(default)]
    pub builtin_roles: HashMap<AgentRole, RoleConfig>,
    /// Populated by the CLI from the `providers:` YAML section.
    #[serde(default)]
    pub models: HashMap<String, ModelDefaults>,
}

impl AgentsConfig {
    /// Verify that every built-in `AgentRole` has a `RoleConfig` and that
    /// every referenced model name exists in `self.models`. Called from
    /// `App::init` so a misconfigured deployment fails loudly at startup.
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
