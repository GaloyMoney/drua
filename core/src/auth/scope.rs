use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{AuthResource, AuthVerb};
use crate::primitives::{SandboxId, WorkspaceId};

/// A typed authorization scope carried by [`super::AuthSubject`] variants.
///
/// Serializes as a plain string so that existing event-store JSON and config
/// files (which store scopes as `["read","write"]`) remain compatible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuthScope {
    /// Full administrative access.
    Admin,
    /// The subject is an admin of a specific workspace (granted to the
    /// `WorkspaceLead` agent role today). Currently the only
    /// workspace-level scope: gates workspace management tools
    /// (list/create/update agents and sandboxes, query audit logs, etc.)
    /// and is checked by per-tool visibility to *hide* sandbox-backed
    /// filesystem tools (admins orchestrate; they don't run inside a
    /// sandbox themselves).
    WorkspaceAdmin(WorkspaceId),
    /// Use access — the agent may invoke sandbox tools, including
    /// state-mutating ones (e.g. the `bash` top-level tool). Granted on a
    /// `Write` attach.
    SandboxUse(SandboxId),
    /// Read-only access — the agent may invoke sandbox tools that
    /// don't modify state. Granted on a `Read` attach.
    SandboxRead(SandboxId),
    /// An externally-defined scope string. Grants access only to
    /// [`AuthResource::External`] resources whose name matches.
    External(String),
}

impl AuthScope {
    /// Whether this single scope grants the requested verb+resource.
    pub fn permits(&self, verb: AuthVerb, resource: &AuthResource) -> bool {
        match self {
            // Admin grants everything.
            AuthScope::Admin => true,

            // WorkspaceAdmin grants any verb on any resource within its workspace.
            AuthScope::WorkspaceAdmin(ws) => {
                resource.workspace_id().is_some_and(|res_ws| res_ws == *ws)
            }

            // SandboxUse grants Use and Read on the matching sandbox
            // (write implies read).
            AuthScope::SandboxUse(sb) => {
                matches!(verb, AuthVerb::Use | AuthVerb::Read)
                    && matches!(
                        resource,
                        AuthResource::Sandbox(_, Some(res_sb)) if *res_sb == *sb
                    )
            }

            // SandboxRead grants only Read on the matching sandbox.
            AuthScope::SandboxRead(sb) => {
                verb == AuthVerb::Read
                    && matches!(
                        resource,
                        AuthResource::Sandbox(_, Some(res_sb)) if *res_sb == *sb
                    )
            }

            // External scopes match External resources by name.
            AuthScope::External(name) => {
                matches!(resource, AuthResource::External(res_name) if res_name == name)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Display / FromStr
// ---------------------------------------------------------------------------

impl fmt::Display for AuthScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthScope::Admin => f.write_str("admin"),
            AuthScope::WorkspaceAdmin(id) => write!(f, "ws:{id}:admin"),
            AuthScope::SandboxUse(id) => write!(f, "sandbox:{id}:use"),
            AuthScope::SandboxRead(id) => write!(f, "sandbox:{id}:read"),
            AuthScope::External(s) => f.write_str(s),
        }
    }
}

impl FromStr for AuthScope {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "admin" {
            return Ok(AuthScope::Admin);
        }

