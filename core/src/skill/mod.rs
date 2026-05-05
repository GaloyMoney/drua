mod entity;
pub mod error;
pub mod file;
pub mod importer;
pub(crate) mod repo;

pub use file::{default_skill_path, name_from_filename, parse_skill_markdown, ParsedSkill};
pub use importer::SkillsImporter;

pub const SKILL_DOC_TYPE: drua_library::DocType = drua_library::DocType::new("skill");

use std::sync::Arc;

use drua_library::{CommitAttribution, SearchHit, WriteOp};
use es_entity::AtomicOperation;
use tracing::instrument;

use crate::agent::AgentScope;
use crate::audit::Audit;
use crate::library::SpaceMounts;
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

/// Single-row projection returned by [`Skills::list_for_scope`] —
/// covers every skill an agent in `project_id` can invoke, regardless
/// of where it lives. Differs from [`Skill`] because sandbox-exported
/// skills are runtime-only (no DB row).
#[derive(Debug, Clone)]
pub struct ScopedSkill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: SkillSource,
}

#[derive(Debug, Clone)]
pub enum SkillSource {
    /// DB-backed, owned by `project_id`. Editable via the project
    /// surface (`SkillTool` / GraphQL `skillDelete` etc.).
    Project {
        skill_id: SkillId,
        project_id: ProjectId,
    },
    /// DB-backed, no owner. Visible to every project; editable only
    /// via admin.
    Global { skill_id: SkillId },
    /// DB-backed, owned by a space `project_id` mounts. Editable only
    /// via the spaces tool.
    Space {
        skill_id: SkillId,
        space_id: SpaceId,
        space_slug: String,
    },
    /// Runtime-only — exported by a Ready sandbox in the project. No
    /// DB row, no editing surface; rebuilt every time the sandbox
    /// reports its `exported_skills`.
    Sandbox {
        sandbox_id: SandboxId,
        sandbox_name: String,
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
    /// Read facade for the project↔space mount relationship — held
    /// alongside `sandboxes` so `find_by_name` / `interpolate_skill`
    /// can resolve mounted-space skills internally without callers
    /// passing the scope through. `None` in `new_without_library` test
    /// contexts; mounted spaces resolve as empty there.
    space_mounts: Option<Arc<SpaceMounts>>,
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
        space_mounts: Arc<SpaceMounts>,
        users: Arc<Users>,
        context_generation: ContextGeneration,
    ) -> Self {
        let repo = SkillRepo::new(pool, library.clone());
        Self {
            repo,
            sandboxes,
            library: Some(library),
            space_mounts: Some(space_mounts),
            pool: Some(pool.clone()),
            users: Some(users),
            context_generation,
        }
    }

