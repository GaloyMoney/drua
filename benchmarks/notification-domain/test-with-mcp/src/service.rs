use crate::entity::Notification;
use crate::error::NotificationError;
use crate::primitives::NotificationId;
use crate::repo::NotificationRepo;

pub struct NotificationService<R: NotificationRepo> {
    repo: R,
}

impl<R: NotificationRepo> NotificationService<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    pub fn create(
        &self,
        title: String,
        body: String,
        recipient: String,
    ) -> Result<Notification, NotificationError> {
        let mut notification = Notification::create(title, body, recipient);
        self.repo.persist(&mut notification)?;
        Ok(notification)
    }

    pub fn mark_as_read(&self, id: NotificationId) -> Result<Notification, NotificationError> {
        let mut notification = self.repo.find_by_id(id)?;
        notification.mark_as_read()?;
        self.repo.persist(&mut notification)?;
        Ok(notification)
    }

    pub fn dismiss(&self, id: NotificationId) -> Result<Notification, NotificationError> {
        let mut notification = self.repo.find_by_id(id)?;
        notification.dismiss()?;
        self.repo.persist(&mut notification)?;
        Ok(notification)
    }

    pub fn find_by_id(&self, id: NotificationId) -> Result<Notification, NotificationError> {
        self.repo.find_by_id(id)
    }

    pub fn find_by_recipient(
        &self,
        recipient: &str,
    ) -> Result<Vec<Notification>, NotificationError> {
        self.repo.find_by_recipient(recipient)
    }
}
