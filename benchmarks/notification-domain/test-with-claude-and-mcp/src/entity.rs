use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use super::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "NotificationId")]
pub enum NotificationEvent {
    Initialized {
        id: NotificationId,
        title: String,
        body: String,
        recipient: String,
    },
    MarkedAsRead,
    Dismissed,
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Notification {
    pub id: NotificationId,
    pub title: String,
    pub body: String,
    pub recipient: String,
    pub status: NotificationStatus,
    events: EntityEvents<NotificationEvent>,
}

impl Notification {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("Notification has never been persisted")
    }

    pub fn mark_as_read(&mut self) -> Result<Idempotent<()>, NotificationCommandError> {
        match self.status {
            NotificationStatus::Dismissed => {
                return Err(NotificationCommandError::AlreadyDismissed)
            }
            NotificationStatus::Read => return Ok(Idempotent::AlreadyApplied),
            NotificationStatus::Unread => {}
        }

        self.events.push(NotificationEvent::MarkedAsRead);
        self.status = NotificationStatus::Read;
        Ok(Idempotent::Executed(()))
    }

    pub fn dismiss(&mut self) -> Idempotent<()> {
        if self.status == NotificationStatus::Dismissed {
            return Idempotent::AlreadyApplied;
        }

        self.events.push(NotificationEvent::Dismissed);
        self.status = NotificationStatus::Dismissed;
        Idempotent::Executed(())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum NotificationCommandError {
    #[error("NotificationCommandError - AlreadyDismissed")]
    AlreadyDismissed,
}

impl TryFromEvents<NotificationEvent> for Notification {
    fn try_from_events(
        events: EntityEvents<NotificationEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = NotificationBuilder::default();

        for event in events.iter_all() {
            match event {
                NotificationEvent::Initialized {
                    id,
                    title,
                    body,
                    recipient,
                } => {
                    builder = builder
                        .id(*id)
                        .title(title.clone())
                        .body(body.clone())
                        .recipient(recipient.clone())
                        .status(NotificationStatus::Unread);
                }
                NotificationEvent::MarkedAsRead => {
                    builder = builder.status(NotificationStatus::Read);
                }
                NotificationEvent::Dismissed => {
                    builder = builder.status(NotificationStatus::Dismissed);
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewNotification {
    #[builder(setter(into))]
    pub(super) id: NotificationId,
    #[builder(setter(into))]
    pub(super) title: String,
    #[builder(setter(into))]
    pub(super) body: String,
    #[builder(setter(into))]
    pub(super) recipient: String,
}

impl NewNotification {
    pub fn builder() -> NewNotificationBuilder {
        NewNotificationBuilder::default()
    }
}

impl IntoEvents<NotificationEvent> for NewNotification {
    fn into_events(self) -> EntityEvents<NotificationEvent> {
        EntityEvents::init(
            self.id,
            [NotificationEvent::Initialized {
                id: self.id,
                title: self.title,
                body: self.body,
                recipient: self.recipient,
            }],
        )
    }
}