    /// Resolves the agent's mounted-space ids via the held `SpaceMounts`.
    /// Returns empty when there's no project context, no `SpaceMounts`
    /// wired (test contexts), or the lookup itself fails (logged + swallowed
    /// — skill resolution always falls through to project + global tiers).
    async fn mounted_space_ids_for(&self, project_id: Option<ProjectId>) -> Vec<SpaceId> {
        let (Some(pid), Some(sm)) = (project_id, self.space_mounts.as_ref()) else {
            return Vec::new();
        };
        match sm.space_ids_for_project(pid).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "space_ids_for_project failed; resolving without space tier");
                Vec::new()
            }
        }
    }

    /// Populates `EventContext` with the rich
    /// [`drua_library::CommitAttribution`] so post-persist library
    /// sync hooks pick it up, and returns the resolved attribution for
    /// callers (e.g. delete) that need to enqueue a write op of their
    /// own. Returns the library default when `users` isn't wired
    /// (test constructors via `new_without_library`).
    async fn populate_attribution(&self) -> CommitAttribution {
        if let Some(users) = self.users.as_ref() {
            users.commit_attribution().await
        } else {
            let attribution = CommitAttribution::library_default();
            attribution.put_in_event_context();
            attribution
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
            let hook = ContextBumpHook::new(self.context_generation.clone(), pool.clone(), scope);
            op.add_commit_hook(hook)
                .expect("DbOp supports commit hooks");
        }
        Ok(op)
    }

    /// Composable variant: registers a `ContextBumpHook` on the caller's
    /// `op` for `scope`. Used by reverse-sync importers and other internal
    /// callers that already own a transaction. No-op when `pool` is `None`.
    pub(crate) fn register_context_bump<OP: AtomicOperation>(&self, op: &mut OP, scope: ScopeId) {
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

    /// Wipe the search-store row in the same DbOp and queue a
    /// [`WriteOp::DeleteFile`] so the upstream git repo loses the
    /// markdown on the next library write tick. No-op when no library
    /// is wired (test constructors via `new_without_library`).
    ///
    /// The search-row delete is what `Skills::delete` was missing
    /// pre-PR — soft-delete events don't fire the post_persist_hook,
    /// so without an explicit `deletes:` entry the `library_documents`
    /// row would orphan and stale `library.search` results would
    /// keep returning the deleted skill.
    async fn enqueue_skill_delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        skill: &Skill,
        attribution: CommitAttribution,
    ) -> Result<(), SkillError> {
        let Some(library) = self.library.as_ref() else {
            return Ok(());
        };
        let id_uuid: uuid::Uuid = skill.id.into();
        let message = format!(
            "skill: delete {}-{}",
            &skill.name,
            &id_uuid.to_string()[..8]
        );
        library
            .enqueue_full_in_op(
                op,
                None,
                vec![(id_uuid, SKILL_DOC_TYPE)],
                Some(WriteOp::DeleteFile {
                    path: skill.path.clone(),
                    message,
                }),
                attribution,
            )
            .await?;
        Ok(())
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

    /// Lookup order (highest precedence first):
    /// 1. Mounted space the agent's project has — Space > all (matches the
    ///    `<available_skills>` precedence so a space skill the agent saw
    ///    listed is always invocable). Mounted spaces are resolved
    ///    internally via the held `SpaceMounts`; callers only pass
    ///    `project_id`.
    /// 2. Project-scoped (the agent's own project).
    /// 3. Truly-global (no project AND no space).
    /// 4. Sandbox-exported (only the agent's attached sandbox).
    ///
    /// `Ok(None)` when no source matches.
    #[instrument(name = "skill.find_by_name", skip_all, fields(name = %name, sandbox_id))]
    pub async fn find_by_name(
        &self,
        name: &str,
        project_id: Option<ProjectId>,
        sandbox_id: Option<SandboxId>,
    ) -> Result<Option<SkillBody>, SkillError> {
        // Single query fetches every skill with this name across all scopes;
        // we filter by precedence here.
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
        let mounted_space_ids = self.mounted_space_ids_for(project_id).await;

        let best = candidates
            .iter()
            // Mounted-space skill — Space > Sandbox > Project > Global per
            // `skills_context_for_scope`'s precedence.
            .find(|s| {
                s.space_id
                    .is_some_and(|sid| mounted_space_ids.contains(&sid))
            })
            .or_else(|| {
                project_id.and_then(|pid| candidates.iter().find(|s| s.project_id == Some(pid)))
            })
            .or_else(|| {
                candidates
                    .iter()
                    .find(|s| s.project_id.is_none() && s.space_id.is_none())
            });
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

    /// Project-scoped + global, with auth check. Returns DB-backed
    /// entities only — does NOT include mounted-space or sandbox-
    /// exported skills. Use [`Self::list_for_scope`] for the full set
    /// an agent in this project can actually invoke.
    #[instrument(name = "skill.list_for_project", skip(self, sub))]
    pub async fn list_for_project(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<Skill>, SkillError> {
        sub.can(AuthVerb::Read, AuthResource::Skill(project_id, None))?;
        self.list_for_project_inner(project_id).await
    }

    /// Every skill an agent in `project_id` can invoke, with
    /// provenance: project-owned, truly-global, mounted-space, and
    /// sandbox-exported (runtime). Mirrors the resolution that
    /// [`Self::skills_context_for_scope`] does for the agent's
    /// `<available_skills>` block — the UI calls this so the human
    /// sees the same set the agent does. Lookups against
    /// `space_mounts` / `sandboxes` failures are logged + swallowed
    /// so a missing lower tier degrades the result rather than fails
    /// the whole request.
    #[instrument(name = "skill.list_for_scope", skip(self, sub))]
    pub async fn list_for_scope(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<ScopedSkill>, SkillError> {
        sub.can(AuthVerb::Read, AuthResource::Skill(project_id, None))?;
        let mut out: Vec<ScopedSkill> = Vec::new();

        // 1. Project + truly-global (existing list_for_project_inner).
        for s in self.list_for_project_inner(project_id).await? {
            let source = match s.project_id {
                Some(pid) => SkillSource::Project {
                    skill_id: s.id,
                    project_id: pid,
                },
                None => SkillSource::Global { skill_id: s.id },
            };
            out.push(ScopedSkill {
                name: s.name,
                description: s.description,
                body: s.body,
                source,
            });
        }

        // 2. Mounted spaces — one query per mount; mount-set is small
        // (capped at SPACES_BLOCK_LIMIT in space_mounts).
        let mounted_space_ids = self.mounted_space_ids_for(Some(project_id)).await;
        for space_id in mounted_space_ids {
            let res = self
                .repo
                .list_for_space_id_by_created_at(
                    Some(space_id),
                    es_entity::PaginatedQueryArgs {
                        first: 100,
                        after: None,
                    },
                    es_entity::ListDirection::Ascending,
                )
                .await?;
            for s in res.entities {
                let space_slug = s.space_slug.clone().unwrap_or_default();
                out.push(ScopedSkill {
                    name: s.name,
                    description: s.description,
                    body: s.body,
                    source: SkillSource::Space {
                        skill_id: s.id,
                        space_id,
                        space_slug,
                    },
                });
            }
        }

        // 3. Sandbox-exported — every Ready sandbox in the project.
        // Best-effort: a Sandbox-Read denial here just hides this tier
        // (consistent with the existing `space_mounts` failure path).
        match self.sandboxes.list_for_project(sub, project_id).await {
            Ok(sandboxes) => {
                for sandbox in sandboxes {
                    if sandbox.state != crate::sandbox::SandboxState::Ready {
                        continue;
                    }
                    for exp in &sandbox.exported_skills {
                        out.push(ScopedSkill {
                            name: exp.name.clone(),
                            description: exp.description.clone().unwrap_or_default(),
                            body: exp.content.clone(),
                            source: SkillSource::Sandbox {
                                sandbox_id: sandbox.id,
                                sandbox_name: sandbox.name.clone(),
                            },
                        });
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "sandboxes.list_for_project failed; sandbox tier hidden from list_for_scope");
            }
        }

        Ok(out)
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
        // The repo query for `project_id IS NULL` returns BOTH truly-global
        // skills (no project, no space) AND space-scoped skills (no
        // project, but `space_id IS Some`). Filter out the space-scoped
        // ones — those are reachable only via mounted spaces and are
        // resolved separately in `skills_context_for_scope`.
        let global_skills: Vec<Skill> = global_result
            .entities
            .into_iter()
            .filter(|s| s.space_id.is_none())
            .collect();

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

        // `path` is derived inside `NewSkillBuilder::build()` from the
        // `(name, project_name, space_slug)` triple — no need to set it
        // explicitly on the create surface.
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

    /// Soft-deletes the skill row and enqueues a [`WriteOp::DeleteFile`]
    /// for the canonical markdown path so the next library write tick
    /// removes it from the upstream git repo. Space-scoped skills are
    /// rejected — callers must route through the spaces tool to keep
    /// the project / space surfaces cleanly separated.
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
        if skill.space_id.is_some() {
            return Err(SkillError::SpaceScopedDeleteViaSpaces);
        }
        if skill.project_id != Some(project_id) {
            return Err(SkillError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Delete,
                resource: AuthResource::Skill(project_id, Some(id)),
            }));
        }
        let attribution = self.populate_attribution().await;
        self.enqueue_skill_delete_in_op(&mut op, &skill, attribution)
            .await?;
        self.repo.delete_in_op(&mut op, skill).await?;
        op.commit().await?;
        Ok(())
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

    /// Reverse-sync delete: soft-deletes the skill whose canonical
    /// markdown lives at `path` and bumps the appropriate
    /// `ContextGeneration` so agents stop seeing the skill in their
    /// `<available_skills>` block on the next turn. Returns
    /// `Ok(Some(skill_id))` when a row was actually deleted (caller
    /// uses this to wipe the search row), or `Ok(None)` when nothing
    /// matched (file authored without an id suffix, already-deleted
    /// row, etc.). Skips the library write op — the file is already
    /// gone in git, this is the inbound direction.
    ///
    /// Resolves `path → doc_id` via `library_documents` rather than an
    /// id-prefix scan: the search-store row is rewritten in lockstep
    /// with the entity's canonical path (post_persist_hook owns both),
    /// so an exact path match is unambiguous and avoids the
    /// ~N²/2³³ UUIDv7 prefix collision risk a `LIKE 'XXXXXXXX%'`
    /// query would have at scale.
    pub(crate) async fn delete_by_path_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        path: &str,
    ) -> Result<Option<SkillId>, SkillError> {
        let Some(skill) = self.repo.maybe_find_by_path_in_op(&mut *op, path).await? else {
            return Ok(None);
        };
        let id = skill.id;
        let scope = match (skill.space_id, skill.project_id) {
            (Some(sid), _) => ScopeId::Space(sid),
            (None, Some(pid)) => ScopeId::Project(pid),
            (None, None) => ScopeId::Global,
        };
        // Wipe the search-store row in the same op. `delete_in_op` is a
        // no-op when no library is wired (test contexts).
        if let Some(library) = self.library.as_ref() {
            library
                .search()
                .delete_in_op(op, id.into(), SKILL_DOC_TYPE)
                .await?;
        }
        self.repo.delete_in_op(op, skill).await?;
        self.register_context_bump(op, scope);
        Ok(Some(id))
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
        // `maybe_find_by_id_include_deleted_in_op` returns soft-deleted
        // rows too — we need them so `spaces edit op=move` can revive
        // the row that `delete-old-path` just soft-deleted (otherwise
        // `create_in_op` would PK-conflict on the same id). The default
        // `maybe_find_by_id_in_op` filters deleted; this one doesn't.
        if let Some(existing) = self
            .repo
            .maybe_find_by_id_include_deleted_in_op(&mut *op, parsed.skill_id)
            .await?
        {
            // Flip the soft-delete flag if it's still set; idempotent
            // SQL — clears `deleted = TRUE` only if it was set. No
            // separate "is it deleted?" probe needed.
            self.repo.revive_in_op(&mut *op, parsed.skill_id).await?;
            return self
                .reconcile_existing_skill_in_op(op, existing, parsed, scope)
                .await;
        }

        // Truly new — fresh create.
        let mut builder = NewSkill::builder()
            .id(parsed.skill_id)
            .name(parsed.name)
            .description(parsed.description)
            .body(parsed.body)
            .path(parsed.path);
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
                builder = builder.space_id(*space_id).space_slug(space_slug.clone());
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
            space_mounts: None,
            pool: None,
            users: None,
            context_generation: ContextGeneration::new(),
        }
    }

    /// Reconcile content (Updated event) and path (PathChanged event)
    /// on an existing live skill against an incoming `ParsedSkill`
    /// from the reverse-sync importer. `Ok(None)` when no delta
    /// produced an event (round-trip already in sync).
    ///
    /// Granular comparison rather than hash-based: the rendered file
    /// doesn't include `name:` (Model A — filename is the canonical
    /// name), so a stale DB row whose `name` differs from the filename
    /// slug renders to the same bytes as the parsed file. The hash
    /// would say "no change" while the entity-level fields are wrong.
    /// `update_content` walks the fields and pushes an Updated event
    /// only when something actually differs.
    async fn reconcile_existing_skill_in_op<OP: AtomicOperation>(
        &self,
        op: &mut OP,
        mut existing: Skill,
        parsed: ParsedSkill,
        scope: ImportScope,
    ) -> Result<Option<Skill>, SkillError> {
        let mut updated = existing
            .update_content(
                Some(parsed.name.clone()),
                Some(parsed.description.clone()),
                Some(parsed.body.clone()),
            )
            .did_execute();
        updated |= existing.change_path(parsed.path).did_execute();
        if !updated {
            return Ok(None);
        }
        self.repo.update_in_op(op, &mut existing).await?;
        self.register_context_bump(op, scope.bump_scope());
        Ok(Some(existing))
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
