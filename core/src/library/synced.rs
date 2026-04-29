use std::sync::Arc;

use es_entity::{
    operation::hooks::{CommitHook, HookOperation, PreCommitRet},
    AtomicOperation,
};
use job::{CurrentJob, Job, JobCompletion, JobId, JobInitializer, JobRunner, JobSpawner, JobType};
use serde::{Deserialize, Serialize};

use super::file::{DocType, GitFileHash, SearchableFields, UpstreamOp};
use super::search::SearchStore;
use super::{Library, LibraryError, LIBRARY_LOCK_QUEUE};

use crate::primitives::WorkspaceId;
use crate::workspace::Workspaces;

// ── SyncedFile: collapsed payload shape for every entity-backed file ─────────

/// Type-erased projection of a `LibrarySynced` entity into a serialisable
/// shape suitable for the obix inbox + git write pipeline.
///
/// `rendered` carries the canonical on-disk content (markdown w/ frontmatter,
/// YAML, …) so `git hash-object` short-circuits re-writes without dispatching
/// back to the originating entity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncedFile {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default)]
    pub workspace_name: Option<String>,
    pub slug: String,
    pub id_prefix: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
    pub rendered: String,
}

impl SyncedFile {
    /// `runtime/workspaces/{ws}/{subdir}/{slug}-{id_prefix}.{ext}` when scoped,
    /// else `runtime/{subdir}/{slug}-{id_prefix}.{ext}` (Skill/Workflow only).
    pub fn relative_path(&self) -> String {
        let subdir = self.doc_type.subdir();
        let ext = self.doc_type.ext();
        match &self.workspace_name {
            Some(ws) => format!(
                "runtime/workspaces/{}/{}/{}-{}.{}",
                ws, subdir, self.slug, self.id_prefix, ext
            ),
            None => format!(
                "runtime/{}/{}-{}.{}",
                subdir, self.slug, self.id_prefix, ext
            ),
        }
    }

    pub fn commit_message(&self) -> String {
        format!(
            "{}: {}-{}",
            self.doc_type.as_str(),
            self.slug,
            self.id_prefix
        )
    }

    pub fn searchable_fields(&self) -> SearchableFields {
        SearchableFields {
            doc_id: self.doc_id,
            doc_type: self.doc_type,
            workspace_id: self
                .workspace_id
                .map(uuid::Uuid::from)
                .unwrap_or(uuid::Uuid::nil()),
            title: self.title.clone(),
            body: self.body.clone(),
            tags: self.tags.clone(),
        }
    }

    /// Identical to `git hash-object` on the rendered bytes.
    pub fn file_hash(&self) -> GitFileHash {
        GitFileHash::from_blob_bytes(self.rendered.as_bytes())
    }
}

// ── LibrarySynced trait ──────────────────────────────────────────────────────

/// Implemented on entity types whose mutations should sync to the library.
pub trait LibrarySynced: Sized + Send + Sync + 'static {
    type Event: es_entity::EsEvent + 'static;

    const DOC_TYPE: DocType;

    /// Sync only fires when one of these event variants was just persisted.
    fn is_content_event(ev: &Self::Event) -> bool;

    /// `None` means "global" file (not workspace-scoped).
    fn workspace(&self) -> Option<(WorkspaceId, &str)>;

    fn id(&self) -> uuid::Uuid;

    /// Display name of the entity — also used as the searchable `title`
    /// and as the slug source (after slugification).
    fn display_name(&self) -> &str;

    fn created_at(&self) -> chrono::DateTime<chrono::Utc>;
    fn updated_at(&self) -> chrono::DateTime<chrono::Utc>;
    fn original_path(&self) -> Option<&str> {
        None
    }

    /// Searchable body projection (note content, skill description,
    /// workflow description, …).
    fn index_body(&self) -> &str;

    fn index_tags(&self) -> Vec<String> {
        Vec::new()
    }

    /// Render the canonical on-disk content. Must be deterministic so
    /// `WriteToRuntime`'s hash short-circuit doesn't loop.
    fn render(&self) -> String;

    fn to_synced_file(&self) -> SyncedFile {
        let id = self.id();
        let id_prefix = id.to_string()[..8].to_string();
        let (workspace_id, workspace_name) = match self.workspace() {
            Some((id, name)) => (Some(id), Some(name.to_string())),
            None => (None, None),
        };
        SyncedFile {
            doc_id: id,
            doc_type: Self::DOC_TYPE,
            workspace_id,
            workspace_name,
            slug: slugify(self.display_name()),
            id_prefix,
            created_at: self.created_at().to_rfc3339(),
            updated_at: self.updated_at().to_rfc3339(),
            title: self.display_name().to_string(),
            body: self.index_body().to_string(),
            tags: self.index_tags(),
            original_path: self.original_path().map(|s| s.to_string()),
            rendered: self.render(),
        }
    }
}

