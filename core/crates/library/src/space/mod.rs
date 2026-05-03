mod entity;
pub mod error;
pub(crate) mod repo;

use std::collections::HashMap;
use std::sync::Arc;

pub use entity::{NewSpace, Space, SpaceEvent};
pub use error::*;

use self::repo::SpaceRepo;
use crate::git::GitEngine;
use crate::importer::{DocType, GitFileHash, LibraryImporter, UpsertError};
use crate::SearchableFields;

const SPACE_DOC_TYPE: DocType = DocType::new("space_file");

/// UUID v5 namespace for deriving deterministic doc_ids from `<slug>/<rel_path>`.
const SPACE_DOC_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x4e, 0x97, 0x05, 0x53, 0x53, 0x46, 0x4d, 0x73, 0xa3, 0x9d, 0xa6, 0x4d, 0x6e, 0x39, 0x4d, 0x53,
]);

#[derive(Clone)]
pub struct Spaces {
    git: Arc<GitEngine>,
    repo: SpaceRepo,
}

impl Spaces {
    pub fn new(git: &Arc<GitEngine>, pool: &sqlx::PgPool) -> Self {
        Self {
            git: Arc::clone(git),
            repo: SpaceRepo::new(pool),
        }
    }

    #[tracing::instrument(name = "library.spaces.create", skip_all, fields(%slug))]
    pub async fn create(
        &self,
        slug: String,
        description: Option<String>,
    ) -> Result<Space, SpaceError> {
        let mut op = self.repo.begin_op().await?;
        let space = self.create_in_op(&mut op, slug, description).await?;
        op.commit().await?;
        Ok(space)
    }

