use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::ProjectId;

use super::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "GitProxyAllowlistEntryId")]
pub enum GitProxyAllowlistEntryEvent {
    Initialized {
        id: GitProxyAllowlistEntryId,
        project_id: ProjectId,
        owner: String,
        repo: String,
        allowed_ref_patterns: Vec<String>,
        modes: Vec<GitProxyMode>,
    },
    RefPatternsUpdated {
        allowed_ref_patterns: Vec<String>,
    },
    ModesUpdated {
        modes: Vec<GitProxyMode>,
    },
    Revoked {
        revoked_at: DateTime<Utc>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct GitProxyAllowlistEntry {
    pub id: GitProxyAllowlistEntryId,
    pub project_id: ProjectId,
    pub owner: String,
    pub repo: String,
    #[builder(default)]
    pub allowed_ref_patterns: Vec<String>,
    #[builder(default)]
    pub modes: Vec<GitProxyMode>,
    #[builder(setter(strip_option), default)]
    pub revoked_at: Option<DateTime<Utc>>,
    events: EntityEvents<GitProxyAllowlistEntryEvent>,
}

impl GitProxyAllowlistEntry {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    pub fn allows(&self, ref_name: &str, mode: GitProxyMode) -> bool {
        if self.is_revoked() {
            return false;
        }
        if !self.modes.contains(&mode) {
            return false;
        }
        super::policy::RefPatternSet::new(&self.allowed_ref_patterns)
            .map(|set| set.matches(ref_name))
            .unwrap_or(false)
    }

    pub(super) fn update_ref_patterns(&mut self, patterns: Vec<String>) -> Idempotent<()> {
        if self.allowed_ref_patterns == patterns {
            return Idempotent::AlreadyApplied;
        }
        self.allowed_ref_patterns = patterns.clone();
        self.events
            .push(GitProxyAllowlistEntryEvent::RefPatternsUpdated {
                allowed_ref_patterns: patterns,
            });
        Idempotent::Executed(())
    }

    pub(super) fn update_modes(&mut self, modes: Vec<GitProxyMode>) -> Idempotent<()> {
        if self.modes == modes {
            return Idempotent::AlreadyApplied;
        }
        self.modes = modes.clone();
        self.events
            .push(GitProxyAllowlistEntryEvent::ModesUpdated { modes });
        Idempotent::Executed(())
    }

    pub(super) fn revoke(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all(),
            already_applied: GitProxyAllowlistEntryEvent::Revoked { .. }
        );
        let revoked_at = Utc::now();
        self.revoked_at = Some(revoked_at);
        self.events
            .push(GitProxyAllowlistEntryEvent::Revoked { revoked_at });
        Idempotent::Executed(())
    }
}

impl TryFromEvents<GitProxyAllowlistEntryEvent> for GitProxyAllowlistEntry {
    fn try_from_events(
        events: EntityEvents<GitProxyAllowlistEntryEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = GitProxyAllowlistEntryBuilder::default();
        for event in events.iter_all() {
            match event {
                GitProxyAllowlistEntryEvent::Initialized {
                    id,
                    project_id,
                    owner,
                    repo,
                    allowed_ref_patterns,
                    modes,
                } => {
                    builder = builder
                        .id(*id)
                        .project_id(*project_id)
                        .owner(owner.clone())
                        .repo(repo.clone())
                        .allowed_ref_patterns(allowed_ref_patterns.clone())
                        .modes(modes.clone());
                }
                GitProxyAllowlistEntryEvent::RefPatternsUpdated {
                    allowed_ref_patterns,
                } => {
                    builder = builder.allowed_ref_patterns(allowed_ref_patterns.clone());
                }
                GitProxyAllowlistEntryEvent::ModesUpdated { modes } => {
                    builder = builder.modes(modes.clone());
                }
                GitProxyAllowlistEntryEvent::Revoked { revoked_at } => {
                    builder = builder.revoked_at(*revoked_at);
                }
            }
        }
        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewGitProxyAllowlistEntry {
    #[builder(setter(into))]
    pub(super) id: GitProxyAllowlistEntryId,
    #[builder(setter(into))]
    pub(super) project_id: ProjectId,
    #[builder(setter(into))]
    pub(super) owner: String,
    #[builder(setter(into))]
    pub(super) repo: String,
    #[builder(default)]
    pub(super) allowed_ref_patterns: Vec<String>,
    #[builder(default)]
    pub(super) modes: Vec<GitProxyMode>,
}

impl NewGitProxyAllowlistEntry {
    pub fn builder() -> NewGitProxyAllowlistEntryBuilder {
        let mut builder = NewGitProxyAllowlistEntryBuilder::default();
        builder.id(GitProxyAllowlistEntryId::new());
        builder
    }
}

impl IntoEvents<GitProxyAllowlistEntryEvent> for NewGitProxyAllowlistEntry {
    fn into_events(self) -> EntityEvents<GitProxyAllowlistEntryEvent> {
        EntityEvents::init(
            self.id,
            [GitProxyAllowlistEntryEvent::Initialized {
                id: self.id,
                project_id: self.project_id,
                owner: self.owner,
                repo: self.repo,
                allowed_ref_patterns: self.allowed_ref_patterns,
                modes: self.modes,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::*;

    fn entry_with(modes: Vec<GitProxyMode>, patterns: Vec<&str>) -> GitProxyAllowlistEntry {
        let new = NewGitProxyAllowlistEntry::builder()
            .project_id(ProjectId::new())
            .owner("GaloyMoney".to_string())
            .repo("drua".to_string())
            .allowed_ref_patterns(patterns.into_iter().map(String::from).collect())
            .modes(modes)
            .build()
            .unwrap();
        GitProxyAllowlistEntry::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn allows_pull_when_mode_and_pattern_match() {
        let e = entry_with(vec![GitProxyMode::Pull], vec!["refs/heads/main"]);
        assert!(e.allows("refs/heads/main", GitProxyMode::Pull));
    }

    #[test]
    fn denies_when_mode_missing() {
        let e = entry_with(vec![GitProxyMode::Pull], vec!["refs/heads/bot/*"]);
        assert!(!e.allows("refs/heads/bot/foo", GitProxyMode::Push));
    }

    #[test]
    fn denies_when_pattern_does_not_match() {
        let e = entry_with(vec![GitProxyMode::Push], vec!["refs/heads/bot/*"]);
        assert!(!e.allows("refs/heads/main", GitProxyMode::Push));
    }

    #[test]
    fn empty_patterns_denies_all() {
        let e = entry_with(vec![GitProxyMode::Push], vec![]);
        assert!(!e.allows("refs/heads/bot/foo", GitProxyMode::Push));
    }

    #[test]
    fn revoked_entry_denies_all() {
        let mut e = entry_with(vec![GitProxyMode::Pull], vec!["refs/heads/main"]);
        let _ = e.revoke();
        assert!(!e.allows("refs/heads/main", GitProxyMode::Pull));
    }

    #[test]
    fn update_modes_is_idempotent() {
        let mut e = entry_with(vec![GitProxyMode::Pull], vec!["refs/heads/main"]);
        let res = e.update_modes(vec![GitProxyMode::Pull]);
        assert!(matches!(res, Idempotent::AlreadyApplied));
        let res = e.update_modes(vec![GitProxyMode::Pull, GitProxyMode::Push]);
        assert!(matches!(res, Idempotent::Executed(())));
    }

    #[test]
    fn update_ref_patterns_is_idempotent() {
        let mut e = entry_with(vec![GitProxyMode::Pull], vec!["refs/heads/main"]);
        let res = e.update_ref_patterns(vec!["refs/heads/main".to_string()]);
        assert!(matches!(res, Idempotent::AlreadyApplied));
        let res = e.update_ref_patterns(vec!["refs/heads/bot/*".to_string()]);
        assert!(matches!(res, Idempotent::Executed(())));
    }

    #[test]
    fn revoke_is_idempotent() {
        let mut e = entry_with(vec![GitProxyMode::Pull], vec!["refs/heads/main"]);
        assert!(matches!(e.revoke(), Idempotent::Executed(())));
        assert!(matches!(e.revoke(), Idempotent::AlreadyApplied));
    }
}