        // Parse "ws:{uuid}:admin"
        if let Some(rest) = s.strip_prefix("ws:") {
            if let Some(uuid_str) = rest.strip_suffix(":admin") {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::WorkspaceAdmin(WorkspaceId::from(uuid)));
                }
            }
        }

        // Parse sandbox scopes — accept both current and legacy suffixes.
        if let Some(rest) = s.strip_prefix("sandbox:") {
            // Current: "sandbox:{uuid}:use", legacy: "sandbox:{uuid}:use_all"
            if let Some(uuid_str) = rest
                .strip_suffix(":use")
                .or_else(|| rest.strip_suffix(":use_all"))
            {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::SandboxUse(SandboxId::from(uuid)));
                }
            }
            // Current: "sandbox:{uuid}:read", legacy: "sandbox:{uuid}:use_read_only"
            if let Some(uuid_str) = rest
                .strip_suffix(":read")
                .or_else(|| rest.strip_suffix(":use_read_only"))
            {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::SandboxRead(SandboxId::from(uuid)));
                }
            }
        }

        Ok(AuthScope::External(s.to_owned()))
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

    fn test_sandbox_id() -> SandboxId {
        SandboxId::from(uuid::Uuid::parse_str("e1e2e3e4-f1f2-1112-2122-313233343536").unwrap())
    }

    /// Round-trip: every variant must survive `Display` → `FromStr`.
    /// When adding a new variant, add it to this list so CI catches any
    /// mismatch immediately.
    #[test]
    fn round_trip_all_variants() {
        let ws_id = test_workspace_id();
        let sb_id = test_sandbox_id();
        let variants = vec![
            AuthScope::Admin,
            AuthScope::WorkspaceAdmin(ws_id),
            AuthScope::SandboxUse(sb_id),
            AuthScope::SandboxRead(sb_id),
            AuthScope::External("custom:thing".to_owned()),
        ];

        for scope in variants {
            let serialized = scope.to_string();
            let parsed: AuthScope = serialized.parse().unwrap();
            assert_eq!(scope, parsed);
        }
    }

    #[test]
    fn display_sandbox_scopes() {
        let sb_id = test_sandbox_id();
        assert_eq!(
            AuthScope::SandboxUse(sb_id).to_string(),
            "sandbox:e1e2e3e4-f1f2-1112-2122-313233343536:use"
        );
        assert_eq!(
            AuthScope::SandboxRead(sb_id).to_string(),
            "sandbox:e1e2e3e4-f1f2-1112-2122-313233343536:read"
        );
    }

    #[test]
    fn from_str_sandbox() {
        let sb_id = test_sandbox_id();
        let use_scope: AuthScope = "sandbox:e1e2e3e4-f1f2-1112-2122-313233343536:use"
            .parse()
            .unwrap();
        assert_eq!(use_scope, AuthScope::SandboxUse(sb_id));

        let read_scope: AuthScope = "sandbox:e1e2e3e4-f1f2-1112-2122-313233343536:read"
            .parse()
            .unwrap();
        assert_eq!(read_scope, AuthScope::SandboxRead(sb_id));
    }

    /// Legacy serialized forms must still deserialize correctly.
    #[test]
    fn from_str_sandbox_legacy() {
        let sb_id = test_sandbox_id();
        let use_scope: AuthScope = "sandbox:e1e2e3e4-f1f2-1112-2122-313233343536:use_all"
            .parse()
            .unwrap();
        assert_eq!(use_scope, AuthScope::SandboxUse(sb_id));

        let read_scope: AuthScope = "sandbox:e1e2e3e4-f1f2-1112-2122-313233343536:use_read_only"
            .parse()
            .unwrap();
        assert_eq!(read_scope, AuthScope::SandboxRead(sb_id));
    }

    #[test]
    fn display_admin() {
        assert_eq!(AuthScope::Admin.to_string(), "admin");
    }

    #[test]
    fn display_workspace_scopes() {
        let ws_id = test_workspace_id();
        assert_eq!(
            AuthScope::WorkspaceAdmin(ws_id).to_string(),
            "ws:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:admin"
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
        let admin: AuthScope = "ws:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:admin"
            .parse()
            .unwrap();
        assert_eq!(admin, AuthScope::WorkspaceAdmin(ws_id));
    }

    #[test]
    fn from_str_unknown_falls_back_to_external() {
        let scope: AuthScope = "read".parse().unwrap();
        assert_eq!(scope, AuthScope::External("read".to_owned()));
    }

    /// Eq-based comparison works across all variants.
    #[test]
    fn eq_all_variants() {
        let ws_id = test_workspace_id();
        assert_eq!(AuthScope::Admin, AuthScope::Admin);
        assert_ne!(AuthScope::Admin, AuthScope::External("admin".to_owned()));

        assert_eq!(
            AuthScope::WorkspaceAdmin(ws_id),
            AuthScope::WorkspaceAdmin(ws_id)
        );
        assert_ne!(AuthScope::WorkspaceAdmin(ws_id), AuthScope::Admin);

        assert_eq!(
            AuthScope::External("custom".to_owned()),
            AuthScope::External("custom".to_owned())
        );
        assert_ne!(
            AuthScope::External("custom".to_owned()),
            AuthScope::External("other".to_owned())
        );
    }

    /// JSON must round-trip as a plain string (not `{"External":"…"}`), so that
    /// existing event-store payloads and config files remain compatible.
    #[test]
    fn serde_round_trip_plain_string() {
        let ws_id = test_workspace_id();
        let variants = vec![
            (AuthScope::Admin, r#""admin""#),
            (
                AuthScope::WorkspaceAdmin(ws_id),
                r#""ws:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:admin""#,
            ),
            (AuthScope::External("custom".to_owned()), r#""custom""#),
        ];

        for (scope, expected_json) in variants {
            let json = serde_json::to_string(&scope).unwrap();
            assert_eq!(json, expected_json);
            let parsed: AuthScope = serde_json::from_str(&json).unwrap();
            assert_eq!(scope, parsed);
        }
    }

    /// Deserializing from a plain JSON string — "admin" now parses to Admin
    /// variant, not External("admin").
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
