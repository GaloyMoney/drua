mod error;
pub mod space_fs;
pub(crate) mod space_path;

use std::sync::Arc;

use es_entity::AtomicOperation;

pub use drua_library::{
    GitFileHash, NewSpace, SearchableFields, Space, SpaceError, SpaceEvent, WriteOp,
};
pub use error::LibraryError;
pub use space_fs::{FileView, SpaceFs};

use crate::github_app::GitHubAppTokenProvider;

const DEFAULT_SKILL_SYNC_INTERVAL_SECS: u64 = 20;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct LibraryConfig {
    /// Defaults to `<repo-root>/.library/`.
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default)]
    pub repo_url: Option<String>,
    /// Default 20s. Reused as the upstream-fetch interval.
    #[serde(default = "default_skill_sync_interval_secs")]
    pub skill_sync_interval_secs: u64,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            repo_url: None,
            skill_sync_interval_secs: DEFAULT_SKILL_SYNC_INTERVAL_SECS,
        }
    }
}

fn default_skill_sync_interval_secs() -> u64 {
    DEFAULT_SKILL_SYNC_INTERVAL_SECS
}

impl LibraryConfig {
    pub fn repo_path(&self) -> std::path::PathBuf {
        match &self.data_dir {
            Some(d) => std::path::PathBuf::from(d).join("repo"),
            None => std::path::PathBuf::from(".library"),
        }
    }
}

/// Closed enum of doc types this codebase emits. drua_library uses an
/// open string type; the conversions are trivial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocType {
    Note,
    Skill,
    Workflow,
    SpaceFile,
}

impl DocType {
    pub fn as_str(&self) -> &'static str {
        match self {
            DocType::Note => "note",
            DocType::Skill => "skill",
            DocType::Workflow => "workflow",
            DocType::SpaceFile => "space_file",
        }
    }

    pub fn subdir(&self) -> &'static str {
        match self {
            DocType::Note => "notes",
            DocType::Skill => "skills",
            DocType::Workflow => "workflows",
            DocType::SpaceFile => "spaces",
        }
    }

    pub fn ext(&self) -> &'static str {
        match self {
            DocType::Note => "md",
            DocType::Skill => "md",
            DocType::Workflow => "yml",
            DocType::SpaceFile => "md",
        }
    }
}

impl From<DocType> for drua_library::DocType {
    fn from(d: DocType) -> Self {
        drua_library::DocType::new(d.as_str())
    }
}

impl TryFrom<&str> for DocType {
    type Error = String;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "note" => Ok(DocType::Note),
            "skill" => Ok(DocType::Skill),
            "workflow" => Ok(DocType::Workflow),
            "space_file" => Ok(DocType::SpaceFile),
            other => Err(format!("unknown doc_type: {other}")),
        }
    }
}

/// Implemented on entity types whose mutations should sync to the
/// library repo. `core::library::Library::sync_entity_in_op` projects
/// an `E: LibrarySynced` into the underlying `drua_library` write
/// pipeline (search row + upstream commit) without leaking `WriteOp`
/// or `SearchableFields` to the entity layer.
pub trait LibrarySynced: Sized + Send + Sync + 'static {
    type Event: es_entity::EsEvent + 'static;

    const DOC_TYPE: DocType;

    /// Sync only fires when one of these event variants was just persisted.
    fn is_content_event(ev: &Self::Event) -> bool;

    /// `None` means "global" file (not project-scoped).
    fn project(&self) -> Option<(crate::primitives::ProjectId, &str)>;

    fn id(&self) -> uuid::Uuid;

    /// Display name → searchable `title` and slug source.
    fn display_name(&self) -> &str;

    fn created_at(&self) -> chrono::DateTime<chrono::Utc>;
    fn updated_at(&self) -> chrono::DateTime<chrono::Utc>;
    fn original_path(&self) -> Option<&str> {
        None
    }

    /// Searchable body projection.
    fn index_body(&self) -> &str;

    fn index_tags(&self) -> Vec<String> {
        Vec::new()
    }

    /// Render the canonical on-disk content. Must be deterministic so
    /// the upstream write's hash short-circuit doesn't loop.
    fn render(&self) -> String;
}

