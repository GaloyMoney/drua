pub mod audit;
mod entity;
pub mod error;
pub mod policy;
pub mod primitives;
pub mod repo;

use tracing::instrument;

pub use audit::GitProxyAuditLog;
pub use entity::{GitProxyAllowlistEntry, NewGitProxyAllowlistEntry};
pub use error::GitProxyError;
pub use policy::RefPatternSet;
pub use primitives::{
    GitProxyAllowlistEntryId, GitProxyDecision, GitProxyMode, GitService, RepoCoord,
};

use crate::auth::{error::AuthorizationError, AuthSubject};
use crate::primitives::ProjectId;

use repo::GitProxyAllowlistEntryRepo;

/// Service surface for the git-proxy allow-list.
///
/// Allow-list entries are seeded from a YAML fixture at boot (per memo
/// 019dfebc §7.2 — admin UI is a follow-up). Authorization is consulted
/// once per smart-HTTP request from the axum handler before any data is
/// forwarded to / from `git http-backend`.
#[derive(Clone)]
pub struct GitProxies {
    repo: GitProxyAllowlistEntryRepo,
    audit: GitProxyAuditLog,
}

impl GitProxies {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self {
            repo: GitProxyAllowlistEntryRepo::new(pool),
            audit: GitProxyAuditLog::new(pool),
        }
    }

    pub fn audit(&self) -> &GitProxyAuditLog {
        &self.audit
    }

    /// Used by config-time seeding only — not exposed to API callers.
    /// Idempotent on `(project_id, owner, repo)`: a second call updates
    /// the patterns/modes via events rather than creating a duplicate.
    #[instrument(name = "domain.git_proxy.upsert_allowlist_entry", skip(self))]
    pub async fn upsert_allowlist_entry(
        &self,
        project_id: ProjectId,
        owner: impl Into<String> + std::fmt::Debug,
        repo: impl Into<String> + std::fmt::Debug,
        allowed_ref_patterns: Vec<String>,
        modes: Vec<GitProxyMode>,
    ) -> Result<GitProxyAllowlistEntry, GitProxyError> {
        // Validate the patterns up-front; bad config should crash boot,
        // not silently fail every push.
        let _ = RefPatternSet::new(&allowed_ref_patterns)?;

        let owner = owner.into();
        let repo = repo.into();

        let mut op = self.repo.begin_op().await?;

        let mut existing = self
            .repo
            .list_for_project_id_by_created_at_in_op(
                &mut op,
                project_id,
                es_entity::PaginatedQueryArgs {
                    first: 200,
                    after: None,
                },
                es_entity::ListDirection::Descending,
            )
            .await?;

        if let Some(idx) = existing
            .entities
            .iter()
            .position(|e| e.owner == owner && e.repo == repo)
        {
            let mut entry = existing.entities.swap_remove(idx);
            let pat_changed = entry
                .update_ref_patterns(allowed_ref_patterns)
                .did_execute();
            let mode_changed = entry.update_modes(modes).did_execute();
            if pat_changed || mode_changed {
                self.repo.update_in_op(&mut op, &mut entry).await?;
            }
            op.commit().await?;
            return Ok(entry);
        }

        let new_entry = NewGitProxyAllowlistEntry::builder()
            .project_id(project_id)
            .owner(owner)
            .repo(repo)
            .allowed_ref_patterns(allowed_ref_patterns)
            .modes(modes)
            .build()
            .expect("NewGitProxyAllowlistEntry builder field set");
        let entry = self.repo.create_in_op(&mut op, new_entry).await?;
        op.commit().await?;
        Ok(entry)
    }

    /// Revokes a specific allow-list entry. Used by ops dashboards
    /// (project-admin scope) and by the YAML reseed path when an entry
    /// disappears from config. Idempotent.
    #[instrument(name = "domain.git_proxy.revoke_allowlist_entry", skip(self, sub))]
    pub async fn revoke_allowlist_entry(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        entry_id: GitProxyAllowlistEntryId,
    ) -> Result<GitProxyAllowlistEntry, GitProxyError> {
        sub.can(
            crate::auth::AuthVerb::Update,
            crate::auth::AuthResource::AuditLog(project_id),
        )?;
        let mut op = self.repo.begin_op().await?;
        let mut entry = self.repo.find_by_id_in_op(&mut op, entry_id).await?;
        if entry.project_id != project_id {
            return Err(GitProxyError::Authorization(
                AuthorizationError::Forbidden {
                    verb: crate::auth::AuthVerb::Update,
                    resource: crate::auth::AuthResource::AuditLog(project_id),
                },
            ));
        }
        if entry.revoke().did_execute() {
            self.repo.update_in_op(&mut op, &mut entry).await?;
        }
        op.commit().await?;
        Ok(entry)
    }

    #[instrument(name = "domain.git_proxy.list_for_project", skip(self, sub))]
    pub async fn list_for_project(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<GitProxyAllowlistEntry>, GitProxyError> {
        // Reuses `AuditLog` resource — audit-log read implies "see
        // policy decisions for this project". Dashboard-only surface.
        sub.can(
            crate::auth::AuthVerb::Read,
            crate::auth::AuthResource::AuditLog(project_id),
        )?;
        let res = self
            .repo
            .list_for_project_id_by_created_at(
                project_id,
                es_entity::PaginatedQueryArgs {
                    first: 200,
                    after: None,
                },
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(res
            .entities
            .into_iter()
            .filter(|e| !e.is_revoked())
            .collect())
    }

    /// Authorization point called per smart-HTTP request.
    ///
    /// `refs_in_request` may be empty for `info/refs` — in that case
    /// we authorize the *mode* against the (project, owner, repo)
    /// triple but don't reject on ref-pattern (the ref list will be
    /// checked again when the actual upload-pack/receive-pack body
    /// arrives). For `git-receive-pack` POSTs we get the parsed ref
    /// list from the pre-receive hook callback (M1.5 work).
    ///
    /// Fail-closed: any error path (subject without project, repo not
    /// in allow-list, mode not allowed, ref pattern denied) returns
    /// `Err(_)` so the caller can record the rejection and respond
    /// 403 without forwarding to upstream.
    #[instrument(
        name = "domain.git_proxy.check_authorization",
        skip(self, sub, refs_in_request)
    )]
    pub async fn check_authorization(
        &self,
        sub: &AuthSubject,
        owner: &str,
        repo: &str,
        mode: GitProxyMode,
        refs_in_request: &[String],
    ) -> Result<GitProxyAllowlistEntry, GitProxyError> {
        let project_id = sub
            .project_id()
            .ok_or(GitProxyError::SubjectMissingProject)?;
        if !sub.is_agent() {
            return Err(GitProxyError::Authorization(
                AuthorizationError::AuthenticationRequired,
            ));
        }

        let mut listed = self
            .repo
            .list_for_project_id_by_created_at(
                project_id,
                es_entity::PaginatedQueryArgs {
                    first: 200,
                    after: None,
                },
                es_entity::ListDirection::Descending,
            )
            .await?;

        let entry = listed
            .entities
            .drain(..)
            .find(|e| e.owner == owner && e.repo == repo && !e.is_revoked())
            .ok_or_else(|| GitProxyError::RepoNotAllowed {
                owner: owner.to_string(),
                repo: repo.to_string(),
            })?;

        if !entry.modes.contains(&mode) {
            return Err(GitProxyError::ModeNotAllowed {
                owner: owner.to_string(),
                repo: repo.to_string(),
                mode,
            });
        }

        if !refs_in_request.is_empty() {
            let pattern_set = RefPatternSet::new(&entry.allowed_ref_patterns)?;
            for r in refs_in_request {
                if !pattern_set.matches(r) {
                    return Err(GitProxyError::RefPatternDenied {
                        owner: owner.to_string(),
                        repo: repo.to_string(),
                        mode,
                        ref_name: r.clone(),
                    });
                }
            }
        }

        Ok(entry)
    }
}
