mod entity;
mod error;
mod event;
mod primitives;
mod repo;

pub use entity::*;
pub use error::*;
pub use event::*;
pub use primitives::*;
pub use repo::*;

pub struct Notifications {
    repo: NotificationRepo,
}

impl Notifications {
    pub fn new() -> Self {
        Self {
            repo: NotificationRepo::new(),
        }
    }

    pub fn create(
        &mut self,
        title: impl Into<String>,
        body: impl Into<String>,
        recipient: impl Into<String>,
    ) -> Result<Notification, NotificationError> {
        let new = NewNotification::new(title, body, recipient);
        self.repo.create(new)
    }

    pub fn mark_as_read(&mut self, id: NotificationId) -> Result<Notification, NotificationError> {
        let mut notification = self.repo.find_by_id(id)?;
        notification.mark_as_read()?;
        self.repo.update(&mut notification)?;
        Ok(notification)
    }

    pub fn dismiss(&mut self, id: NotificationId) -> Result<Notification, NotificationError> {
        let mut notification = self.repo.find_by_id(id)?;
        notification.dismiss()?;
        self.repo.update(&mut notification)?;
        Ok(notification)
    }

    pub fn find_by_id(&self, id: NotificationId) -> Result<Notification, NotificationError> {
        self.repo.find_by_id(id)
    }

    pub fn list_all(&self) -> Result<Vec<Notification>, NotificationError> {
        self.repo.list_all()
    }

    pub fn list_for_recipient(
        &self,
        recipient: &str,
    ) -> Result<Vec<Notification>, NotificationError> {
        self.repo.find_by_recipient(recipient)
    }
}

impl Default for Notifications {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_notification() {
        let mut svc = Notifications::new();
        let n = svc
            .create("Alert", "Server is down", "ops@example.com")
            .unwrap();
        assert_eq!(n.title, "Alert");
        assert_eq!(n.body, "Server is down");
        assert_eq!(n.recipient, "ops@example.com");
        assert_eq!(n.status, NotificationStatus::Unread);
    }

    #[test]
    fn mark_as_read() {
        let mut svc = Notifications::new();
        let n = svc
            .create("Alert", "Server is down", "ops@example.com")
            .unwrap();
        let n = svc.mark_as_read(n.id).unwrap();
        assert_eq!(n.status, NotificationStatus::Read);
    }

    #[test]
    fn dismiss_notification() {
        let mut svc = Notifications::new();
        let n = svc
            .create("Alert", "Server is down", "ops@example.com")
            .unwrap();
        let n = svc.dismiss(n.id).unwrap();
        assert_eq!(n.status, NotificationStatus::Dismissed);
    }

    #[test]
    fn cannot_read_after_dismiss() {
        let mut svc = Notifications::new();
        let n = svc.create("Alert", "body", "user@example.com").unwrap();
        svc.dismiss(n.id).unwrap();
        let err = svc.mark_as_read(n.id).unwrap_err();
        assert!(matches!(err, NotificationError::AlreadyDismissed));
    }

    #[test]
    fn dismiss_is_idempotent() {
        let mut svc = Notifications::new();
        let n = svc.create("Alert", "body", "user@example.com").unwrap();
        svc.dismiss(n.id).unwrap();
        svc.dismiss(n.id).unwrap();
    }

    #[test]
    fn mark_as_read_is_idempotent() {
        let mut svc = Notifications::new();
        let n = svc.create("Alert", "body", "user@example.com").unwrap();
        svc.mark_as_read(n.id).unwrap();
        svc.mark_as_read(n.id).unwrap();
    }

    #[test]
    fn hydration_rebuilds_state() {
        let mut svc = Notifications::new();
        let n = svc.create("Title", "Body", "user@example.com").unwrap();
        let id = n.id;
        svc.mark_as_read(id).unwrap();

        let reloaded = svc.find_by_id(id).unwrap();
        assert_eq!(reloaded.status, NotificationStatus::Read);
        assert_eq!(reloaded.title, "Title");
    }

    #[test]
    fn list_for_recipient() {
        let mut svc = Notifications::new();
        svc.create("A", "body", "alice@example.com").unwrap();
        svc.create("B", "body", "bob@example.com").unwrap();
        svc.create("C", "body", "alice@example.com").unwrap();

        let alice = svc.list_for_recipient("alice@example.com").unwrap();
        assert_eq!(alice.len(), 2);
    }

    #[test]
    fn not_found() {
        let svc = Notifications::new();
        let err = svc.find_by_id(NotificationId::new()).unwrap_err();
        assert!(matches!(err, NotificationError::NotFound));
    }

    #[test]
    fn events_serialize_tagged() {
        let event = NotificationEvent::Initialized {
            id: NotificationId::new(),
            title: "Test".into(),
            body: "Body".into(),
            recipient: "user@example.com".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "initialized");

        let json = serde_json::to_value(&NotificationEvent::MarkedAsRead).unwrap();
        assert_eq!(json["type"], "marked_as_read");

        let json = serde_json::to_value(&NotificationEvent::Dismissed).unwrap();
        assert_eq!(json["type"], "dismissed");
    }
}
