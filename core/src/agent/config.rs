use std::collections::HashMap;

use llm::{ModelChain as LlmModelChain, ReasoningEffort};
use serde::{Deserialize, Deserializer, Serialize};

use super::error::AgentError;
use super::session::{BreakerConfig, CompactionConfig};
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
    #[serde(default)]
    pub breaker: BreakerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDefaults {
    pub model: String,
    pub max_tokens_per_response: u32,
    pub context_window_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
}

impl Default for ModelDefaults {
    fn default() -> Self {
        Self {
            model: String::new(),
            max_tokens_per_response: 4096,
            context_window_tokens: 200_000,
            effort: None,
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
        let primary = resolve_entry(&policy.primary, models)?;
        let fallbacks = policy
            .fallbacks
            .iter()
            .map(|spec| resolve_entry(spec, models))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { primary, fallbacks })
    }
}

fn resolve_entry(
    spec: &llm::ModelSpec,
    models: &HashMap<String, ModelDefaults>,
) -> Result<ModelDefaults, AgentError> {
    let mut defaults = models
        .get(spec.name.as_str())
        .cloned()
        .ok_or_else(|| AgentError::ModelNotConfigured(spec.name.clone()))?;
    if let Some(mt) = spec.max_tokens {
        defaults.max_tokens_per_response = mt;
    }
    if let Some(effort) = spec.effort {
        defaults.effort = Some(effort);
    }
    Ok(defaults)
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
            effort: None,
        }
    }

    #[test]
    fn model_defaults_effort_serde_backward_compatible() {
        let no_effort_key = serde_json::json!({
            "model": "primary",
            "max_tokens_per_response": 4096,
            "context_window_tokens": 100_000,
        });
        let parsed: ModelDefaults = serde_json::from_value(no_effort_key).unwrap();
        assert_eq!(parsed.effort, None);

        let with_medium = serde_json::json!({
            "model": "primary",
            "max_tokens_per_response": 4096,
            "context_window_tokens": 100_000,
            "effort": "medium",
        });
        let parsed: ModelDefaults = serde_json::from_value(with_medium).unwrap();
        assert_eq!(parsed.effort, Some(ReasoningEffort::Medium));

        let json = serde_json::to_value(defaults_for("primary")).unwrap();
        assert!(json.get("effort").is_none());
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
                effort: None,
            },
        );
        cfg.models.insert(
            "backup".into(),
            ModelDefaults {
                model: "backup".into(),
                max_tokens_per_response: 4096,
                context_window_tokens: 128_000,
                effort: None,
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
    fn spec_max_tokens_overrides_registry() {
        let mut cfg = AgentsConfig {
            default_chain: Some(
                LlmModelChain::new(llm::ModelSpec::new("primary").with_max_tokens(2048))
                    .with_fallback(llm::ModelSpec::new("backup").with_max_tokens(1024))
                    .with_fallback("backup"),
            ),
            ..Default::default()
        };
        cfg.models.insert(
            "primary".into(),
            ModelDefaults {
                model: "primary".into(),
                max_tokens_per_response: 8192,
                context_window_tokens: 200_000,
                effort: None,
            },
        );
        cfg.models.insert(
            "backup".into(),
            ModelDefaults {
                model: "backup".into(),
                max_tokens_per_response: 4096,
                context_window_tokens: 128_000,
                effort: None,
            },
        );
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());
        let chain = cfg.resolve_chain(AgentRole::Agent, None).unwrap();
        assert_eq!(chain.primary.max_tokens_per_response, 2048);
        assert_eq!(chain.fallbacks[0].max_tokens_per_response, 1024);
        assert_eq!(chain.fallbacks[1].max_tokens_per_response, 4096);
    }

    #[test]
    fn spec_effort_overrides_registry() {
        let mut cfg = AgentsConfig {
            default_chain: Some(LlmModelChain::new(
                llm::ModelSpec::new("primary").with_effort(ReasoningEffort::High),
            )),
            ..Default::default()
        };
        cfg.models.insert("primary".into(), defaults_for("primary"));
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());
        let chain = cfg.resolve_chain(AgentRole::Agent, None).unwrap();
        assert_eq!(chain.primary.effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn spec_without_effort_keeps_registry_effort() {
        let mut cfg = AgentsConfig {
            default_chain: Some(LlmModelChain::new("primary")),
            ..Default::default()
        };
        cfg.models.insert(
            "primary".into(),
            ModelDefaults {
                model: "primary".into(),
                max_tokens_per_response: 4096,
                context_window_tokens: 100_000,
                effort: Some(ReasoningEffort::Medium),
            },
        );
        cfg.builtin_roles
            .insert(AgentRole::Agent, RoleConfig::default());
        let chain = cfg.resolve_chain(AgentRole::Agent, None).unwrap();
        assert_eq!(chain.primary.effort, Some(ReasoningEffort::Medium));
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

    #[test]
    fn role_config_without_breaker_block_deserialises_to_defaults() {
        let yaml = r#"
chain:
  primary: { name: "some/model" }
"#;
        let role: RoleConfig = serde_yaml::from_str(yaml).unwrap();
        let defaults = BreakerConfig::default();
        assert_eq!(role.breaker.enabled, defaults.enabled);
        assert_eq!(
            role.breaker.consecutive_error_turns,
            defaults.consecutive_error_turns
        );
        assert_eq!(
            role.breaker.identical_failing_calls,
            defaults.identical_failing_calls
        );
        assert_eq!(
            role.breaker.consecutive_max_tokens,
            defaults.consecutive_max_tokens
        );
        assert_eq!(
            role.breaker.max_turns_per_prompt,
            defaults.max_turns_per_prompt
        );
    }

    #[test]
    fn role_config_breaker_disabled_round_trips() {
        let yaml = r#"
chain:
  primary: { name: "some/model" }
breaker:
  enabled: false
  consecutive_error_turns: 7
"#;
        let role: RoleConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!role.breaker.enabled);
        assert_eq!(role.breaker.consecutive_error_turns, 7);
        // Fields left unset in the YAML still fall back to defaults.
        assert_eq!(
            role.breaker.identical_failing_calls,
            BreakerConfig::default().identical_failing_calls
        );

        let reserialized = serde_yaml::to_string(&role).unwrap();
        let round_tripped: RoleConfig = serde_yaml::from_str(&reserialized).unwrap();
        assert!(!round_tripped.breaker.enabled);
        assert_eq!(round_tripped.breaker.consecutive_error_turns, 7);
    }
}
