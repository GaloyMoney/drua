use std::sync::Arc;

use drua_library::{
    DocType as DruaDocType, GitFileHash, LibraryImporter, SearchableFields, UpsertError,
};

use crate::project::Projects;
use crate::skill::file::parse_skill_markdown;
use crate::skill::Skills;

/// Adapter that lets the drua_library reverse-sync job route
/// `runtime/{skills,projects/*/skills}/*.md` paths into the Skills
/// service. Built post-`Library::init` (Skills + Projects already
/// exist) and registered via `Library::register_importer`.
pub struct SkillsImporter {
    skills: Arc<Skills>,
    projects: Arc<Projects>,
}

impl SkillsImporter {
    pub fn new(skills: Arc<Skills>, projects: Arc<Projects>) -> Self {
        Self { skills, projects }
    }
}

#[async_trait::async_trait]
impl LibraryImporter for SkillsImporter {
    fn matches(&self, path: &str) -> bool {
        if !path.ends_with(".md") {
            return false;
        }
        let parts: Vec<&str> = path.split('/').collect();
        matches!(
            parts.as_slice(),
            ["runtime", "skills", _] | ["runtime", "projects", _, "skills", _]
        )
    }

    fn doc_type(&self) -> DruaDocType {
        DruaDocType::new("skill")
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
            .map_err(|e| UpsertError::Parse(format!("non-utf8 skill content: {e}")))?;
        let Some(parsed) = parse_skill_markdown(content_str, path) else {
            return Ok(None);
        };

        let project_id = match parsed.project_name.as_deref() {
            Some(name) => match self.projects.find_by_name(name).await {
                Ok(Some(project)) => Some(project.id),
                Ok(None) => {
                    tracing::warn!(
                        project_name = %name,
                        path,
                        "skill references unknown project; importing as global"
                    );
                    None
                }
                Err(e) => {
                    return Err(UpsertError::Other(format!(
                        "project lookup failed for {name}: {e}"
                    )))
                }
            },
            None => None,
        };

        let parsed_for_persist = parsed.clone();
        let skill = self
            .skills
            .import_from_library(op, parsed_for_persist, project_id)
            .await
            .map_err(|e| UpsertError::Other(format!("skill upsert: {e}")))?;

        Ok(Some(SearchableFields {
            doc_id: uuid::Uuid::from(skill.id),
            doc_type: DruaDocType::new("skill"),
            scope_id: project_id.map(uuid::Uuid::from),
            scope_slug: parsed.project_name,
            name: parsed.name,
            path: Some(path.to_string()),
            content: parsed.description,
        }))
    }
}
