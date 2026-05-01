use es_entity::operation::hooks::{CommitHook, HookOperation, PreCommitRet};

use super::file::{DocType, GitFileHash, SearchableFields, UpstreamOp};
use super::search::SearchStore;

use crate::primitives::ProjectId;

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
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub project_name: Option<String>,
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
    /// `runtime/projects/{project}/{subdir}/{slug}-{id_prefix}.{ext}` when scoped,
    /// else `runtime/{subdir}/{slug}-{id_prefix}.{ext}` (Skill/Workflow only).
    pub fn relative_path(&self) -> String {
        let subdir = self.doc_type.subdir();
        let ext = self.doc_type.ext();
        match &self.project_name {
            Some(project) => format!(
                "runtime/projects/{}/{}/{}-{}.{}",
                project, subdir, self.slug, self.id_prefix, ext
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
            project_id: self
                .project_id
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

/// Implemented on entity types whose mutations should sync to the library.
pub trait LibrarySynced: Sized + Send + Sync + 'static {
    type Event: es_entity::EsEvent + 'static;

    const DOC_TYPE: DocType;

    /// Sync only fires when one of these event variants was just persisted.
    fn is_content_event(ev: &Self::Event) -> bool;

    /// `None` means "global" file (not project-scoped).
    fn project(&self) -> Option<(ProjectId, &str)>;

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
        let (project_id, project_name) = match self.project() {
            Some((id, name)) => (Some(id), Some(name.to_string())),
            None => (None, None),
        };
        SyncedFile {
            doc_id: id,
            doc_type: Self::DOC_TYPE,
            project_id,
            project_name,
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

/// Boxed std error so the generic runner can log per-type upsert failures
/// uniformly without forcing a `From` impl on `LibraryError` per service.
pub type UpsertError = Box<dyn std::error::Error + Send + Sync>;

/// Implemented on **services** (`Skills`, `Workflows`, …) that import
/// entities from the git-backed library. Decoupled from `LibrarySynced`:
/// the importer claims paths, parses bytes, and upserts/deletes; the
/// unified sync runner dispatches on `matches`.
pub trait LibraryImporter: Send + Sync + 'static {
    /// True if this importer claims the path.
    fn matches(&self, path: &str) -> bool;

    fn doc_type(&self) -> DocType;

    /// `true` rejects parsed files whose folder doesn't resolve to a
    /// project; `false` allows global files.
    fn project_required(&self) -> bool {
        false
    }

    /// Parse on-disk bytes into a `SyncedFile`. `None` means "skip"
    /// (logged by the runner). `needs_rewrite` is reflected back to the
    /// write job to trigger canonicalisation.
    fn parse(&self, content: &[u8], path: &str) -> Option<ParsedFile>;

    fn upsert_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        file: &SyncedFile,
        path: &str,
        project_id: Option<ProjectId>,
        file_hash: GitFileHash,
    ) -> impl std::future::Future<Output = Result<(), UpsertError>> + Send;

    fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        path: &str,
    ) -> impl std::future::Future<Output = Result<(), UpsertError>> + Send;
}

/// `runtime/{subdir}/<file>.{ext}` (global) or
/// `runtime/projects/*/{subdir}/<file>.{ext}` (project-scoped).
pub fn matches_runtime_subdir(path: &str, subdir: &str, ext: &str) -> bool {
    let dot_ext = format!(".{ext}");
    if !path.ends_with(&dot_ext) {
        return false;
    }
    if path.starts_with(&format!("runtime/{subdir}/")) {
        return true;
    }
    if path.starts_with("runtime/projects/") {
        return path.contains(&format!("/{subdir}/"));
    }
    false
}

#[derive(Debug)]
pub struct ParsedFile {
    pub file: SyncedFile,
    /// True when the on-disk form is non-canonical (no `id:`, stale path,
    /// missing frontmatter); the write job rewrites with canonical headers.
    pub needs_rewrite: bool,
}

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

pub(crate) fn slugify(title: &str) -> String {
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