    #[tracing::instrument(name = "library.spaces.create_in_op", skip_all, fields(%slug))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        slug: String,
        description: Option<String>,
    ) -> Result<Space, SpaceError> {
        let mut builder = NewSpace::builder().slug(slug);
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        let new_space = builder.build()?;
        let space = self.repo.create_in_op(op, new_space).await?;

        self.git
            .update_file(
                format!("spaces/{}/.gitkeep", space.slug),
                |_input_path, _current| Ok((None, Some(Vec::new()))),
                format!("space: init {}", space.slug),
            )
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))?;

        Ok(space)
    }

    /// Blind overwrite of `spaces/{slug}/{relative_path}`.
    #[tracing::instrument(name = "library.spaces.write_file", skip_all, fields(%slug, %relative_path))]
    pub async fn write_file(
        &self,
        slug: &str,
        relative_path: &str,
        content: String,
    ) -> Result<(), SpaceError> {
        let path = format!("spaces/{slug}/{relative_path}");
        let bytes = content.into_bytes();
        self.git
            .update_file(
                path,
                move |_, _| Ok((None, Some(bytes.clone()))),
                format!("space:{slug}: write {relative_path}"),
            )
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))
    }

    /// Removes `spaces/{slug}/{relative_path}`. No-op if absent at HEAD.
    #[tracing::instrument(name = "library.spaces.delete_file", skip_all, fields(%slug, %relative_path))]
    pub async fn delete_file(&self, slug: &str, relative_path: &str) -> Result<(), SpaceError> {
        let path = format!("spaces/{slug}/{relative_path}");
        self.git
            .update_file(
                path,
                |_, _| Ok((None, None)),
                format!("space:{slug}: delete {relative_path}"),
            )
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))
    }

    /// Read–modify–write substitution: errors if `old_str` doesn't appear
    /// exactly once in the freshest disk content.
    #[tracing::instrument(name = "library.spaces.str_replace", skip_all, fields(%slug, %relative_path))]
    pub async fn str_replace(
        &self,
        slug: &str,
        relative_path: &str,
        old_str: String,
        new_str: String,
    ) -> Result<(), SpaceError> {
        let path = format!("spaces/{slug}/{relative_path}");
        let path_for_err = path.clone();
        self.git
            .update_file(
                path,
                move |_, current| {
                    let current = current.ok_or_else(|| {
                        crate::LibraryError::Validation(format!(
                            "str_replace: file does not exist: {path_for_err}"
                        ))
                    })?;
                    let current_str = std::str::from_utf8(current).map_err(|e| {
                        crate::LibraryError::Validation(format!(
                            "str_replace: non-utf8 content in {path_for_err}: {e}"
                        ))
                    })?;
                    let count = current_str.matches(&old_str).count();
                    if count == 0 {
                        return Err(crate::LibraryError::Validation(format!(
                            "str_replace: old_str not found in {path_for_err}"
                        )));
                    }
                    if count > 1 {
                        return Err(crate::LibraryError::Validation(format!(
                            "str_replace: old_str appears {count} times in {path_for_err}; must be unique"
                        )));
                    }
                    let new_content = current_str.replacen(&old_str, &new_str, 1);
                    Ok((None, Some(new_content.into_bytes())))
                },
                format!("space:{slug}: edit {relative_path}"),
            )
            .await
            .map_err(|e| match e {
                crate::LibraryError::Validation(msg) => SpaceError::Validation(msg),
                other => SpaceError::Git(other.to_string()),
            })
    }

    /// Read–modify–write insert. `line_number == 0` inserts at the
    /// beginning; out-of-range numbers append at EOF.
    #[tracing::instrument(name = "library.spaces.insert", skip_all, fields(%slug, %relative_path))]
    pub async fn insert(
        &self,
        slug: &str,
        relative_path: &str,
        line_number: usize,
        text: String,
    ) -> Result<(), SpaceError> {
        let path = format!("spaces/{slug}/{relative_path}");
        let path_for_err = path.clone();
        self.git
            .update_file(
                path,
                move |_, current| {
                    let current = current.ok_or_else(|| {
                        crate::LibraryError::Validation(format!(
                            "insert: file does not exist: {path_for_err}"
                        ))
                    })?;
                    let current_str = std::str::from_utf8(current).map_err(|e| {
                        crate::LibraryError::Validation(format!(
                            "insert: non-utf8 content in {path_for_err}: {e}"
                        ))
                    })?;
                    let mut lines: Vec<String> = current_str.lines().map(String::from).collect();
                    let idx = line_number.min(lines.len());
                    for (offset, t) in text.lines().enumerate() {
                        lines.insert(idx + offset, t.to_string());
                    }
                    let mut new_content = lines.join("\n");
                    if current_str.ends_with('\n') {
                        new_content.push('\n');
                    }
                    Ok((None, Some(new_content.into_bytes())))
                },
                format!("space:{slug}: insert {relative_path}"),
            )
            .await
            .map_err(|e| match e {
                crate::LibraryError::Validation(msg) => SpaceError::Validation(msg),
                other => SpaceError::Git(other.to_string()),
            })
    }

    /// Lookup by slug; soft-deleted entries drop out at the SQL layer.
    #[tracing::instrument(name = "library.spaces.maybe_find_by_slug", skip_all, fields(%slug))]
    pub async fn maybe_find_by_slug(&self, slug: &str) -> Result<Option<Space>, SpaceError> {
        Ok(self.repo.maybe_find_by_slug(slug).await?)
    }

    /// Bulk hydration. Soft-deleted ids silently drop out. Order of the
    /// returned `Vec` is not aligned with the input slice.
    #[tracing::instrument(name = "library.spaces.find_by_ids", skip_all, fields(count = ids.len()))]
    pub async fn find_by_ids(
        &self,
        ids: &[crate::primitives::SpaceId],
    ) -> Result<Vec<Space>, SpaceError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let map = self.repo.find_all::<Space>(ids).await?;
        Ok(map.into_values().collect())
    }

    /// Reads a blob at `spaces/<slug>/<rel_path>` from HEAD's tree.
    /// `Ok(None)` when the file doesn't exist (or the repo is unborn).
    #[tracing::instrument(name = "library.spaces.read_file", skip_all, fields(%slug, %rel_path))]
    pub async fn read_file(
        &self,
        slug: &str,
        rel_path: &str,
    ) -> Result<Option<Vec<u8>>, SpaceError> {
        let path = format!("spaces/{slug}/{rel_path}");
        self.git
            .read_blob_at_head(&path)
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))
    }

    /// Lists immediate children under `spaces/<slug>/<rel_path>` at
    /// HEAD. `Ok(None)` when the directory doesn't exist. Empty
    /// `rel_path` lists the space's root.
    #[tracing::instrument(name = "library.spaces.list_dir", skip_all, fields(%slug, %rel_path))]
    pub async fn list_dir(
        &self,
        slug: &str,
        rel_path: &str,
    ) -> Result<Option<Vec<crate::git::DirEntry>>, SpaceError> {
        let path = if rel_path.is_empty() {
            format!("spaces/{slug}")
        } else {
            format!("spaces/{slug}/{rel_path}")
        };
        self.git
            .list_dir_at_head(&path)
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))
    }

    /// Recursively walks every blob under `spaces/<slug>/<rel_path>`.
    /// Returned paths are relative to `spaces/<slug>/` (not the repo root).
    #[tracing::instrument(name = "library.spaces.walk", skip_all, fields(%slug, %rel_path))]
    pub async fn walk(
        &self,
        slug: &str,
        rel_path: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, SpaceError> {
        let path = if rel_path.is_empty() {
            format!("spaces/{slug}")
        } else {
            format!("spaces/{slug}/{rel_path}")
        };
        let strip = format!("spaces/{slug}/");
        let mut blobs = self
            .git
            .walk_blobs_at_head(&path)
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))?;
        for (p, _) in blobs.iter_mut() {
            if let Some(rest) = p.strip_prefix(&strip) {
                *p = rest.to_string();
            }
        }
        Ok(blobs)
    }

    /// Lists every space, paginated through the `slug` list_by index.
    #[tracing::instrument(name = "library.spaces.list_all", skip_all)]
    pub async fn list_all(&self) -> Result<Vec<Space>, SpaceError> {
        use es_entity::ListDirection;
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .repo
                .list_by_slug(
                    es_entity::PaginatedQueryArgs { first: 200, after },
                    ListDirection::Ascending,
                )
                .await?;
            out.extend(page.entities);
            if !page.has_next_page {
                break;
            }
            after = page.end_cursor;
        }
        Ok(out)
    }

    /// Apply N space ops in a single git commit. Per-op validation
    /// (`str_replace` uniqueness, file-exists for RMW commands, etc.) runs
    /// against an in-memory accumulator rooted at HEAD; failed ops are
    /// excluded from the commit but are reported in the returned vec
    /// (which preserves input order). If the final commit/push fails,
    /// every op that survived validation is downgraded to `Err(Git)`.
    #[tracing::instrument(name = "library.spaces.apply_batch", skip_all, fields(n = ops.len()))]
    pub async fn apply_batch(&self, ops: Vec<SpaceOp>) -> Vec<Result<(), SpaceError>> {
        if ops.is_empty() {
            return Vec::new();
        }

        let mut state: HashMap<String, FileState> = HashMap::new();
        let mut results: Vec<Result<(), SpaceError>> = Vec::with_capacity(ops.len());

        for op in &ops {
            let full = format!("spaces/{}/{}", op.slug, op.rel_path);
            let res = match &op.kind {
                SpaceOpKind::Write { content } => {
                    state
                        .entry(full.clone())
                        .or_insert_with(FileState::new_unread)
                        .set_current(Some(content.clone()));
                    Ok(())
                }
                SpaceOpKind::Delete => {
                    self.read_into(&mut state, &full).await;
                    let entry = state.get_mut(&full).expect("read_into populated this path");
                    if let Some(msg) = entry.read_error.as_ref() {
                        Err(SpaceError::Validation(msg.clone()))
                    } else {
                        entry.set_current(None);
                        Ok(())
                    }
                }
                SpaceOpKind::StrReplace { old_str, new_str } => {
                    self.read_into(&mut state, &full).await;
                    apply_str_replace(&mut state, &full, old_str, new_str)
                }
                SpaceOpKind::Insert { line_number, text } => {
                    self.read_into(&mut state, &full).await;
                    apply_insert(&mut state, &full, *line_number, text)
                }
                SpaceOpKind::Move { to_rel_path } => {
                    let to_full = format!("spaces/{}/{}", op.slug, to_rel_path);
                    self.read_into(&mut state, &full).await;
                    self.read_into(&mut state, &to_full).await;
                    apply_move(&mut state, &full, &to_full)
                }
            };
            results.push(res);
        }

        let changes: Vec<(String, Option<Vec<u8>>)> = state
            .into_iter()
            .filter_map(|(path, st)| st.into_change(path))
            .collect();

        if changes.is_empty() {
            return results;
        }

        let n_changes = changes.len();
        let msg = format!("drua: batch ({n_changes} writes)");
        if let Err(e) = self.git.commit_changes(changes, msg).await {
            let reason = e.to_string();
            for r in results.iter_mut() {
                if r.is_ok() {
                    *r = Err(SpaceError::BatchAborted {
                        reason: reason.clone(),
                    });
                }
            }
        }

        results
    }

    /// Reads `path`'s blob from HEAD into `state` if not already cached.
    /// Read failures (including `Validation` for tree-at-path) are
    /// surfaced lazily via `state.head` so the next per-op step can
    /// produce its own error.
    async fn read_into(&self, state: &mut HashMap<String, FileState>, path: &str) {
        if state.contains_key(path) {
            return;
        }
        let entry = match self.git.read_blob_at_head(path).await {
            Ok(opt) => FileState::from_head(opt),
            Err(e) => FileState::read_error(e.to_string()),
        };
        state.insert(path.to_string(), entry);
    }

    /// Renames `spaces/{slug}/{from}` → `spaces/{slug}/{to}`. Errors if
    /// `from` is missing or `to` already exists.
    #[tracing::instrument(name = "library.spaces.move_file", skip_all, fields(%slug, %from, %to))]
    pub async fn move_file(&self, slug: &str, from: &str, to: &str) -> Result<(), SpaceError> {
        let from_path = format!("spaces/{slug}/{from}");
        let to_path = format!("spaces/{slug}/{to}");
        self.git
            .move_file(
                from_path,
                to_path,
                format!("space:{slug}: move {from} -> {to}"),
            )
            .await
            .map_err(|e| match e {
                crate::LibraryError::Validation(msg) => SpaceError::Validation(msg),
                other => SpaceError::Git(other.to_string()),
            })
    }
}

