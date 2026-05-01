mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;

use es_entity::AtomicOperation;
use tracing::instrument;

use crate::library::{DocType, GitFileHash, Library, SearchResult};
pub use crate::primitives::*;
use crate::sandbox::Sandboxes;
pub use entity::*;
pub use error::*;
use repo::*;

const SKILLS_CONTEXT_MAX: usize = 30;
const SKILLS_CONTEXT_BUDGET: usize = 4000;
const MAX_DESCRIPTION_LEN: usize = 200;

#[derive(Clone)]
pub struct Skills {
    repo: SkillRepo,
    sandboxes: Arc<Sandboxes>,
    library: Option<Library>,
    pool: Option<sqlx::PgPool>,
    context_generation: ContextGeneration,
}

impl Skills {
    pub fn new(
        pool: &sqlx::PgPool,
        sandboxes: Arc<Sandboxes>,
        library: Library,
        context_generation: ContextGeneration,
    ) -> Self {
        let repo = SkillRepo::new(pool, library.clone());
        Self {
            repo,
            sandboxes,
            library: Some(library),
            pool: Some(pool.clone()),
            context_generation,
        }
    }

    /// No-op when `Skills` has no `pool` (test contexts using `new_without_library`).
    fn register_context_bump<OP: AtomicOperation>(
        &self,
        op: &mut OP,
        project_id: Option<ProjectId>,
    ) {
        let Some(pool) = self.pool.as_ref() else {
            return;
        };
        let hook = ContextBumpHook::new(self.context_generation.clone(), pool.clone(), project_id);
        if op.add_commit_hook(hook).is_err() {
            tracing::warn!("AtomicOperation rejected ContextBumpHook; context bump skipped");
        }
    }

    pub fn sandboxes(&self) -> &Sandboxes {
        &self.sandboxes
    }

