use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{AuthResource, AuthVerb};
use crate::primitives::{ProjectId, SandboxId};

/// Typed authorization scope. Serializes as a plain string for backward
/// compatibility with event-store JSON storing `["read","write"]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuthScope {
    Admin,
    /// Project admin (granted to `ProjectLead` role). Gates project
    /// management tools and hides sandbox-backed filesystem tools (admins
    /// orchestrate; they don't run inside a sandbox themselves).
    ProjectAdmin(ProjectId),
    /// Project membership. Distinction from `ProjectAdmin` is in tool
    /// *visibility*, not in what the agent is *authorized* to do.
    ProjectMember(ProjectId),
    /// May invoke sandbox tools including state-mutating ones. Granted on `Write` attach.
    SandboxUse(SandboxId),
    /// Read-only sandbox access. Granted on `Read` attach.
    SandboxRead(SandboxId),
    /// Marker for agents spawned by a workflow run. Gates the
    /// synthesised `submit_output` tool's visibility — only workflow
    /// agents are expected to terminate via `submit_output`.
    WorkflowStepAgent,
    WorkflowScript,
    /// Grants access only to [`AuthResource::External`] resources whose name matches.
    External(String),
}

impl AuthScope {
    pub fn permits(&self, verb: AuthVerb, resource: &AuthResource) -> bool {
        match self {
            AuthScope::Admin => true,

            AuthScope::ProjectAdmin(project) => {
                // Spaces are library-wide; project admins manage the
                // *collection* (`Create`/`Read`/`Propose` on
                // `Space(None)`) but have no blanket authority over
                // OTHER project-scoped resources — that's the `in_ws`
                // fallthrough below. A specific space is the one
                // exception: leads may write `main` directly
                // (`Update`) or stage (`Propose`) regardless of mount
                // visibility, which is a project-membership concern,
                // not an authorization one.
                if matches!(
                    (verb, resource),
                    (AuthVerb::Create, AuthResource::Space(None))
                        | (AuthVerb::Read, AuthResource::Space(None))
                        | (AuthVerb::Propose, AuthResource::Space(None))
                ) {
                    return true;
                }
                if let AuthResource::Space(Some(_)) = resource {
                    return matches!(verb, AuthVerb::Update | AuthVerb::Propose);
                }
                resource
                    .project_id()
                    .is_some_and(|res_ws| res_ws == *project)
            }

            // Limited set within project; expand as new tools need it.
            // `Space` is handled first and unconditionally — it's
            // library-wide (`AuthResource::project_id()` is always
            // `None` for it), so the `in_ws` check below would always
            // reject it.
            AuthScope::ProjectMember(project) => {
                if let AuthResource::Space(_) = resource {
                    // §4.2 OQ-2 (default: option a) — members stage,
                    // they don't write `main` directly. This is what
                    // keeps a workflow step agent's *combined* scopes
                    // (`ProjectMember` + `WorkflowStepAgent`, both
                    // `Propose`-only) from ever summing to `Update` via
                    // `AuthSubject::can`'s any-scope-permits semantics —
                    // see `scope_tests::workflow_step_agent_combined_scopes_cannot_write_main`.
                    return verb == AuthVerb::Propose;
                }
                let in_ws = resource
                    .project_id()
                    .is_some_and(|res_ws| res_ws == *project);
                if !in_ws {
                    return false;
                }
                matches!(
                    (verb, resource),
                    (AuthVerb::Read, AuthResource::Project(_))
                        | (AuthVerb::Create, AuthResource::Note(..))
                        | (AuthVerb::Read, AuthResource::Note(..))
                        | (AuthVerb::Update, AuthResource::Note(..))
                        | (AuthVerb::Delete, AuthResource::Note(..))
                        | (AuthVerb::Read, AuthResource::Skill(..))
                        | (AuthVerb::Use, AuthResource::Skill(..))
                )
            }

            // Write implies read.
            AuthScope::SandboxUse(sb) => {
                matches!(verb, AuthVerb::Use | AuthVerb::Read)
                    && matches!(
                        resource,
                        AuthResource::Sandbox(_, Some(res_sb)) if *res_sb == *sb
                    )
            }

            AuthScope::SandboxRead(sb) => {
                verb == AuthVerb::Read
                    && matches!(
                        resource,
                        AuthResource::Sandbox(_, Some(res_sb)) if *res_sb == *sb
                    )
            }

            // Both markers grant only `Propose` on `Space` — a step
            // agent or a script step may stage edits through a
            // changeset, never write `main` directly, regardless of
            // what its OTHER carried scopes (typically `ProjectMember`,
            // itself `Propose`-only) would otherwise sum to.
            AuthScope::WorkflowStepAgent | AuthScope::WorkflowScript => {
                matches!(
                    (verb, resource),
                    (AuthVerb::Propose, AuthResource::Space(_))
                )
            }

            AuthScope::External(name) => {
                matches!(resource, AuthResource::External(res_name) if res_name == name)
            }
        }
    }
}

