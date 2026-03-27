use serde::{Deserialize, Serialize};

use crate::primitives::NotificationId;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationEvent {
    Initialized {
        id: NotificationId,
        title: String,
        body: String,
        recipient: String,
    },
    MarkedAsRead,
    Dismissed,
}
