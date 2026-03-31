use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use sqlite_vec::sqlite3_vec_init;
use zerocopy::AsBytes;

use crate::label_store::ChunkKey;
use crate::labeler::{ChunkClassification, ChunkData};
use crate::request_log::{LabelCount, QueryCount, RequestLogEntry, StatsResponse};

pub const DEFAULT_COLLECTION: &str = "code_chunks";
pub const VECTOR_DIM: u64 = 768;

/// SQLite-backed vector store for code chunks (using sqlite-vec).
pub struct VectorStore {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for VectorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorStore").finish_non_exhaustive()
    }
}

/// A chunk ready to be indexed (embedding already computed).
pub struct IndexedChunk {
    /// UUID string
    pub id: String,
    pub embedding: Vec<f32>,
    pub content: String,
    pub file_path: String,
    pub repo: String,
    pub chunk_type: String,
    pub entity_name: Option<String>,
    pub module_path: String,
    /// Language identifier: `rust`, `bats`, `bash`.
    pub language: String,
    pub line_start: usize,
    pub line_end: usize,
}

/// A point retrieved via the scroll API, carrying its ID and chunk data.
#[derive(Debug, Clone)]
pub struct ScrolledPoint {
    pub point_id: String,
    pub chunk: ChunkData,
}

/// A search result returned from the vector store.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub content: String,
    pub file_path: String,
    pub repo: String,
    pub chunk_type: String,
    pub entity_name: Option<String>,
    /// Language identifier: `rust`, `bats`, `bash`. Empty for legacy chunks.
    pub language: String,
    /// Primary architectural label (single, from new taxonomy).
    pub labels: Vec<String>,
    /// Architectural layer (domain/application/infrastructure/interface).
    pub layer: Option<String>,
    /// Content-derived usage tags.
    pub uses: Vec<String>,
    pub score: f32,
    pub line_start: usize,
    pub line_end: usize,
}

/// Re-export the canonical label lists from the labeler module.
pub use crate::labeler::{KNOWN_LAYERS, KNOWN_PRIMARY_LABELS, KNOWN_USES};

/// A chunk loaded for review in the TUI, including label metadata.
#[derive(Debug, Clone)]
pub struct ReviewChunk {
    /// Chunk ID (UUID string).
    pub id: String,
    pub content: String,
    pub file_path: String,
    pub repo: String,
    pub chunk_type: String,
    pub entity_name: Option<String>,
    pub language: String,
    pub line_start: usize,
    pub line_end: usize,
    /// Current labels on this chunk (0 or 1 primary label).
    pub labels: Vec<String>,
    /// Architectural layer (domain/application/infrastructure/interface).
    pub layer: Option<String>,
    /// Content-derived usage tags.
    pub uses: Vec<String>,
    /// "heuristic" or "human".
    pub label_source: String,
    /// Whether a human has reviewed this chunk.
    pub reviewed: bool,
}

impl ReviewChunk {
    /// Build a `ChunkKey` from this chunk's metadata.
    pub fn chunk_key(&self) -> ChunkKey {
        ChunkKey {
            repo: self.repo.clone(),
            file_path: self.file_path.clone(),
            chunk_type: self.chunk_type.clone(),
            entity_name: self.entity_name.clone().unwrap_or_default(),
        }
    }
}

/// A search result from the anti-patterns collection.
#[derive(Debug, Clone)]
pub struct AntiPatternResult {
    pub pattern_id: String,
    pub repo: String,
    pub pr_number: i64,
    pub reviewer: String,
    pub review_comment: String,
    pub before_code: String,
    pub after_code: String,
    pub has_fix: bool,
    pub similarity: f32,
}

/// Counts of labels grouped by origin (label_source).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LabelOriginCounts {
    pub human: i64,
    pub model: i64,
    pub heuristic: i64,
    pub total: i64,
}

/// Embedding cache entry stored alongside vector data.
pub struct CachedEmbedding {
    pub vector: Vec<f32>,
}