impl fmt::Display for AuthScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthScope::Admin => f.write_str("admin"),
            AuthScope::ProjectAdmin(id) => write!(f, "project:{id}:admin"),
            AuthScope::ProjectMember(id) => write!(f, "project:{id}:member"),
            AuthScope::SandboxUse(id) => write!(f, "sandbox:{id}:use"),
            AuthScope::SandboxRead(id) => write!(f, "sandbox:{id}:read"),
            AuthScope::WorkflowStepAgent => f.write_str("workflow:step_agent"),
            AuthScope::WorkflowScript => f.write_str("workflow:script"),
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
        if s == "workflow:script" {
            return Ok(AuthScope::WorkflowScript);
        }
        if s == "workflow:step_agent" {
            return Ok(AuthScope::WorkflowStepAgent);
        }

        if let Some(rest) = s.strip_prefix("project:") {
            if let Some(uuid_str) = rest.strip_suffix(":admin") {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::ProjectAdmin(ProjectId::from(uuid)));
                }
            }
            if let Some(uuid_str) = rest.strip_suffix(":member") {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::ProjectMember(ProjectId::from(uuid)));
                }
            }
        }

        if let Some(rest) = s.strip_prefix("sandbox:") {
            if let Some(uuid_str) = rest.strip_suffix(":use") {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::SandboxUse(SandboxId::from(uuid)));
                }
            }
            if let Some(uuid_str) = rest.strip_suffix(":read") {
                if let Ok(uuid) = uuid_str.parse::<uuid::Uuid>() {
                    return Ok(AuthScope::SandboxRead(SandboxId::from(uuid)));
                }
            }
        }

        Ok(AuthScope::External(s.to_owned()))
    }
}

impl From<String> for AuthScope {
    fn from(s: String) -> Self {
        s.parse().unwrap()
    }
}

