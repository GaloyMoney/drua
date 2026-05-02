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

        let project_id = project.id;
        let parsed_for_search = ParsedSearch::from(&parsed);
        self.workflows
            .import_from_library(op, parsed, project_id)
            .await
            .map_err(|e| UpsertError::Other(format!("workflow upsert: {e}")))?;

        Ok(Some(SearchableFields {
            doc_id: parsed_for_search.doc_id,
            doc_type: DruaDocType::new("workflow"),
            scope_id: Some(uuid::Uuid::from(project_id)),
            scope_slug: parsed_for_search.scope_slug,
            name: parsed_for_search.name,
            path: Some(path.to_string()),
            content: parsed_for_search.content,
        }))
    }
}

struct ParsedSearch {
    doc_id: uuid::Uuid,
    scope_slug: Option<String>,
    name: String,
    content: String,
}

impl From<&crate::workflow::yaml::ParsedWorkflow> for ParsedSearch {
    fn from(p: &crate::workflow::yaml::ParsedWorkflow) -> Self {
        Self {
            doc_id: uuid::Uuid::from(p.workflow_id),
            scope_slug: p.project_name.clone(),
            name: p.name.clone(),
            content: p.description.clone().unwrap_or_default(),
        }
    }
}