impl VectorStore {
    /// Open (or create) the SQLite database at the given path with sqlite-vec loaded.
    pub fn new(db_path: &Path) -> anyhow::Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut i8,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> i32,
            >(
                sqlite3_vec_init as *const ()
            )));
        }

        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Create tables if they don't already exist.
    pub fn ensure_collection(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS code_chunks USING vec0(
                chunk_id TEXT PRIMARY KEY,
                embedding float[{VECTOR_DIM}] distance_metric=cosine
            );"
        ))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chunk_metadata (
                chunk_id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                file_path TEXT NOT NULL,
                repo TEXT NOT NULL,
                chunk_type TEXT,
                entity_name TEXT,
                module_path TEXT,
                language TEXT,
                line_start INTEGER,
                line_end INTEGER
            );

            CREATE TABLE IF NOT EXISTS chunk_labels (
                chunk_id TEXT PRIMARY KEY,
                labels TEXT,
                layer TEXT,
                uses TEXT,
                label_source TEXT,
                reviewed INTEGER DEFAULT 0,
                label_confidence REAL,
                label_signals TEXT
            );

            CREATE TABLE IF NOT EXISTS embedding_cache (
                cache_key TEXT PRIMARY KEY,
                vector BLOB NOT NULL,
                model TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS request_log (
                id INTEGER PRIMARY KEY,
                ts TEXT NOT NULL,
                tool TEXT NOT NULL,
                query TEXT NOT NULL,
                filters TEXT,
                result_count INTEGER NOT NULL,
                top_score REAL,
                latency_ms INTEGER NOT NULL,
                error TEXT
            );",
        )?;

        Ok(())
    }

    /// Upsert chunks with their embeddings into the store.
    pub fn upsert_chunks(&self, chunks: Vec<IndexedChunk>) -> anyhow::Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }

        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let tx = conn.unchecked_transaction()?;

        for c in &chunks {
            // Upsert into vec0 table (DELETE + INSERT since vec0 doesn't support UPDATE)
            tx.execute("DELETE FROM code_chunks WHERE chunk_id = ?1", [&c.id])?;
            tx.execute(
                "INSERT INTO code_chunks(chunk_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![c.id, c.embedding.as_bytes()],
            )?;

            // Upsert metadata
            tx.execute(
                "INSERT OR REPLACE INTO chunk_metadata
                    (chunk_id, content, file_path, repo, chunk_type, entity_name,
                     module_path, language, line_start, line_end)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    c.id,
                    c.content,
                    c.file_path,
                    c.repo,
                    c.chunk_type,
                    c.entity_name.as_deref().unwrap_or(""),
                    c.module_path,
                    c.language,
                    c.line_start as i64,
                    c.line_end as i64,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Search for similar chunks by embedding vector.
    #[allow(clippy::too_many_arguments)]
    pub fn search(
        &self,
        query_embedding: Vec<f32>,
        limit: u64,
        repo_filter: Option<&str>,
        language_filter: Option<&str>,
        label_filter: Option<&str>,
        layer_filter: Option<&str>,
        uses_filter: Option<&str>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        // Build dynamic WHERE clause for metadata/label filters
        let mut conditions = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        // When filters are active, over-fetch from KNN since post-filtering
        // may discard most results. The final output is LIMITed to the
        // requested count.
        let has_filters = repo_filter.is_some()
            || language_filter.is_some()
            || label_filter.is_some()
            || layer_filter.is_some()
            || uses_filter.is_some();
        let knn_k = if has_filters { 1000.max(limit) } else { limit };

        // First two params are the query embedding and k
        params.push(Box::new(query_embedding.as_bytes().to_vec()));
        params.push(Box::new(knn_k as i64));

        if let Some(repo) = repo_filter {
            conditions.push(format!("m.repo = ?{}", params.len() + 1));
            params.push(Box::new(repo.to_string()));
        }
        if let Some(lang) = language_filter {
            conditions.push(format!("m.language = ?{}", params.len() + 1));
            params.push(Box::new(lang.to_string()));
        }
        if let Some(label) = label_filter {
            if label == "none" {
                conditions
                    .push("(l.labels IS NULL OR l.labels = '[]' OR l.labels = '')".to_string());
            } else {
                conditions.push(format!("l.labels LIKE '%' || ?{} || '%'", params.len() + 1));
                params.push(Box::new(format!("\"{}\"", label)));
            }
        }
        if let Some(layer) = layer_filter {
            conditions.push(format!("l.layer = ?{}", params.len() + 1));
            params.push(Box::new(layer.to_string()));
        }
        if let Some(uses) = uses_filter {
            conditions.push(format!("l.uses LIKE '%' || ?{} || '%'", params.len() + 1));
            params.push(Box::new(format!("\"{}\"", uses)));
        }

        let filter_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        // Use a CTE for KNN, then filter + join
        let sql = format!(
            "WITH knn AS (
                SELECT chunk_id, distance
                FROM code_chunks
                WHERE embedding MATCH ?1 AND k = ?2
            )
            SELECT
                m.chunk_id,
                m.content,
                m.file_path,
                m.repo,
                m.chunk_type,
                m.entity_name,
                m.language,
                m.line_start,
                m.line_end,
                knn.distance,
                COALESCE(l.labels, '[]') as labels,
                l.layer,
                COALESCE(l.uses, '[]') as uses
            FROM knn
            JOIN chunk_metadata m ON m.chunk_id = knn.chunk_id
            LEFT JOIN chunk_labels l ON l.chunk_id = knn.chunk_id
            {filter_clause}
            ORDER BY knn.distance ASC
            LIMIT {dedup_limit}",
            dedup_limit = limit * 2
        );

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| &**p).collect();

        let mut stmt = conn.prepare(&sql)?;
        let results = stmt
            .query_map(param_refs.as_slice(), |row| {
                let labels_json: String = row.get(10)?;
                let layer: Option<String> = row.get(11)?;
                let uses_json: String = row.get(12)?;
                let distance: f64 = row.get(9)?;
                let entity_name: String = row.get::<_, String>(5)?;

                Ok(SearchResult {
                    content: row.get(1)?,
                    file_path: row.get(2)?,
                    repo: row.get(3)?,
                    chunk_type: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    entity_name: if entity_name.is_empty() {
                        None
                    } else {
                        Some(entity_name)
                    },
                    language: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    labels: parse_json_string_array(&labels_json),
                    layer: layer.filter(|s| !s.is_empty()),
                    uses: parse_json_string_array(&uses_json),
                    // cosine distance → similarity: score = 1.0 - distance
                    score: 1.0 - distance as f32,
                    line_start: row.get::<_, i64>(7)? as usize,
                    line_end: row.get::<_, i64>(8)? as usize,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Deduplicate by code body — identical or near-identical code from
        // different files is not useful to show twice. Strip the leading
        // comment line (e.g. "// impl Foo") before comparing since the chunker
        // prepends it as context.
        let mut seen = HashSet::new();
        let results: Vec<_> = results
            .into_iter()
            .filter(|r| {
                let body = r
                    .content
                    .strip_prefix("// impl ")
                    .and_then(|s| s.find('\n').map(|i| &s[i + 1..]))
                    .unwrap_or(&r.content);
                seen.insert(body.to_string())
            })
            .take(limit as usize)
            .collect();

        Ok(results)
    }

    /// Scroll through all points in the collection, returning chunk data
    /// suitable for labeling.
    pub fn scroll_all(&self) -> anyhow::Result<Vec<ScrolledPoint>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut stmt = conn.prepare(
            "SELECT chunk_id, content, file_path, chunk_type, entity_name
             FROM chunk_metadata",
        )?;

        let points = stmt
            .query_map([], |row| {
                let entity_name: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();
                Ok(ScrolledPoint {
                    point_id: row.get(0)?,
                    chunk: ChunkData {
                        content: row.get(1)?,
                        file_path: row.get(2)?,
                        chunk_type: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        entity_name,
                    },
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(points)
    }

    /// Update classification payload fields on a batch of points.
    ///
    /// Each update is `(chunk_id, classification, label_source)` where source is
    /// `"ml"`, `"heuristic"`, etc.
    pub fn set_labels(
        &self,
        updates: &[(String, ChunkClassification, String)],
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let tx = conn.unchecked_transaction()?;

        for (point_id, cls, label_source) in updates {
            let labels: Vec<String> = cls
                .primary_label
                .as_ref()
                .map(|l| vec![l.clone()])
                .unwrap_or_default();
            let labels_json = serde_json::to_string(&labels)?;
            let uses_json = serde_json::to_string(&cls.uses)?;
            let signals_json = serde_json::to_string(&cls.primary_signals)?;

            tx.execute(
                "INSERT OR REPLACE INTO chunk_labels
                    (chunk_id, labels, layer, uses, label_source, reviewed,
                     label_confidence, label_signals)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    point_id,
                    labels_json,
                    cls.layer,
                    uses_json,
                    label_source,
                    false,
                    cls.primary_confidence,
                    signals_json,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Delete all chunks belonging to a specific repo (for re-indexing).
    pub fn delete_repo(&self, repo: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let tx = conn.unchecked_transaction()?;

        // Get chunk IDs for this repo
        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT chunk_id FROM chunk_metadata WHERE repo = ?1")?;
            let result = stmt
                .query_map([repo], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            result
        };

        for id in &ids {
            tx.execute("DELETE FROM code_chunks WHERE chunk_id = ?1", [id])?;
            tx.execute("DELETE FROM chunk_metadata WHERE chunk_id = ?1", [id])?;
            tx.execute("DELETE FROM chunk_labels WHERE chunk_id = ?1", [id])?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Scroll through all chunks in the collection for review.
    /// Returns all chunks with their label metadata.
    pub fn scroll_all_chunks(&self) -> anyhow::Result<Vec<ReviewChunk>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut stmt = conn.prepare(
            "SELECT
                m.chunk_id,
                m.content,
                m.file_path,
                m.repo,
                m.chunk_type,
                m.entity_name,
                m.language,
                m.line_start,
                m.line_end,
                COALESCE(l.labels, '[]') as labels,
                l.layer,
                COALESCE(l.uses, '[]') as uses,
                COALESCE(l.label_source, '') as label_source,
                COALESCE(l.reviewed, 0) as reviewed
             FROM chunk_metadata m
             LEFT JOIN chunk_labels l ON l.chunk_id = m.chunk_id",
        )?;

        let chunks = stmt
            .query_map([], |row| {
                let entity_name: String = row.get::<_, Option<String>>(5)?.unwrap_or_default();
                let labels_json: String = row.get(9)?;
                let layer: Option<String> = row.get(10)?;
                let uses_json: String = row.get(11)?;
                let reviewed_int: i64 = row.get(13)?;

                Ok(ReviewChunk {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    file_path: row.get(2)?,
                    repo: row.get(3)?,
                    chunk_type: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    entity_name: if entity_name.is_empty() {
                        None
                    } else {
                        Some(entity_name)
                    },
                    language: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    line_start: row.get::<_, i64>(7)? as usize,
                    line_end: row.get::<_, i64>(8)? as usize,
                    labels: parse_json_string_array(&labels_json),
                    layer: layer.filter(|s| !s.is_empty()),
                    uses: parse_json_string_array(&uses_json),
                    label_source: row.get(12)?,
                    reviewed: reviewed_int != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(chunks)
    }

    /// Update a chunk's labels after human review.
    pub fn set_chunk_labels(
        &self,
        point_id: &str,
        labels: &[String],
        label_source: &str,
        reviewed: bool,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let labels_json = serde_json::to_string(labels)?;

        conn.execute(
            "INSERT INTO chunk_labels (chunk_id, labels, label_source, reviewed)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chunk_id) DO UPDATE SET
                labels = excluded.labels,
                label_source = excluded.label_source,
                reviewed = excluded.reviewed",
            rusqlite::params![point_id, labels_json, label_source, reviewed as i64],
        )?;

        Ok(())
    }

    /// Look up a cached embedding by cache key. Returns None if not found.
    pub fn get_cached_embedding(&self, cache_key: &str) -> anyhow::Result<Option<CachedEmbedding>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut stmt = conn.prepare("SELECT vector FROM embedding_cache WHERE cache_key = ?1")?;

        let result = stmt
            .query_row([cache_key], |row| {
                let blob: Vec<u8> = row.get(0)?;
                Ok(blob)
            })
            .optional()?;

        match result {
            Some(blob) => {
                let vector = bytes_to_f32_vec(&blob);
                Ok(Some(CachedEmbedding { vector }))
            }
            None => Ok(None),
        }
    }

    /// Store an embedding in the cache.
    pub fn put_cached_embedding(
        &self,
        cache_key: &str,
        vector: &[f32],
        model: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        let now = {
            use std::time::SystemTime;
            let dur = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            format!("{}", dur.as_secs())
        };

        conn.execute(
            "INSERT OR REPLACE INTO embedding_cache (cache_key, vector, model, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![cache_key, vector.as_bytes(), model, now],
        )?;

        Ok(())
    }

    /// Return the total number of chunks in the metadata table.
    pub fn chunk_count(&self) -> anyhow::Result<u64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM chunk_metadata", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    /// Count labelled chunks grouped by their origin (`label_source`).
    pub fn label_origin_counts(&self) -> anyhow::Result<LabelOriginCounts> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut stmt = conn.prepare(
            "SELECT COALESCE(label_source, '') AS src, COUNT(*) AS cnt
             FROM chunk_labels
             WHERE labels IS NOT NULL AND labels != '[]' AND labels != ''
             GROUP BY src",
        )?;

        let mut counts = LabelOriginCounts::default();
        let rows = stmt.query_map([], |row| {
            let source: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((source, count))
        })?;

        for row in rows {
            let (source, count) = row?;
            match source.as_str() {
                "human" => counts.human = count,
                "ml" | "model" => counts.model += count,
                "heuristic" => counts.heuristic = count,
                _ => {}
            }
            counts.total += count;
        }

        Ok(counts)
    }

    /// Ensure anti-pattern tables exist (for server startup without full ReviewMiner).
    pub fn ensure_anti_pattern_tables(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS anti_patterns_vec USING vec0(
                pattern_id TEXT PRIMARY KEY,
                embedding float[{VECTOR_DIM}] distance_metric=cosine
            );"
        ))?;

        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS anti_patterns_code_vec USING vec0(
                pattern_id TEXT PRIMARY KEY,
                embedding float[{VECTOR_DIM}] distance_metric=cosine
            );"
        ))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS anti_patterns_meta (
                pattern_id TEXT PRIMARY KEY,
                repo TEXT NOT NULL,
                pr_number INTEGER,
                reviewer TEXT,
                review_comment TEXT,
                file_path TEXT,
                before_code TEXT,
                after_code TEXT,
                diff_hunk TEXT,
                has_fix TEXT,
                created_at TEXT
            );",
        )?;

        Ok(())
    }

    /// Return the total number of anti-patterns in the meta table.
    pub fn anti_pattern_count(&self) -> anyhow::Result<u64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM anti_patterns_meta", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);
        Ok(count as u64)
    }

    /// Search anti-patterns by review comment + code embeddings.
    pub fn search_anti_patterns(
        &self,
        query_embedding: Vec<f32>,
        limit: u64,
    ) -> anyhow::Result<Vec<AntiPatternResult>> {
        self.search_anti_patterns_table("anti_patterns_vec", query_embedding, limit)
    }

    /// Search anti-patterns by code-only embeddings (code-to-code matching).
    pub fn search_anti_patterns_by_code(
        &self,
        query_embedding: Vec<f32>,
        limit: u64,
    ) -> anyhow::Result<Vec<AntiPatternResult>> {
        self.search_anti_patterns_table("anti_patterns_code_vec", query_embedding, limit)
    }

    /// Shared implementation for anti-pattern KNN search on a given vec table.
    fn search_anti_patterns_table(
        &self,
        vec_table: &str,
        query_embedding: Vec<f32>,
        limit: u64,
    ) -> anyhow::Result<Vec<AntiPatternResult>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        let sql = format!(
            "WITH knn AS (
                SELECT pattern_id, distance
                FROM {vec_table}
                WHERE embedding MATCH ?1 AND k = ?2
            )
            SELECT
                m.pattern_id,
                m.repo,
                m.pr_number,
                m.reviewer,
                m.review_comment,
                m.before_code,
                m.after_code,
                m.has_fix,
                knn.distance
            FROM knn
            JOIN anti_patterns_meta m ON m.pattern_id = knn.pattern_id
            ORDER BY knn.distance ASC"
        );

        let mut stmt = conn.prepare(&sql)?;
        let results = stmt
            .query_map(
                rusqlite::params![query_embedding.as_bytes(), limit as i64],
                |row| {
                    let distance: f64 = row.get(8)?;
                    let has_fix_str: String = row.get::<_, Option<String>>(7)?.unwrap_or_default();
                    Ok(AntiPatternResult {
                        pattern_id: row.get(0)?,
                        repo: row.get(1)?,
                        pr_number: row.get(2)?,
                        reviewer: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        review_comment: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        before_code: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                        after_code: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                        has_fix: has_fix_str == "true",
                        similarity: 1.0 - distance as f32,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(results)
    }

    /// Insert a request log entry. Intended to be called fire-and-forget.
    pub fn log_request(&self, entry: &RequestLogEntry) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "INSERT INTO request_log (ts, tool, query, filters, result_count, top_score, latency_ms, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                entry.ts,
                entry.tool,
                entry.query,
                entry.filters,
                entry.result_count,
                entry.top_score,
                entry.latency_ms,
                entry.error,
            ],
        )?;
        Ok(())
    }

    /// Query aggregated stats from the request_log table.
    pub fn query_stats(&self, low_score_threshold: f64) -> anyhow::Result<StatsResponse> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        let count_since = |interval: &str| -> anyhow::Result<i64> {
            let sql = format!(
                "SELECT COUNT(*) FROM request_log WHERE ts >= datetime('now', '-{interval}')"
            );
            let n: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
            Ok(n)
        };

        let total_24h = count_since("1 day")?;
        let total_7d = count_since("7 days")?;
        let total_30d = count_since("30 days")?;

        // Empty result rate (last 30 days)
        let empty_result_rate: f64 = conn.query_row(
            "SELECT COALESCE(
                CAST(SUM(CASE WHEN result_count = 0 THEN 1 ELSE 0 END) AS REAL)
                / NULLIF(COUNT(*), 0), 0.0)
             FROM request_log
             WHERE ts >= datetime('now', '-30 days') AND error IS NULL",
            [],
            |row| row.get(0),
        )?;

        // Low score rate (last 30 days, only successful searches with results)
        let low_score_rate: f64 = conn.query_row(
            "SELECT COALESCE(
                CAST(SUM(CASE WHEN top_score IS NOT NULL AND top_score < ?1 THEN 1 ELSE 0 END) AS REAL)
                / NULLIF(SUM(CASE WHEN top_score IS NOT NULL THEN 1 ELSE 0 END), 0), 0.0)
             FROM request_log
             WHERE ts >= datetime('now', '-30 days') AND error IS NULL AND result_count > 0",
            rusqlite::params![low_score_threshold],
            |row| row.get(0),
        )?;

        // Error rate (last 30 days)
        let error_rate: f64 = conn.query_row(
            "SELECT COALESCE(
                CAST(SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END) AS REAL)
                / NULLIF(COUNT(*), 0), 0.0)
             FROM request_log
             WHERE ts >= datetime('now', '-30 days')",
            [],
            |row| row.get(0),
        )?;

        // Average latency (last 30 days)
        let avg_latency_ms: f64 = conn.query_row(
            "SELECT COALESCE(AVG(latency_ms), 0.0)
             FROM request_log
             WHERE ts >= datetime('now', '-30 days')",
            [],
            |row| row.get(0),
        )?;

        // Top queries (last 30 days)
        let top_queries = {
            let mut stmt = conn.prepare(
                "SELECT query, COUNT(*) as cnt
                 FROM request_log
                 WHERE ts >= datetime('now', '-30 days')
                 GROUP BY query
                 ORDER BY cnt DESC
                 LIMIT 20",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(QueryCount {
                    query: row.get(0)?,
                    count: row.get(1)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        // Top label filters (last 30 days, extracted from JSON filters)
        let top_labels = {
            let mut stmt = conn.prepare(
                "SELECT json_extract(filters, '$.label') as lbl, COUNT(*) as cnt
                 FROM request_log
                 WHERE ts >= datetime('now', '-30 days')
                   AND json_extract(filters, '$.label') IS NOT NULL
                 GROUP BY lbl
                 ORDER BY cnt DESC
                 LIMIT 20",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(LabelCount {
                    label: row.get(0)?,
                    count: row.get(1)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        Ok(StatsResponse {
            total_requests_24h: total_24h,
            total_requests_7d: total_7d,
            total_requests_30d: total_30d,
            empty_result_rate,
            low_score_rate,
            low_score_threshold,
            error_rate,
            avg_latency_ms,
            top_queries,
            top_labels,
        })
    }
}

/// Parse a JSON array of strings, returning an empty vec on any parse failure.
fn parse_json_string_array(json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(json).unwrap_or_default()
}

/// Convert a byte slice back to a Vec<f32>.
fn bytes_to_f32_vec(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Trait extension for optional query results.
trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
