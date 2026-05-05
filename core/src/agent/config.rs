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
/// set. The legacy `model: "x"` field deserialises into a length-1 chain
/// transparently for backwards compatibility with pre-chains drua.yml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleConfig {
    /// Optional per-role chain override. Falls through to
    /// `AgentsConfig.default_chain` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<ModelChain>,
    /// Legacy single-model field; deserialised into a length-1 chain on
    /// load. Kept for back-compat with existing drua.yml deployments.
    /// Serialisation strips it.
    #[serde(default, skip_serializing)]
    pub model: Option<String>,
    #[serde(default)]
    pub compaction: CompactionConfig,
}

impl RoleConfig {
    /// Returns the resolved chain: `chain` if set, else legacy `model`
    /// promoted to length-1, else `None` (caller falls through to default).
    pub fn resolved_chain(&self) -> Option<ModelChain> {
        if let Some(c) = &self.chain {
            return Some(c.clone());
        }
        self.model
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|name| ModelChain::new(name.clone()))
    }
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
    /// Default chain applied to user-spawned agents whose role lacks an
    /// override. Required at startup (validated below) unless every
    /// role has its own `chain` or legacy `model` set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_chain_loose")]
    pub default_chain: Option<ModelChain>,
    /// Default chain applied to workflow-spawned agents. Independent of
    /// `default_chain` so workflows can run on a cheaper model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_chain_loose")]
    pub workflow_default_chain: Option<ModelChain>,
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
    /// role must resolve to *some* chain (its own override or a default).
    pub fn validate(&self) -> Result<(), AgentError> {
        for role in REQUIRED_ROLES {
            let role_cfg = self
                .builtin_roles
                .get(role)
                .ok_or(AgentError::RoleNotConfigured(*role))?;
            let resolved = role_cfg
                .resolved_chain()
                .or_else(|| self.default_chain.clone());
            let chain = resolved.ok_or_else(|| {
                AgentError::ModelNotConfigured(format!(
                    "no chain resolvable for role {role:?}: set agents.default_chain \
                     or agents.builtin_roles.{role:?}.chain"
                ))
            })?;
            // Every model id in the chain must be in the registry — the
            // executor will skip unknown ids at runtime, but failing fast
            // at boot is cleaner.
            for spec in chain.iter() {
                if !self.models.contains_key(&spec.name) {
                    return Err(AgentError::ModelNotConfigured(spec.name.clone()));
                }
            }
        }
        // Workflow default (if configured) is also validated — but it's
        // optional; absence falls back to `default_chain`.
        if let Some(chain) = &self.workflow_default_chain {
            for spec in chain.iter() {
                if !self.models.contains_key(&spec.name) {
                    return Err(AgentError::ModelNotConfigured(spec.name.clone()));
                }
            }
        }
        Ok(())
    }

    /// Resolve the effective chain for a non-workflow agent of `role`.
    /// Precedence: role override > default. Returns the chain primary's
    /// `ModelDefaults` for context-window / max_tokens.
    pub fn resolve_chain(&self, role: AgentRole) -> Result<ModelChain, AgentError> {
        let role_cfg = self
            .builtin_roles
            .get(&role)
            .ok_or(AgentError::RoleNotConfigured(role))?;
        role_cfg
            .resolved_chain()
            .or_else(|| self.default_chain.clone())
            .ok_or_else(|| {
                AgentError::ModelNotConfigured(format!(
                    "no chain resolvable for role {role:?}"
                ))
            })
    }

    /// Resolve the effective chain for a workflow-spawned agent. Falls
    /// through `workflow_default_chain` → `default_chain` → role override
    /// (in case a deployment wants the workflow agent to share role
    /// settings).
    pub fn resolve_workflow_chain(&self, role: AgentRole) -> Result<ModelChain, AgentError> {
        if let Some(c) = &self.workflow_default_chain {
            return Ok(c.clone());
        }
        self.resolve_chain(role)
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
    fn legacy_role_model_string_resolves_to_length_one_chain() {
        let cfg = RoleConfig {
            chain: None,
            model: Some("legacy/model".into()),
            compaction: Default::default(),
        };
        let chain = cfg.resolved_chain().expect("legacy model resolves");
        assert_eq!(chain.primary.name, "legacy/model");
        assert!(chain.fallbacks.is_empty());
    }

    #[test]
    fn explicit_chain_wins_over_legacy_model() {
        let cfg = RoleConfig {
            chain: Some(ModelChain::new("new/model")),
            model: Some("legacy/model".into()),
            compaction: Default::default(),
        };
        assert_eq!(cfg.resolved_chain().unwrap().primary.name, "new/model");
    }

    #[test]
    fn validate_passes_with_default_chain_and_role_without_override() {
        let mut cfg = AgentsConfig {
            default_chain: Some(ModelChain::new("primary").with_fallback("backup")),
            ..Default::default()
        };
        cfg.models.insert("primary".into(), defaults_for("primary"));
        cfg.models.insert("backup".into(), defaults_for("backup"));
        cfg.builtin_roles.insert(
            AgentRole::ProjectLead,
            RoleConfig {
                chain: None,
                model: None,
                compaction: Default::default(),
            },
        );
        cfg.builtin_roles.insert(
            AgentRole::Agent,
            RoleConfig {
                chain: None,
                model: None,
                compaction: Default::default(),
            },
        );
        cfg.validate().expect("should validate");
    }

    #[test]
    fn validate_fails_when_chain_references_unregistered_model() {
        let mut cfg = AgentsConfig {
            default_chain: Some(ModelChain::new("missing")),
            ..Default::default()
        };
        cfg.builtin_roles.insert(
            AgentRole::ProjectLead,
            RoleConfig {
                chain: None,
                model: None,
                compaction: Default::default(),
            },
        );
        cfg.builtin_roles.insert(
            AgentRole::Agent,
            RoleConfig {
                chain: None,
                model: None,
                compaction: Default::default(),
            },
        );
        let err = cfg.validate().expect_err("missing model id");
        assert!(format!("{err}").contains("missing"));
    }

    #[test]
    fn workflow_chain_falls_back_to_default_chain() {
        let mut cfg = AgentsConfig {
            default_chain: Some(ModelChain::new("agent-default")),
            workflow_default_chain: None,
            ..Default::default()
        };
        cfg.models
            .insert("agent-default".into(), defaults_for("agent-default"));
        cfg.builtin_roles.insert(
            AgentRole::Agent,
            RoleConfig {
                chain: None,
                model: None,
                compaction: Default::default(),
            },
        );
        cfg.builtin_roles.insert(
            AgentRole::ProjectLead,
            RoleConfig {
                chain: None,
                model: None,
                compaction: Default::default(),
            },
        );
        let resolved = cfg.resolve_workflow_chain(AgentRole::Agent).unwrap();
        assert_eq!(resolved.primary.name, "agent-default");
    }

    #[test]
    fn workflow_default_chain_takes_precedence() {
        let mut cfg = AgentsConfig {
            default_chain: Some(ModelChain::new("agent-default")),
            workflow_default_chain: Some(ModelChain::new("workflow-default")),
            ..Default::default()
        };
        cfg.models
            .insert("agent-default".into(), defaults_for("agent-default"));
        cfg.models
            .insert("workflow-default".into(), defaults_for("workflow-default"));
        cfg.builtin_roles.insert(
            AgentRole::Agent,
            RoleConfig {
                chain: None,
                model: None,
                compaction: Default::default(),
            },
        );
        cfg.builtin_roles.insert(
            AgentRole::ProjectLead,
            RoleConfig {
                chain: None,
                model: None,
                compaction: Default::default(),
            },
        );
        let resolved = cfg.resolve_workflow_chain(AgentRole::Agent).unwrap();
        assert_eq!(resolved.primary.name, "workflow-default");
    }

    #[test]
    fn deserialise_bare_string_default_chain() {
        let yaml = r#"
default_chain: "anthropic/sonnet"
builtin_roles:
  project_lead:
    model: "anthropic/sonnet"
  agent:
    model: "anthropic/sonnet"
models:
  anthropic/sonnet:
    model: "anthropic/sonnet"
    max_tokens_per_response: 8192
    context_window_tokens: 200000
"#;
        let cfg: AgentsConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.default_chain.unwrap().primary.name,
            "anthropic/sonnet"
        );
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
  project_lead:
    model: "anthropic/opus"
  agent:
    model: "anthropic/opus"
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
}
