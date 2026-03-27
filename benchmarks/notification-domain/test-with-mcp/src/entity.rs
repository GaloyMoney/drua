use chrono::{DateTime, Utc};

use crate::error::NotificationError;
use crate::event::NotificationEvent;
use crate::primitives::NotificationId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationStatus {
    Unread,
    Read,
    Dismissed,
}

#[derive(Debug)]
pub struct Notification {
    pub id: NotificationId,
    pub title: String,
    pub body: String,
    pub recipient: String,
    pub status: NotificationStatus,
    pub created_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
    pub dismissed_at: Option<DateTime<Utc>>,
    events: Vec<NotificationEvent>,
}

impl Notification {
    pub fn create(title: String, body: String, recipient: String) -> Self {
        let id = NotificationId::new();
        let created_at = Utc::now();

        let event = NotificationEvent::Initialized {
            id,
            title: title.clone(),
            body: body.clone(),
            recipient: recipient.clone(),
            created_at,
        };

        Self {
            id,
            title,
            body,
            recipient,
            status: NotificationStatus::Unread,
            created_at,
            read_at: None,
            dismissed_at: None,
            events: vec![event],
        }
    }

    pub fn mark_as_read(&mut self) -> Result<(), NotificationError> {
        if self.status == NotificationStatus::Read {
            return Err(NotificationError::AlreadyRead);
        }
        if self.status == NotificationStatus::Dismissed {
            return Err(NotificationError::AlreadyDismissed);
        }

        let read_at = Utc::now();
        self.events
            .push(NotificationEvent::MarkedAsRead { read_at });
        self.status = NotificationStatus::Read;
        self.read_at = Some(read_at);
        Ok(())
    }

    pub fn dismiss(&mut self) -> Result<(), NotificationError> {
        if self.status == NotificationStatus::Dismissed {
            return Err(NotificationError::AlreadyDismissed);
        }

        let dismissed_at = Utc::now();
        self.events
            .push(NotificationEvent::Dismissed { dismissed_at });
        self.status = NotificationStatus::Dismissed;
        self.dismissed_at = Some(dismissed_at);
        Ok(())
    }

    pub fn pending_events(&self) -> &[NotificationEvent] {
        &self.events
    }

    pub fn clear_events(&mut self) {
        self.events.clear();
    }

    /// Rebuild entity state from a sequence of persisted events.
    pub fn try_from_events(
        events: impl IntoIterator<Item = NotificationEvent>,
    ) -> Result<Self, NotificationError> {
        let mut id = None;
        let mut title = None;
        let mut body = None;
        let mut recipient = None;
        let mut status = NotificationStatus::Unread;
        let mut created_at = None;
        let mut read_at = None;
        let mut dismissed_at = None;

        for event in events {
            match event {
                NotificationEvent::Initialized {
                    id: eid,
                    title: t,
                    body: b,
                    recipient: r,
                    created_at: c,
                } => {
                    id = Some(eid);
                    title = Some(t);
                    body = Some(b);
                    recipient = Some(r);
                    created_at = Some(c);
                }
                NotificationEvent::MarkedAsRead { read_at: r } => {
                    status = NotificationStatus::Read;
                    read_at = Some(r);
                }
                NotificationEvent::Dismissed { dismissed_at: d } => {
                    status = NotificationStatus::Dismissed;
                    dismissed_at = Some(d);
                }
            }
        }

        let id = id.ok_or_else(|| {
            NotificationError::Hydration("missing Initialized event".to_string())
        })?;

        Ok(Self {
            id,
            title: title.unwrap(),
            body: body.unwrap(),
            recipient: recipient.unwrap(),
            status,
            created_at: created_at.unwrap(),
            read_at,
            dismissed_at,
            events: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_notification() {
        let n = Notification::create(
            "Alert".into(),
            "Something happened".into(),
            "user@example.com".into(),
        );
        assert_eq!(n.status, NotificationStatus::Unread);
        assert_eq!(n.title, "Alert");
        assert_eq!(n.pending_events().len(), 1);
    }

    #[test]
    fn mark_as_read() {
        let mut n = Notification::create("T".into(), "B".into(), "r".into());
        n.mark_as_read().unwrap();
        assert_eq!(n.status, NotificationStatus::Read);
        assert!(n.read_at.is_some());
    }

    #[test]
    fn mark_as_read_twice_fails() {
        let mut n = Notification::create("T".into(), "B".into(), "r".into());
        n.mark_as_read().unwrap();
        assert!(n.mark_as_read().is_err());
    }

    #[test]
    fn dismiss() {
        let mut n = Notification::create("T".into(), "B".into(), "r".into());
        n.dismiss().unwrap();
        assert_eq!(n.status, NotificationStatus::Dismissed);
        assert!(n.dismissed_at.is_some());
    }

    #[test]
    fn dismiss_twice_fails() {
        let mut n = Notification::create("T".into(), "B".into(), "r".into());
        n.dismiss().unwrap();
        assert!(n.dismiss().is_err());
    }

    #[test]
    fn dismiss_after_read() {
        let mut n = Notification::create("T".into(), "B".into(), "r".into());
        n.mark_as_read().unwrap();
        n.dismiss().unwrap();
        assert_eq!(n.status, NotificationStatus::Dismissed);
    }

    #[test]
    fn read_after_dismiss_fails() {
        let mut n = Notification::create("T".into(), "B".into(), "r".into());
        n.dismiss().unwrap();
        assert!(n.mark_as_read().is_err());
    }

    #[test]
    fn hydrate_from_events() {
        let mut n = Notification::create("T".into(), "B".into(), "r".into());
        n.mark_as_read().unwrap();

        let events: Vec<_> = n.pending_events().to_vec();
        let hydrated = Notification::try_from_events(events).unwrap();
        assert_eq!(hydrated.id, n.id);
        assert_eq!(hydrated.status, NotificationStatus::Read);
    }

    #[test]
    fn hydrate_empty_events_fails() {
        let result = Notification::try_from_events(vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn events_roundtrip_via_serde() {
        let n = Notification::create("T".into(), "B".into(), "r".into());
        let json = serde_json::to_string(n.pending_events()).unwrap();
        let deserialized: Vec<NotificationEvent> = serde_json::from_str(&json).unwrap();
        let hydrated = Notification::try_from_events(deserialized).unwrap();
        assert_eq!(hydrated.id, n.id);
    }
}
