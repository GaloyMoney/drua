use serde::{Deserialize, Serialize};

use crate::NotificationId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NotificationEvent {
    Created {
        id: NotificationId,
        title: String,
        body: String,
        recipient: String,
    },
    MarkedAsRead {
        id: NotificationId,
    },
    Dismissed {
        id: NotificationId,
    },
}

impl NotificationEvent {
    pub fn id(&self) -> NotificationId {
        match self {
            Self::Created { id, .. } | Self::MarkedAsRead { id } | Self::Dismissed { id } => *id,
        }
    }
}
