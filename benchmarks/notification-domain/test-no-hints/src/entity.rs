use crate::{NotificationError, NotificationEvent, NotificationId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationStatus {
    Unread,
    Read,
    Dismissed,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: NotificationId,
    pub title: String,
    pub body: String,
    pub recipient: String,
    pub status: NotificationStatus,
}

impl Notification {
    pub fn create(
        id: NotificationId,
        title: String,
        body: String,
        recipient: String,
    ) -> (Self, NotificationEvent) {
        let event = NotificationEvent::Created {
            id,
            title: title.clone(),
            body: body.clone(),
            recipient: recipient.clone(),
        };
        let entity = Self {
            id,
            title,
            body,
            recipient,
            status: NotificationStatus::Unread,
        };
        (entity, event)
    }

    pub fn mark_as_read(&mut self) -> Result<NotificationEvent, NotificationError> {
        if self.status == NotificationStatus::Read {
            return Err(NotificationError::AlreadyRead(self.id));
        }
        self.status = NotificationStatus::Read;
        Ok(NotificationEvent::MarkedAsRead { id: self.id })
    }

    pub fn dismiss(&mut self) -> Result<NotificationEvent, NotificationError> {
        if self.status == NotificationStatus::Dismissed {
            return Err(NotificationError::AlreadyDismissed(self.id));
        }
        self.status = NotificationStatus::Dismissed;
        Ok(NotificationEvent::Dismissed { id: self.id })
    }

    pub fn apply_event(&mut self, event: &NotificationEvent) {
        match event {
            NotificationEvent::Created { .. } => {}
            NotificationEvent::MarkedAsRead { .. } => {
                self.status = NotificationStatus::Read;
            }
            NotificationEvent::Dismissed { .. } => {
                self.status = NotificationStatus::Dismissed;
            }
        }
    }

    pub fn hydrate(events: &[NotificationEvent]) -> Option<Self> {
        let mut events_iter = events.iter();
        let first = events_iter.next()?;

        let mut entity = match first {
            NotificationEvent::Created {
                id,
                title,
                body,
                recipient,
            } => Self {
                id: *id,
                title: title.clone(),
                body: body.clone(),
                recipient: recipient.clone(),
                status: NotificationStatus::Unread,
            },
            _ => return None,
        };

        for event in events_iter {
            entity.apply_event(event);
        }

        Some(entity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_notification() {
        let id = NotificationId::new();
        let (n, event) = Notification::create(
            id,
            "Test".into(),
            "Body".into(),
            "user@example.com".into(),
        );
        assert_eq!(n.status, NotificationStatus::Unread);
        assert_eq!(n.id, id);
        assert_eq!(event.id(), id);
    }

    #[test]
    fn mark_as_read() {
        let id = NotificationId::new();
        let (mut n, _) =
            Notification::create(id, "T".into(), "B".into(), "u@e.com".into());
        let event = n.mark_as_read().unwrap();
        assert_eq!(n.status, NotificationStatus::Read);
        assert_eq!(event.id(), id);
    }

    #[test]
    fn mark_as_read_twice_errors() {
        let id = NotificationId::new();
        let (mut n, _) =
            Notification::create(id, "T".into(), "B".into(), "u@e.com".into());
        n.mark_as_read().unwrap();
        assert!(n.mark_as_read().is_err());
    }

    #[test]
    fn dismiss() {
        let id = NotificationId::new();
        let (mut n, _) =
            Notification::create(id, "T".into(), "B".into(), "u@e.com".into());
        let event = n.dismiss().unwrap();
        assert_eq!(n.status, NotificationStatus::Dismissed);
        assert_eq!(event.id(), id);
    }

    #[test]
    fn dismiss_twice_errors() {
        let id = NotificationId::new();
        let (mut n, _) =
            Notification::create(id, "T".into(), "B".into(), "u@e.com".into());
        n.dismiss().unwrap();
        assert!(n.dismiss().is_err());
    }

    #[test]
    fn hydrate_from_events() {
        let id = NotificationId::new();
        let events = vec![
            NotificationEvent::Created {
                id,
                title: "Hello".into(),
                body: "World".into(),
                recipient: "u@e.com".into(),
            },
            NotificationEvent::MarkedAsRead { id },
        ];
        let n = Notification::hydrate(&events).unwrap();
        assert_eq!(n.status, NotificationStatus::Read);
        assert_eq!(n.title, "Hello");
    }

    #[test]
    fn hydrate_empty_returns_none() {
        assert!(Notification::hydrate(&[]).is_none());
    }
}
