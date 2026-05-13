use std::sync::Arc;

use drua_library::{
    DocType as DruaDocType, GitFileHash, LibraryImporter, SearchableFields, UpsertError,
};

use crate::project::Projects;
use crate::workflow::yaml::parse_workflow_yaml;
use crate::workflow::Workflows;

/// Adapter that lets the drua_library reverse-sync job route
/// `runtime/projects/*/workflows/*.yml` paths into the Workflows
/// service.
pub struct WorkflowsImporter {
    workflows: Arc<Workflows>,
    projects: Arc<Projects>,
}

impl WorkflowsImporter {
    pub fn new(workflows: Arc<Workflows>, projects: Arc<Projects>) -> Self {
        Self {
            workflows,
            projects,
        }
    }
}

#[async_trait::async_trait]
impl LibraryImporter for WorkflowsImporter {
    fn matches(&self, path: &str) -> bool {
        if !path.ends_with(".yml") {
            return false;
        }
        let parts: Vec<&str> = path.split('/').collect();
        matches!(
            parts.as_slice(),
            ["runtime", "workflows", _] | ["runtime", "projects", _, "workflows", _]
        )
    }

    fn doc_type(&self) -> DruaDocType {
        DruaDocType::new("workflow")
    }

    async fn upsert_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        _old_file_hash: Option<GitFileHash>,
        _file_hash: GitFileHash,
        path: &str,
        content: &[u8],
    ) -> Result<Option<SearchableFields>, UpsertError> {
        let content_str = std::str::from_utf8(content)
            .map_err(|e| UpsertError::Parse(format!("non-utf8 workflow content: {e}")))?;
        let Some(parsed) = parse_workflow_yaml(content_str, path) else {
            return Ok(None);
        };

        let project_name = parsed
            .project_name
            .as_deref()
            .ok_or_else(|| UpsertError::Parse(format!("workflow at {path} requires a project")))?;

        let project = match self.projects.find_by_name(project_name).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                tracing::warn!(
                    project_name,
                    path,
                    "workflow references unknown project; skipping"
                );
                return Ok(None);
            }
            Err(e) => {
                return Err(UpsertError::Other(format!(
                    "project lookup failed for {project_name}: {e}"
                )))
            }
        };

        // Always returns Ok(None): see SkillsImporter for the rationale
        // — the post_persist_hook (LibrarySyncHook) on the workflow
        // repo is the single source of truth for search+embed when
        // the entity changes; the importer is just the entity-persist
        // path.
        self.workflows
            .import_from_library(op, parsed, project.id)
            .await
            .map_err(|e| UpsertError::Other(format!("workflow upsert: {e}")))?;

        Ok(None)
    }

    /// Reverse-sync delete: the YAML at `path` was removed in git.
    /// Resolve `(project_name, canonical-path)` from the path, look
    /// up the live workflow under that project, soft-delete it +
    /// wipe its search row. Paths that don't parse into the
    /// `runtime/projects/{name}/workflows/{slug}.yml` shape, or that
    /// reference an unknown project, are silently skipped (mirrors
    /// `upsert_in_op`'s unknown-project behaviour and the skill
    /// importer's hand-authored-path policy).
    async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        path: &str,
    ) -> Result<Option<uuid::Uuid>, UpsertError> {
        let Some(project_name) = crate::workflow::yaml::project_name_from_workflow_path(path)
        else {
            return Ok(None);
        };
        let project = match self.projects.find_by_name(&project_name).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                tracing::debug!(
                    project_name,
                    path,
                    "workflow delete references unknown project; skipping"
                );
                return Ok(None);
            }
            Err(e) => {
                return Err(UpsertError::Other(format!(
                    "project lookup failed for {project_name}: {e}"
                )))
            }
        };
        let deleted = self
            .workflows
            .delete_by_path_in_op(op, project.id, path)
            .await
            .map_err(|e| UpsertError::Other(format!("workflow reverse-delete: {e}")))?;
        Ok(deleted.map(uuid::Uuid::from))
    }
}
