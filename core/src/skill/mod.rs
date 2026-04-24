mod entity;
pub mod error;
pub(crate) mod job;
pub(crate) mod repo;

use std::sync::Arc;

use tracing::instrument;

use crate::library::{DocType, GitFileHash, Library, RuntimeFile, SearchResult};
pub use crate::primitives::*;
use crate::sandbox::Sandboxes;
pub use entity::*;
pub use error::*;
use repo::*;

/// Maximum number of skills to include in the system prompt listing.
const SKILLS_CONTEXT_MAX: usize = 30;

/// Maximum characters for the skill context block.
const SKILLS_CONTEXT_BUDGET: usize = 4000;

/// Maximum characters per skill description in the listing.
const MAX_DESCRIPTION_LEN: usize = 200;

#[derive(Clone)]
pub struct Skills {
    repo: SkillRepo,
    sandboxes: Arc<Sandboxes>,
    library: Option<Library>,
}

impl Skills {
    pub fn new(pool: &sqlx::PgPool, sandboxes: Arc<Sandboxes>, library: Library) -> Self {
        let repo = SkillRepo::new(pool, library.clone());
        Self {
            repo,
            sandboxes,
            library: Some(library),
        }
    }

    pub fn sandboxes(&self) -> &Sandboxes {
        &self.sandboxes
    }

    #[instrument(name = "skill.create", skip_all)]
    pub async fn create(&self, _sub: &AuthSubject, new: NewSkill) -> Result<Skill, SkillError> {
        let skill = self.repo.create(new).await?;
        Ok(skill)
    }

