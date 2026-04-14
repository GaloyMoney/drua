use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::WorkspaceId;

/// A typed authorization scope carried by [`super::AuthSubject`] variants.
///
/// Serializes as a plain string so that existing event-store JSON and config
/// files (which store scopes as `["read","write"]`) remain compatible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuthScope {
    /// Full administrative access.
    Admin,
    /// Read access scoped to a specific workspace.
    WorkspaceRead(WorkspaceId),
    /// Write access scoped to a specific workspace.
    WorkspaceWrite(WorkspaceId),
    /// Catch-all for scope strings that don't (yet) have a dedicated variant.
    Raw(String),
}

// ---------------------------------------------------------------------------
// Display / FromStr
// ---------------------------------------------------------------------------

impl fmt::Display for AuthScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthScope::Admin => f.write_str("admin"),
            AuthScope::WorkspaceRead(id) => write!(f, "ws:{id}:read"),
            AuthScope::WorkspaceWrite(id) => write!(f, "ws:{id}:write"),
            AuthScope::Raw(s) => f.write_str(s),
        }
    }
}

impl FromStr for AuthScope {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "admin" {
            return Ok(AuthScope::Admin);
        }

        // Parse "ws:{uuid}:read" / "ws:{uuid}:write"
        if let Some(rest) = s.strip_prefix("ws:") {
            if let Some(uuid_str) = rest.strip_suffix(":read") {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::WorkspaceRead(WorkspaceId::from(uuid)));
                }
            } else if let Some(uuid_str) = rest.strip_suffix(":write") {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::WorkspaceWrite(WorkspaceId::from(uuid)));
                }
            }
        }

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
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for AuthScope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(AuthScope::from(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_workspace_id() -> WorkspaceId {
        WorkspaceId::from(uuid::Uuid::parse_str("a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8").unwrap())
    }

    /// Round-trip: every variant must survive `Display` → `FromStr`.
    /// When adding a new variant, add it to this list so CI catches any
    /// mismatch immediately.
    #[test]
    fn round_trip_all_variants() {
        let ws_id = test_workspace_id();
        let variants = vec![
            AuthScope::Admin,
            AuthScope::WorkspaceRead(ws_id),
            AuthScope::WorkspaceWrite(ws_id),
            AuthScope::Raw("custom:thing".to_owned()),
        ];

        for scope in variants {
            let serialized = scope.to_string();
            let parsed: AuthScope = serialized.parse().unwrap();
            assert_eq!(scope, parsed);
        }
    }

    #[test]
    fn display_admin() {
        assert_eq!(AuthScope::Admin.to_string(), "admin");
    }

    #[test]
    fn display_workspace_scopes() {
        let ws_id = test_workspace_id();
        assert_eq!(
            AuthScope::WorkspaceRead(ws_id).to_string(),
            "ws:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:read"
        );
        assert_eq!(
            AuthScope::WorkspaceWrite(ws_id).to_string(),
            "ws:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:write"
        );
    }

    #[test]
    fn from_str_admin() {
        let scope: AuthScope = "admin".parse().unwrap();
        assert_eq!(scope, AuthScope::Admin);
    }

    #[test]
    fn from_str_workspace() {
        let ws_id = test_workspace_id();
        let read: AuthScope = "ws:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:read"
            .parse()
            .unwrap();
        assert_eq!(read, AuthScope::WorkspaceRead(ws_id));

        let write: AuthScope = "ws:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:write"
            .parse()
            .unwrap();
        assert_eq!(write, AuthScope::WorkspaceWrite(ws_id));
    }

    #[test]
    fn from_str_unknown_falls_back_to_raw() {
        let scope: AuthScope = "read".parse().unwrap();
        assert_eq!(scope, AuthScope::Raw("read".to_owned()));
    }

    /// Eq-based comparison works across all variants.
    #[test]
    fn eq_all_variants() {
        let ws_id = test_workspace_id();
        assert_eq!(AuthScope::Admin, AuthScope::Admin);
        assert_ne!(AuthScope::Admin, AuthScope::Raw("admin".to_owned()));

        assert_eq!(
            AuthScope::WorkspaceRead(ws_id),
            AuthScope::WorkspaceRead(ws_id)
        );
        assert_ne!(
            AuthScope::WorkspaceRead(ws_id),
            AuthScope::WorkspaceWrite(ws_id)
        );

        assert_eq!(
            AuthScope::Raw("custom".to_owned()),
            AuthScope::Raw("custom".to_owned())
        );
        assert_ne!(
            AuthScope::Raw("custom".to_owned()),
            AuthScope::Raw("other".to_owned())
        );
    }

    /// JSON must round-trip as a plain string (not `{"Raw":"…"}`), so that
    /// existing event-store payloads and config files remain compatible.
    #[test]
    fn serde_round_trip_plain_string() {
        let ws_id = test_workspace_id();
        let variants = vec![
            (AuthScope::Admin, r#""admin""#),
            (
                AuthScope::WorkspaceRead(ws_id),
                r#""ws:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:read""#,
            ),
            (AuthScope::Raw("custom".to_owned()), r#""custom""#),
        ];

        for (scope, expected_json) in variants {
            let json = serde_json::to_string(&scope).unwrap();
            assert_eq!(json, expected_json);
            let parsed: AuthScope = serde_json::from_str(&json).unwrap();
            assert_eq!(scope, parsed);
        }
    }

    /// Deserializing from a plain JSON string — "admin" now parses to Admin
    /// variant, not Raw("admin").
    #[test]
    fn deserialize_admin_from_plain_string() {
        let parsed: AuthScope = serde_json::from_str(r#""admin""#).unwrap();
        assert_eq!(parsed, AuthScope::Admin);
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
