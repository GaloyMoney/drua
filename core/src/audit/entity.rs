use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

use super::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "AuditEntryId")]
pub enum AuditEntryEvent {
    Initialized {
        id: AuditEntryId,
        subject: AuditSubject,
        interaction_type: InteractionType,
        action: String,
        metadata: serde_json::Value,
        outcome: InteractionOutcome,
        #[serde(default)]
        duration_ms: Option<u64>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct AuditEntry {
    pub id: AuditEntryId,
    pub subject: AuditSubject,
    pub interaction_type: InteractionType,
    pub action: String,
    pub metadata: serde_json::Value,
    pub outcome: InteractionOutcome,
    #[builder(setter(strip_option), default)]
    pub duration_ms: Option<u64>,
    events: EntityEvents<AuditEntryEvent>,
}

impl AuditEntry {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }
}

impl core::fmt::Display for AuditEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AuditEntry: {}, action: {}, subject: {}",
            self.id, self.action, self.subject,
        )
    }
}

impl TryFromEvents<AuditEntryEvent> for AuditEntry {
    fn try_from_events(
        events: EntityEvents<AuditEntryEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = AuditEntryBuilder::default();

        for event in events.iter_all() {
            match event {
                AuditEntryEvent::Initialized {
                    id,
                    subject,
                    interaction_type,
                    action,
                    metadata,
                    outcome,
                    duration_ms,
                } => {
                    builder = builder
                        .id(*id)
                        .subject(subject.clone())
                        .interaction_type(interaction_type.clone())
                        .action(action.clone())
                        .metadata(metadata.clone())
                        .outcome(outcome.clone());
                    if let Some(ms) = duration_ms {
                        builder = builder.duration_ms(*ms);
                    }
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewAuditEntry {
    #[builder(setter(into))]
    pub(super) id: AuditEntryId,
    pub(super) subject: AuditSubject,
    pub(super) interaction_type: InteractionType,
    #[builder(setter(into))]
    pub(super) action: String,
    #[builder(default = "serde_json::Value::Null")]
    pub(super) metadata: serde_json::Value,
    pub(super) outcome: InteractionOutcome,
    #[builder(setter(strip_option), default)]
    pub(super) duration_ms: Option<u64>,
}

impl NewAuditEntry {
    pub fn builder() -> NewAuditEntryBuilder {
        let mut builder = NewAuditEntryBuilder::default();
        builder.id(AuditEntryId::new());
        builder
    }

    pub(super) fn subject_string(&self) -> String {
        self.subject.to_string()
    }

    pub(super) fn interaction_type_string(&self) -> String {
        self.interaction_type.to_string()
    }
}

impl IntoEvents<AuditEntryEvent> for NewAuditEntry {
    fn into_events(self) -> EntityEvents<AuditEntryEvent> {
        EntityEvents::init(
            self.id,
            [AuditEntryEvent::Initialized {
                id: self.id,
                subject: self.subject,
                interaction_type: self.interaction_type,
                action: self.action,
                metadata: self.metadata,
                outcome: self.outcome,
                duration_ms: self.duration_ms,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use crate::primitives::{AuditEntryId, UserId};

    use super::*;

    fn new_audit_entry() -> AuditEntry {
        let new_entry = NewAuditEntry::builder()
            .id(AuditEntryId::new())
            .subject(AuditSubject::User {
                user_id: UserId::new(),
            })
            .interaction_type(InteractionType::ApiCall)
            .action("POST /agents/create")
            .metadata(serde_json::json!({"method": "POST", "path": "/agents/create"}))
            .outcome(InteractionOutcome::Success)
            .duration_ms(42u64)
            .build()
            .unwrap();

        AuditEntry::try_from_events(new_entry.into_events()).unwrap()
    }

    #[test]
    fn audit_entry_hydration() {
        let entry = new_audit_entry();
        assert_eq!(entry.action, "POST /agents/create");
        assert!(matches!(entry.interaction_type, InteractionType::ApiCall));
        assert!(matches!(entry.outcome, InteractionOutcome::Success));
        assert_eq!(entry.duration_ms, Some(42));
    }

    #[test]
    fn audit_entry_hydration_without_duration() {
        let new_entry = NewAuditEntry::builder()
            .subject(AuditSubject::Anonymous)
            .interaction_type(InteractionType::McpCall)
            .action("search_tools")
            .outcome(InteractionOutcome::Error {
                message: "not found".to_string(),
            })
            .build()
            .unwrap();

        let entry = AuditEntry::try_from_events(new_entry.into_events()).unwrap();
        assert_eq!(entry.action, "search_tools");
        assert!(matches!(entry.interaction_type, InteractionType::McpCall));
        assert_eq!(entry.duration_ms, None);
    }
}
