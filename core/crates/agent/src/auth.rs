//! Re-export of the auth subject type from the primitives crate so code in
//! this crate can refer to it as `crate::auth::AuthSubject` (matching the
//! old `core/src/auth/mod.rs` path).

pub use primitives::AuthSubject;
