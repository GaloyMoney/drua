use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "UserId")]
pub enum UserEvent {
    Initialized {
        id: UserId,
        github_id: String,
        email: Option<String>,
        name: Option<String>,
        #[serde(default)]
        github_username: Option<String>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct User {
    pub id: UserId,
    pub github_id: String,
    #[builder(setter(strip_option), default)]
    pub email: Option<String>,
    #[builder(setter(strip_option), default)]
    pub name: Option<String>,
    #[builder(setter(strip_option), default)]
    pub github_username: Option<String>,
    events: EntityEvents<UserEvent>,
}

impl User {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }
}

impl core::fmt::Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "User: {}, github_id: {}", self.id, self.github_id)
    }
}

impl TryFromEvents<UserEvent> for User {
    fn try_from_events(events: EntityEvents<UserEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = UserBuilder::default();

        for event in events.iter_all() {
            match event {
                UserEvent::Initialized {
                    id,
                    github_id,
                    email,
                    name,
                    github_username,
                } => {
                    builder = builder.id(*id).github_id(github_id.clone());
                    if let Some(email) = email {
                        builder = builder.email(email.clone());
                    }
                    if let Some(name) = name {
                        builder = builder.name(name.clone());
                    }
                    if let Some(github_username) = github_username {
                        builder = builder.github_username(github_username.clone());
                    }
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewUser {
    #[builder(setter(into))]
    pub(super) id: UserId,
    #[builder(setter(into))]
    pub(super) github_id: String,
    #[builder(setter(into, strip_option), default)]
    pub(super) email: Option<String>,
    #[builder(setter(into, strip_option), default)]
    pub(super) name: Option<String>,
    #[builder(setter(into, strip_option), default)]
    pub(super) github_username: Option<String>,
}

impl NewUser {
    pub fn builder() -> NewUserBuilder {
        let mut builder = NewUserBuilder::default();
        builder.id(UserId::new());
        builder
    }
}

impl IntoEvents<UserEvent> for NewUser {
    fn into_events(self) -> EntityEvents<UserEvent> {
        EntityEvents::init(
            self.id,
            [UserEvent::Initialized {
                id: self.id,
                github_id: self.github_id,
                email: self.email,
                name: self.name,
                github_username: self.github_username,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use crate::primitives::UserId;

    use super::{NewUser, User};

    fn new_user() -> User {
        let new_user = NewUser::builder()
            .id(UserId::new())
            .github_id("gh-123")
            .email("test@example.com".to_string())
            .name("Test User".to_string())
            .github_username("testuser".to_string())
            .build()
            .unwrap();

        User::try_from_events(new_user.into_events()).unwrap()
    }

    #[test]
    fn user_hydration() {
        let user = new_user();
        assert_eq!(user.github_id, "gh-123");
        assert_eq!(user.email, Some("test@example.com".to_string()));
        assert_eq!(user.name, Some("Test User".to_string()));
        assert_eq!(user.github_username, Some("testuser".to_string()));
    }

    #[test]
    fn user_hydration_without_optional_fields() {
        let new_user = NewUser::builder()
            .id(UserId::new())
            .github_id("gh-456")
            .build()
            .unwrap();

        let user = User::try_from_events(new_user.into_events()).unwrap();
        assert_eq!(user.github_id, "gh-456");
        assert_eq!(user.email, None);
        assert_eq!(user.name, None);
    }
}
