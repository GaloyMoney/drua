use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::error::AgentError;
use super::AgentRole;

/// Every `AgentRole` variant that must be present in
/// [`AgentsConfig::builtin_roles`] for the service to start. Add new
/// variants to the enum AND to this list — the compiler won't help, but
/// [`AgentsConfig::validate`] will fail fast at startup.
const REQUIRED_ROLES: &[AgentRole] = &[AgentRole::WorkspaceLead, AgentRole::Agent];

/// Whole-second auto-reset threshold for an `AgentSession`. Wraps a
/// `u32` count of seconds; the `#[serde(transparent)]` derive lets it
/// (de)serialize as a bare integer in YAML / JSONB instead of serde's
/// awkward `{ secs, nanos }` shape for `std::time::Duration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResetTimeDeltaSeconds(pub u32);

impl ResetTimeDeltaSeconds {
    pub fn as_seconds(&self) -> u32 {
        self.0
    }

    pub fn as_duration(&self) -> Duration {
        Duration::from_secs(self.0 as u64)
    }

    /// True when at least `self` seconds have elapsed between
    /// `last_user_message_at` and `now`. Negative spans (now < last)
    /// return `false` — clock skew never triggers a reset.
    pub fn should_reset(&self, last_user_message_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        now.signed_duration_since(last_user_message_at)
            .to_std()
            .map(|elapsed| elapsed > self.as_duration())
            .unwrap_or(false)
    }
}

impl From<u32> for ResetTimeDeltaSeconds {
    fn from(s: u32) -> Self {
        Self(s)
    }
}

/// Per-role defaults applied when an agent with that role is created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleConfig {
    pub model: String,
    pub system: Vec<llm::prompt::SystemBlock>,
    pub max_tokens: u32,
    /// If set, a new thread is started when a user message arrives more
    /// than this many seconds after the previous user message in the
    /// current thread. `None` disables the auto-reset.
    #[serde(default)]
    pub reset_time_delta_seconds: Option<ResetTimeDeltaSeconds>,
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