#[async_trait::async_trait]
impl LibraryImporter for Spaces {
    fn matches(&self, path: &str) -> bool {
        let mut parts = path.splitn(3, '/');
        parts.next() == Some("spaces") && parts.next().is_some() && parts.next().is_some()
    }

    fn doc_type(&self) -> DocType {
        SPACE_DOC_TYPE
    }

    async fn upsert_in_op(
        &self,
        _op: &mut es_entity::DbOp<'_>,
        _old_file_hash: Option<GitFileHash>,
        _file_hash: GitFileHash,
        path: &str,
        content: &[u8],
    ) -> Result<Option<SearchableFields>, UpsertError> {
        if path.ends_with("/.gitkeep") {
            return Ok(None);
        }
        let mut parts = path.splitn(3, '/');
        let _ = parts.next();
        let slug = parts
            .next()
            .ok_or_else(|| UpsertError::Parse(format!("bad space path: {path}")))?;
        let rel = parts
            .next()
            .ok_or_else(|| UpsertError::Parse(format!("bad space path: {path}")))?;

        let space = match self
            .repo
            .maybe_find_by_slug(slug)
            .await
            .map_err(|e| UpsertError::Other(e.to_string()))?
        {
            Some(s) => s,
            None => {
                tracing::debug!(slug, path, "space not found; skipping");
                return Ok(None);
            }
        };

        let content_str = std::str::from_utf8(content)
            .map_err(|e| UpsertError::Parse(format!("non-utf8 content: {e}")))?
            .to_string();
        let name = std::path::Path::new(rel)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(rel)
            .to_string();

        let doc_id = uuid::Uuid::new_v5(
            &SPACE_DOC_NAMESPACE,
            format!("{}/{}", space.slug, rel).as_bytes(),
        );

        Ok(Some(SearchableFields {
            doc_id,
            doc_type: SPACE_DOC_TYPE,
            scope_id: Some(space.id.into()),
            scope_slug: Some(space.slug.clone()),
            name,
            path: Some(rel.to_string()),
            content: content_str,
        }))
    }