pub fn slugify(title: &str) -> String {
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

/// `runtime/projects/{project}/{subdir}/{slug}-{id_prefix}.{ext}` when scoped,
/// else `runtime/{subdir}/{slug}-{id_prefix}.{ext}` (Skill/Workflow only).
fn relative_path_for<E: LibrarySynced>(entity: &E) -> String {
    let id = entity.id();
    let id_prefix = &id.to_string()[..8];
    let slug = slugify(entity.display_name());
    let subdir = E::DOC_TYPE.subdir();
    let ext = E::DOC_TYPE.ext();
    match entity.project() {
        Some((_, project_name)) => {
            format!("runtime/projects/{project_name}/{subdir}/{slug}-{id_prefix}.{ext}")
        }
        None => format!("runtime/{subdir}/{slug}-{id_prefix}.{ext}"),
    }
}

fn project_searchable<E: LibrarySynced>(entity: &E) -> SearchableFields {
    let project = entity.project();
    SearchableFields {
        doc_id: entity.id(),
        doc_type: E::DOC_TYPE.into(),
        scope_id: project.map(|(id, _)| id.into()),
        scope_slug: project.map(|(_, name)| name.to_string()),
        name: entity.display_name().to_string(),
        path: Some(relative_path_for(entity)),
        content: entity.index_body().to_string(),
    }
}

fn project_write_op<E: LibrarySynced>(entity: &E) -> WriteOp {
    let canonical_path = relative_path_for(entity);
    let content = entity.render().into_bytes();
    let message = format!(
        "{}: {}-{}",
        E::DOC_TYPE.as_str(),
        slugify(entity.display_name()),
        &entity.id().to_string()[..8],
    );
    match entity.original_path() {
        Some(orig) if orig != canonical_path => WriteOp::WriteFileWithRename {
            old_path: orig.to_string(),
            new_path: canonical_path,
            content,
            message,
        },
        _ => WriteOp::WriteFile {
            path: canonical_path,
            content,
            message,
        },
    }
}

/// Search-result shapes returned by `Library::search`/`search_global`/`get_files`.
/// Tags are dropped (the new schema folds them into the FTS-indexed content);
/// `project_id` is preserved as the nil UUID for global / unscoped docs.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub score: f64,
}

impl std::fmt::Display for SearchResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "id: {}\ntitle: {}\n", self.doc_id, self.title)?;
        if !self.tags.is_empty() {
            writeln!(f, "  tags: {}", self.tags.join(", "))?;
        }
        let preview: String = self.content.chars().take(200).collect();
        write!(f, "preview: {preview}")
    }
}