    #[instrument(name = "skill.find_by_id", skip_all)]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        id: SkillId,
        project_id: ProjectId,
    ) -> Result<Skill, SkillError> {
        sub.can(AuthVerb::Read, AuthResource::Skill(project_id, Some(id)))?;
        let skill = self.repo.find_by_id(id).await?;
        Ok(skill)
    }

    /// Lookup order (project shadows global):
    /// 1. Project-scoped, 2. Global, 3. Sandbox-exported.
    /// `Ok(None)` when no source matches.
    #[instrument(name = "skill.find_by_name", skip_all, fields(name = %name, sandbox_id))]
    pub async fn find_by_name(
        &self,
        name: &str,
        project_id: Option<ProjectId>,
        sandbox_id: Option<SandboxId>,
    ) -> Result<Option<SkillBody>, SkillError> {
        // Single query fetches project-scoped + global; project wins.
        let query = es_entity::PaginatedQueryArgs {
            first: 10,
            after: None,
        };
        let result = self
            .repo
            .list_for_name_by_created_at(
                name.to_string(),
                query,
                es_entity::ListDirection::Ascending,
            )
            .await?;
        let candidates = result.entities;
        let best = project_id
            .and_then(|project_id| candidates.iter().find(|s| s.project_id == Some(project_id)))
            .or_else(|| candidates.iter().find(|s| s.project_id.is_none()));
        if let Some(skill) = best {
            return Ok(Some(SkillBody::new(skill.body.clone())));
        }

        if let Some(sandbox_id) = sandbox_id {
            tracing::Span::current().record("sandbox_id", sandbox_id.to_string());
            let sandbox = self
                .sandboxes
                .maybe_find_by_id(sandbox_id)
                .await
                .map_err(|e| SkillError::SandboxLookup(e.to_string()))?;
            if let Some(body) = sandbox.and_then(|s| s.find_skill(name)) {
                return Ok(Some(SkillBody::new(body)));
            }
        }

        Ok(None)
    }

    /// Combines [`find_by_name`](Self::find_by_name) with argument interpolation.
    /// `Ok(None)` when no skill matches.
    #[instrument(name = "skill.interpolate_skill", skip_all, fields(name = %name))]
    pub async fn interpolate_skill(
        &self,
        name: &str,
        project_id: Option<ProjectId>,
        sandbox_id: Option<SandboxId>,
        arguments: Option<&str>,
    ) -> Result<Option<String>, SkillError> {
        let body = self.find_by_name(name, project_id, sandbox_id).await?;
        Ok(body.map(|b| b.interpolate(arguments)))
    }

    #[instrument(name = "skill.list_by_project_id", skip_all)]
    pub async fn list_by_project_id(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<Skill>, SkillError> {
        sub.can(AuthVerb::Read, AuthResource::Skill(project_id, None))?;
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_project_id_by_created_at(
                Some(project_id),
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }

    /// Project-scoped + global, with auth check.
    #[instrument(name = "skill.list_for_project", skip(self, sub))]
    pub async fn list_for_project(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<Skill>, SkillError> {
        sub.can(AuthVerb::Read, AuthResource::Skill(project_id, None))?;
        self.list_for_project_inner(project_id).await
    }

    async fn list_for_project_inner(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Skill>, SkillError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let ws_result = self
            .repo
            .list_for_project_id_by_created_at(
                Some(project_id),
                query,
                es_entity::ListDirection::Ascending,
            )
            .await?;
        let global_query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let global_result = self
            .repo
            .list_for_project_id_by_created_at(
                None,
                global_query,
                es_entity::ListDirection::Ascending,
            )
            .await?;
        let global_skills = global_result.entities;

        // Project first, dedup by name; project wins over global.
        let mut seen_names = std::collections::HashSet::new();
        let mut merged = Vec::new();
        for skill in ws_result.entities {
            seen_names.insert(skill.name.clone());
            merged.push(skill);
        }
        for skill in global_skills {
            if !seen_names.contains(&skill.name) {
                merged.push(skill);
            }
        }
        Ok(merged)
    }

    #[instrument(name = "skill.search", skip(self))]
    pub async fn search(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, SkillError> {
        sub.can(AuthVerb::Read, AuthResource::Skill(project_id, None))?;
        let library = self
            .library
            .as_ref()
            .ok_or_else(|| SkillError::SandboxLookup("library not configured".to_string()))?;
        // SearchStore::search() includes globals (sentinel nil UUID).
        library
            .search(
                uuid::Uuid::from(project_id),
                query,
                Some(DocType::Skill),
                limit,
            )
            .await
            .map_err(SkillError::from)
    }

    /// Build skills context for system prompt injection.
    ///
    /// Returns an `<available_skills>` block listing project + global +
    /// sandbox-exported skills with their descriptions, truncated to fit the
    /// character budget. DB skills shadow sandbox skills of the same name.
    /// Returns `None` if there are no skills.
    #[instrument(name = "skill.skills_context_for_project", skip(self))]
    pub async fn skills_context_for_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<String>, SkillError> {
        let db_skills = self.list_for_project_inner(project_id).await?;

        // Collect sandbox-exported skills, dedup against DB skills.
        let sandbox_skills = match self.sandboxes.exported_skills_for_project(project_id).await {
            Ok(skills) => skills,
            Err(e) => {
                tracing::warn!(error = %e, "failed to fetch sandbox exported skills");
                Vec::new()
            }
        };

        // Build a unified listing: (name, scope_tag, description)
        let mut seen_names = std::collections::HashSet::new();
        let mut entries: Vec<(&str, &str, String)> = Vec::new();

        for skill in &db_skills {
            seen_names.insert(skill.name.as_str());
            let scope = if skill.project_id.is_some() {
                ""
            } else {
                " [global]"
            };
            entries.push((&skill.name, scope, skill.description.clone()));
        }
        for skill in &sandbox_skills {
            if seen_names.contains(skill.name.as_str()) {
                continue;
            }
            seen_names.insert(&skill.name);
            let desc = skill.description.clone().unwrap_or_default();
            entries.push((&skill.name, " [sandbox]", desc));
        }

        if entries.is_empty() {
            return Ok(None);
        }

        let header = "<available_skills>\n\
             The following skills are available. Use the \
             `use_skill` tool to invoke them when relevant to the user's request.\n\
             When the user types /name, always invoke that skill via use_skill.\n\
             Use `use_skill` with action \"search\" to discover skills by topic.\n\n";
        let footer = "</available_skills>";

        let mut buf = String::from(header);
        let mut remaining = SKILLS_CONTEXT_BUDGET
            .saturating_sub(header.len())
            .saturating_sub(footer.len());

        for (name, scope, description) in entries.iter().take(SKILLS_CONTEXT_MAX) {
            let desc: String = description.chars().take(MAX_DESCRIPTION_LEN).collect();
            let truncated = if desc.len() < description.len() {
                format!("{desc}...")
            } else {
                desc
            };
            let line = format!("- {name}{scope} — {truncated}\n");
            if line.len() > remaining {
                buf.push_str("- ... (more skills available — use search to find them)\n");
                break;
            }
            buf.push_str(&line);
            remaining -= line.len();
        }

        buf.push_str(footer);
        Ok(Some(buf))
    }

    /// DB-first; `post_persist_hook(sync_to_library)` writes the
    /// canonical file asynchronously.
    #[instrument(name = "skill.create", skip(self, body))]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        project_name: &str,
        name: String,
        description: String,
        body: String,
    ) -> Result<Skill, SkillError> {
        sub.can(AuthVerb::Create, AuthResource::Skill(project_id, None))?;
        if name.trim().is_empty() {
            return Err(SkillError::BuildEntity("skill name required".into()));
        }

        let new = NewSkill::builder()
            .id(SkillId::new())
            .project_id(project_id)
            .project_name(project_name)
            .name(name)
            .description(description)
            .body(body)
            .build()
            .map_err(|e| SkillError::BuildEntity(e.to_string()))?;

        let skill = self.repo.create(new).await?;
        Ok(skill)
    }

    #[instrument(name = "skill.update", skip(self, body))]
    pub async fn update(
        &self,
        sub: &AuthSubject,
        id: SkillId,
        project_id: ProjectId,
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
    ) -> Result<Skill, SkillError> {
        sub.can(AuthVerb::Update, AuthResource::Skill(project_id, Some(id)))?;
        let mut skill = self.repo.find_by_id(id).await?;
        if skill.project_id != Some(project_id) {
            return Err(SkillError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Update,
                resource: AuthResource::Skill(project_id, Some(id)),
            }));
        }
        if skill.update_content(name, description, body).did_execute() {
            self.repo.update(&mut skill).await?;
        }
        Ok(skill)
    }

    /// Soft-delete only — the on-disk file is left in the library
    /// until the owning project is deleted (single-file removal
    /// isn't plumbed through `WriteToRuntime` yet).
    #[instrument(name = "skill.delete", skip(self))]
    pub async fn delete(
        &self,
        sub: &AuthSubject,
        id: SkillId,
        project_id: ProjectId,
    ) -> Result<(), SkillError> {
        sub.can(AuthVerb::Delete, AuthResource::Skill(project_id, Some(id)))?;
        let skill = self.repo.find_by_id(id).await?;
        if skill.project_id != Some(project_id) {
            return Err(SkillError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Delete,
                resource: AuthResource::Skill(project_id, Some(id)),
            }));
        }
        let mut op = self.repo.begin_op().await?;
        self.repo.delete_in_op(&mut op, skill).await?;
        op.commit().await?;
        Ok(())
    }
}

