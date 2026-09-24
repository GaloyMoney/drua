use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::changeset::{Changeset, ChangesetStatus, Changesets, TouchedFile, TouchedKind};
use crate::primitives::ChangesetId;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, OutputSchema};

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum ChangesetParams {
    /// Opens a changeset at `main`'s current tip and binds the caller
    /// to it. File tools then read and write the branch automatically.
    Open {
        title: String,
        #[serde(default)]
        description: Option<String>,
    },
    /// Commits, mergeability, and touched files for a changeset.
    /// `id` defaults to the caller's bound changeset.
    Status {
        #[serde(default)]
        id: Option<ChangesetId>,
    },
    /// Changesets in the caller's project, newest first.
    List {
        #[serde(default)]
        status: Option<ChangesetStatus>,
    },
    /// Joins an existing `Open` changeset without closing any changeset
    /// already bound.
    Bind { id: ChangesetId },
    /// Leaves the caller's bound changeset without closing it.
    Unbind,
    /// Closes a changeset without landing it. `id` defaults to the
    /// caller's bound changeset.
    Discard {
        #[serde(default)]
        id: Option<ChangesetId>,
        #[serde(default)]
        reason: Option<String>,
    },
}

impl ChangesetParams {
    fn command_name(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::Status { .. } => "status",
            Self::List { .. } => "list",
            Self::Bind { .. } => "bind",
            Self::Unbind => "unbind",
            Self::Discard { .. } => "discard",
        }
    }
}

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct ChangesetSummary {
    id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    status: String,
    branch: String,
    base_oid: String,
    head_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr_url: Option<String>,
}