#[derive(Debug, Clone)]
pub struct GlobalSearchHit {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub project_id: uuid::Uuid,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub score: f64,
    pub space_slug: Option<String>,
    pub relative_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LibraryFile {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub project_id: uuid::Uuid,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub space_slug: Option<String>,
    pub relative_path: Option<String>,
}

fn hit_to_result(hit: drua_library::SearchHit) -> Option<SearchResult> {
    let doc_type = DocType::try_from(hit.fields.doc_type.as_str()).ok()?;
    Some(SearchResult {
        doc_id: hit.fields.doc_id,
        doc_type,
        title: hit.fields.name,
        content: hit.fields.content,
        tags: Vec::new(),
        score: hit.score,
    })
}

fn hit_to_global(hit: drua_library::SearchHit) -> Option<GlobalSearchHit> {
    let doc_type = DocType::try_from(hit.fields.doc_type.as_str()).ok()?;
    let space_slug = if matches!(doc_type, DocType::SpaceFile) {
        hit.fields.scope_slug.clone()
    } else {
        None
    };
    let relative_path = if matches!(doc_type, DocType::SpaceFile) {
        hit.fields.path.clone()
    } else {
        None
    };
    Some(GlobalSearchHit {
        doc_id: hit.fields.doc_id,
        doc_type,
        project_id: hit.fields.scope_id.unwrap_or_else(uuid::Uuid::nil),
        title: hit.fields.name,
        content: hit.fields.content,
        tags: Vec::new(),
        score: hit.score,
        space_slug,
        relative_path,
    })
}

fn fields_to_library_file(fields: SearchableFields) -> Option<LibraryFile> {
    let doc_type = DocType::try_from(fields.doc_type.as_str()).ok()?;
    let space_slug = if matches!(doc_type, DocType::SpaceFile) {
        fields.scope_slug.clone()
    } else {
        None
    };
    let relative_path = if matches!(doc_type, DocType::SpaceFile) {
        fields.path.clone()
    } else {
        None
    };
    Some(LibraryFile {
        doc_id: fields.doc_id,
        doc_type,
        project_id: fields.scope_id.unwrap_or_else(uuid::Uuid::nil),
        title: fields.name,
        body: fields.content,
        tags: Vec::new(),
        space_slug,
        relative_path,
    })
}

/// Auth-gated facade over `drua_library::Library`. Repos call
/// `sync_entity_in_op` from their `post_persist_hook`; tools call
/// `search`/`search_global`/`get_files`; spaces work via the dedicated
/// space-file methods or `SpaceFs`.
#[derive(Clone)]
pub struct Library {
    inner: Arc<drua_library::Library>,
}

impl Library {
    pub async fn init(
        config: &LibraryConfig,
        pool: &sqlx::PgPool,
        embedder: Arc<code_assistant_core::embedder::Embedder>,
        jobs: &mut ::job::Jobs,
        github_app: Option<Arc<GitHubAppTokenProvider>>,
    ) -> Result<Self, LibraryError> {
        let repo_url = config.repo_url.clone().unwrap_or_default();
        let data_dir = config.repo_path().to_string_lossy().into_owned();
        let drua_config = drua_library::LibraryConfig {
            data_dir,
            repo_url,
            fetch_interval_ms: config
                .skill_sync_interval_secs
                .saturating_mul(1000)
                .max(1000),
        };
        let inner = drua_library::Library::init(pool, &drua_config, embedder, jobs, github_app)
            .await
            .map_err(LibraryError::Drua)?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Per-repo `post_persist_hook` body: project the entity into a
    /// `(SearchableFields, WriteOp)` pair and enqueue both on the
    /// per-transaction batch (search row inline; write job spawned via
    /// `spawn_in_op`).
    #[tracing::instrument(
        name = "library.sync_entity_in_op",
        skip_all,
        fields(doc_type = E::DOC_TYPE.as_str())
    )]
    pub async fn sync_entity_in_op<E, OP>(
        &self,
        op: &mut OP,
        entity: &E,
        new_events: &mut es_entity::LastPersisted<'_, E::Event>,
    ) -> Result<(), LibraryError>
    where
        E: LibrarySynced,
        OP: AtomicOperation,
    {
        if !new_events.any(|p| E::is_content_event(&p.event)) {
            return Ok(());
        }
        let fields = project_searchable(entity);
        let write_op = project_write_op(entity);
        self.inner
            .enqueue_full_in_op(op, Some(fields), Vec::new(), Some(write_op))
            .await?;
        Ok(())
    }

    /// Queues a single batched commit that materialises `notes/`,
    /// `skills/`, `workflows/` `.gitkeep` markers under the project's
    /// runtime dir.
    #[tracing::instrument(name = "library.sync_project_folder_in_op", skip_all)]
    pub async fn sync_project_folder_in_op(
        &self,
        op: &mut impl AtomicOperation,
        project_name: &str,
    ) -> Result<(), LibraryError> {
        let changes = ["notes", "skills", "workflows"]
            .iter()
            .map(|sub| {
                (
                    format!("runtime/projects/{project_name}/{sub}/.gitkeep"),
                    Some(Vec::new()),
                )
            })
            .collect::<Vec<_>>();
        self.inner
            .enqueue_write_in_op(
                op,
                WriteOp::Batch {
                    changes,
                    message: format!("project: init {project_name}"),
                },
            )
            .await?;
        Ok(())
    }

