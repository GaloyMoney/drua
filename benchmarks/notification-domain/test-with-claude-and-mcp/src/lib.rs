mod entity;
mod error;
mod primitives;
mod repo;

use tracing::instrument;

use es_entity::{clock::ClockHandle, *};

use repo::notification_cursor::NotificationsByRecipientCursor;

pub use entity::*;
pub use error::*;
pub use primitives::*;
pub use repo::*;

#[derive(Clone)]
pub struct Notifications {
    repo: NotificationRepo,
}

impl Notifications {
    pub fn new(pool: &sqlx::PgPool, clock: ClockHandle) -> Self {
        Self {
            repo: NotificationRepo::new(pool, clock),
        }
    }

    #[instrument(name = "notification.create", skip_all)]
    pub async fn create(
        &self,
        new_notification: NewNotification,
    ) -> Result<Notification, NotificationError> {
        let mut op = self.repo.begin_op().await?;
        let notification = self.create_in_op(&mut op, new_notification).await?;
        op.commit().await?;
        Ok(notification)
    }

    #[instrument(name = "notification.create_in_op", skip_all)]
    pub async fn create_in_op(
        &self,
        op: &mut impl AtomicOperation,
        new_notification: NewNotification,
    ) -> Result<Notification, NotificationError> {
        let notification = self.repo.create_in_op(&mut *op, new_notification).await?;
        Ok(notification)
    }

    #[instrument(name = "notification.find_by_id", skip_all)]
    pub async fn find_by_id(&self, id: NotificationId) -> Result<Notification, NotificationError> {
        let notification = self.repo.find_by_id(id).await?;
        Ok(notification)
    }

    #[instrument(name = "notification.list_by_recipient", skip_all)]
    pub async fn list_by_recipient(
        &self,
        query: PaginatedQueryArgs<NotificationsByRecipientCursor>,
        direction: ListDirection,
    ) -> Result<PaginatedQueryRet<Notification, NotificationsByRecipientCursor>, NotificationError>
    {
        Ok(self.repo.list_by_recipient(query, direction).await?)
    }

    #[instrument(name = "notification.mark_as_read", skip_all)]
    pub async fn mark_as_read(
        &self,
        id: NotificationId,
    ) -> Result<Notification, NotificationError> {
        let mut op = self.repo.begin_op().await?;
        let notification = self.mark_as_read_in_op(&mut op, id).await?;
        op.commit().await?;
        Ok(notification)
    }

    #[instrument(name = "notification.mark_as_read_in_op", skip_all)]
    pub async fn mark_as_read_in_op(
        &self,
        op: &mut impl AtomicOperation,
        id: NotificationId,
    ) -> Result<Notification, NotificationError> {
        let mut notification = self.repo.find_by_id(id).await?;
        notification.mark_as_read()?;
        self.repo.update_in_op(&mut *op, &mut notification).await?;
        Ok(notification)
    }

    #[instrument(name = "notification.dismiss", skip_all)]
    pub async fn dismiss(&self, id: NotificationId) -> Result<Notification, NotificationError> {
        let mut op = self.repo.begin_op().await?;
        let notification = self.dismiss_in_op(&mut op, id).await?;
        op.commit().await?;
        Ok(notification)
    }

    #[instrument(name = "notification.dismiss_in_op", skip_all)]
    pub async fn dismiss_in_op(
        &self,
        op: &mut impl AtomicOperation,
        id: NotificationId,
    ) -> Result<Notification, NotificationError> {
        let mut notification = self.repo.find_by_id(id).await?;
        notification.dismiss();
        self.repo.update_in_op(&mut *op, &mut notification).await?;
        Ok(notification)
    }
}
