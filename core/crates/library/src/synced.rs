use es_entity::operation::hooks::{CommitHook, HookOperation, PreCommitRet};
use job::{JobId, JobSpawner};

use crate::attribution::CommitAttribution;
use crate::importer::DocType;
use crate::job::{LibraryEmbedConfig, LibraryWriteConfig, WriteOp};
use crate::search::{SearchStore, SearchableFields};

/// Implemented on entity types whose mutations should sync to the
/// library repo. The library crate calls `searchable_fields` to upsert
/// a search row and `write_op` to schedule the upstream commit; the
/// trait does not assume any particular project / scope hierarchy.
pub trait LibrarySynced: Sized + Send + Sync + 'static {
    type Event: es_entity::EsEvent + 'static;

    /// True for events that change rendered content (skips no-op syncs).
    fn is_content_event(ev: &Self::Event) -> bool;

    /// Search-index projection. `doc_id` should be derivable from the
    /// entity id; `scope_id` is whatever the caller's scope concept is
    /// (project id, space id, …); `path` carries the relative path
    /// inside the scope when meaningful.
    fn searchable_fields(&self) -> SearchableFields;

    /// Upstream write to perform on commit. Typically
    /// [`WriteOp::WriteFile`] or [`WriteOp::WriteFileWithRename`] when
    /// importing a non-canonical file.
    fn write_op(&self) -> WriteOp;

    /// Optional doc-id-by-doc-type that should also be removed from the
    /// search index when this entity is synced (e.g. cleanup of a
    /// previous project_id row when an entity moves projects). Default:
    /// nothing extra.
    fn extra_search_deletes(&self) -> Vec<(uuid::Uuid, DocType)> {
        Vec::new()
    }
}

/// Single entry in the per-transaction batch.
pub(crate) struct HookEntry {
    pub fields: Option<SearchableFields>,
    pub deletes: Vec<(uuid::Uuid, DocType)>,
    pub write_op: Option<WriteOp>,
    pub attribution: CommitAttribution,
}

/// Batches per-transaction library writes. On the per-transaction
/// pre-commit:
/// - upserts every search row in the same op
/// - spawns one embed job per upserted row so the embedding lands
///   without waiting for the upstream-commit round-trip (importer
///   short-circuits on file_hash match to avoid re-embedding the
///   round-trip's tick)
/// - spawns one [`crate::job::LibraryWriteJobInitializer`] job per
///   write so the upstream commit is durable past the transaction
///   (the job is persisted in the same op as the entity, so the
///   outbox property holds — entity + job land or roll back together).
pub(crate) struct LibrarySyncHook {
    write_spawner: JobSpawner<LibraryWriteConfig>,
    embed_spawner: JobSpawner<LibraryEmbedConfig>,
    search: SearchStore,
    entries: Vec<HookEntry>,
}

impl LibrarySyncHook {
    pub(crate) fn new(
        write_spawner: JobSpawner<LibraryWriteConfig>,
        embed_spawner: JobSpawner<LibraryEmbedConfig>,
        search: SearchStore,
        entry: HookEntry,
    ) -> Self {
        Self {
            write_spawner,
            embed_spawner,
            search,
            entries: vec![entry],
        }
    }
}

impl CommitHook for LibrarySyncHook {
    async fn pre_commit(
        self,
        mut op: HookOperation<'_>,
    ) -> Result<PreCommitRet<'_, Self>, sqlx::Error> {
        for entry in &self.entries {
            if let Some(fields) = &entry.fields {
                self.search
                    .upsert_in_op(&mut op, fields)
                    .await
                    .map_err(sqlx_from_library_err)?;
                let embed_config = LibraryEmbedConfig {
                    doc_id: fields.doc_id,
                    doc_type: fields.doc_type.clone(),
                };
                if let Err(e) = self
                    .embed_spawner
                    .spawn_in_op(&mut op, JobId::new(), embed_config)
                    .await
                {
                    return Err(sqlx::Error::Protocol(format!("library embed spawn: {e}")));
                }
            }
            for (doc_id, doc_type) in &entry.deletes {
                self.search
                    .delete_in_op(&mut op, *doc_id, doc_type.clone())
                    .await
                    .map_err(sqlx_from_library_err)?;
            }
            if let Some(write_op) = &entry.write_op {
                let config = LibraryWriteConfig {
                    op: write_op.clone(),
                    attribution: entry.attribution.clone(),
                };
                if let Err(e) = self
                    .write_spawner
                    .spawn_in_op(&mut op, JobId::new(), config)
                    .await
                {
                    return Err(sqlx::Error::Protocol(format!("library write spawn: {e}")));
                }
            }
        }
        PreCommitRet::ok(self, op)
    }

    fn merge(&mut self, other: &mut Self) -> bool {
        self.entries.append(&mut other.entries);
        true
    }
}

fn sqlx_from_library_err(e: crate::LibraryError) -> sqlx::Error {
    match e {
        crate::LibraryError::Sqlx(s) => s,
        other => sqlx::Error::Protocol(format!("library: {other}")),
    }
}
