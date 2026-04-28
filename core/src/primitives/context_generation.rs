use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use es_entity::operation::hooks::CommitHook;

use super::WorkspaceId;

/// Cross-cutting "something changed" signal for workspace context (notes/skills).
/// Bumped on every Notes/Skills mutation; read by Agents to detect cache staleness.
/// HA-safe via PG LISTEN/NOTIFY: the App spawns a listener that bumps this on
/// `context_changed` notifications from other instances.
#[derive(Clone, Debug)]
pub struct ContextGeneration(Arc<AtomicU64>);

impl ContextGeneration {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::Release);
    }

    pub fn current(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for ContextGeneration {
    fn default() -> Self {
        Self::new()
    }
}

/// Commit hook that bumps the shared `ContextGeneration` and broadcasts a
/// `context_changed` PG NOTIFY *after* the originating transaction commits.
/// Register on a `DbOp` via `op.add_commit_hook(ContextBumpHook::new(...))`.
///
/// `post_commit` is sync, so the `pg_notify` is dispatched via `tokio::spawn`.
/// Failure of the spawned query is logged and swallowed — the local atomic
/// has already been bumped, so this instance has the fresh signal regardless.
#[derive(Debug, Clone)]
pub struct ContextBumpHook {
    generation: ContextGeneration,
    pool: sqlx::PgPool,
    /// `None` for global-scoped mutations (e.g. global skill upserts);
    /// `Some(ws)` for workspace-scoped mutations. The payload is
    /// informational — listeners on other instances bump unconditionally.
    workspace_id: Option<WorkspaceId>,
}

impl ContextBumpHook {
    pub fn new(
        generation: ContextGeneration,
        pool: sqlx::PgPool,
        workspace_id: Option<WorkspaceId>,
    ) -> Self {
        Self {
            generation,
            pool,
            workspace_id,
        }
    }
}

impl CommitHook for ContextBumpHook {
    fn post_commit(self) {
        // Bump local atomic immediately; broadcast via PG NOTIFY async
        // (delayed notify only affects peers, never this instance).
        self.generation.bump();
        let pool = self.pool;
        let payload = self
            .workspace_id
            .map(|w| w.to_string())
            .unwrap_or_else(|| "global".to_string());
        tokio::spawn(async move {
            if let Err(e) = sqlx::query("SELECT pg_notify('context_changed', $1)")
                .bind(payload)
                .execute(&pool)
                .await
            {
                tracing::warn!(error = %e, "pg_notify(context_changed) failed");
            }
        });
    }

    fn merge(&mut self, other: &mut Self) -> bool {
        // Same-scope bumps collapse; different scopes keep separate hooks
        // so each peer gets a per-scope payload.
        self.workspace_id == other.workspace_id
    }
}
