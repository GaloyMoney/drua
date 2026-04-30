mod entity;
pub mod error;
pub mod file_sync;
pub(crate) mod repo;

pub use entity::{NewSpace, Space, SpaceEvent};
pub use error::*;