    /// Removes search-index rows for the project + queues `git rm -rf
    /// runtime/projects/{name}/`. Run inside the project delete txn.
    #[tracing::instrument(name = "library.cleanup_project_in_op", skip_all)]
    pub async fn cleanup_project_in_op(
        &self,
        op: &mut impl AtomicOperation,
        project_id: uuid::Uuid,
        project_name: &str,
    ) -> Result<(), LibraryError> {
        self.inner
            .cleanup_for_scope_in_op(
                op,
                project_id,
                format!("runtime/projects/{project_name}"),
                format!("project: delete {project_name}"),
            )
            .await?;
        Ok(())
    }

    /// Persists a new `Space`, scaffolds `spaces/<slug>/.gitkeep` upstream,
    /// and waits for the upstream commit to land.
    #[tracing::instrument(name = "library.create_space", skip(self, sub))]
    pub async fn create_space(
        &self,
        sub: &crate::auth::AuthSubject,
        slug: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Space, LibraryError> {
        sub.can(
            crate::auth::AuthVerb::Create,
            crate::auth::AuthResource::Space(None),
        )?;
        crate::audit::Audit::record_action_if_unset("space.create");
        let space = self
            .inner
            .spaces()
            .create(slug.into(), description)
            .await?;
        tracing::info!(space.id = %space.id, space.slug = %space.slug, "space created");
        Ok(space)
    }

    /// Lists every space, paginated. Soft-deleted spaces drop out
    /// at the SQL layer.
    #[tracing::instrument(name = "library.list_all_spaces", skip(self, sub))]
    pub async fn list_all_spaces(
        &self,
        sub: &crate::auth::AuthSubject,
    ) -> Result<Vec<Space>, LibraryError> {
        sub.can(
            crate::auth::AuthVerb::Read,
            crate::auth::AuthResource::Space(None),
        )?;
        crate::audit::Audit::record_action_if_unset("space.list_all");
        Ok(self.inner.spaces().list_all().await?)
    }

    pub fn space_root(&self, slug: &str) -> std::path::PathBuf {
        self.inner.repo_path().join("spaces").join(slug)
    }

    #[tracing::instrument(name = "library.find_space_by_slug", skip(self))]
    pub async fn find_space_by_slug(&self, slug: &str) -> Result<Option<Space>, LibraryError> {
        Ok(self.inner.spaces().maybe_find_by_slug(slug).await?)
    }

    #[tracing::instrument(name = "library.find_spaces_by_ids", skip(self, ids))]
    pub async fn find_spaces_by_ids(
        &self,
        ids: &[crate::primitives::SpaceId],
    ) -> Result<Vec<Space>, LibraryError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.inner.spaces().find_by_ids(ids).await?)
    }

    /// Trusted-caller — `SpaceFs::write` is the public boundary;
    /// mount-membership authz lives in `Projects`.
    #[tracing::instrument(name = "library.write_space_file", skip(self, space, content), fields(slug = %space.slug))]
    pub(in crate::library) async fn write_space_file(
        &self,
        space: &Space,
        relative_path: String,
        content: String,
    ) -> Result<(), LibraryError> {
        crate::audit::Audit::record_action_if_unset("space.write_file");
        self.inner
            .spaces()
            .write_file(&space.slug, &relative_path, content)
            .await?;
        Ok(())
    }

    #[tracing::instrument(name = "library.delete_space_file", skip(self, space), fields(slug = %space.slug))]
    pub(in crate::library) async fn delete_space_file(
        &self,
        space: &Space,
        relative_path: String,
    ) -> Result<(), LibraryError> {
        crate::audit::Audit::record_action_if_unset("space.delete_file");
        self.inner
            .spaces()
            .delete_file(&space.slug, &relative_path)
            .await?;
        Ok(())
    }

    #[tracing::instrument(name = "library.str_replace_space_file", skip(self, space, old_str, new_str), fields(slug = %space.slug))]
    pub(in crate::library) async fn str_replace_space_file(
        &self,
        space: &Space,
        relative_path: String,
        old_str: String,
        new_str: String,
    ) -> Result<(), LibraryError> {
        crate::audit::Audit::record_action_if_unset("space.str_replace");
        self.inner
            .spaces()
            .str_replace(&space.slug, &relative_path, old_str, new_str)
            .await?;
        Ok(())
    }

    #[tracing::instrument(name = "library.insert_space_file", skip(self, space, text), fields(slug = %space.slug))]
    pub(in crate::library) async fn insert_space_file(
        &self,
        space: &Space,
        relative_path: String,
        line_number: usize,
        text: String,
    ) -> Result<(), LibraryError> {
        crate::audit::Audit::record_action_if_unset("space.insert");
        self.inner
            .spaces()
            .insert(&space.slug, &relative_path, line_number, text)
            .await?;
        Ok(())
    }

    #[tracing::instrument(name = "library.move_space_file", skip(self, space), fields(slug = %space.slug))]
    pub(in crate::library) async fn move_space_file(
        &self,
        space: &Space,
        from_relative_path: String,
        to_relative_path: String,
    ) -> Result<(), LibraryError> {
        crate::audit::Audit::record_action_if_unset("space.move_file");
        self.inner
            .spaces()
            .move_file(&space.slug, &from_relative_path, &to_relative_path)
            .await?;
        Ok(())
    }

    #[tracing::instrument(name = "library.search", skip(self))]
    pub async fn search(
        &self,
        _project_id: uuid::Uuid,
        query: &str,
        doc_type: Option<DocType>,
        limit: usize,
    ) -> Result<Vec<SearchResult>, LibraryError> {
        let doc_types: Vec<drua_library::DocType> = doc_type.into_iter().map(Into::into).collect();
        let hits = self
            .inner
            .search()
            .search(query, None, &[], &doc_types, limit)
            .await?;
        Ok(hits.into_iter().filter_map(hit_to_result).collect())
    }

    /// Cross-project search. Open to any non-anonymous subject;
    /// library content is globally discoverable. Empty `project_ids`
    /// = no project filter; otherwise hits are restricted to those
    /// scope_ids (plus globals — null scope_id).
    #[tracing::instrument(name = "library.search_global", skip(self, sub, project_ids))]
    pub async fn search_global(
        &self,
        sub: &crate::auth::AuthSubject,
        project_ids: &[uuid::Uuid],
        query: &str,
        doc_types: &[DocType],
        limit: usize,
    ) -> Result<Vec<GlobalSearchHit>, LibraryError> {
        if matches!(sub, crate::auth::AuthSubject::Anonymous) {
            return Err(crate::auth::error::AuthorizationError::AuthenticationRequired.into());
        }
        crate::audit::Audit::record_action_if_unset("library.search");
        let effective: Vec<DocType> = if doc_types.is_empty() {
            vec![DocType::Skill, DocType::Note, DocType::SpaceFile]
        } else {
            doc_types
                .iter()
                .copied()
                .filter(|d| !matches!(d, DocType::Workflow))
                .collect()
        };
        if effective.is_empty() {
            return Ok(Vec::new());
        }
        let drua_types: Vec<drua_library::DocType> =
            effective.into_iter().map(Into::into).collect();
        let hits = self
            .inner
            .search()
            .search(query, None, project_ids, &drua_types, limit)
            .await?;
        Ok(hits.into_iter().filter_map(hit_to_global).collect())
    }

    #[tracing::instrument(name = "library.get_files", skip(self, sub, ids))]
    pub async fn get_files(
        &self,
        sub: &crate::auth::AuthSubject,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<LibraryFile>, LibraryError> {
        if matches!(sub, crate::auth::AuthSubject::Anonymous) {
            return Err(crate::auth::error::AuthorizationError::AuthenticationRequired.into());
        }
        crate::audit::Audit::record_action_if_unset("library.get_files");
        let rows = self.inner.search().find_by_ids(ids).await?;
        Ok(rows.into_iter().filter_map(fields_to_library_file).collect())
    }

    /// Append a custom `LibraryImporter` (skills, workflows, …). Called
    /// by `App::init` once the service repos exist; the next CommitTick
    /// routes matching paths to the new importer.
    pub async fn register_importer(&self, importer: Arc<dyn drua_library::LibraryImporter>) {
        self.inner.register_importer(importer).await;
    }
}
