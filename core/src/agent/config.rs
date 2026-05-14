use std::collections::HashMap;

use llm::ModelChain as LlmModelChain;
use serde::{Deserialize, Deserializer, Serialize};

use super::error::AgentError;
use super::session::CompactionConfig;
use super::AgentRole;

const REQUIRED_ROLES: &[AgentRole] = &[
    AgentRole::ProjectLead,
    AgentRole::Agent,
    AgentRole::WorkflowStepAgent,
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<LlmModelChain>,
    #[serde(default)]
    pub compaction: CompactionConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChain {
    pub primary: ModelDefaults,
    #[serde(default)]
    pub fallbacks: Vec<ModelDefaults>,
}

impl ModelChain {
    pub fn iter(&self) -> impl Iterator<Item = &ModelDefaults> {
        std::iter::once(&self.primary).chain(self.fallbacks.iter())
    }

    pub(super) fn from_policy(
        policy: &LlmModelChain,
        models: &HashMap<String, ModelDefaults>,
    ) -> Result<Self, AgentError> {
        let primary = resolve_spec(&policy.primary, models)?;
        let fallbacks = policy
            .fallbacks
            .iter()
            .map(|spec| resolve_spec(spec, models))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { primary, fallbacks })
    }
}

/// Pins `.model` to the lookup key (so downstream code can never see the YAML
/// `model:` field diverge from the registry key), then applies the policy's
/// `max_tokens` override on top of the registry default.
fn resolve_spec(
    spec: &llm::ModelSpec,
    models: &HashMap<String, ModelDefaults>,
) -> Result<ModelDefaults, AgentError> {
    let defaults = models
        .get(spec.name.as_str())
        .cloned()
        .ok_or_else(|| AgentError::ModelNotConfigured(spec.name.clone()))?;
    Ok(ModelDefaults {
        model: spec.name.clone(),
        max_tokens_per_response: spec
            .max_tokens
            .unwrap_or(defaults.max_tokens_per_response),
        context_window_tokens: defaults.context_window_tokens,
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_chain_loose")]
    pub default_chain: Option<LlmModelChain>,
    #[serde(default)]
    pub builtin_roles: HashMap<AgentRole, RoleConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelDefaults>,
}

fn deserialize_chain_loose<'de, D>(deserializer: D) -> Result<Option<LlmModelChain>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Bare(String),
        Full(LlmModelChain),
    }
    let opt: Option<Either> = Option::deserialize(deserializer)?;
    Ok(opt.map(|e| match e {
        Either::Bare(name) => LlmModelChain::new(name),
        Either::Full(chain) => chain,
    }))
}

impl AgentsConfig {
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