/// Extracts the markdown body from a canonical skill file (everything after
/// the closing `---` of the frontmatter, leading/trailing newlines trimmed).
fn extract_skill_body(rendered: &str) -> Option<String> {
    let after_first = rendered.strip_prefix("---")?;
    let (_, after_fm) = after_first.split_once("\n---")?;
    Some(
        after_fm
            .trim_start_matches('\n')
            .trim_end_matches('\n')
            .to_string(),
    )
}

impl crate::library::LibraryImporter for Skills {
    fn matches(&self, path: &str) -> bool {
        crate::library::matches_runtime_subdir(path, "skills", "md")
    }

    fn doc_type(&self) -> DocType {
        DocType::Skill
    }

    fn parse(&self, content: &[u8], path: &str) -> Option<crate::library::ParsedFile> {
        let content = std::str::from_utf8(content).ok()?;
        crate::library::parse_skill_markdown(content, path)
    }

    /// Upsert a skill from a library file. When the file carries an
    /// `original_path` the entity stores it so the `WriteToRuntime` job
    /// can remove the old file after writing the canonical one. The
    /// skill content is `(title, body)` projected onto
    /// `(name, description)`; the on-disk markdown body is reconstructed
    /// from the rendered file.
    #[instrument(name = "skill.library_importer.upsert_in_op", skip_all)]
    async fn upsert_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        file: &crate::library::SyncedFile,
        _path: &str,
        project_id: Option<ProjectId>,
        file_hash: GitFileHash,
    ) -> Result<(), crate::library::UpsertError> {
        if file.doc_type != DocType::Skill {
            return Ok(());
        }
        let doc_id = SkillId::from(file.doc_id);
        let name = &file.title;
        let description = &file.body;
        let body = extract_skill_body(&file.rendered).unwrap_or_default();
        let project_name = file.project_name.clone();
        let original_path = file.original_path.clone();

        if let Some(mut existing) = self.repo.maybe_find_by_id_in_op(&mut *op, doc_id).await? {
            if existing
                .update(
                    Some(name.clone()),
                    Some(description.clone()),
                    Some(body.clone()),
                    file_hash,
                )
                .did_execute()
            {
                self.repo.update_in_op(op, &mut existing).await?;
            }
            tracing::info!(id = %doc_id, name = %name, "updated skill from library");
        } else {
            let mut builder = NewSkill::builder()
                .id(doc_id)
                .name(name.clone())
                .description(description.clone())
                .body(body);
            if let Some(project_id) = project_id {
                builder = builder.project_id(project_id);
            }
            if let Some(ws_name) = project_name {
                builder = builder.project_name(ws_name);
            }
            builder = builder.original_path(original_path.ok_or_else(|| {
                SkillError::BuildEntity("original_path required for library import".into())
            })?);
            let new = builder
                .build()
                .map_err(|e| SkillError::BuildEntity(e.to_string()))?;
            self.repo.create_in_op(op, new).await?;
            tracing::info!(id = %doc_id, name = %name, "created skill from library");
        }
        self.register_context_bump(op, project_id);
        Ok(())
    }

    // TODO: implement entity deletion when service supports delete-by-id-prefix.
    // Today's typed runner has no delete handling either, so we preserve parity.
    async fn delete_in_op(
        &self,
        _op: &mut es_entity::DbOp<'_>,
        path: &str,
    ) -> Result<(), crate::library::UpsertError> {
        tracing::warn!(path, "skill delete from library not yet supported");
        Ok(())
    }
}