impl From<&str> for AuthScope {
    fn from(s: &str) -> Self {
        s.parse().unwrap()
    }
}

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

    fn test_project_id() -> ProjectId {
        ProjectId::from(uuid::Uuid::parse_str("a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8").unwrap())
    }

    fn test_sandbox_id() -> SandboxId {
        SandboxId::from(uuid::Uuid::parse_str("e1e2e3e4-f1f2-1112-2122-313233343536").unwrap())
    }

    /// Round-trip: every variant must survive `Display` → `FromStr`.
    /// When adding a new variant, add it to this list so CI catches any
    /// mismatch immediately.
    #[test]
    fn round_trip_all_variants() {
        let project_id = test_project_id();
        let sb_id = test_sandbox_id();
        let variants = vec![
            AuthScope::Admin,
            AuthScope::ProjectAdmin(project_id),
            AuthScope::ProjectMember(project_id),
            AuthScope::SandboxUse(sb_id),
            AuthScope::SandboxRead(sb_id),
            AuthScope::WorkflowStepAgent,
            AuthScope::WorkflowScript,
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

    #[test]
    fn display_admin() {
        assert_eq!(AuthScope::Admin.to_string(), "admin");
    }

    #[test]
    fn display_project_scopes() {
        let project_id = test_project_id();
        assert_eq!(
            AuthScope::ProjectAdmin(project_id).to_string(),
            "project:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:admin"
        );
        assert_eq!(
            AuthScope::ProjectMember(project_id).to_string(),
            "project:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:member"
        );
    }

    #[test]
    fn from_str_admin() {
        let scope: AuthScope = "admin".parse().unwrap();
        assert_eq!(scope, AuthScope::Admin);
    }

    #[test]
    fn from_str_project() {
        let project_id = test_project_id();
        let admin: AuthScope = "project:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:admin"
            .parse()
            .unwrap();
        assert_eq!(admin, AuthScope::ProjectAdmin(project_id));

        let member: AuthScope = "project:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:member"
            .parse()
            .unwrap();
        assert_eq!(member, AuthScope::ProjectMember(project_id));
    }

    #[test]
    fn from_str_unknown_falls_back_to_external() {
        let scope: AuthScope = "read".parse().unwrap();
        assert_eq!(scope, AuthScope::External("read".to_owned()));
    }

    /// Eq-based comparison works across all variants.
    #[test]
    fn eq_all_variants() {
        let project_id = test_project_id();
        assert_eq!(AuthScope::Admin, AuthScope::Admin);
        assert_ne!(AuthScope::Admin, AuthScope::External("admin".to_owned()));

        assert_eq!(
            AuthScope::ProjectAdmin(project_id),
            AuthScope::ProjectAdmin(project_id)
        );
        assert_ne!(AuthScope::ProjectAdmin(project_id), AuthScope::Admin);

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
        let project_id = test_project_id();
        let variants = vec![
            (AuthScope::Admin, r#""admin""#),
            (
                AuthScope::ProjectAdmin(project_id),
                r#""project:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:admin""#,
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

    /// Project admins may create and list library-wide spaces
    /// (`Space(None)`), and — unlike a member — may write `main`
    /// directly (`Update`) or stage (`Propose`) on any specific space
    /// (`Space(Some(_))`); *mount* visibility there is still decided
    /// separately by `Project.mounted_spaces` (`SpaceFs`'s mount gate
    /// runs first). `Read` on a specific space and `Delete` on the
    /// collection stay ungranted — nothing needs them through this
    /// layer today.
    #[test]
    fn project_admin_space_authz() {
        use crate::primitives::SpaceId;
        let s = AuthScope::ProjectAdmin(test_project_id());

        assert!(s.permits(AuthVerb::Create, &AuthResource::Space(None)));
        assert!(s.permits(AuthVerb::Read, &AuthResource::Space(None)));
        assert!(s.permits(AuthVerb::Propose, &AuthResource::Space(None)));

        let space_id = SpaceId::new();
        assert!(!s.permits(AuthVerb::Read, &AuthResource::Space(Some(space_id))));
        assert!(s.permits(AuthVerb::Update, &AuthResource::Space(Some(space_id))));
        assert!(s.permits(AuthVerb::Propose, &AuthResource::Space(Some(space_id))));
        assert!(!s.permits(AuthVerb::Update, &AuthResource::Space(None)));
        assert!(!s.permits(AuthVerb::Delete, &AuthResource::Space(None)));
    }

    /// §4.2 OQ-2 (default: option a) — a plain project member stages,
    /// it never writes `main` directly, on any specific space or the
    /// collection.
    #[test]
    fn project_member_space_authz_is_propose_only() {
        use crate::primitives::SpaceId;
        let project = test_project_id();
        let s = AuthScope::ProjectMember(project);
        let space_id = SpaceId::new();

        assert!(s.permits(AuthVerb::Propose, &AuthResource::Space(Some(space_id))));
        assert!(s.permits(AuthVerb::Propose, &AuthResource::Space(None)));
        assert!(!s.permits(AuthVerb::Update, &AuthResource::Space(Some(space_id))));
        assert!(!s.permits(AuthVerb::Update, &AuthResource::Space(None)));
        assert!(!s.permits(AuthVerb::Read, &AuthResource::Space(Some(space_id))));
        assert!(!s.permits(AuthVerb::Create, &AuthResource::Space(None)));
    }

    /// §4.2 — the `WorkflowStepAgent`/`WorkflowScript` markers grant
    /// `Propose` on `Space` and nothing else; they carry no project
    /// scoping of their own (that's `ProjectMember`'s job).
    #[test]
    fn workflow_markers_grant_propose_on_space_only() {
        use crate::primitives::SpaceId;
        let space_id = SpaceId::new();
        for marker in [AuthScope::WorkflowStepAgent, AuthScope::WorkflowScript] {
            assert!(
                marker.permits(AuthVerb::Propose, &AuthResource::Space(Some(space_id))),
                "{marker} should permit Propose on a specific space"
            );
            assert!(
                marker.permits(AuthVerb::Propose, &AuthResource::Space(None)),
                "{marker} should permit Propose on the space collection"
            );
            assert!(
                !marker.permits(AuthVerb::Update, &AuthResource::Space(Some(space_id))),
                "{marker} must never permit Update on a specific space"
            );
            assert!(
                !marker.permits(
                    AuthVerb::Read,
                    &AuthResource::Project(Some(test_project_id()))
                ),
                "{marker} should not reach into unrelated project resources"
            );
        }
    }

    /// THE any-scope-permits trap this handoff exists to close: a real
    /// workflow step agent's subject carries BOTH `ProjectMember(p)`
    /// (from `AgentRole::WorkflowStepAgent` — see `agent/mod.rs`'s
    /// `role_scopes`) AND the `WorkflowStepAgent` marker. `AuthSubject::can`
    /// is any-scope-permits, so if EITHER scope granted `Update` on a
    /// specific space, the combination would let a step agent write
    /// `main` directly. Both scopes are `Propose`-only on `Space`
    /// (§4.2), so the combination stays `Propose`-only too — this is
    /// the property that actually matters, not either scope alone.
    #[test]
    fn workflow_step_agent_combined_scopes_cannot_write_main() {
        use crate::auth::AuthSubject;
        use crate::primitives::{AgentId, SpaceId};
        let project = test_project_id();
        let space_id = SpaceId::new();
        let sub = AuthSubject::Agent(
            project,
            AgentId::new(),
            vec![
                AuthScope::ProjectMember(project),
                AuthScope::WorkflowStepAgent,
            ],
        );

        assert!(
            sub.can(AuthVerb::Propose, AuthResource::Space(Some(space_id)))
                .is_ok(),
            "a step agent must be able to stage edits through a changeset"
        );
        assert!(
            sub.can(AuthVerb::Update, AuthResource::Space(Some(space_id)))
                .is_err(),
            "a step agent must NEVER be able to write main directly, \
             even though it carries ProjectMember + WorkflowStepAgent"
        );
    }

    /// Contrast case: a project lead (`ProjectAdmin`, no `ProjectMember`
    /// or workflow marker) DOES get `Update` — confirming the denial
    /// above is `WorkflowStepAgent`-specific, not a blanket bug that
    /// would also break legitimate direct writes.
    #[test]
    fn project_lead_alone_can_write_main() {
        use crate::auth::AuthSubject;
        use crate::primitives::{AgentId, SpaceId};
        let project = test_project_id();
        let space_id = SpaceId::new();
        let sub = AuthSubject::Agent(
            project,
            AgentId::new(),
            vec![AuthScope::ProjectAdmin(project)],
        );

        assert!(sub
            .can(AuthVerb::Update, AuthResource::Space(Some(space_id)))
            .is_ok());
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
