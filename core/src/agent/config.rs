use std::collections::HashMap;

use llm::ModelChain;
use serde::{Deserialize, Deserializer, Serialize};

use super::error::AgentError;
use super::session::CompactionConfig;
use super::AgentRole;

/// Roles that must be present in `builtin_roles`. New variants must be
/// added here too — `validate` fails fast at startup if missing.
const REQUIRED_ROLES: &[AgentRole] = &[AgentRole::ProjectLead, AgentRole::Agent];

/// Per-role config. `chain` overrides `AgentsConfig.default_chain` when
/// set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleConfig {
    /// Optional per-role chain override. Falls through to
    /// `AgentsConfig.default_chain` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<ModelChain>,
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
    /// Default chain applied to every agent (user- or workflow-spawned)
    /// whose role has no chain override and whose creation site supplies
    /// no explicit override. Required at startup unless every required
    /// role has its own `chain` set.
    ///
    /// Workflow-specific overrides live on the WorkflowDefinition entity
    /// itself (and per-step on `WorkflowStepDef`), passed through
    /// `Agents::create_in_op`'s `chain_override` parameter at the call
    /// site. They do not get a separate config-level default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_chain_loose")]
    pub default_chain: Option<ModelChain>,
    #[serde(default)]
    pub builtin_roles: HashMap<AgentRole, RoleConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelDefaults>,
}

/// Accepts both `default_chain: { primary: { name: "x" }, fallbacks: [] }`
/// and the bare-string shortcut `default_chain: "x"`.
fn deserialize_chain_loose<'de, D>(deserializer: D) -> Result<Option<ModelChain>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Bare(String),
        Full(ModelChain),
    }
    let opt: Option<Either> = Option::deserialize(deserializer)?;
    Ok(opt.map(|e| match e {
        Either::Bare(name) => ModelChain::new(name),
        Either::Full(chain) => chain,
    }))
}

impl AgentsConfig {
    /// Called from `App::init` to fail loudly at startup. Each required
    /// role must resolve to *some* chain (its own override or the
    /// default), and every model id referenced in any chain must be in
    /// the registry.
    pub fn validate(&self) -> Result<(), AgentError> {
        for role in REQUIRED_ROLES {
            let role_cfg = self
                .builtin_roles
                .get(role)
                .ok_or(AgentError::RoleNotConfigured(*role))?;
            let chain = role_cfg
                .chain
                .clone()
                .or_else(|| self.default_chain.clone())
                .ok_or_else(|| {
                    AgentError::ModelNotConfigured(format!(
                        "no chain resolvable for role {role:?}: set agents.default_chain \
                         or agents.builtin_roles.{role:?}.chain"
                    ))
                })?;
            for spec in chain.iter() {
                if !self.models.contains_key(&spec.name) {
                    return Err(AgentError::ModelNotConfigured(spec.name.clone()));
                }
            }
        }
        Ok(())
    }

