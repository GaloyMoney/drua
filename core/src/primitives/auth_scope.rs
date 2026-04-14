use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A typed authorization scope carried by [`super::AuthSubject`] variants.
///
/// Currently only has a catch-all [`Raw`](AuthScope::Raw) variant; concrete
/// scopes (e.g. workspace-level read/write, admin) will be added incrementally.
///
/// Serializes as a plain string so that existing event-store JSON and config
/// files (which store scopes as `["read","write"]`) remain compatible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuthScope {
    /// Catch-all for scope strings that don't (yet) have a dedicated variant.
    Raw(String),
}

// ---------------------------------------------------------------------------
// Display / FromStr
// ---------------------------------------------------------------------------

impl fmt::Display for AuthScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthScope::Raw(s) => f.write_str(s),
        }
    }
}

impl FromStr for AuthScope {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // As concrete variants are added, match known strings here first.
        Ok(AuthScope::Raw(s.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// Convenience From impls
// ---------------------------------------------------------------------------

impl From<String> for AuthScope {
    fn from(s: String) -> Self {
        // Re-use FromStr so the mapping stays in one place.
        s.parse().unwrap()
    }
}

impl From<&str> for AuthScope {
    fn from(s: &str) -> Self {
        s.parse().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Serde — serialize as a plain string for backward-compatible JSON
// ---------------------------------------------------------------------------

impl Serialize for AuthScope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AuthScope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(AuthScope::from(s))
    }
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

impl AuthScope {
    /// Return the scope as a string slice, regardless of variant.
    pub fn as_str(&self) -> &str {
        match self {
            AuthScope::Raw(s) => s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: every variant must survive `Display` → `FromStr`.
    /// When adding a new variant, add it to this list so CI catches any
    /// mismatch immediately.
    #[test]
    fn round_trip_all_variants() {
        let variants = vec![AuthScope::Raw("ws:abc:read".to_owned())];

        for scope in variants {
            let serialized = scope.to_string();
            let parsed: AuthScope = serialized.parse().unwrap();
            assert_eq!(scope, parsed);
        }
    }

    #[test]
    fn display_raw() {
        let scope = AuthScope::Raw("admin".to_owned());
        assert_eq!(scope.to_string(), "admin");
    }

    #[test]
    fn from_str_raw() {
        let scope: AuthScope = "read".parse().unwrap();
        assert_eq!(scope, AuthScope::Raw("read".to_owned()));
    }

    /// JSON must round-trip as a plain string (not `{"Raw":"…"}`), so that
    /// existing event-store payloads and config files remain compatible.
    #[test]
    fn serde_round_trip_plain_string() {
        let scope = AuthScope::Raw("ws:123:write".to_owned());
        let json = serde_json::to_string(&scope).unwrap();
        assert_eq!(json, r#""ws:123:write""#);
        let parsed: AuthScope = serde_json::from_str(&json).unwrap();
        assert_eq!(scope, parsed);
    }

    /// Deserializing from a plain JSON string (as stored in existing events).
    #[test]
    fn deserialize_from_plain_string() {
        let parsed: AuthScope = serde_json::from_str(r#""admin""#).unwrap();
        assert_eq!(parsed, AuthScope::Raw("admin".to_owned()));
    }

    /// Vec<AuthScope> serializes the same as the old Vec<String>.
    #[test]
    fn serde_vec_compat() {
        let scopes = vec![AuthScope::from("read"), AuthScope::from("write")];
        let json = serde_json::to_string(&scopes).unwrap();
        assert_eq!(json, r#"["read","write"]"#);
        let parsed: Vec<AuthScope> = serde_json::from_str(&json).unwrap();
        assert_eq!(scopes, parsed);
    }
}
