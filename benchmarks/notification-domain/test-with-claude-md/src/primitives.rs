use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NotificationId(Uuid);

impl NotificationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for NotificationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NotificationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<Uuid> for NotificationId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

impl From<NotificationId> for Uuid {
    fn from(id: NotificationId) -> Self {
        id.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationStatus {
    Unread,
    Read,
    Dismissed,
}

pub struct NewNotification {
    pub(crate) id: NotificationId,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) recipient: String,
}

impl NewNotification {
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        recipient: impl Into<String>,
    ) -> Self {
        Self {
            id: NotificationId::new(),
            title: title.into(),
            body: body.into(),
            recipient: recipient.into(),
        }
    }

    pub fn id(&self) -> NotificationId {
        self.id
    }
}