impl From<&Changeset> for ChangesetSummary {
    fn from(cs: &Changeset) -> Self {
        Self {
            id: cs.id.to_string(),
            title: cs.title.clone(),
            description: cs.description.clone(),
            status: format!("{:?}", cs.status).to_lowercase(),
            branch: cs.branch(),
            base_oid: cs.base_oid.clone(),
            head_oid: cs.head_oid.clone(),
            pr_url: cs.pr_url.clone(),
        }
    }
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct TouchedFileOut {
    space: String,
    path: String,
    kind: String,
}

impl From<&TouchedFile> for TouchedFileOut {
    fn from(t: &TouchedFile) -> Self {
        Self {
            space: t.space_slug.clone(),
            path: t.path.clone(),
            kind: match t.kind {
                TouchedKind::Added => "added",
                TouchedKind::Modified => "modified",
                TouchedKind::Deleted => "deleted",
            }
            .to_string(),
        }
    }
}

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct ChangesetOutput {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    changeset: Option<ChangesetSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changesets: Option<Vec<ChangesetSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commits: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    main_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mergeable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflicts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    touched: Option<Vec<TouchedFileOut>>,
}

static CHANGESET_OUTPUT: LazyLock<OutputSchema<ChangesetOutput>> = LazyLock::new(OutputSchema::new);

static CHANGESET_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["open", "status", "list", "bind", "unbind", "discard"],
                "description": "Which changeset operation to perform."
            },
            "title": {
                "type": "string",
                "description": "Short summary of the change. Required for open."
            },
            "description": {
                "type": "string",
                "description": "Longer optional description. Used by open."
            },
            "id": {
                "type": "string",
                "description": "Changeset id (uuid). Required for bind; optional for status/discard (defaults to the caller's bound changeset)."
            },
            "status": {
                "type": "string",
                "enum": ["open", "submitted", "merged", "applied", "discarded", "abandoned"],
                "description": "Optional status filter for list."
            },
            "reason": {
                "type": "string",
                "description": "Optional free-text reason. Used by discard."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

pub struct ChangesetTool {
    changesets: Arc<Changesets>,
}

impl ChangesetTool {
    pub fn new(changesets: Arc<Changesets>) -> Self {
        Self { changesets }
    }

    async fn resolve_id(
        &self,
        sub: &AuthSubject,
        id: Option<ChangesetId>,
    ) -> Result<ChangesetId, ToolSetsError> {
        if let Some(id) = id {
            return Ok(id);
        }
        self.changesets
            .active_for_subject(sub)
            .await?
            .map(|cs| cs.id)
            .ok_or_else(|| {
                ToolSetsError::InvalidArgument(
                    "no changeset id given and the caller isn't bound to one".to_string(),
                )
            })
    }
}

#[async_trait::async_trait]
impl TopLevelTool for ChangesetTool {
    fn name(&self) -> &str {
        "changeset"
    }

    fn description(&self) -> &str {
        "Stage edits to `space:` paths as a changeset (a branch in the \
         library repo) instead of writing `main` directly. `open` a \
         changeset, make edits with the normal file tools (Read, Edit, \
         LS, Glob, Grep — they read and write your changeset \
         automatically once bound), then submit or apply it (see the \
         `submit`/`apply` commands once available) for review or to \
         land it. `status` shows commits, mergeability and touched \
         files (`id` defaults to your bound changeset). `list` shows \
         every changeset in your project. `bind`/`unbind` join or \
         leave a changeset without closing it. `discard` closes a \
         changeset without landing it."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &CHANGESET_SCHEMA
    }

    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        Some(CHANGESET_OUTPUT.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject
            .can(AuthVerb::Propose, AuthResource::Space(None))
            .is_ok()
            || subject
                .can(AuthVerb::Update, AuthResource::Space(None))
                .is_ok()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ChangesetParams = parse_params(arguments)?;
        Audit::record_action(format!("changeset.{}", params.command_name()));

        let (text, out) = match params {
            ChangesetParams::Open { title, description } => {
                let cs = self.changesets.open(subject, title, description).await?;
                let text = format!(
                    "Changeset opened.\n  id: {}\n  branch: {}\n  base: {}",
                    cs.id,
                    cs.branch(),
                    cs.base_oid,
                );
                let out = ChangesetOutput {
                    command: "open".to_string(),
                    changeset: Some(ChangesetSummary::from(&cs)),
                    ..Default::default()
                };
                (text, out)
            }
            ChangesetParams::Status { id } => {
                let id = self.resolve_id(subject, id).await?;
                let view = self.changesets.status(subject, id).await?;
                let touched: Vec<TouchedFileOut> = view.touched.iter().map(Into::into).collect();
                let text = format!(
                    "Changeset {}\n  title: {}\n  status: {:?}\n  commits: {}\n  mergeable: {}{}\n  touched: {} file(s)",
                    view.id,
                    view.title,
                    view.status,
                    view.commits,
                    view.mergeable,
                    if view.mergeable {
                        String::new()
                    } else {
                        format!(" (conflicts: {})", view.conflicts.join(", "))
                    },
                    touched.len(),
                );
                let out = ChangesetOutput {
                    command: "status".to_string(),
                    changeset: Some(ChangesetSummary {
                        id: view.id.to_string(),
                        title: view.title.clone(),
                        description: None,
                        status: format!("{:?}", view.status).to_lowercase(),
                        branch: Changeset::branch_for(view.id),
                        base_oid: view.base_oid.clone(),
                        head_oid: view.head_oid.clone(),
                        pr_url: view.pr_url.clone(),
                    }),
                    commits: Some(view.commits),
                    main_oid: Some(view.main_oid),
                    mergeable: Some(view.mergeable),
                    conflicts: (!view.conflicts.is_empty()).then_some(view.conflicts),
                    touched: Some(touched),
                    ..Default::default()
                };
                (text, out)
            }
            ChangesetParams::List { status } => {
                let list = self.changesets.list(subject, status).await?;
                let summaries: Vec<ChangesetSummary> = list.iter().map(Into::into).collect();
                let text = if summaries.is_empty() {
                    "No changesets in this project.".to_string()
                } else {
                    let lines: Vec<String> = summaries
                        .iter()
                        .map(|c| format!("  - {} [{}] {}", c.id, c.status, c.title))
                        .collect();
                    format!("Changesets ({}):\n{}", summaries.len(), lines.join("\n"))
                };
                let out = ChangesetOutput {
                    command: "list".to_string(),
                    changesets: Some(summaries),
                    ..Default::default()
                };
                (text, out)
            }
            ChangesetParams::Bind { id } => {
                self.changesets.bind(subject, id).await?;
                let text = format!("Bound to changeset {id}.");
                let out = ChangesetOutput {
                    command: "bind".to_string(),
                    ..Default::default()
                };
                (text, out)
            }
            ChangesetParams::Unbind => {
                self.changesets.unbind(subject).await?;
                let out = ChangesetOutput {
                    command: "unbind".to_string(),
                    ..Default::default()
                };
                ("Unbound.".to_string(), out)
            }
            ChangesetParams::Discard { id, reason } => {
                let id = self.resolve_id(subject, id).await?;
                let cs = self.changesets.discard(subject, id, reason).await?;
                let text = format!("Changeset {} discarded.", cs.id);
                let out = ChangesetOutput {
                    command: "discard".to_string(),
                    changeset: Some(ChangesetSummary::from(&cs)),
                    ..Default::default()
                };
                (text, out)
            }
        };

        Ok(CHANGESET_OUTPUT.success(text, &out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_open_minimal() {
        let json = serde_json::json!({"command": "open", "title": "curate the docs"});
        let params: ChangesetParams = serde_json::from_value(json).unwrap();
        match params {
            ChangesetParams::Open { title, description } => {
                assert_eq!(title, "curate the docs");
                assert!(description.is_none());
            }
            _ => panic!("expected Open"),
        }
    }

    #[test]
    fn parse_status_without_id() {
        let json = serde_json::json!({"command": "status"});
        let params: ChangesetParams = serde_json::from_value(json).unwrap();
        assert!(matches!(params, ChangesetParams::Status { id: None }));
    }

    #[test]
    fn parse_bind_requires_id() {
        let json = serde_json::json!({"command": "bind"});
        let err = serde_json::from_value::<ChangesetParams>(json).unwrap_err();
        assert!(err.to_string().contains("id"));
    }

    #[test]
    fn command_name_matches_audit_action() {
        assert_eq!(ChangesetParams::Unbind.command_name(), "unbind");
        assert_eq!(
            ChangesetParams::Discard {
                id: None,
                reason: None
            }
            .command_name(),
            "discard"
        );
    }

    #[test]
    fn schema_advertises_every_command() {
        let schema = &*CHANGESET_SCHEMA;
        let cmd_enum = schema
            .pointer("/properties/command/enum")
            .expect("command.enum")
            .as_array()
            .expect("array");
        for cmd in ["open", "status", "list", "bind", "unbind", "discard"] {
            assert!(
                cmd_enum.iter().any(|v| v == cmd),
                "schema missing command {cmd}"
            );
        }
    }
}
