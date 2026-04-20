use thiserror::Error;

use super::{AuthResource, AuthVerb};

#[derive(Error, Debug)]
pub enum AuthorizationError {
    #[error("AuthorizationError - Forbidden: {verb:?} on {resource:?}")]
    Forbidden {
        verb: AuthVerb,
        resource: AuthResource,
    },
    #[error("AuthorizationError - AuthenticationRequired")]
    AuthenticationRequired,
}