/// Per-repo `post_persist_hook` body collapses to a one-liner over this.
pub async fn sync_to_library<E, OP>(
    library: Option<&Library>,
    op: &mut OP,
    entity: &E,
    new_events: &mut es_entity::LastPersisted<'_, E::Event>,
) -> Result<(), LibraryError>
where
    E: LibrarySynced,
    OP: AtomicOperation,
{
    let Some(library) = library else {
        return Ok(());
    };
    if !new_events.any(|p| E::is_content_event(&p.event)) {
        return Ok(());
    }
    let file = UpstreamOp::Synced(Box::new(entity.to_synced_file()));
    library.write_in_op(op, &file).await
}

// ── LibraryImporter (reverse sync — implemented on the service) ─────────────

/// Boxed std error used by the generic runner to log per-type upsert
/// failures uniformly (skill/workflow services return their own error
/// enums; boxing avoids a per-type `From` impl on `LibraryError`).
pub type UpsertError = Box<dyn std::error::Error + Send + Sync>;

/// Implemented on **services** (`Skills`, `Workflows`, …) that import
/// entities from the git-backed library. The associated `Entity` carries
/// the forward-sync projection (`LibrarySynced::DOC_TYPE` is the source of
/// truth for subdir/extension and file format identity).
pub trait LibraryImporter: Send + Sync + 'static {
    /// The entity type this service imports from the library.
    type Entity: LibrarySynced;

    /// Reverse-sync `JobType` name; must be unique per impl.
    const JOB_TYPE: &'static str;

    /// `true` rejects parsed files whose folder doesn't resolve to a
    /// workspace; `false` allows global files.
    const WORKSPACE_REQUIRED: bool = false;

    /// Parse on-disk bytes into a `SyncedFile`. `None` means "skip"
    /// (logged by the runner). `needs_rewrite` is reflected back to the
    /// write job to trigger canonicalisation.
    fn parse(content: &str, path: &str) -> Option<ParsedFile>;

    /// Apply a parsed file to the entity service inside a transaction.
    fn upsert_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        file: &SyncedFile,
        workspace_id: Option<WorkspaceId>,
        file_hash: GitFileHash,
    ) -> impl std::future::Future<Output = Result<(), UpsertError>> + Send;
}

#[derive(Debug)]
pub struct ParsedFile {
    pub file: SyncedFile,
    /// True when the on-disk form is non-canonical (no `id:`, stale path,
    /// missing frontmatter); the write job rewrites with canonical headers.
    pub needs_rewrite: bool,
}

#[derive(Debug)]
pub struct Changes {
    pub head_commit: String,
    pub files: Vec<ParsedFile>,
}

// ── Generic reverse-sync runner ──────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncFromLibraryConfig {
    pub sync_interval_secs: u64,
    #[serde(default)]
    pub last_sync_commit: Option<String>,
}

impl SyncFromLibraryConfig {
    fn next(&self, last_sync_commit: Option<String>) -> Self {
        Self {
            sync_interval_secs: self.sync_interval_secs,
            last_sync_commit,
        }
    }
}

/// Generic over `S: LibraryImporter`. The service is the parameter — the
/// associated `Entity` (`LibrarySynced` impl) supplies subdir/extension/
/// `DOC_TYPE` for the parametric `find_changes`, and `S::JOB_TYPE` is the
/// unique job-runner name.
pub struct SyncFromLibraryJobInitializer<S: LibraryImporter> {
    library: Arc<Library>,
    service: Arc<S>,
    workspaces: Arc<Workspaces>,
}

impl<S: LibraryImporter> SyncFromLibraryJobInitializer<S> {
    pub fn new(library: Arc<Library>, service: Arc<S>, workspaces: Arc<Workspaces>) -> Self {
        Self {
            library,
            service,
            workspaces,
        }
    }
}

impl<S: LibraryImporter> JobInitializer for SyncFromLibraryJobInitializer<S> {
    type Config = SyncFromLibraryConfig;

    fn job_type(&self) -> JobType {
        JobType::new(S::JOB_TYPE)
    }

    fn init(
        &self,
        job: &Job,
        spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: SyncFromLibraryConfig = job.config()?;
        Ok(Box::new(SyncFromLibraryRunner::<S> {
            library: Arc::clone(&self.library),
            service: Arc::clone(&self.service),
            workspaces: Arc::clone(&self.workspaces),
            config,
            spawner,
        }))
    }
}

struct SyncFromLibraryRunner<S: LibraryImporter> {
    library: Arc<Library>,
    service: Arc<S>,
    workspaces: Arc<Workspaces>,
    config: SyncFromLibraryConfig,
    spawner: JobSpawner<SyncFromLibraryConfig>,
}

#[async_trait::async_trait]
impl<S: LibraryImporter> JobRunner for SyncFromLibraryRunner<S> {
    async fn run(
        &self,
        current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let next_commit = match self.sync_once(&current_job).await {
            Ok(commit) => commit,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    doc_type = <S::Entity as LibrarySynced>::DOC_TYPE.as_str(),
                    "library sync cycle failed"
                );
                self.config.last_sync_commit.clone()
            }
        };

        let next_config = self.config.next(next_commit);
        let schedule_at =
            chrono::Utc::now() + chrono::Duration::seconds(self.config.sync_interval_secs as i64);
        self.spawner
            .spawn_at_with_queue_id(JobId::new(), next_config, schedule_at, LIBRARY_LOCK_QUEUE)
            .await?;

        Ok(JobCompletion::Complete)
    }
}