    /// Resolve the chain for an agent at creation time.
    ///
    /// Precedence (highest first):
    /// 1. `override_chain` — supplied by the call site (workflow executor
    ///    threading through a `WorkflowDefinition.model_chain` /
    ///    `WorkflowStepDef::AgentStep.model_chain`, or a GraphQL/web UI
    ///    creation form supplying a user-chosen chain)
    /// 2. `builtin_roles[role].chain` — per-role config override
    /// 3. `default_chain` — config-level default
    pub fn resolve_chain(
        &self,
        role: AgentRole,
        override_chain: Option<ModelChain>,
    ) -> Result<ModelChain, AgentError> {
        if let Some(c) = override_chain {
            return Ok(c);
        }
        let role_cfg = self
            .builtin_roles
            .get(&role)
            .ok_or(AgentError::RoleNotConfigured(role))?;
        role_cfg
            .chain
            .clone()
            .or_else(|| self.default_chain.clone())
            .ok_or_else(|| {
                AgentError::ModelNotConfigured(format!("no chain resolvable for role {role:?}"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults_for(name: &str) -> ModelDefaults {
        ModelDefaults {
            model: name.to_string(),
            max_tokens_per_response: 4096,
            context_window_tokens: 100_000,
        }
    }

    #[test]
    fn validate_passes_with_default_chain_and_role_without_override() {
        let mut cfg = AgentsConfig {
            default_chain: Some(ModelChain::new("primary").with_fallback("backup")),
            ..Default::default()
        };
        cfg.models.insert("primary".into(), defaults_for("primary"));
        cfg.models.insert("backup".into(), defaults_for("backup"));
        cfg.builtin_roles
            .insert(AgentRole::ProjectLead, RoleConfig::default());
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());
        cfg.validate().expect("should validate");
    }

    #[test]
    fn validate_fails_when_chain_references_unregistered_model() {
        let mut cfg = AgentsConfig {
            default_chain: Some(ModelChain::new("missing")),
            ..Default::default()
        };
        cfg.builtin_roles
            .insert(AgentRole::ProjectLead, RoleConfig::default());
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());
        let err = cfg.validate().expect_err("missing model id");
        assert!(format!("{err}").contains("missing"));
    }

    #[test]
    fn role_chain_overrides_default() {
        let mut cfg = AgentsConfig {
            default_chain: Some(ModelChain::new("default")),
            ..Default::default()
        };
        cfg.models.insert("default".into(), defaults_for("default"));
        cfg.models.insert("role".into(), defaults_for("role"));
        cfg.builtin_roles.insert(
            AgentRole::Agent,
            RoleConfig {
                chain: Some(ModelChain::new("role")),
                ..Default::default()
            },
        );
        let resolved = cfg.resolve_chain(AgentRole::Agent, None).unwrap();
        assert_eq!(resolved.primary.name, "role");
    }

    #[test]
    fn explicit_override_beats_role_and_default() {
        let mut cfg = AgentsConfig {
            default_chain: Some(ModelChain::new("default")),
            ..Default::default()
        };
        cfg.builtin_roles.insert(
            AgentRole::Agent,
            RoleConfig {
                chain: Some(ModelChain::new("role")),
                ..Default::default()
            },
        );
        let resolved = cfg
            .resolve_chain(AgentRole::Agent, Some(ModelChain::new("explicit")))
            .unwrap();
        assert_eq!(resolved.primary.name, "explicit");
    }

    #[test]
    fn falls_back_to_default_when_role_has_no_override() {
        let mut cfg = AgentsConfig {
            default_chain: Some(ModelChain::new("default")),
            ..Default::default()
        };
        cfg.models.insert("default".into(), defaults_for("default"));
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());
        let resolved = cfg.resolve_chain(AgentRole::Agent, None).unwrap();
        assert_eq!(resolved.primary.name, "default");
    }

    #[test]
    fn deserialise_bare_string_default_chain() {
        let yaml = r#"
default_chain: "anthropic/sonnet"
builtin_roles:
  project_lead: {}
  agent: {}
models:
  anthropic/sonnet:
    model: "anthropic/sonnet"
    max_tokens_per_response: 8192
    context_window_tokens: 200000
"#;
        let cfg: AgentsConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.default_chain.unwrap().primary.name, "anthropic/sonnet");
    }

    #[test]
    fn deserialise_full_chain_object() {
        let yaml = r#"
default_chain:
  primary: { name: "anthropic/opus", max_tokens: 16384 }
  fallbacks:
    - { name: "anthropic/sonnet" }
    - { name: "openai/gpt-4o" }
builtin_roles:
  project_lead: {}
  agent: {}
models:
  anthropic/opus:
    model: "anthropic/opus"
    max_tokens_per_response: 16384
    context_window_tokens: 200000
  anthropic/sonnet:
    model: "anthropic/sonnet"
    max_tokens_per_response: 8192
    context_window_tokens: 200000
  openai/gpt-4o:
    model: "openai/gpt-4o"
    max_tokens_per_response: 4096
    context_window_tokens: 128000
"#;
        let cfg: AgentsConfig = serde_yaml::from_str(yaml).unwrap();
        let chain = cfg.default_chain.unwrap();
        assert_eq!(chain.primary.name, "anthropic/opus");
        assert_eq!(chain.primary.max_tokens, Some(16384));
        assert_eq!(chain.fallbacks.len(), 2);
    }

    #[test]
    fn role_with_explicit_chain_in_yaml() {
        let yaml = r#"
default_chain: "primary/model"
builtin_roles:
  project_lead:
    chain:
      primary: { name: "lead/special" }
  agent: {}
models:
  primary/model:
    model: "primary/model"
    max_tokens_per_response: 8192
    context_window_tokens: 200000
  lead/special:
    model: "lead/special"
    max_tokens_per_response: 8192
    context_window_tokens: 200000
"#;
        let cfg: AgentsConfig = serde_yaml::from_str(yaml).unwrap();
        let lead_chain = cfg.resolve_chain(AgentRole::ProjectLead, None).unwrap();
        assert_eq!(lead_chain.primary.name, "lead/special");
        let agent_chain = cfg.resolve_chain(AgentRole::Agent, None).unwrap();
        assert_eq!(agent_chain.primary.name, "primary/model");
    }
}