    #[instrument(name = "skill.create_in_op", skip_all)]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        new: NewSkill,
    ) -> Result<Skill, SkillError> {
        let skill = self.repo.create_in_op(op, new).await?;
        Ok(skill)
    }

    #[instrument(name = "skill.find_by_id", skip_all)]
    pub async fn find_by_id(&self, _sub: &AuthSubject, id: SkillId) -> Result<Skill, SkillError> {
        let skill = self.repo.find_by_id(id).await?;
        Ok(skill)
    }

    /// Resolve a skill by name and return its body.
    ///
    /// Lookup order (workspace skill shadows global):
    /// 1. Workspace-scoped skill (if `workspace_id` is `Some`)
    /// 2. Global skill (`workspace_id IS NULL`)
    /// 3. Sandbox exported skill (if `sandbox_id` is `Some`)
    ///
    /// Returns `Ok(None)` when no source matches.
    #[instrument(name = "skill.find_by_name", skip_all, fields(name = %name, sandbox_id))]
    pub async fn find_by_name(
        &self,
        name: &str,
        workspace_id: Option<WorkspaceId>,
        sandbox_id: Option<SandboxId>,
    ) -> Result<Option<SkillBody>, SkillError> {
        // Single query fetches both workspace-scoped and global matches.
        // Workspace-scoped (matching the caller's workspace) wins over global.
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
        let best = workspace_id
            .and_then(|ws_id| candidates.iter().find(|s| s.workspace_id == Some(ws_id)))
            .or_else(|| candidates.iter().find(|s| s.workspace_id.is_none()));
        if let Some(skill) = best {
            return Ok(Some(SkillBody::new(skill.body.clone())));
        }

        // Sandbox fallback
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

    /// Resolve a skill by name and perform `$ARGUMENTS` substitution.
    ///
    /// Combines [`find_by_name`](Self::find_by_name) with argument interpolation:
    /// - If the body contains `$ARGUMENTS`, all occurrences are replaced.
    /// - Otherwise, if arguments are provided they are appended.
    ///
    /// Returns `Ok(None)` when no skill matches.
    #[instrument(name = "skill.interpolate_skill", skip_all, fields(name = %name))]
    pub async fn interpolate_skill(
        &self,
        name: &str,
        workspace_id: Option<WorkspaceId>,
        sandbox_id: Option<SandboxId>,
        arguments: Option<&str>,
    ) -> Result<Option<String>, SkillError> {
        let body = self.find_by_name(name, workspace_id, sandbox_id).await?;
        Ok(body.map(|b| b.interpolate(arguments)))
    }

    /// List workspace-scoped skills (public, with auth check).
    #[instrument(name = "skill.list_by_workspace_id", skip_all)]
    pub async fn list_by_workspace_id(
        &self,
        _sub: &AuthSubject,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Skill>, SkillError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(
                Some(workspace_id),
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }

    /// List skills visible to a workspace: workspace-scoped + global.
    /// Used by system prompt builder and skills context injection.
    #[instrument(name = "skill.list_for_workspace", skip(self))]
    async fn list_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Skill>, SkillError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let ws_result = self
            .repo
            .list_for_workspace_id_by_created_at(
                Some(workspace_id),
                query,
                es_entity::ListDirection::Ascending,
            )
            .await?;
        let global_skills = self.repo.list_global().await?;

        // Merge: workspace skills first, then globals (dedup by name,
        // workspace wins).
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

    /// Hybrid search across workspace + global skills via the library index.
    #[instrument(name = "skill.search", skip(self))]
    pub async fn search(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, SkillError> {
        let library = self
            .library
            .as_ref()
            .ok_or_else(|| SkillError::SandboxLookup("library not configured".to_string()))?;
        // Library search already includes global skills (sentinel nil UUID)
        // alongside workspace-scoped results — see SearchStore::search().
        library
            .search(
                uuid::Uuid::from(workspace_id),
                query,
                Some(DocType::Skill),
                limit,
            )
            .await
            .map_err(SkillError::from)
    }

    /// Build skills context for system prompt injection.
    ///
    /// Returns an `<available_skills>` block listing workspace + global +
    /// sandbox-exported skills with their descriptions, truncated to fit the
    /// character budget. DB skills shadow sandbox skills of the same name.
    /// Returns `None` if there are no skills.
    #[instrument(name = "skill.skills_context_for_workspace", skip(self))]
    pub async fn skills_context_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<String>, SkillError> {
        let db_skills = self.list_for_workspace(workspace_id).await?;

        // Collect sandbox-exported skills, dedup against DB skills.
        let sandbox_skills = match self
            .sandboxes
            .exported_skills_for_workspace(workspace_id)
            .await
        {
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
            let scope = if skill.workspace_id.is_some() {
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

    /// Upsert a skill from a library git file (reverse sync).
    ///
    /// - If the skill already exists and its `file_hash` matches → skip.
    /// - If it exists and the hash differs → update name/description/body.
    /// - If it doesn't exist → create a new entity.
    ///
    /// The post-persist hook fires `library.write_in_op()` (embedding +
    /// WriteToRuntime), but the WriteToRuntime runner will see the file hash
    /// on disk already matches and skip the write.
    pub(crate) async fn begin_op(&self) -> Result<es_entity::DbOp<'_>, SkillError> {
        Ok(self.repo.begin_op().await?)
    }

    /// Upsert a skill from a library file within an existing transaction.
    ///
    /// When the file carries an `original_path` the entity stores it so the
    /// `WriteToRuntime` job can remove the old file after writing the
    /// canonical one.
    #[instrument(name = "skill.upsert_from_library_in_op", skip_all)]
    pub(crate) async fn upsert_from_library_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        file: &RuntimeFile,
        workspace_id: Option<WorkspaceId>,
        file_hash: GitFileHash,
    ) -> Result<(), SkillError> {
        let (doc_id, name, description, body, workspace_name, original_path) = match file {
            RuntimeFile::Skill {
                doc_id,
                name,
                description,
                body,
                workspace_name,
                original_path,
                ..
            } => (
                *doc_id,
                name,
                description,
                body,
                workspace_name.clone(),
                original_path.clone(),
            ),
            _ => return Ok(()),
        };

        if let Some(mut existing) = self.repo.maybe_find_by_id(doc_id).await? {
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
                .body(body.clone())
                .file_hash(Some(file_hash));
            if let Some(ws_id) = workspace_id {
                builder = builder.workspace_id(ws_id);
            }
            if let Some(ws_name) = workspace_name {
                builder = builder.workspace_name(ws_name);
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
        Ok(())
    }

    #[instrument(name = "skill.update", skip_all)]
    pub async fn update(&self, _sub: &AuthSubject, skill: &mut Skill) -> Result<(), SkillError> {
        self.repo.update(skill).await?;
        Ok(())
    }

    #[instrument(name = "skill.delete", skip_all)]
    pub async fn delete(&self, _sub: &AuthSubject, id: SkillId) -> Result<(), SkillError> {
        let skill = self.repo.find_by_id(id).await?;
        self.repo.delete(skill).await?;
        Ok(())
    }

    #[instrument(name = "skill.delete_in_op", skip_all)]
    pub async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: SkillId,
    ) -> Result<(), SkillError> {
        let skill = self.repo.find_by_id(id).await?;
        self.repo.delete_in_op(op, skill).await?;
        Ok(())
    }
}

impl Skills {
    /// Constructor without library sync — for tests or contexts where
    /// git-backed persistence is not needed.
    pub fn new_without_library(pool: &sqlx::PgPool, sandboxes: Arc<Sandboxes>) -> Self {
        let repo = SkillRepo::new_without_library(pool);
        Self {
            repo,
            sandboxes,
            library: None,
        }
    }
}
