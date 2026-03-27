use es_entity::{clock::ClockHandle, *};
use sqlx::PgPool;

use super::{entity::*, primitives::*};

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Notification",
    id = "NotificationId",
    columns(recipient(ty = "String", list_by)),
    tbl = "notifications",
    events_tbl = "notification_events"
)]
pub struct NotificationRepo {
    pool: PgPool,
    #[allow(dead_code)]
    clock: ClockHandle,
}

impl NotificationRepo {
    pub fn new(pool: &PgPool, clock: ClockHandle) -> Self {
        Self {
            pool: pool.clone(),
            clock,
        }
    }
}
