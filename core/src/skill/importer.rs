use std::sync::Arc;

use drua_library::{
    DocType as DruaDocType, GitFileHash, LibraryImporter, SearchableFields, UpsertError,
};

use crate::library::AuthedSpaces;
use crate::project::Projects;
use crate::skill::file::{parse_skill_markdown, SKILL_DIR_FILE};
use crate::skill::{ImportScope, Skills};

/// Adapter that lets the drua_library reverse-sync job route
/// skill markdown paths into the Skills service. Three scopes:
///
/// - `runtime/skills/` — global
/// - `runtime/projects/{project}/skills/` — project-scoped
/// - `spaces/{slug}/skills/` — space-scoped (the spaces tree
///   intentionally lives at the repo root, not under `runtime/`; see
///   `library::Spaces::write_file`)
///
/// Each scope accepts two file layouts under `skills/`:
///
/// - flat: `<name>.md`
/// - directory: `<name>/SKILL.md` (the Anthropic Agent Skills layout
///   the sandbox already scans from `.claude/skills/`). Only the
///   `SKILL.md` is a skill; sibling files such as `references/*.md`
///   or `scripts/*` are not claimed here and, in a space, fall
///   through to the `Spaces` catch-all as ordinary space files.
///
/// Built post-`Library::init` (Skills, Projects, and Spaces already
/// exist) and registered via `Library::register_importer`.
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

pub(crate) fn claims_path(path: &str) -> bool {
    if !path.ends_with(".md") {
        return false;
    }
    let parts: Vec<&str> = path.split('/').collect();
    matches!(
        parts.as_slice(),
        ["runtime", "skills", _]
            | ["runtime", "projects", _, "skills", _]
            | ["spaces", _, "skills", _]
            | ["runtime", "skills", _, SKILL_DIR_FILE]
            | ["runtime", "projects", _, "skills", _, SKILL_DIR_FILE]
            | ["spaces", _, "skills", _, SKILL_DIR_FILE]
    )
}

#[async_trait::async_trait]
impl LibraryImporter for SkillsImporter {
    fn matches(&self, path: &str) -> bool {
        claims_path(path)
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

    /// Reverse-sync delete: an external author removed the skill's
    /// markdown file in git. Soft-delete the matching DB row by
    /// id-prefix lookup and return its id so the runner also wipes
    /// the search row. Hand-authored files without a canonical
    /// `<slug>-<id8>.md` suffix can't be resolved and are silently
    /// skipped (same as upsert when the path doesn't parse).
    async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        path: &str,
        _content: &[u8],
    ) -> Result<Option<uuid::Uuid>, UpsertError> {
        let deleted = self
            .skills
            .delete_by_path_in_op(op, path)
            .await
            .map_err(|e| UpsertError::Other(format!("skill reverse-delete: {e}")))?;
        Ok(deleted.map(uuid::Uuid::from))
    }
}

#[cfg(test)]
mod path_claim_tests {
    use super::claims_path;

    #[test]
    fn flat_layout_in_each_scope() {
        assert!(claims_path("runtime/skills/deploy.md"));
        assert!(claims_path("runtime/projects/alpha/skills/deploy.md"));
        assert!(claims_path("spaces/mkt/skills/write-blog-post.md"));
    }

    #[test]
    fn dir_layout_in_each_scope() {
        assert!(claims_path("runtime/skills/deploy/SKILL.md"));
        assert!(claims_path("runtime/projects/alpha/skills/deploy/SKILL.md"));
        assert!(claims_path("spaces/mkt/skills/write-blog-post/SKILL.md"));
    }

    #[test]
    fn dir_layout_claims_only_the_skill_file() {
        // Sibling material beside a SKILL.md is not a skill. In a
        // space it falls through to the Spaces catch-all.
        assert!(!claims_path("spaces/mkt/skills/write-blog-post/README.md"));
        assert!(!claims_path(
            "spaces/mkt/skills/write-blog-post/references/voice.md"
        ));
        assert!(!claims_path(
            "spaces/mkt/skills/write-blog-post/scripts/build.sh"
        ));
        // Case matters: the convention is `SKILL.md`.
        assert!(!claims_path("spaces/mkt/skills/write-blog-post/skill.md"));
    }

    #[test]
    fn nesting_deeper_than_one_directory_is_not_a_skill() {
        assert!(!claims_path("spaces/mkt/skills/a/b/SKILL.md"));
        assert!(!claims_path("runtime/skills/a/b/SKILL.md"));
    }

    #[test]
    fn other_subtrees_untouched() {
        assert!(!claims_path("spaces/mkt/notes/foo.md"));
        assert!(!claims_path("spaces/mkt/_shared/galoy-context.md"));
        assert!(!claims_path("runtime/workflows/daily.yml"));
        assert!(!claims_path("runtime/spaces/mkt/skills/foo.md"));
    }
}
