use crate::error::NotificationError;
use crate::event::NotificationEvent;
use crate::primitives::{NewNotification, NotificationId, NotificationStatus};

#[derive(Debug, Clone)]
pub struct EntityEvents<E> {
    persisted: Vec<E>,
    new: Vec<E>,
}

impl<E> EntityEvents<E> {
    pub fn init(initial_events: impl IntoIterator<Item = E>) -> Self {
        Self {
            persisted: Vec::new(),
            new: initial_events.into_iter().collect(),
        }
    }

    pub fn load(persisted: Vec<E>) -> Self {
        Self {
            persisted,
            new: Vec::new(),
        }
    }

    pub fn push(&mut self, event: E) {
        self.new.push(event);
    }

    pub fn iter_all(&self) -> impl DoubleEndedIterator<Item = &E> {
        self.persisted.iter().chain(self.new.iter())
    }

    pub fn new_events(&self) -> &[E] {
        &self.new
    }

    pub fn any_new(&self) -> bool {
        !self.new.is_empty()
    }

    pub fn mark_persisted(&mut self) {
        self.persisted.append(&mut self.new);
    }
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: NotificationId,
    pub title: String,
    pub body: String,
    pub recipient: String,
    pub status: NotificationStatus,
    pub(crate) events: EntityEvents<NotificationEvent>,
}

impl Notification {
    pub fn try_from_events(
        events: EntityEvents<NotificationEvent>,
    ) -> Result<Self, NotificationError> {
        let mut id = None;
        let mut title = None;
        let mut body = None;
        let mut recipient = None;
        let mut status = NotificationStatus::Unread;

        for event in events.iter_all() {
            match event {
                NotificationEvent::Initialized {
                    id: eid,
                    title: t,
                    body: b,
                    recipient: r,
                } => {
                    id = Some(*eid);
                    title = Some(t.clone());
                    body = Some(b.clone());
                    recipient = Some(r.clone());
                }
                NotificationEvent::MarkedAsRead => {
                    status = NotificationStatus::Read;
                }
                NotificationEvent::Dismissed => {
                    status = NotificationStatus::Dismissed;
                }
            }
        }

        Ok(Self {
            id: id.ok_or(NotificationError::HydrationError("id"))?,
            title: title.ok_or(NotificationError::HydrationError("title"))?,
            body: body.ok_or(NotificationError::HydrationError("body"))?,
            recipient: recipient.ok_or(NotificationError::HydrationError("recipient"))?,
            status,
            events,
        })
    }

    pub fn mark_as_read(&mut self) -> Result<(), NotificationError> {
        if self.status == NotificationStatus::Dismissed {
            return Err(NotificationError::AlreadyDismissed);
        }
        if self.status == NotificationStatus::Read {
            return Ok(());
        }
        self.status = NotificationStatus::Read;
        self.events.push(NotificationEvent::MarkedAsRead);
        Ok(())
    }

    pub fn dismiss(&mut self) -> Result<(), NotificationError> {
        if self.status == NotificationStatus::Dismissed {
            return Ok(());
        }
        self.status = NotificationStatus::Dismissed;
        self.events.push(NotificationEvent::Dismissed);
        Ok(())
    }

    pub fn events(&self) -> &EntityEvents<NotificationEvent> {
        &self.events
    }
}

impl From<NewNotification> for EntityEvents<NotificationEvent> {
    fn from(new: NewNotification) -> Self {
        EntityEvents::init([NotificationEvent::Initialized {
            id: new.id,
            title: new.title,
            body: new.body,
            recipient: new.recipient,
        }])
    }
}
