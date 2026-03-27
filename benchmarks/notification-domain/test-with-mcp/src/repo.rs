use crate::entity::Notification;
use crate::error::NotificationError;
use crate::primitives::NotificationId;

pub trait NotificationRepo {
    fn find_by_id(&self, id: NotificationId) -> Result<Notification, NotificationError>;
    fn find_by_recipient(&self, recipient: &str) -> Result<Vec<Notification>, NotificationError>;
    fn persist(&self, notification: &mut Notification) -> Result<(), NotificationError>;
}