    async fn delete_in_op(
        &self,
        _op: &mut es_entity::DbOp<'_>,
        path: &str,
    ) -> Result<Option<uuid::Uuid>, UpsertError> {
        if path.ends_with("/.gitkeep") {
            return Ok(None);
        }
        let mut parts = path.splitn(3, '/');
        let _ = parts.next();
        let slug = parts
            .next()
            .ok_or_else(|| UpsertError::Parse(format!("bad space path: {path}")))?;
        let rel = parts
            .next()
            .ok_or_else(|| UpsertError::Parse(format!("bad space path: {path}")))?;

        Ok(Some(uuid::Uuid::new_v5(
            &SPACE_DOC_NAMESPACE,
            format!("{slug}/{rel}").as_bytes(),
        )))
    }
}

/// One logical write against a space, queued into [`Spaces::apply_batch`].
#[derive(Debug, Clone)]
pub struct SpaceOp {
    pub slug: String,
    pub rel_path: String,
    pub kind: SpaceOpKind,
}

#[derive(Debug, Clone)]
pub enum SpaceOpKind {
    Write {
        content: Vec<u8>,
    },
    Delete,
    StrReplace {
        old_str: String,
        new_str: String,
    },
    Insert {
        line_number: usize,
        text: String,
    },
    /// Rename within the same space. `to_rel_path` is the destination
    /// relative to `spaces/<slug>/`.
    Move {
        to_rel_path: String,
    },
}

