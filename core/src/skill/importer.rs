use std::sync::Arc;

use drua_library::{
    DocType as DruaDocType, GitFileHash, LibraryImporter, SearchableFields, UpsertError,
};

use crate::library::AuthedSpaces;
use crate::project::Projects;
use crate::skill::file::parse_skill_markdown;
use crate::skill::{ImportScope, Skills};

/// Adapter that lets the drua_library reverse-sync job route
/// `runtime/{skills, projects/*/skills, spaces/*/skills}/*.md` paths
/// into the Skills service. Built post-`Library::init` (Skills,
/// Projects, and Spaces already exist) and registered via
/// `Library::register_importer`.
pub struct SkillsImporter {
    skills: Arc<Skills>,
    projects: Arc<Projects>,
    spaces: AuthedSpaces,
}

impl SkillsImporter {
    pub fn new(skills: Arc<Skills>, projects: Arc<Projects>, spaces: AuthedSpaces) -> Self {
        Self {
            skills,
            projects,
            spaces,
        }
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
            ["runtime", "skills", _]
                | ["runtime", "projects", _, "skills", _]
                | ["runtime", "spaces", _, "skills", _]
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

        // Path determines scope: spaces/* wins over projects/* (paths
        // are mutually exclusive by `parse_skill_markdown` shape).
        let scope = if let Some(slug) = parsed.space_slug.as_deref() {
            match self.spaces.find_by_slug(slug).await {
                Ok(Some(space)) => ImportScope::Space {
                    space_id: space.id,
                    space_slug: space.slug,
                },
                Ok(None) => {
                    tracing::debug!(
                        space_slug = %slug,
                        path,
                        "skill references unknown space; skipping import"
                    );
                    return Ok(None);
                }
                Err(e) => {
                    return Err(UpsertError::Other(format!(
                        "space lookup failed for {slug}: {e}"
                    )))
                }
            }
        } else if let Some(name) = parsed.project_name.as_deref() {
            match self.projects.find_by_name(name).await {
                Ok(Some(project)) => ImportScope::Project {
                    project_id: project.id,
                    project_name: project.name.clone(),
                },
                Ok(None) => {
                    tracing::debug!(
                        project_name = %name,
                        path,
                        "skill references unknown project; skipping import"
                    );
                    return Ok(None);
                }
                Err(e) => {
                    return Err(UpsertError::Other(format!(
                        "project lookup failed for {name}: {e}"
                    )))
                }
            }
        } else {
            ImportScope::Global
        };

        // Always returns Ok(None): when `import_from_library` persists
        // the skill, its `post_persist_hook` (LibrarySyncHook) is what
        // upserts the search row and spawns the embed job. When it
        // signals idempotency (file_hash unchanged) we want to skip
        // the hook AND any sync.rs-side re-index. Either way the
        // importer is just the entity-persist path; the hook owns
        // search+embed for entity-backed doc types.
        self.skills
            .import_from_library(op, parsed, scope)
            .await
            .map_err(|e| UpsertError::Other(format!("skill upsert: {e}")))?;

        Ok(None)
    }
}