    /// Precedence: `override_chain` > role `chain` > `default_chain`.
    pub fn resolve_policy(
        &self,
        role: AgentRole,
        override_chain: Option<LlmModelChain>,
    ) -> Result<LlmModelChain, AgentError> {
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

    pub(crate) fn resolve_chain(
        &self,
        role: AgentRole,
        override_chain: Option<LlmModelChain>,
    ) -> Result<ModelChain, AgentError> {
        let policy = self.resolve_policy(role, override_chain)?;
        ModelChain::from_policy(&policy, &self.models)
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
            default_chain: Some(LlmModelChain::new("primary").with_fallback("backup")),
            ..Default::default()
        };
        cfg.models.insert("primary".into(), defaults_for("primary"));
        cfg.models.insert("backup".into(), defaults_for("backup"));
        cfg.builtin_roles
            .insert(AgentRole::ProjectLead, RoleConfig::default());
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());
        cfg.builtin_roles
            .insert(AgentRole::WorkflowStepAgent, RoleConfig::default());
        cfg.validate().expect("should validate");
    }

    #[test]
    fn validate_fails_when_chain_references_unregistered_model() {
        let mut cfg = AgentsConfig {
            default_chain: Some(LlmModelChain::new("missing")),
            ..Default::default()
        };
        cfg.builtin_roles
            .insert(AgentRole::ProjectLead, RoleConfig::default());
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());
        cfg.builtin_roles
            .insert(AgentRole::WorkflowStepAgent, RoleConfig::default());
        let err = cfg.validate().expect_err("missing model id");
        assert!(format!("{err}").contains("missing"));
    }

    #[test]
    fn resolve_chain_pulls_limits_from_models_map() {
        let mut cfg = AgentsConfig {
            default_chain: Some(LlmModelChain::new("primary").with_fallback("backup")),
            ..Default::default()
        };
        cfg.models.insert(
            "primary".into(),
            ModelDefaults {
                model: "primary".into(),
                max_tokens_per_response: 8192,
                context_window_tokens: 200_000,
            },
        );
        cfg.models.insert(
            "backup".into(),
            ModelDefaults {
                model: "backup".into(),
                max_tokens_per_response: 4096,
                context_window_tokens: 128_000,
            },
        );
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());
        let chain = cfg.resolve_chain(AgentRole::Agent, None).unwrap();
        assert_eq!(chain.primary.model, "primary");
        assert_eq!(chain.primary.max_tokens_per_response, 8192);
        assert_eq!(chain.primary.context_window_tokens, 200_000);
        assert_eq!(chain.fallbacks.len(), 1);
        assert_eq!(chain.fallbacks[0].model, "backup");
        assert_eq!(chain.fallbacks[0].max_tokens_per_response, 4096);
        assert_eq!(chain.fallbacks[0].context_window_tokens, 128_000);
    }

    #[test]
    fn policy_max_tokens_overrides_registry_else_registry_wins() {
        use llm::ModelSpec;

        let mut cfg = AgentsConfig::default();
        cfg.models.insert(
            "model-a".into(),
            ModelDefaults {
                model: "model-a".into(),
                max_tokens_per_response: 4096,
                context_window_tokens: 128_000,
            },
        );
        cfg.models.insert(
            "model-b".into(),
            ModelDefaults {
                model: "model-b".into(),
                max_tokens_per_response: 8192,
                context_window_tokens: 200_000,
            },
        );
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());

        // Primary specifies an override, fallback omits it.
        let policy = LlmModelChain::new(ModelSpec::new("model-a").with_max_tokens(2048))
            .with_fallback(ModelSpec::new("model-b"));
        let chain = ModelChain::from_policy(&policy, &cfg.models).unwrap();

        assert_eq!(chain.primary.max_tokens_per_response, 2048);
        assert_eq!(chain.fallbacks[0].max_tokens_per_response, 8192);
        // Context window is always from the registry; not overridable per-call.
        assert_eq!(chain.primary.context_window_tokens, 128_000);
        // `.model` is pinned to the lookup key, never the registry's `model` field.
        assert_eq!(chain.primary.model, "model-a");
        assert_eq!(chain.fallbacks[0].model, "model-b");
    }

    #[test]
    fn explicit_override_beats_role_and_default() {
        let mut cfg = AgentsConfig {
            default_chain: Some(LlmModelChain::new("default")),
            ..Default::default()
        };
        cfg.builtin_roles.insert(
            AgentRole::Agent,
            RoleConfig {
                chain: Some(LlmModelChain::new("role")),
                ..Default::default()
            },
        );
        let resolved = cfg
            .resolve_policy(AgentRole::Agent, Some(LlmModelChain::new("explicit")))
            .unwrap();
        assert_eq!(resolved.primary.name, "explicit");
    }

    #[test]
    fn role_chain_overrides_default() {
        let mut cfg = AgentsConfig {
            default_chain: Some(LlmModelChain::new("default")),
            ..Default::default()
        };
        cfg.models.insert("default".into(), defaults_for("default"));
        cfg.models.insert("role".into(), defaults_for("role"));
        cfg.builtin_roles.insert(
            AgentRole::Agent,
            RoleConfig {
                chain: Some(LlmModelChain::new("role")),
                ..Default::default()
            },
        );
        let chain = cfg.resolve_chain(AgentRole::Agent, None).unwrap();
        assert_eq!(chain.primary.model, "role");
    }

    #[test]
    fn falls_back_to_default_when_role_has_no_override() {
        let mut cfg = AgentsConfig {
            default_chain: Some(LlmModelChain::new("default")),
            ..Default::default()
        };
        cfg.models.insert("default".into(), defaults_for("default"));
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());
        let chain = cfg.resolve_chain(AgentRole::Agent, None).unwrap();
        assert_eq!(chain.primary.model, "default");
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
        let lead = cfg.resolve_chain(AgentRole::ProjectLead, None).unwrap();
        assert_eq!(lead.primary.model, "lead/special");
        let agent = cfg.resolve_chain(AgentRole::Agent, None).unwrap();
        assert_eq!(agent.primary.model, "primary/model");
    }
}