/// Per-path accumulator state. Tracks both the value at HEAD (so the
/// final commit only includes paths that actually changed) and the
/// current value as ops are layered on. `read_error` carries a
/// `Validation` from libgit2 (e.g. path-is-a-directory) so the next
/// RMW op against this path fails cleanly.
struct FileState {
    head: Option<Vec<u8>>,
    current: Option<Vec<u8>>,
    touched: bool,
    read_error: Option<String>,
}

impl FileState {
    fn new_unread() -> Self {
        Self {
            head: None,
            current: None,
            touched: false,
            read_error: None,
        }
    }

    fn from_head(bytes: Option<Vec<u8>>) -> Self {
        Self {
            head: bytes.clone(),
            current: bytes,
            touched: false,
            read_error: None,
        }
    }

    fn read_error(msg: String) -> Self {
        Self {
            head: None,
            current: None,
            touched: false,
            read_error: Some(msg),
        }
    }

    fn set_current(&mut self, content: Option<Vec<u8>>) {
        self.current = content;
        self.touched = true;
    }

    fn into_change(self, path: String) -> Option<(String, Option<Vec<u8>>)> {
        if !self.touched || self.current == self.head {
            None
        } else {
            Some((path, self.current))
        }
    }
}

fn apply_str_replace(
    state: &mut HashMap<String, FileState>,
    path: &str,
    old_str: &str,
    new_str: &str,
) -> Result<(), SpaceError> {
    let entry = state.get_mut(path).expect("read_into populated this path");
    if let Some(msg) = entry.read_error.as_ref() {
        return Err(SpaceError::Validation(msg.clone()));
    }
    let current = entry.current.as_ref().ok_or_else(|| {
        SpaceError::Validation(format!("str_replace: file does not exist: {path}"))
    })?;
    let current_str = std::str::from_utf8(current).map_err(|e| {
        SpaceError::Validation(format!("str_replace: non-utf8 content in {path}: {e}"))
    })?;
    let count = current_str.matches(old_str).count();
    if count == 0 {
        return Err(SpaceError::Validation(format!(
            "str_replace: old_str not found in {path}"
        )));
    }
    if count > 1 {
        return Err(SpaceError::Validation(format!(
            "str_replace: old_str appears {count} times in {path}; must be unique"
        )));
    }
    let new_content = current_str.replacen(old_str, new_str, 1).into_bytes();
    entry.set_current(Some(new_content));
    Ok(())
}

fn apply_move(
    state: &mut HashMap<String, FileState>,
    from: &str,
    to: &str,
) -> Result<(), SpaceError> {
    if from == to {
        return Err(SpaceError::Validation(format!(
            "move: src and dest are the same: {from}"
        )));
    }
    let from_bytes = {
        let from_entry = state.get(from).expect("read_into populated this path");
        if let Some(msg) = from_entry.read_error.as_ref() {
            return Err(SpaceError::Validation(msg.clone()));
        }
        from_entry
            .current
            .clone()
            .ok_or_else(|| SpaceError::Validation(format!("move: src does not exist: {from}")))?
    };
    {
        let to_entry = state.get(to).expect("read_into populated this path");
        if let Some(msg) = to_entry.read_error.as_ref() {
            return Err(SpaceError::Validation(msg.clone()));
        }
        if to_entry.current.is_some() {
            return Err(SpaceError::Validation(format!(
                "move: dest already exists: {to}"
            )));
        }
    }
    state
        .get_mut(from)
        .expect("read_into populated this path")
        .set_current(None);
    state
        .get_mut(to)
        .expect("read_into populated this path")
        .set_current(Some(from_bytes));
    Ok(())
}

fn apply_insert(
    state: &mut HashMap<String, FileState>,
    path: &str,
    line_number: usize,
    text: &str,
) -> Result<(), SpaceError> {
    let entry = state.get_mut(path).expect("read_into populated this path");
    if let Some(msg) = entry.read_error.as_ref() {
        return Err(SpaceError::Validation(msg.clone()));
    }
    let current = entry
        .current
        .as_ref()
        .ok_or_else(|| SpaceError::Validation(format!("insert: file does not exist: {path}")))?;
    let current_str = std::str::from_utf8(current)
        .map_err(|e| SpaceError::Validation(format!("insert: non-utf8 content in {path}: {e}")))?;
    let mut lines: Vec<String> = current_str.lines().map(String::from).collect();
    let idx = line_number.min(lines.len());
    for (offset, t) in text.lines().enumerate() {
        lines.insert(idx + offset, t.to_string());
    }
    let mut new_content = lines.join("\n");
    if current_str.ends_with('\n') {
        new_content.push('\n');
    }
    entry.set_current(Some(new_content.into_bytes()));
    Ok(())
}
