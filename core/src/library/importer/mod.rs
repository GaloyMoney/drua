use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::library::{DocType, GitFileHash, LibraryImporter, ParsedFile, SyncedFile, UpsertError};
use crate::primitives::ProjectId;

mod space_files;

pub use space_files::SpaceFilesImporter;

/// Resolves blob paths to importer dispatch. Registration order =
/// priority: the first matcher that claims a path wins.
pub struct ImporterRegistry {
    importers: Vec<Box<dyn ErasedImporter>>,
}

impl Default for ImporterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ImporterRegistry {
    pub fn new() -> Self {
        Self {
            importers: Vec::new(),
        }
    }

    pub fn register<I: LibraryImporter>(mut self, importer: Arc<I>) -> Self {
        self.importers
            .push(Box::new(TypedImporter { inner: importer }));
        self
    }

    pub fn dispatch_for(&self, path: &str) -> Option<&dyn ErasedImporter> {
        self.importers
            .iter()
            .find(|i| i.matches(path))
            .map(|i| i.as_ref())
    }
}

/// Object-safe shim over `LibraryImporter`. Async methods return
/// `Pin<Box<dyn Future>>` because `impl Future` isn't dyn-compatible.
pub trait ErasedImporter: Send + Sync {
    fn matches(&self, path: &str) -> bool;
    fn doc_type(&self) -> DocType;
    fn project_required(&self) -> bool;
    fn parse(&self, content: &[u8], path: &str) -> Option<ParsedFile>;
    fn upsert<'a>(
        &'a self,
        op: &'a mut es_entity::DbOp<'_>,
        file: &'a SyncedFile,
        path: &'a str,
        project: Option<ProjectId>,
        hash: GitFileHash,
    ) -> Pin<Box<dyn Future<Output = Result<(), UpsertError>> + Send + 'a>>;
    fn delete<'a>(
        &'a self,
        op: &'a mut es_entity::DbOp<'_>,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), UpsertError>> + Send + 'a>>;
}

struct TypedImporter<I> {
    inner: Arc<I>,
}

impl<I: LibraryImporter> ErasedImporter for TypedImporter<I> {
    fn matches(&self, path: &str) -> bool {
        self.inner.matches(path)
    }
    fn doc_type(&self) -> DocType {
        self.inner.doc_type()
    }
    fn project_required(&self) -> bool {
        self.inner.project_required()
    }
    fn parse(&self, content: &[u8], path: &str) -> Option<ParsedFile> {
        self.inner.parse(content, path)
    }
    fn upsert<'a>(
        &'a self,
        op: &'a mut es_entity::DbOp<'_>,
        file: &'a SyncedFile,
        path: &'a str,
        project: Option<ProjectId>,
        hash: GitFileHash,
    ) -> Pin<Box<dyn Future<Output = Result<(), UpsertError>> + Send + 'a>> {
        Box::pin(self.inner.upsert_in_op(op, file, path, project, hash))
    }
    fn delete<'a>(
        &'a self,
        op: &'a mut es_entity::DbOp<'_>,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), UpsertError>> + Send + 'a>> {
        Box::pin(self.inner.delete_in_op(op, path))
    }
}