impl Skills {
    /// Bulk soft-delete all project-scoped skills within a transaction.
    /// Used during project cascade deletion.
    #[instrument(name = "skill.delete_for_project_in_op", skip_all)]
    pub(crate) async fn delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), SkillError> {
        self.repo
            .cascade_delete_for_project_in_op(op, project_id)
            .await?;
        Ok(())
    }

    /// Constructor without library sync — for tests or contexts where
    /// git-backed persistence is not needed.
    pub fn new_without_library(pool: &sqlx::PgPool, sandboxes: Arc<Sandboxes>) -> Self {
        let repo = SkillRepo::new_without_library(pool);
        Self {
            repo,
            sandboxes,
            library: None,
            pool: None,
            context_generation: ContextGeneration::new(),
        }
    }
}

#[cfg(test)]
mod skill_extract_tests {
    use super::extract_skill_body;

    #[test]
    fn extracts_body_from_canonical_skill_markdown() {
        let rendered = "---\nid: 0\nname: \"X\"\ndescription: \"Y\"\ncreated: \nupdated: \n---\n\n#!/bin/bash\necho hi\n";
        assert_eq!(
            extract_skill_body(rendered).unwrap(),
            "#!/bin/bash\necho hi"
        );
    }

    #[test]
    fn returns_empty_when_body_absent() {
        let rendered = "---\nid: 0\n---\n\n";
        assert_eq!(extract_skill_body(rendered).unwrap(), "");
    }
}
