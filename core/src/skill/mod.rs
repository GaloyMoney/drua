mod entity;
pub mod error;
pub mod file;
pub mod importer;
pub(crate) mod repo;

pub use file::{name_from_filename, parse_skill_markdown, ParsedSkill};
pub use importer::SkillsImporter;

pub const SKILL_DOC_TYPE: drua_library::DocType = drua_library::DocType::new("skill");

use std::sync::Arc;

use drua_library::SearchHit;
use es_entity::AtomicOperation;
use tracing::instrument;

use crate::agent::AgentScope;
use crate::audit::Audit;
pub use crate::primitives::*;
use crate::sandbox::Sandboxes;
use crate::user::Users;
pub use entity::*;
pub use error::*;
use repo::*;

const SKILLS_CONTEXT_MAX: usize = 30;
const SKILLS_CONTEXT_BUDGET: usize = 4000;
const MAX_DESCRIPTION_LEN: usize = 200;

/// Resolved scope for `Skills::import_from_library`. The importer maps a
/// path prefix (`runtime/projects/{p}/skills/...`,
/// `runtime/spaces/{s}/skills/...`, or `runtime/skills/...`) to one of
/// these variants by looking up the project / space referenced by the
/// path segment.
#[derive(Debug, Clone)]
pub enum ImportScope {
    Global,
    Project {
        project_id: ProjectId,
        project_name: String,
    },
    Space {
        space_id: SpaceId,
        space_slug: String,
    },
}

impl ImportScope {
    fn bump_scope(&self) -> ScopeId {
        match self {
            ImportScope::Global => ScopeId::Global,
            ImportScope::Project { project_id, .. } => ScopeId::Project(*project_id),
            ImportScope::Space { space_id, .. } => ScopeId::Space(*space_id),
        }
    }
}

#[derive(Clone)]
pub struct Skills {
    repo: SkillRepo,
    sandboxes: Arc<Sandboxes>,
    library: Option<drua_library::Library>,
    /// `None` in test contexts (`new_without_library`); `Some` everywhere
    /// else. Used to wire `ContextBumpHook` onto mutation ops.
    pool: Option<sqlx::PgPool>,
    /// `None` in test contexts (`new_without_library`); `Some` everywhere
    /// else. Used to populate `EventContext` with `CommitAttribution` so
    /// the post-persist `library.sync_entity_in_op` hook picks up rich
    /// commit author / committer info.
    users: Option<Arc<Users>>,
    context_generation: ContextGeneration,
}

impl Skills {
    pub fn new(
        pool: &sqlx::PgPool,
        sandboxes: Arc<Sandboxes>,
        library: drua_library::Library,
        users: Arc<Users>,
        context_generation: ContextGeneration,
    ) -> Self {
        let repo = SkillRepo::new(pool, library.clone());
        Self {
            repo,
            sandboxes,
            library: Some(library),
            pool: Some(pool.clone()),
            users: Some(users),
            context_generation,
        }
    }

    /// Side-effect: populates `EventContext` with the rich
    /// [`drua_library::CommitAttribution`] so post-persist library
    /// sync hooks pick it up. No-op when `users` isn't wired (test
    /// constructors via `new_without_library`).
    async fn populate_attribution(&self) {
        if let Some(users) = self.users.as_ref() {
            let _ = users.commit_attribution().await;
        }
    }

