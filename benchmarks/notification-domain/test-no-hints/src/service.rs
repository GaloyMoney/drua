use crate::{
    Notification, NotificationError, NotificationId, NotificationRepository,
};

pub struct NotificationService<R: NotificationRepository> {
    repo: R,
}

impl<R: NotificationRepository> NotificationService<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    pub fn create(
        &self,
        title: String,
        body: String,
        recipient: String,
    ) -> Result<Notification, NotificationError> {
        let id = NotificationId::new();
        let (entity, event) = Notification::create(id, title, body, recipient);
        self.repo.persist_event(&event)?;
        Ok(entity)
    }

    pub fn mark_as_read(&self, id: &NotificationId) -> Result<Notification, NotificationError> {
        let mut notification = self.load(id)?;
        let event = notification.mark_as_read()?;
        self.repo.persist_event(&event)?;
        Ok(notification)
    }

    pub fn dismiss(&self, id: &NotificationId) -> Result<Notification, NotificationError> {
        let mut notification = self.load(id)?;
        let event = notification.dismiss()?;
        self.repo.persist_event(&event)?;
        Ok(notification)
    }

    pub fn get(&self, id: &NotificationId) -> Result<Notification, NotificationError> {
        self.load(id)
    }

    fn load(&self, id: &NotificationId) -> Result<Notification, NotificationError> {
        let events = self.repo.load_events(id)?;
        Notification::hydrate(&events).ok_or(NotificationError::NotFound(*id))
    }
}
