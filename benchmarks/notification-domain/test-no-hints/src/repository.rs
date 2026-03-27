use crate::{NotificationError, NotificationEvent, NotificationId};

pub trait NotificationRepository {
    fn load_events(
        &self,
        id: &NotificationId,
    ) -> Result<Vec<NotificationEvent>, NotificationError>;

    fn persist_event(&self, event: &NotificationEvent) -> Result<(), NotificationError>;
}
