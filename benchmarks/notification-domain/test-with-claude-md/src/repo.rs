use std::collections::HashMap;

use crate::entity::{EntityEvents, Notification};
use crate::error::NotificationError;
use crate::event::NotificationEvent;
use crate::primitives::{NewNotification, NotificationId};

#[derive(Debug, Default)]
pub struct NotificationRepo {
    store: HashMap<NotificationId, Vec<NotificationEvent>>,
}

impl NotificationRepo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self, new: NewNotification) -> Result<Notification, NotificationError> {
        let id = new.id;
        let events: EntityEvents<NotificationEvent> = new.into();
        let entity = Notification::try_from_events(events)?;
        self.store
            .insert(id, entity.events().iter_all().cloned().collect());
        Ok(entity)
    }

    pub fn find_by_id(&self, id: NotificationId) -> Result<Notification, NotificationError> {
        let persisted = self.store.get(&id).ok_or(NotificationError::NotFound)?;
        let events = EntityEvents::load(persisted.clone());
        Notification::try_from_events(events)
    }

    pub fn update(&mut self, entity: &mut Notification) -> Result<(), NotificationError> {
        let stored = self
            .store
            .get_mut(&entity.id)
            .ok_or(NotificationError::NotFound)?;
        stored.extend(entity.events.new_events().iter().cloned());
        entity.events.mark_persisted();
        Ok(())
    }

    pub fn list_all(&self) -> Result<Vec<Notification>, NotificationError> {
        self.store
            .values()
            .map(|events| {
                let events = EntityEvents::load(events.clone());
                Notification::try_from_events(events)
            })
            .collect()
    }

    pub fn find_by_recipient(
        &self,
        recipient: &str,
    ) -> Result<Vec<Notification>, NotificationError> {
        self.list_all().map(|all| {
            all.into_iter()
                .filter(|n| n.recipient == recipient)
                .collect()
        })
    }
}
