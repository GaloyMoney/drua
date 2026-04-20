use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{SandboxId, WorkspaceId};

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
    /// Full use access — the agent may invoke any sandbox tool, including
    /// state-mutating ones (e.g. the `bash` top-level tool). Granted on a
    /// `Write` attach.
    SandboxUseAll(SandboxId),
    /// Read-only use access — the agent may invoke sandbox tools that
    /// don't modify state. Granted on a `Read` attach.
    SandboxUseReadOnly(SandboxId),
    /// Authorizes the holder to register a tunnel connector for a specific
    /// `deployment_id`. The `/tunnel/ws` handler matches this scope against
    /// the `deployment_id` in the connector's Register frame; a token scoped
    /// to `Tunnel("galoy-staging")` cannot register as `galoy-production`.
    Tunnel(String),
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
            AuthScope::WorkspaceAdmin(id) => write!(f, "ws:{id}:admin"),
            AuthScope::SandboxUseAll(id) => write!(f, "sandbox:{id}:use_all"),
            AuthScope::SandboxUseReadOnly(id) => write!(f, "sandbox:{id}:use_read_only"),
            AuthScope::Tunnel(id) => write!(f, "tunnel:{id}"),
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

        // Parse "ws:{uuid}:admin"
        if let Some(rest) = s.strip_prefix("ws:") {
            if let Some(uuid_str) = rest.strip_suffix(":admin") {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::WorkspaceAdmin(WorkspaceId::from(uuid)));
                }
            }
        }

        // Parse "sandbox:{uuid}:use_all" / ":use_read_only"
        if let Some(rest) = s.strip_prefix("sandbox:") {
            if let Some(uuid_str) = rest.strip_suffix(":use_all") {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::SandboxUseAll(SandboxId::from(uuid)));
                }
            } else if let Some(uuid_str) = rest.strip_suffix(":use_read_only") {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::SandboxUseReadOnly(SandboxId::from(uuid)));
                }
            }
        }

        // Parse "tunnel:{deployment_id}"
        if let Some(deployment_id) = s.strip_prefix("tunnel:") {
            if !deployment_id.is_empty() {
                return Ok(AuthScope::Tunnel(deployment_id.to_owned()));
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
            AuthScope::SandboxUseAll(sb_id),
            AuthScope::SandboxUseReadOnly(sb_id),
            AuthScope::Tunnel("galoy-staging".to_owned()),
            AuthScope::Raw("custom:thing".to_owned()),
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
            AuthScope::SandboxUseAll(sb_id).to_string(),
            "sandbox:e1e2e3e4-f1f2-1112-2122-313233343536:use_all"
        );
        assert_eq!(
            AuthScope::SandboxUseReadOnly(sb_id).to_string(),
            "sandbox:e1e2e3e4-f1f2-1112-2122-313233343536:use_read_only"
        );
    }

    #[test]
    fn from_str_sandbox() {
        let sb_id = test_sandbox_id();
        let all: AuthScope = "sandbox:e1e2e3e4-f1f2-1112-2122-313233343536:use_all"
            .parse()
            .unwrap();
        assert_eq!(all, AuthScope::SandboxUseAll(sb_id));

        let ro: AuthScope = "sandbox:e1e2e3e4-f1f2-1112-2122-313233343536:use_read_only"
            .parse()
            .unwrap();
        assert_eq!(ro, AuthScope::SandboxUseReadOnly(sb_id));
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
    fn from_str_unknown_falls_back_to_raw() {
        let scope: AuthScope = "read".parse().unwrap();
        assert_eq!(scope, AuthScope::Raw("read".to_owned()));
    }

    #[test]
    fn tunnel_round_trip() {
        let scope = AuthScope::Tunnel("galoy-staging".to_owned());
        assert_eq!(scope.to_string(), "tunnel:galoy-staging");
        let parsed: AuthScope = "tunnel:galoy-staging".parse().unwrap();
        assert_eq!(parsed, scope);
    }

    #[test]
    fn tunnel_empty_deployment_id_falls_back_to_raw() {
        // `tunnel:` with no id must not silently become a Tunnel scope —
        // guards against a malformed token accidentally matching any
        // deployment. It falls through to Raw, which never matches a
        // `Tunnel(X)` check via `has_scope`.
        let scope: AuthScope = "tunnel:".parse().unwrap();
        assert_eq!(scope, AuthScope::Raw("tunnel:".to_owned()));
    }

    /// Eq-based comparison works across all variants.
    #[test]
    fn eq_all_variants() {
        let ws_id = test_workspace_id();
        assert_eq!(AuthScope::Admin, AuthScope::Admin);
        assert_ne!(AuthScope::Admin, AuthScope::Raw("admin".to_owned()));

        assert_eq!(
            AuthScope::WorkspaceAdmin(ws_id),
            AuthScope::WorkspaceAdmin(ws_id)
        );
        assert_ne!(AuthScope::WorkspaceAdmin(ws_id), AuthScope::Admin);

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
                AuthScope::WorkspaceAdmin(ws_id),
                r#""ws:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:admin""#,
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