impl<S: LibraryImporter> SyncFromLibraryRunner<S> {
    async fn sync_once(
        &self,
        current_job: &CurrentJob,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let changes = self
            .library
            .find_changes::<S>(self.config.last_sync_commit.as_deref())
            .await?;

        if changes.files.is_empty() {
            let commit = if changes.head_commit.is_empty() {
                None
            } else {
                Some(changes.head_commit)
            };
            return Ok(commit);
        }

        let doc_type = <S::Entity as LibrarySynced>::DOC_TYPE.as_str();

        tracing::info!(
            count = changes.files.len(),
            head = %changes.head_commit,
            doc_type,
            "processing changed files from library"
        );

        let mut ws_cache: std::collections::HashMap<String, Option<WorkspaceId>> =
            std::collections::HashMap::new();
        let mut resolved: Vec<(ParsedFile, Option<WorkspaceId>)> =
            Vec::with_capacity(changes.files.len());

        for parsed in changes.files {
            let ws_id = match &parsed.file.workspace_name {
                Some(name) => match ws_cache.get(name).copied() {
                    Some(cached) => cached,
                    None => {
                        let id = match self.workspaces.find_by_name(name).await {
                            Ok(Some(ws)) => Some(ws.id),
                            Ok(None) => {
                                tracing::warn!(
                                    workspace_name = %name,
                                    "workspace not found for library file"
                                );
                                None
                            }
                            Err(e) => {
                                tracing::warn!(
                                    workspace_name = %name,
                                    error = %e,
                                    "workspace lookup failed"
                                );
                                None
                            }
                        };
                        ws_cache.insert(name.clone(), id);
                        id
                    }
                },
                None => None,
            };

            if S::WORKSPACE_REQUIRED && ws_id.is_none() {
                tracing::warn!(doc_type, "workspace required but unresolved; skipping");
                continue;
            }
            resolved.push((parsed, ws_id));
        }

        let mut op = current_job.begin_op().await?;
        for (parsed, ws_id) in &resolved {
            let file_hash = parsed.file.file_hash();
            if let Err(e) = self
                .service
                .upsert_in_op(&mut op, &parsed.file, *ws_id, file_hash)
                .await
            {
                tracing::warn!(
                    error = %e,
                    doc_type,
                    "failed to upsert from library, skipping"
                );
            }
        }
        op.commit().await?;

        Ok(Some(changes.head_commit))
    }
}

// ── Hook (Phase 0) ───────────────────────────────────────────────────────────

/// Batches all library writes in a single transaction into one inbox row +
/// one search-index pass. Registered via `op.add_commit_hook` from per-repo
/// `post_persist_hook` bodies; `merge` collapses repeated registrations
/// (same `TypeId`) into a single hook.
pub(super) struct LibrarySyncHook {
    inbox: obix::Inbox,
    search: SearchStore,
    files: Vec<UpstreamOp>,
}

impl LibrarySyncHook {
    pub(super) fn new(inbox: obix::Inbox, search: SearchStore, file: UpstreamOp) -> Self {
        Self {
            inbox,
            search,
            files: vec![file],
        }
    }
}

impl CommitHook for LibrarySyncHook {
    async fn pre_commit(
        self,
        mut op: HookOperation<'_>,
    ) -> Result<PreCommitRet<'_, Self>, sqlx::Error> {
        for file in &self.files {
            if let Some(fields) = file.searchable_fields() {
                self.search.upsert_in_op(&mut op, &fields).await?;
            }

            // `InboxError::{Job,Deserialization}` aren't sqlx errors but the
            // hook contract is `sqlx::Error`-only; unwrap the inner sqlx
            // error when present, otherwise stringify via `Protocol`.
            if let Err(e) = self
                .inbox
                .persist_and_queue_job_in_op(&mut op, file.idempotency_key(), file)
                .await
            {
                return Err(match e {
                    obix::InboxError::Sqlx(s) => s,
                    other => sqlx::Error::Protocol(format!("library inbox: {other}")),
                });
            }
        }

        PreCommitRet::ok(self, op)
    }

    fn merge(&mut self, other: &mut Self) -> bool {
        self.files.append(&mut other.files);
        true
    }
}

/// Register a library write to fire at transaction commit.
pub(super) async fn enqueue_write<OP: AtomicOperation>(
    op: &mut OP,
    inbox: &obix::Inbox,
    search: &SearchStore,
    file: UpstreamOp,
) -> Result<(), sqlx::Error> {
    let hook = LibrarySyncHook::new(inbox.clone(), search.clone(), file);
    if let Err(hook) = op.add_commit_hook(hook) {
        let _ = hook.force_execute_pre_commit(op).await?;
    }
    Ok(())
}

// ── slugify ──────────────────────────────────────────────────────────────────

pub(super) fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