    /// Begin a mutation op with a `ContextBumpHook` registered for `scope`,
    /// so committing the op bumps the local `ContextGeneration` and fires a
    /// `context_changed` PG NOTIFY. All Skill mutations must go through this
    /// helper rather than `repo.begin_op()` so agents see fresh skills.
    /// Falls back to a plain `repo.begin_op()` when `pool` is `None`
    /// (test contexts using `new_without_library`).
    async fn begin_op(&self, scope: ScopeId) -> Result<es_entity::DbOp<'static>, SkillError> {
        let mut op = self.repo.begin_op().await?;
        if let Some(pool) = self.pool.as_ref() {
            let hook =
                ContextBumpHook::new(self.context_generation.clone(), pool.clone(), scope);
            op.add_commit_hook(hook)
                .expect("DbOp supports commit hooks");
        }
        Ok(op)
    }

    /// Composable variant: registers a `ContextBumpHook` on the caller's
    /// `op` for `scope`. Used by reverse-sync importers and other internal
    /// callers that already own a transaction. No-op when `pool` is `None`.
    pub(crate) fn register_context_bump<OP: AtomicOperation>(
        &self,
        op: &mut OP,
        scope: ScopeId,
    ) {
        let Some(pool) = self.pool.as_ref() else {
            return;
        };
        let hook = ContextBumpHook::new(self.context_generation.clone(), pool.clone(), scope);
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
    ) -> Result<Vec<SearchHit>, SkillError> {
        sub.can(AuthVerb::Read, AuthResource::Skill(project_id, None))?;
        let library = self
            .library
            .as_ref()
            .ok_or_else(|| SkillError::SandboxLookup("library not configured".to_string()))?;
        Ok(library
            .search()
            .search(query, None, &[], &[SKILL_DOC_TYPE], &[], limit)
            .await?)
    }

    /// Wraps [`Self::skills_context_for_scope`] with a single-tier scope —
    /// only `(project_id, attached_sandbox_id)`, no mounted spaces. Kept as
    /// the entry point used by `attach_sandbox` / `detach_sandbox`'s skills
    /// refresh, which doesn't need to fold space skills back in (the sandbox
    /// dimension is the only thing changing in those paths).
    #[instrument(name = "skill.skills_context_for_agent", skip(self))]
    pub async fn skills_context_for_agent(
        &self,
        project_id: ProjectId,
        sandbox_id: Option<SandboxId>,
    ) -> Result<Option<String>, SkillError> {
        let scope = AgentScope {
            project_id,
            attached_sandbox_id: sandbox_id,
            mounted_space_ids: Vec::new(),
        };
        self.skills_context_for_scope(&scope).await
    }

    /// Build the `<available_skills>` block for an agent.
    ///
    /// Folds the agent's full scope: project + global DB skills, every
    /// mounted space's DB skills, and the attached sandbox's exported
    /// skills (when `Ready`). Same-name conflicts resolve by precedence —
    /// **Space > Sandbox > Project > Global** — so a space skill called
    /// `deploy` shadows a project skill of the same name. Returns `None`
    /// if no skill source contributes anything.
    #[instrument(name = "skill.skills_context_for_scope", skip(self, scope))]
    pub async fn skills_context_for_scope(
        &self,
        scope: &AgentScope,
    ) -> Result<Option<String>, SkillError> {
        let project_and_global = self.list_for_project_inner(scope.project_id).await?;

        let mut space_skills: Vec<Skill> = Vec::new();
        for space_id in &scope.mounted_space_ids {
            let res = self
                .repo
                .list_for_space_id_by_created_at(
                    Some(*space_id),
                    es_entity::PaginatedQueryArgs {
                        first: 100,
                        after: None,
                    },
                    es_entity::ListDirection::Ascending,
                )
                .await?;
            space_skills.extend(res.entities);
        }

        let sandbox_skills = match self
            .sandboxes
            .exported_skills_for_sandbox(scope.attached_sandbox_id)
            .await
        {
            Ok(skills) => skills,
            Err(e) => {
                tracing::warn!(error = %e, "failed to fetch sandbox exported skills");
                Vec::new()
            }
        };

        // Precedence (highest first): Space > Sandbox > Project > Global.
        // First insert wins; later same-name entries are dropped.
        let mut seen_names = std::collections::HashSet::new();
        let mut entries: Vec<(String, &'static str, String)> = Vec::new();

        for skill in &space_skills {
            if seen_names.insert(skill.name.clone()) {
                entries.push((skill.name.clone(), " [space]", skill.description.clone()));
            }
        }
        for skill in &sandbox_skills {
            if seen_names.insert(skill.name.clone()) {
                let desc = skill.description.clone().unwrap_or_default();
                entries.push((skill.name.clone(), "", desc));
            }
        }
        for skill in &project_and_global {
            if seen_names.insert(skill.name.clone()) {
                let scope_tag = if skill.project_id.is_some() {
                    ""
                } else {
                    " [global]"
                };
                entries.push((skill.name.clone(), scope_tag, skill.description.clone()));
            }
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
    /// canonical file asynchronously. Routes through `begin_op` so commit
    /// fires `ContextBumpHook(Project)` — agents see the new skill on the
    /// next `send_message`.
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
        Audit::record_action_if_unset("skill.create");
        Audit::record_project_id(project_id);
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

        let mut op = self.begin_op(ScopeId::Project(project_id)).await?;
        self.populate_attribution().await;
        let skill = self.repo.create_in_op(&mut op, new).await?;
        Audit::record_skill_id(skill.id);
        op.commit().await?;
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
        Audit::record_action_if_unset("skill.update");
        Audit::record_project_id(project_id);
        Audit::record_skill_id(id);
        let mut op = self.begin_op(ScopeId::Project(project_id)).await?;
        let mut skill = self.repo.find_by_id_in_op(&mut op, id).await?;
        if skill.project_id != Some(project_id) {
            return Err(SkillError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Update,
                resource: AuthResource::Skill(project_id, Some(id)),
            }));
        }
        if skill.update_content(name, description, body).did_execute() {
            self.populate_attribution().await;
            self.repo.update_in_op(&mut op, &mut skill).await?;
        }
        op.commit().await?;
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
        Audit::record_action_if_unset("skill.delete");
        Audit::record_project_id(project_id);
        Audit::record_skill_id(id);
        let mut op = self.begin_op(ScopeId::Project(project_id)).await?;
        let skill = self.repo.find_by_id_in_op(&mut op, id).await?;
        if skill.project_id != Some(project_id) {
            return Err(SkillError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Delete,
                resource: AuthResource::Skill(project_id, Some(id)),
            }));
        }
        self.populate_attribution().await;
        self.repo.delete_in_op(&mut op, skill).await?;
        op.commit().await?;
        Ok(())
    }

    /// Create a space-owned skill. Auth: `AuthVerb::Create` against
    /// `AuthResource::SpaceSkill(space_id, None)` — admin-only by
    /// default (no `permits` rule for non-admin scopes). Twin-writes to
    /// `runtime/spaces/{slug}/skills/{name-slug}-{id_prefix}.md` via
    /// the entity's `LibrarySynced::write_op`.
    #[instrument(name = "skill.create_in_space", skip(self, body))]
    pub async fn create_in_space(
        &self,
        sub: &AuthSubject,
        space_id: SpaceId,
        space_slug: &str,
        name: String,
        description: String,
        body: String,
    ) -> Result<Skill, SkillError> {
        sub.can(AuthVerb::Create, AuthResource::SpaceSkill(space_id, None))?;
        Audit::record_action_if_unset("skill.create_in_space");
        Audit::record_space_id(space_id);
        if name.trim().is_empty() {
            return Err(SkillError::BuildEntity("skill name required".into()));
        }
        let new = NewSkill::builder()
            .id(SkillId::new())
            .space_id(space_id)
            .space_slug(space_slug.to_string())
            .name(name)
            .description(description)
            .body(body)
            .build()
            .map_err(|e| SkillError::BuildEntity(e.to_string()))?;

        let mut op = self.begin_op(ScopeId::Space(space_id)).await?;
        let skill = self.repo.create_in_op(&mut op, new).await?;
        Audit::record_skill_id(skill.id);
        op.commit().await?;
        Ok(skill)
    }

    #[instrument(name = "skill.update_in_space", skip(self, body))]
    pub async fn update_in_space(
        &self,
        sub: &AuthSubject,
        id: SkillId,
        space_id: SpaceId,
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
    ) -> Result<Skill, SkillError> {
        sub.can(
            AuthVerb::Update,
            AuthResource::SpaceSkill(space_id, Some(id)),
        )?;
        Audit::record_action_if_unset("skill.update_in_space");
        Audit::record_space_id(space_id);
        Audit::record_skill_id(id);
        let mut op = self.begin_op(ScopeId::Space(space_id)).await?;
        let mut skill = self.repo.find_by_id_in_op(&mut op, id).await?;
        if skill.space_id != Some(space_id) {
            return Err(SkillError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Update,
                resource: AuthResource::SpaceSkill(space_id, Some(id)),
            }));
        }
        if skill.update_content(name, description, body).did_execute() {
            self.repo.update_in_op(&mut op, &mut skill).await?;
        }
        op.commit().await?;
        Ok(skill)
    }

    /// Soft-delete only — same caveat as [`Self::delete`].
    #[instrument(name = "skill.delete_in_space", skip(self))]
    pub async fn delete_in_space(
        &self,
        sub: &AuthSubject,
        id: SkillId,
        space_id: SpaceId,
    ) -> Result<(), SkillError> {
        sub.can(
            AuthVerb::Delete,
            AuthResource::SpaceSkill(space_id, Some(id)),
        )?;
        Audit::record_action_if_unset("skill.delete_in_space");
        Audit::record_space_id(space_id);
        Audit::record_skill_id(id);
        let mut op = self.begin_op(ScopeId::Space(space_id)).await?;
        let skill = self.repo.find_by_id_in_op(&mut op, id).await?;
        if skill.space_id != Some(space_id) {
            return Err(SkillError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Delete,
                resource: AuthResource::SpaceSkill(space_id, Some(id)),
            }));
        }
        self.repo.delete_in_op(&mut op, skill).await?;
        op.commit().await?;
        Ok(())
    }

    /// List skills owned by `space_id` (no global fold-in). Admin-only.
    #[instrument(name = "skill.list_for_space", skip(self, sub))]
    pub async fn list_for_space(
        &self,
        sub: &AuthSubject,
        space_id: SpaceId,
    ) -> Result<Vec<Skill>, SkillError> {
        sub.can(AuthVerb::Read, AuthResource::SpaceSkill(space_id, None))?;
        let result = self
            .repo
            .list_for_space_id_by_created_at(
                Some(space_id),
                es_entity::PaginatedQueryArgs {
                    first: 100,
                    after: None,
                },
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }
}

