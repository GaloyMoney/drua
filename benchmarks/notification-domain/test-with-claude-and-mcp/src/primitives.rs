use serde::{Deserialize, Serialize};

es_entity::entity_id! { NotificationId }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationStatus {
    Unread,
    Read,
    Dismissed,
}

impl Default for NotificationStatus {
    fn default() -> Self {
        Self::Unread
    }
}