/// Extracts the markdown body from a canonical skill file (everything after
/// the closing `---` of the frontmatter, leading/trailing newlines trimmed).
#[allow(dead_code)]
pub(crate) fn extract_skill_body(rendered: &str) -> Option<String> {
    let after_first = rendered.strip_prefix("---")?;
    let (_, after_fm) = after_first.split_once("\n---")?;
    Some(
        after_fm
            .trim_start_matches('\n')
            .trim_end_matches('\n')
            .to_string(),
    )
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

    /// Reverse-sync entry point: persist a `ParsedSkill` produced by
    /// the library importer. Creates or updates depending on whether
    /// the skill already exists. `Ok(None)` signals idempotency — the
    /// existing entity's `file_hash` matches the incoming bytes (i.e.
    /// the round-trip is hitting a forward-write we already handled),
    /// so the caller should skip search re-upsert + embed re-spawn.
    pub(crate) async fn import_from_library<OP: AtomicOperation>(
        &self,
        op: &mut OP,
        parsed: ParsedSkill,
        scope: ImportScope,
    ) -> Result<Option<Skill>, SkillError> {
        let file_hash = parsed.file_hash();

        if let Some(mut existing) = self
            .repo
            .maybe_find_by_id_in_op(&mut *op, parsed.skill_id)
            .await?
        {
            if !existing
                .update(
                    Some(parsed.name.clone()),
                    Some(parsed.description.clone()),
                    Some(parsed.body.clone()),
                    file_hash,
                )
                .did_execute()
            {
                return Ok(None);
            }
            self.repo.update_in_op(op, &mut existing).await?;
            self.register_context_bump(op, scope.bump_scope());
            return Ok(Some(existing));
        }

        let mut builder = NewSkill::builder()
            .id(parsed.skill_id)
            .name(parsed.name)
            .description(parsed.description)
            .body(parsed.body)
            .original_path(parsed.original_path);
        match &scope {
            ImportScope::Project {
                project_id,
                project_name,
            } => {
                builder = builder
                    .project_id(*project_id)
                    .project_name(project_name.clone());
            }
            ImportScope::Space {
                space_id,
                space_slug,
            } => {
                builder = builder
                    .space_id(*space_id)
                    .space_slug(space_slug.clone());
            }
            ImportScope::Global => {}
        }
        let new = builder
            .build()
            .map_err(|e| SkillError::BuildEntity(e.to_string()))?;
        let skill = self.repo.create_in_op(op, new).await?;
        self.register_context_bump(op, scope.bump_scope());
        Ok(Some(skill))
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
            users: None,
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
